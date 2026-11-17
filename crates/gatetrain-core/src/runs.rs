//! Runs in the ledger: `ingest`, reading a run back, `flaky`.
//! A finished run is inserted into `runs` and `steps` once. A run still in progress is kept as snapshots only
//! (design 3.3), so nothing is ever deleted: `--replace` adds a newer snapshot.
use crate::ledger::Ledger;
use crate::logfmt::{parse_iso, ParsedLog, Step, Ts};
use rusqlite::{params, OptionalExtension};

pub struct Loaded {
    pub outcome: String,
    pub commit: Option<String>,
    pub host: Option<String>,
    pub steps: Vec<Step>,
}

/// The outcome of a run id, from the finished rows or else the newest snapshot.
pub fn outcome_of(l: &Ledger, rid: &str) -> crate::Result<Option<String>> {
    if let Some(o) = l.conn.query_row("SELECT outcome FROM runs WHERE id=?", [rid], |r| r.get::<_, Option<String>>(0)).optional()? {
        return Ok(Some(o.unwrap_or_default()));
    }
    Ok(l.conn.query_row("SELECT outcome FROM run_current WHERE run_id=?", [rid], |r| r.get::<_, Option<String>>(0)).optional()?.map(|o| o.unwrap_or_default()))
}

/// What `ingest` did, or why it refused (the text goes to stderr, exit 2).
pub enum Ingest {
    Done,
    Refused(String),
}

#[allow(clippy::too_many_arguments)]
pub fn ingest(l: &Ledger, db_name: &str, rid: &str, p: &ParsedLog, outcome: &str, host: Option<&str>, replace: bool, taken_at: &str) -> crate::Result<Ingest> {
    l.require_current()?;
    if let Some(old) = outcome_of(l, rid)? {
        if !replace {
            return Ok(Ingest::Refused(format!("refused: run {rid} is already in {db_name} (append-only); use --replace only for a still-running run")));
        }
        if old != "running" && old != "stopped" {
            return Ok(Ingest::Refused(format!("refused: run {rid} is {old}, only a running or stopped run can be replaced")));
        }
        let in_runs = l.conn.query_row("SELECT 1 FROM runs WHERE id=?", [rid], |_| Ok(())).optional()?.is_some();
        if in_runs {
            return Ok(Ingest::Refused(format!("refused: run {rid} is a {old} row of an older ledger and rows are never deleted; ingest it under another --run-id")));
        }
    }
    if matches!(outcome, "green" | "red") {
        insert_final(l, rid, p, outcome, host)?;
    }
    l.record_snapshot(rid, p, outcome, host, taken_at)?;
    Ok(Ingest::Done)
}

fn insert_final(l: &Ledger, rid: &str, p: &ParsedLog, outcome: &str, host: Option<&str>) -> crate::Result<()> {
    let (first, last) = (p.first_ts(), p.last_ts());
    let started = first.or(last).map(|t| t.iso()).unwrap_or_else(|| "unknown".into());
    let tx = l.conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome,stop_reason) VALUES(?,?,?,?,?,?,?,?,?)",
        params![rid, "chain", p.branch, host.or(p.host.as_deref()).unwrap_or("unknown"), p.commit, started, last.map(|t| t.iso()), outcome, Option::<String>::None],
    )?;
    for s in &p.steps {
        tx.execute(
            "INSERT INTO steps(run_id,name,started_at,ended_at,exit_code,cached,duration_sec) VALUES(?,?,?,?,?,?,?)",
            params![rid, s.name, s.start.map(|t| t.iso()), s.end().map(|t| t.iso()), s.exit_code, s.cached as i64, s.duration],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// A ledger of version 5 or later keeps the shards of a sharded step in `step_shards`; an older file has no such table (nothing to attach).
fn attach_shards(l: &Ledger, rid: &str, steps: &mut [Step]) -> crate::Result<()> {
    let has: i64 = l.conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='step_shards'", [], |r| r.get(0))?;
    if has == 0 {
        return Ok(());
    }
    let mut st = l.conn.prepare("SELECT step, started_at, duration_sec FROM step_shards WHERE run_id=? ORDER BY step, shard")?;
    let rows = st.query_map([rid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<f64>>(2)?)))?;
    for r in rows {
        let (name, started, dur) = r?;
        if let (Some(t), Some(d)) = (started.and_then(|s| parse_iso(&s).ok()), dur) {
            if let Some(s) = steps.iter_mut().find(|s| s.name == name) {
                s.shard_spans.push((t, d));
            }
        }
    }
    Ok(())
}

type Head = (Option<String>, Option<String>, Option<String>);

/// A run read back: the finished rows, else the newest snapshot.
pub fn load(l: &Ledger, rid: &str) -> crate::Result<Option<Loaded>> {
    let row = l
        .conn
        .query_row("SELECT outcome, commit_sha, host FROM runs WHERE id=?", [rid], |r| Ok::<Head, _>((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    if let Some((o, c, h)) = row {
        let mut st = l.conn.prepare("SELECT name,started_at,exit_code,cached,duration_sec FROM steps WHERE run_id=? ORDER BY rowid")?;
        let rows = st.query_map([rid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<f64>>(4)?))
        })?;
        let mut steps = Vec::new();
        for r in rows {
            let (name, st, ex, cached, dur) = r?;
            steps.push(Step { name, start: st.and_then(|s| parse_iso(&s).ok()), duration: dur, exit_code: ex, cached: cached.unwrap_or(0) != 0, shard_spans: Vec::new() });
        }
        attach_shards(l, rid, &mut steps)?;
        return Ok(Some(Loaded { outcome: o.unwrap_or_default(), commit: c, host: h, steps }));
    }
    let row = l
        .conn
        .query_row("SELECT outcome, commit_sha, host, steps FROM run_current WHERE run_id=?", [rid], |r| Ok::<(Head, String), _>(((r.get(0)?, r.get(1)?, r.get(2)?), r.get(3)?)))
        .optional()?;
    let Some(((o, c, h), js)) = row else { return Ok(None) };
    let v: serde_json::Value = serde_json::from_str(&js)?;
    let mut steps = Vec::new();
    for s in v.as_array().cloned().unwrap_or_default() {
        steps.push(Step {
            name: s["name"].as_str().unwrap_or_default().to_string(),
            start: s["start"].as_str().and_then(|t| parse_iso(t).ok()),
            duration: s["duration"].as_f64(),
            exit_code: s["exit_code"].as_i64(),
            cached: s["cached"].as_bool().unwrap_or(false),
            shard_spans: Vec::new(),
        });
    }
    Ok(Some(Loaded { outcome: o.unwrap_or_default(), commit: c, host: h, steps }))
}

/// The newest run (finished rows and snapshots), for `report` with no log and no `--run`.
pub fn latest(l: &Ledger) -> crate::Result<Option<String>> {
    Ok(l.conn
        .query_row("SELECT id FROM (SELECT id, started_at FROM runs UNION ALL SELECT run_id, started_at FROM run_current) ORDER BY started_at DESC LIMIT 1", [], |r| r.get(0))
        .optional()?)
}

/// One row of `flaky`: step, commit, green runs, red runs.
pub type FlakyRow = (String, Option<String>, i64, i64);

/// `flaky`: steps red in one finished run and green in another on the same commit.
pub fn flaky(l: &Ledger) -> crate::Result<Vec<FlakyRow>> {
    let mut st = l.conn.prepare("SELECT name, commit_sha, green_runs, red_runs FROM flaky_steps ORDER BY name")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn now_iso() -> String {
    Ts::now_utc().iso()
}
