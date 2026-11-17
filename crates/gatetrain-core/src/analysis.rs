//! Timing, concurrency and the report text (port of gatetrain/analysis.py, first half).
use crate::logfmt::{ParsedLog, Step, Ts};
use crate::stats::fmt_dur;

/// Steps with a known start and end, in the original order.
pub fn timed(steps: &[Step]) -> Vec<&Step> {
    steps.iter().filter(|s| s.start.is_some() && s.end().is_some()).collect()
}

pub struct Concurrency {
    pub max: usize,
    pub start: Option<Ts>,
    pub end: Option<Ts>,
    pub avg: f64,
}

/// Python `concurrency`: the highest number of steps at once, the first stretch at that level, the average.
pub fn concurrency(steps: &[Step]) -> Concurrency {
    let mut ev: Vec<(Ts, i64)> = Vec::new();
    for s in timed(steps) {
        // a sharded step occupies one slot per shard, so its shards are the intervals (engine v2 section 6)
        if !s.shard_spans.is_empty() {
            for (t, d) in s.shard_spans.iter().filter(|(_, d)| *d > 0.0) {
                ev.push((*t, 1));
                ev.push((t.plus_seconds(*d), -1));
            }
        } else if s.duration.unwrap_or(0.0) > 0.0 {
            ev.push((s.start.unwrap(), 1));
            ev.push((s.end().unwrap(), -1));
        }
    }
    if ev.is_empty() {
        return Concurrency { max: 0, start: None, end: None, avg: 0.0 };
    }
    ev.sort_by_key(|e| (e.0.instant_us(), e.1)); // stable: ends before starts at the same instant
    let (mut cur, mut best) = (0i64, 0i64);
    let (mut best_start, mut best_end): (Option<Ts>, Option<Ts>) = (None, None);
    let mut busy = 0.0f64;
    let mut prev = ev[0].0;
    for &(t, d) in &ev {
        busy += cur as f64 * t.secs_since(&prev);
        if cur == best && best_start.is_some() && best_end.is_none() && d == -1 {
            best_end = Some(t);
        }
        prev = t;
        cur += d;
        if cur > best {
            best = cur;
            best_start = Some(t);
            best_end = None;
        }
    }
    let span = ev[ev.len() - 1].0.secs_since(&ev[0].0);
    Concurrency { max: best as usize, start: best_start, end: best_end, avg: if span > 0.0 { busy / span } else { 0.0 } }
}

pub struct Meta<'a> {
    pub outcome: &'a str,
    pub commit: Option<&'a str>,
    pub host: Option<&'a str>,
}

fn py(s: Option<&str>) -> &str {
    s.unwrap_or("None") // Python's f-string of None
}

fn is_red(s: &Step) -> bool {
    matches!(s.exit_code, Some(x) if x != 0)
}

/// Python `report_text`: lines joined with "\n", no trailing newline.
pub fn report_text(run_id: &str, meta: &Meta, steps: &[Step]) -> String {
    let t = timed(steps);
    let reds: Vec<&Step> = steps.iter().filter(|s| is_red(s)).collect();
    let first = t.iter().map(|s| s.start.unwrap()).min_by_key(|x| x.instant_us());
    let last = t.iter().map(|s| s.end().unwrap()).max_by_key(|x| x.instant_us());
    let wall = match (first, last) {
        (Some(f), Some(l)) => l.secs_since(&f),
        _ => 0.0,
    };
    let busy = steps.iter().fold(0.0f64, |a, s| a + s.duration.unwrap_or(0.0));
    let unknown = steps.iter().filter(|s| s.start.is_none()).count();
    let c = concurrency(steps);
    let finished = steps.iter().filter(|s| s.exit_code.is_some() || s.duration.is_some()).count();
    let cached: Vec<&str> = steps.iter().filter(|s| s.cached).map(|s| s.name.as_str()).collect();
    let mut l: Vec<String> = vec![
        format!("run {run_id}: {}  commit {}  host {}", meta.outcome, py(meta.commit), py(meta.host)),
        format!("steps {}  finished {finished}  red {}  cached {}  unknown start {unknown}", steps.len(), reds.len(), cached.len()),
        format!("wall time {}  busy time {}  average parallelism {:.1}", fmt_dur(wall), fmt_dur(busy), c.avg),
    ];
    let red_list: Vec<String> = reds.iter().map(|s| format!("{}(rc={})", s.name, s.exit_code.unwrap())).collect();
    l.push(format!("reds: {}", if red_list.is_empty() { "none".to_string() } else { red_list.join(", ") }));
    l.push("10 slowest steps:".into());
    let mut slow: Vec<&Step> = steps.iter().collect();
    slow.sort_by(|a, b| b.duration.unwrap_or(0.0).partial_cmp(&a.duration.unwrap_or(0.0)).unwrap());
    for s in slow.iter().take(10) {
        l.push(format!("  {:>9}  {}", fmt_dur(s.duration.unwrap_or(0.0)), s.name));
    }
    l.push(format!("steps cached: {}", if cached.is_empty() { "none".to_string() } else { cached.join(", ") }));
    l.push("critical path (approximation, steps overlap):".into());
    if c.max > 0 {
        let mut line = format!("  busiest stretch: {} steps at once from {}", c.max, c.start.unwrap().iso());
        if let Some(e) = c.end {
            line.push_str(&format!(" to {}", e.iso()));
        }
        l.push(line);
        let mut ends = t.clone();
        ends.sort_by_key(|s| std::cmp::Reverse(s.end().unwrap().instant_us())); // stable, like sorted(reverse=True)
        let parts: Vec<String> =
            ends.iter().take(5).map(|s| format!("{} ({}, ends {})", s.name, fmt_dur(s.duration.unwrap()), s.end().unwrap().hms())).collect();
        l.push(format!("  steps that end last: {}", parts.join(", ")));
        let longest = t.iter().map(|s| s.duration.unwrap()).fold(f64::MIN, f64::max);
        l.push(format!("  longest single steps bound the wall time from below: {}", fmt_dur(longest)));
    } else {
        l.push("  no timed steps".into());
    }
    l.join("\n")
}

/// diagnose.py `parallelism_note`: a run slower than the host's configured parallelism is slow for a reason that is not the code.
pub fn parallelism_note(p: &ParsedLog, configured: Option<i64>) -> Option<String> {
    let configured = configured.filter(|c| *c != 0)?;
    let (got, how) = match p.parallel {
        Some(v) => (v, "recorded"),
        None => {
            let mc = concurrency(&p.steps).max as i64;
            if mc == 0 {
                return None;
            }
            (mc, "observed")
        }
    };
    if got >= configured {
        return None;
    }
    Some(format!(
        "note: {how} parallelism {got} is below the configured {configured}: this run is slower than the host can do (expect about {:.1}x the wall time on its parallel stretch)",
        configured as f64 / got as f64
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logfmt::parse_log;

    #[test]
    fn concurrency_of_two_overlapping_steps() {
        let p = parse_log("STEP_START a 2026-11-11T10:00:00Z\nSTEP_TIME a 60s\nSTEP_EXIT a 0\nSTEP_START b 2026-11-11T10:00:30Z\nSTEP_TIME b 60s\nSTEP_EXIT b 0\n").unwrap();
        let c = concurrency(&p.steps);
        assert_eq!(c.max, 2);
        assert!((c.avg - 120.0 / 90.0).abs() < 1e-12);
    }

    #[test]
    fn shards_are_slots_the_average_counts_each_one() {
        let p = parse_log("STEP_START t 2026-11-11T10:00:00Z
STEP_SHARD t 1/3 START 2026-11-11T10:00:00Z
STEP_SHARD t 2/3 START 2026-11-11T10:00:00Z
STEP_SHARD t 3/3 START 2026-11-11T10:00:00Z
STEP_SHARD t 1/3 EXIT 0 60.0s
STEP_SHARD t 2/3 EXIT 0 60.0s
STEP_SHARD t 3/3 EXIT 0 60.0s
STEP_TIME t 60s
STEP_EXIT t 0
").unwrap();
        assert_eq!(p.steps.len(), 1);
        let c = concurrency(&p.steps);
        assert_eq!(c.max, 3);
        assert!((c.avg - 3.0).abs() < 1e-9, "{}", c.avg);
        // the same log without the shard lines is one slot
        let q = parse_log("STEP_START t 2026-11-11T10:00:00Z
STEP_TIME t 60s
STEP_EXIT t 0
").unwrap();
        assert_eq!(concurrency(&q.steps).max, 1);
    }

    #[test]
    fn empty_run_reports_no_timed_steps() {
        let c = concurrency(&[]);
        assert_eq!((c.max, c.avg), (0, 0.0));
        let r = report_text("r", &Meta { outcome: "stopped", commit: None, host: None }, &[]);
        assert!(r.starts_with("run r: stopped  commit None  host None"));
        assert!(r.ends_with("  no timed steps"));
    }
}
