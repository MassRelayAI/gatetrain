//! `gatetrain metrics`: the minutes-lost view. Where did the last N batches spend time that a faster chain would not have spent?
//!
//! Read-only over the ledger (`runs`, `steps`, `step_shards`, `batch_events`). Every number is measured from a row; a step with no
//! duration is left out, never invented. Minutes lost are three kinds, each attributed to the CAUSE (the step that went red, or the
//! reason a chain was stopped):
//! - `red`: the time inside chains that ended red (the whole chain is thrown away);
//! - `restart`: the time inside chains that were stopped or errored (a restart);
//! - `wait`: from the end of such a chain to the start of the next chain of the same batch (the fix-to-next-chain wait).
//!
//! The critical path of a green chain is the step whose duration is closest to the chain's span, which for a step that cannot be longer
//! than its chain is the longest step.
use crate::batch::parse_iso;
use crate::ledger::Ledger;
use crate::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lost {
    pub red_min: f64,
    pub restart_min: f64,
    pub wait_min: f64,
    pub chains: usize,
}

impl Lost {
    pub fn total(&self) -> f64 {
        self.red_min + self.restart_min + self.wait_min
    }
    fn add(&mut self, o: &Lost) {
        self.red_min += o.red_min;
        self.restart_min += o.restart_min;
        self.wait_min += o.wait_min;
        self.chains += o.chains;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchRow {
    pub batch: String,
    pub chains: usize,
    pub red: usize,
    pub green: usize,
    pub other: usize,
    /// `restack`, `retry`, `abandon` and `T6a` events that finished, from `batch_events`
    pub restart_events: usize,
    pub lost: Lost,
    pub top_cause: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepRow {
    pub step: String,
    pub ran: usize,
    pub cached: usize,
    pub red: usize,
    pub median_s: Option<f64>,
    pub max_s: Option<f64>,
    /// green chains in which this step was the critical path
    pub critical: usize,
    /// (run, step) groups that have shard rows
    pub shard_groups: usize,
    pub avg_shards: Option<f64>,
    pub avg_slowest_s: Option<f64>,
    pub avg_mean_s: Option<f64>,
    /// slowest shard over mean shard, averaged over the groups: 1.0 is perfectly even
    pub skew: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CauseRow {
    pub cause: String,
    pub lost: Lost,
}

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub batches: Vec<BatchRow>,
    pub steps: Vec<StepRow>,
    pub causes: Vec<CauseRow>,
    pub total: Lost,
    pub chains: usize,
    pub red: usize,
}

struct Chain {
    id: String,
    batch: String,
    start: Option<i64>,
    end: Option<i64>,
    outcome: String,
    stop_reason: Option<String>,
}

struct StepRec {
    name: String,
    dur: Option<f64>,
    exit: Option<i64>,
    cached: bool,
    started: Option<i64>,
    ended: Option<i64>,
}

fn mins(s: i64) -> f64 {
    s as f64 / 60.0
}

/// Compute the view over the last `last` batches (by the start of their first chain; 0 = all).
pub fn compute(l: &Ledger, last: usize) -> Result<Metrics> {
    l.require_current()?;
    let mut st = l.conn.prepare("SELECT id, batch, started_at, ended_at, COALESCE(outcome,''), stop_reason FROM runs WHERE kind='chain' AND batch IS NOT NULL ORDER BY started_at, id")?;
    let all: Vec<Chain> = st
        .query_map([], |r| {
            let (s, e): (String, Option<String>) = (r.get(2)?, r.get(3)?);
            Ok(Chain { id: r.get(0)?, batch: r.get(1)?, start: parse_iso(&s), end: e.as_deref().and_then(parse_iso), outcome: r.get(4)?, stop_reason: r.get(5)? })
        })?
        .collect::<std::result::Result<_, _>>()?;
    // batches in order of first appearance, then the last N of them
    let mut order: Vec<String> = vec![];
    for c in &all {
        if !order.contains(&c.batch) {
            order.push(c.batch.clone());
        }
    }
    if last > 0 && order.len() > last {
        order = order.split_off(order.len() - last);
    }
    let chains: Vec<&Chain> = all.iter().filter(|c| order.contains(&c.batch)).collect();

    let mut steps_of: BTreeMap<String, Vec<StepRec>> = BTreeMap::new();
    let mut sst = l.conn.prepare("SELECT name, started_at, ended_at, duration_sec, exit_code, COALESCE(cached,0), cached_from FROM steps WHERE run_id=? ORDER BY COALESCE(started_at,''), name")?;
    for c in &chains {
        let rows: Vec<StepRec> = sst
            .query_map([&c.id], |r| {
                let (s, e): (Option<String>, Option<String>) = (r.get(1)?, r.get(2)?);
                let cf: Option<String> = r.get(6)?;
                Ok(StepRec { name: r.get(0)?, dur: r.get(3)?, exit: r.get(4)?, cached: r.get::<_, i64>(5)? != 0 || cf.is_some(), started: s.as_deref().and_then(parse_iso), ended: e.as_deref().and_then(parse_iso) })
            })?
            .collect::<std::result::Result<_, _>>()?;
        steps_of.insert(c.id.clone(), rows);
    }

    let mut causes: BTreeMap<String, Lost> = BTreeMap::new();
    let mut batch_rows = vec![];
    let mut step_acc: BTreeMap<String, StepRow> = BTreeMap::new();
    let mut durs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut total = Lost::default();
    let (mut n_chains, mut n_red) = (0, 0);

    for b in &order {
        let mine: Vec<&&Chain> = chains.iter().filter(|c| &c.batch == b).collect();
        let mut row = BatchRow { batch: b.clone(), chains: mine.len(), red: 0, green: 0, other: 0, restart_events: 0, lost: Lost::default(), top_cause: None };
        let mut bcauses: BTreeMap<String, f64> = BTreeMap::new();
        for (i, c) in mine.iter().enumerate() {
            let steps = &steps_of[&c.id];
            match c.outcome.as_str() {
                "green" => row.green += 1,
                "red" => row.red += 1,
                _ => row.other += 1,
            }
            for s in steps {
                let a = step_acc.entry(s.name.clone()).or_insert_with(|| StepRow { step: s.name.clone(), ..Default::default() });
                if s.cached {
                    a.cached += 1;
                } else {
                    a.ran += 1;
                    if let Some(d) = s.dur {
                        durs.entry(s.name.clone()).or_default().push(d);
                    }
                }
                if s.exit.is_some_and(|x| x != 0) && !s.cached {
                    a.red += 1;
                }
            }
            let span = match (c.start, c.end) {
                (Some(s), Some(e)) if e >= s => Some(e - s),
                _ => None,
            };
            if c.outcome == "green" {
                // the critical path: the step that ran whose duration is closest to the span
                if let Some(span) = span {
                    let best = steps.iter().filter(|s| !s.cached).filter_map(|s| s.dur.map(|d| (s, (span as f64 - d).abs()))).min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.name.cmp(&b.0.name)));
                    if let Some((s, _)) = best {
                        step_acc.get_mut(&s.name).unwrap().critical += 1;
                    }
                }
                continue;
            }
            let is_red = c.outcome == "red";
            if !is_red && !matches!(c.outcome.as_str(), "stopped" | "error") {
                continue; // still running: nothing is lost yet
            }
            // the cause: the first red step (by start), else the step still open, else the recorded reason
            let cause = steps
                .iter()
                .filter(|s| s.exit.is_some_and(|x| x != 0) && !s.cached)
                .min_by_key(|s| s.started.unwrap_or(i64::MAX))
                .map(|s| s.name.clone())
                .or_else(|| steps.iter().find(|s| s.started.is_some() && s.ended.is_none()).map(|s| format!("{} (open at stop)", s.name)))
                .or_else(|| c.stop_reason.clone().filter(|r| !r.is_empty()))
                .unwrap_or_else(|| "(no red step recorded)".into());
            let mut l1 = Lost { chains: 1, ..Default::default() };
            if let Some(sp) = span {
                if is_red {
                    l1.red_min = mins(sp);
                } else {
                    l1.restart_min = mins(sp);
                }
            }
            // the fix-to-next-chain wait
            if let (Some(e), Some(next)) = (c.end, mine.get(i + 1).and_then(|n| n.start)) {
                if next >= e {
                    l1.wait_min = mins(next - e);
                }
            }
            row.lost.add(&l1);
            total.add(&l1);
            causes.entry(cause.clone()).or_default().add(&l1);
            *bcauses.entry(cause).or_default() += l1.total();
        }
        row.top_cause = bcauses.into_iter().max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0))).map(|x| x.0);
        row.restart_events = l.conn.query_row("SELECT COUNT(*) FROM batch_events WHERE batch=? AND phase='done' AND transition IN ('restack','retry','abandon','T6a')", [b], |r| r.get::<_, i64>(0))? as usize;
        n_chains += row.chains;
        n_red += row.red;
        batch_rows.push(row);
    }

    // shards: slowest versus mean, per (run, step)
    let mut sh = l.conn.prepare("SELECT step, COUNT(*), MAX(duration_sec), AVG(duration_sec) FROM step_shards WHERE run_id=? AND duration_sec IS NOT NULL GROUP BY step")?;
    let mut shard_acc: BTreeMap<String, (usize, f64, f64, f64, f64)> = BTreeMap::new();
    for c in &chains {
        let rows: Vec<(String, i64, f64, f64)> = sh.query_map([&c.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?;
        for (step, n, mx, mean) in rows {
            let e = shard_acc.entry(step).or_default();
            e.0 += 1;
            e.1 += n as f64;
            e.2 += mx;
            e.3 += mean;
            e.4 += if mean > 0.0 { mx / mean } else { 1.0 };
        }
    }
    let mut steps: Vec<StepRow> = step_acc.into_values().collect();
    for s in &mut steps {
        if let Some(d) = durs.get(&s.step) {
            s.median_s = Some(crate::stats::median(d));
            s.max_s = d.iter().cloned().max_by(f64::total_cmp);
        }
        if let Some((n, sum_n, sum_mx, sum_mean, sum_skew)) = shard_acc.get(&s.step) {
            let n_f = *n as f64;
            s.shard_groups = *n;
            s.avg_shards = Some(sum_n / n_f);
            s.avg_slowest_s = Some(sum_mx / n_f);
            s.avg_mean_s = Some(sum_mean / n_f);
            s.skew = Some(sum_skew / n_f);
        }
    }
    steps.sort_by(|a, b| b.max_s.unwrap_or(0.0).total_cmp(&a.max_s.unwrap_or(0.0)).then(a.step.cmp(&b.step)));
    let mut causes: Vec<CauseRow> = causes.into_iter().map(|(cause, lost)| CauseRow { cause, lost }).collect();
    causes.sort_by(|a, b| b.lost.total().total_cmp(&a.lost.total()).then(a.cause.cmp(&b.cause)));
    Ok(Metrics { batches: batch_rows, steps, causes, total, chains: n_chains, red: n_red })
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn opt(x: Option<f64>) -> Value {
    x.map(|v| json!(r1(v))).unwrap_or(Value::Null)
}

fn lost_json(l: &Lost) -> Value {
    json!({"red_min": r1(l.red_min), "restart_min": r1(l.restart_min), "wait_min": r1(l.wait_min), "total_min": r1(l.total()), "chains": l.chains})
}

pub fn to_json(m: &Metrics) -> Value {
    json!({
        "chains": m.chains, "red_chains": m.red, "lost": lost_json(&m.total),
        "batches": m.batches.iter().map(|b| json!({"batch": b.batch, "chains": b.chains, "red": b.red, "green": b.green, "other": b.other, "restart_events": b.restart_events, "lost": lost_json(&b.lost), "top_cause": b.top_cause})).collect::<Vec<_>>(),
        "steps": m.steps.iter().map(|s| json!({"step": s.step, "ran": s.ran, "cached": s.cached, "red": s.red, "median_s": opt(s.median_s), "max_s": opt(s.max_s), "critical": s.critical, "shard_groups": s.shard_groups, "avg_shards": opt(s.avg_shards), "avg_slowest_s": opt(s.avg_slowest_s), "avg_mean_s": opt(s.avg_mean_s), "skew": s.skew.map(|v| (v * 100.0).round() / 100.0)})).collect::<Vec<_>>(),
        "causes": m.causes.iter().map(|c| json!({"cause": c.cause, "lost": lost_json(&c.lost)})).collect::<Vec<_>>(),
    })
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn num(x: Option<f64>) -> String {
    x.map(|v| format!("{v:.1}")).unwrap_or_default()
}

/// Three tables: (`batches`, `steps`, `causes`), each as CSV text with a header row.
pub fn to_csv(m: &Metrics) -> [(&'static str, String); 3] {
    let mut b = String::from("batch,chains,red,green,other,restart_events,red_min,restart_min,wait_min,total_min,top_cause\n");
    for r in &m.batches {
        b += &format!("{},{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{}\n", csv_field(&r.batch), r.chains, r.red, r.green, r.other, r.restart_events, r.lost.red_min, r.lost.restart_min, r.lost.wait_min, r.lost.total(), csv_field(r.top_cause.as_deref().unwrap_or("")));
    }
    let mut s = String::from("step,ran,cached,red,median_s,max_s,critical,shard_groups,avg_shards,avg_slowest_s,avg_mean_s,skew\n");
    for r in &m.steps {
        s += &format!("{},{},{},{},{},{},{},{},{},{},{},{}\n", csv_field(&r.step), r.ran, r.cached, r.red, num(r.median_s), num(r.max_s), r.critical, r.shard_groups, num(r.avg_shards), num(r.avg_slowest_s), num(r.avg_mean_s), r.skew.map(|v| format!("{v:.2}")).unwrap_or_default());
    }
    let mut c = String::from("cause,chains,red_min,restart_min,wait_min,total_min\n");
    for r in &m.causes {
        c += &format!("{},{},{:.1},{:.1},{:.1},{:.1}\n", csv_field(&r.cause), r.lost.chains, r.lost.red_min, r.lost.restart_min, r.lost.wait_min, r.lost.total());
    }
    [("batches", b), ("steps", s), ("causes", c)]
}

pub fn to_text(m: &Metrics) -> String {
    let mut o = format!(
        "minutes lost: {:.1} min in {} red/stopped chain(s) of {} chain(s) over {} batch(es): red {:.1}, restart {:.1}, wait {:.1}\n",
        m.total.total(),
        m.total.chains,
        m.chains,
        m.batches.len(),
        m.total.red_min,
        m.total.restart_min,
        m.total.wait_min
    );
    o += "\ncauses (minutes lost, largest first)\n";
    for c in &m.causes {
        o += &format!("  {:>7.1} min  {:<28} {} chain(s): red {:.1}, restart {:.1}, wait {:.1}\n", c.lost.total(), c.cause, c.lost.chains, c.lost.red_min, c.lost.restart_min, c.lost.wait_min);
    }
    if m.causes.is_empty() {
        o += "  none: no chain ended red or stopped\n";
    }
    o += "\nbatches\n";
    for b in &m.batches {
        o += &format!(
            "  {:<24} chains {} (red {}, green {}, other {}), restart events {}, lost {:.1} min{}\n",
            b.batch,
            b.chains,
            b.red,
            b.green,
            b.other,
            b.restart_events,
            b.lost.total(),
            b.top_cause.as_ref().map(|c| format!(", mostly {c}")).unwrap_or_default()
        );
    }
    o += "\nsteps (longest first)\n";
    for s in &m.steps {
        o += &format!(
            "  {:<24} ran {} cached {} red {} median {} max {} critical {}",
            s.step,
            s.ran,
            s.cached,
            s.red,
            s.median_s.map(crate::stats::fmt_dur).unwrap_or_else(|| "-".into()),
            s.max_s.map(crate::stats::fmt_dur).unwrap_or_else(|| "-".into()),
            s.critical
        );
        if let (Some(sl), Some(mn), Some(k)) = (s.avg_slowest_s, s.avg_mean_s, s.skew) {
            o += &format!(" | shards {:.1}: slowest {} vs mean {} (skew {:.2})", s.avg_shards.unwrap_or(0.0), crate::stats::fmt_dur(sl), crate::stats::fmt_dur(mn), k);
        }
        o += "\n";
    }
    o
}
