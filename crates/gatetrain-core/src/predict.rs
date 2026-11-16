//! ETA prediction (port of gatetrain/analysis.py `step_medians`, `estimate_step`, `predict`, `eta_text`).
use crate::analysis::{concurrency, timed};
use crate::history::History;
use crate::logfmt::{Step, Ts};
use crate::stats::{fmt_dur, median, quartiles};

pub struct Prediction {
    pub rough: bool,
    pub eta: Ts,
    pub lo: Ts,
    pub hi: Ts,
    pub dominant: Vec<(String, f64)>,
    pub par: f64,
    pub mc: usize,
    pub rate: f64,
    pub remaining: usize,
}

fn prefix(name: &str) -> &str {
    match name.split_once(':') {
        Some((p, _)) => p,
        None => "",
    }
}

/// Median per step name, in history order, skipping names with no durations.
fn step_medians(hist: &History) -> Vec<(&str, f64)> {
    hist.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.as_str(), median(v))).collect()
}

fn estimate_step(name: &str, med: &[(&str, f64)]) -> f64 {
    if let Some((_, m)) = med.iter().find(|(k, _)| *k == name) {
        return *m;
    }
    let p = prefix(name);
    if !p.is_empty() {
        let same: Vec<f64> = med.iter().filter(|(k, _)| prefix(k) == p).map(|(_, v)| *v).collect();
        if !same.is_empty() {
            return median(&same);
        }
    }
    median(&med.iter().map(|(_, v)| *v).collect::<Vec<_>>())
}

fn top5(rem: &[(String, f64)]) -> Vec<(String, f64)> {
    let mut v = rem.to_vec();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // stable, like Python's sorted
    v.truncate(5);
    v
}

/// Python `predict`. `reference` is the last timestamp seen in the log.
pub fn predict(steps: &[Step], hist: &History, reference: Ts) -> Prediction {
    let done: Vec<&str> = steps.iter().filter(|s| (s.exit_code.is_some() || s.duration.is_some()) && s.exit_code.is_some()).map(|s| s.name.as_str()).collect();
    let inprog: Vec<&Step> = steps.iter().filter(|s| !done.contains(&s.name.as_str())).collect();
    let med = step_medians(hist);
    let t = timed(steps);
    let c = concurrency(steps);
    let par = c.avg.max(1.0);
    let first = t.iter().map(|s| s.start.unwrap()).min_by_key(|x| x.instant_us()).unwrap_or(reference);
    let span_min = (reference.secs_since(&first) / 60.0).max(1e-9);
    let rate = t.len() as f64 / span_min;
    let mk = |rough: bool, wall: f64, lo: f64, hi: f64, rem: &[(String, f64)], remaining: usize| Prediction {
        rough,
        eta: reference.plus_seconds(wall),
        lo: reference.plus_seconds(lo),
        hi: reference.plus_seconds(hi),
        dominant: top5(rem),
        par,
        mc: c.max,
        rate,
        remaining,
    };
    if med.is_empty() {
        let own: Vec<f64> = t.iter().filter_map(|s| s.duration).collect();
        if own.is_empty() || inprog.is_empty() {
            return mk(true, 0.0, 0.0, 0.0, &[], inprog.len()).with_dominant(vec![]);
        }
        let d = median(&own);
        let rem: Vec<(String, f64)> = inprog.iter().map(|s| (s.name.clone(), d)).collect();
        let wall = d.max(rem.iter().fold(0.0, |a, x| a + x.1) / par);
        return mk(true, wall, wall * 0.5, wall * 2.0, &rem, inprog.len());
    }
    let mut names: Vec<String> = inprog.iter().map(|s| s.name.clone()).collect();
    for (n, _) in &med {
        if !done.contains(n) && !inprog.iter().any(|s| s.name == *n) {
            names.push((*n).to_string());
        }
    }
    let rem: Vec<(String, f64)> = names.into_iter().map(|n| (estimate_step(&n, &med), n)).map(|(d, n)| (n, d)).collect();
    let total = rem.iter().fold(0.0, |a, x| a + x.1);
    let longest = rem.iter().map(|x| x.1).fold(0.0f64, f64::max);
    let wall = if rem.is_empty() { 0.0 } else { longest.max(total / par) };
    let vals: Vec<f64> = med.iter().map(|(_, v)| *v).collect();
    let (lo_f, hi_f) = if vals.len() >= 4 {
        let q = quartiles(&vals);
        let m = median(&vals);
        (if m != 0.0 { q[0] / m } else { 1.0 }, if m != 0.0 { q[2] / m } else { 1.0 })
    } else {
        (0.75, 1.5)
    };
    let (lo_wall, hi_wall) = if rem.is_empty() { (0.0, 0.0) } else { ((longest * lo_f).max(total * lo_f / par), (longest * hi_f).max(total * hi_f / par)) };
    let n = rem.len();
    mk(false, wall, lo_wall, hi_wall, &rem, n)
}

impl Prediction {
    fn with_dominant(mut self, d: Vec<(String, f64)>) -> Self {
        self.dominant = d;
        self
    }
}

/// Python `eta_text`: lines joined with "\n", no trailing newline.
pub fn eta_text(pred: &Prediction, reference: &Ts, finished: usize) -> String {
    let mut l: Vec<String> = Vec::new();
    if pred.rough {
        l.push("no history - rough estimate (this run's own pace only)".into());
    }
    l.push(format!("eta {}", pred.eta.iso()));
    l.push(format!("band p25-p75: {} .. {}", pred.lo.iso(), pred.hi.iso()));
    l.push(format!(
        "basis: {finished} steps finished at {:.1}/min, parallelism {:.1} (max seen {}), {} steps remaining, last log line {}",
        pred.rate,
        pred.par,
        pred.mc,
        pred.remaining,
        reference.iso()
    ));
    if pred.dominant.is_empty() {
        l.push("dominant remaining steps: none known".into());
    } else {
        let d: Vec<String> = pred.dominant.iter().map(|(n, s)| format!("{n} (~{})", fmt_dur(*s))).collect();
        l.push(format!("dominant remaining steps: {}", d.join(", ")));
    }
    l.join("\n")
}

/// Python `next_tick`: minutes until the next look, and why. `reds` holds the time of every red now (some may be unknown).
pub fn next_tick(first: Option<Ts>, reds: &[Option<Ts>], remaining_sec: Option<f64>, now: Ts, lo: i64, hi: i64) -> (i64, String) {
    let clamp = |m: f64| -> i64 { m.round_ties_even().min(hi as f64).max(lo as f64) as i64 };
    if reds.iter().flatten().any(|t| now.secs_since(t) <= 300.0) {
        return (clamp(lo as f64), "a red appeared in the last 5 minutes: look again at the minimum".into());
    }
    let elapsed = first.map(|f| now.secs_since(&f) / 60.0).unwrap_or(0.0);
    if elapsed < 10.0 {
        return (clamp(lo.max(5) as f64), "run just started: catch early reds".into());
    }
    let has_reds = !reds.is_empty();
    if let Some(r) = remaining_sec {
        let rem = r / 60.0;
        if rem <= 15.0 {
            return (clamp(lo as f64), format!("predicted end in about {rem:.0} min: stay close"));
        }
        let mut m = clamp(rem / 3.0);
        if has_reds {
            m = m.min(30);
        }
        let tail = if has_reds { "; capped at 30 because reds exist" } else { "" };
        return (m, format!("mid-run, about {rem:.0} min predicted left: a third of it{tail}"));
    }
    let m = clamp(elapsed / 3.0);
    (if has_reds { m.min(30) } else { m }, "no history: a third of the elapsed time".into())
}
