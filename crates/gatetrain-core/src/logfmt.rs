//! The chain log parser (docs/log-format.md). Byte parity with gatetrain/parse.py is required:
//! the same markers, the same regexes, the same order of checks. Output JSON is built by `to_json`
//! and must equal tests/corpus/parse_dump.py's output byte for byte.
use crate::{Error, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

/// A timestamp as Python's `datetime.fromisoformat` reads it: wall clock micros plus an optional UTC offset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ts {
    /// microseconds since 1970-01-01 of the wall-clock reading (offset NOT applied)
    local_us: i64,
    /// offset east of UTC in seconds; None = naive
    offset: Option<i32>,
}

impl Ts {
    pub(crate) fn instant(&self) -> i64 {
        self.local_us - self.offset.unwrap_or(0) as i64 * 1_000_000
    }

    /// Python `datetime.isoformat()`.
    pub fn iso(&self) -> String {
        let secs = self.local_us.div_euclid(1_000_000);
        let us = self.local_us.rem_euclid(1_000_000);
        let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
        let (y, m, d) = civil_from_days(days);
        let mut s = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", sod / 3600, sod % 3600 / 60, sod % 60);
        if us != 0 {
            s.push_str(&format!(".{us:06}"));
        }
        if let Some(o) = self.offset {
            let sign = if o < 0 { '-' } else { '+' };
            let a = o.abs();
            s.push_str(&format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60));
            if a % 60 != 0 {
                s.push_str(&format!(":{:02}", a % 60));
            }
        }
        s
    }

    /// Microseconds since the epoch as an instant (offset applied); orders aware timestamps the way Python compares them.
    pub fn instant_us(&self) -> i64 {
        self.instant()
    }

    /// The current time as an aware UTC timestamp (Python `datetime.now(timezone.utc)`).
    pub fn now_utc() -> Ts {
        let us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0);
        Ts { local_us: us, offset: Some(0) }
    }

    /// `(self - other).total_seconds()` of two datetimes: integer microseconds divided by 1e6.
    pub fn secs_since(&self, other: &Ts) -> f64 {
        (self.instant() - other.instant()) as f64 / 1_000_000.0
    }

    /// Python `strftime("%H:%M:%S")` of the wall-clock reading.
    pub fn hms(&self) -> String {
        let sod = self.local_us.div_euclid(1_000_000).rem_euclid(86400);
        format!("{:02}:{:02}:{:02}", sod / 3600, sod % 3600 / 60, sod % 60)
    }

    /// `self + timedelta(seconds=secs)`.
    pub fn plus_seconds(&self, secs: f64) -> Ts {
        Ts { local_us: self.local_us + micros_exact(secs), offset: self.offset }
    }
}

/// Seconds (non-negative, finite) to microseconds, rounded half to even on the float's EXACT value,
/// as Python's `timedelta(seconds=...)` does. `secs * 1e6` in floating point would round twice and differ.
fn micros_exact(secs: f64) -> i64 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0;
    }
    let bits = secs.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (m, e) = if exp == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp - 1075) };
    let n = (m as u128) * 1_000_000u128; // < 2^73
    let q = if e >= 0 {
        if e > 40 { return i64::MAX / 4 } // absurd duration: clamp rather than overflow
        n << e
    } else {
        let shift = (-e) as u32;
        if shift >= 126 {
            0
        } else {
            let (q, rem, half) = (n >> shift, n & ((1u128 << shift) - 1), 1u128 << (shift - 1));
            if rem > half || (rem == half && q & 1 == 1) { q + 1 } else { q }
        }
    };
    q.min(i64::MAX as u128 / 4) as i64
}

// Howard Hinnant's civil-date algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The subset of `datetime.fromisoformat` the logs use: `YYYY-MM-DD`, `T` or space, `HH:MM[:SS[.f]]`, optional `Z` or `+HH[:MM[:SS]]`.
pub fn parse_iso(s: &str) -> Result<Ts> {
    let bad = || Error::Timestamp(s.to_string());
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 10 || !s.is_char_boundary(10) {
        return Err(bad());
    }
    let num = |t: &str| -> Result<i64> { if !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit()) { t.parse().map_err(|_| bad()) } else { Err(bad()) } };
    if b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    let (y, mo, d) = (num(&s[0..4])?, num(&s[5..7])?, num(&s[8..10])?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return Err(bad());
    }
    let mut rest = &s[10..];
    let (mut h, mut mi, mut sec, mut us) = (0, 0, 0, 0);
    let mut offset = None;
    if !rest.is_empty() {
        let mut ch = rest.chars();
        let sep = ch.next().unwrap();
        if sep != 'T' && sep != ' ' && sep != 't' {
            return Err(bad());
        }
        rest = ch.as_str();
        let tz_at = rest.find(['Z', 'z', '+', '-']);
        let (time, tz) = match tz_at {
            Some(i) => (&rest[..i], Some(&rest[i..])),
            None => (rest, None),
        };
        let (hms, frac) = match time.split_once('.') {
            Some((a, f)) => (a, Some(f)),
            None => (time, None),
        };
        let parts: Vec<&str> = hms.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
            return Err(bad());
        }
        h = num(parts[0])?;
        mi = num(parts[1])?;
        if parts.len() == 3 {
            sec = num(parts[2])?;
        }
        if let Some(f) = frac {
            if parts.len() != 3 || f.is_empty() || !f.bytes().all(|c| c.is_ascii_digit()) {
                return Err(bad());
            }
            let digits: String = f.chars().take(6).collect();
            us = num(&digits)? * 10i64.pow(6 - digits.len() as u32);
        }
        if h > 23 || mi > 59 || sec > 59 {
            return Err(bad());
        }
        if let Some(tz) = tz {
            offset = Some(if tz.eq_ignore_ascii_case("z") {
                0
            } else {
                let sign = if tz.starts_with('-') { -1 } else { 1 };
                let body = &tz[1..];
                let digits: Vec<&str> = if body.contains(':') { body.split(':').collect() } else if body.len() == 4 { vec![&body[..2], &body[2..]] } else if body.len() == 2 { vec![body] } else { return Err(bad()) };
                if digits.is_empty() || digits.len() > 3 || digits.iter().any(|p| p.len() != 2) {
                    return Err(bad());
                }
                let mut o = num(digits[0])? * 3600;
                if digits.len() > 1 {
                    o += num(digits[1])? * 60;
                }
                if digits.len() > 2 {
                    o += num(digits[2])?;
                }
                sign * o as i32
            });
        }
    }
    let local_us = (days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec) * 1_000_000 + us;
    Ok(Ts { local_us, offset })
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Step {
    pub name: String,
    pub start: Option<Ts>,
    pub duration: Option<f64>,
    pub exit_code: Option<i64>,
    pub cached: bool,
    /// (start, seconds) of each shard of a sharded step, from its `STEP_SHARD` lines; empty for an ordinary step. Not part of `to_json`.
    pub shard_spans: Vec<(Ts, f64)>,
}

impl Step {
    pub fn end(&self) -> Option<Ts> {
        Some(self.start?.plus_seconds(self.duration?))
    }
}

#[derive(Debug, Default)]
pub struct ParsedLog {
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub host: Option<String>,
    pub mode: Option<String>,
    pub steps: Vec<Step>,
    pub red_now: Vec<(String, i64, Option<Ts>)>,
    pub red_summary: Option<String>,
    pub gates_done: bool,
    pub parallel: Option<i64>,
    /// `STEP_SKIPPED <step> <reason>`: a step that never ran (its need went red, or a fast step was red). Kept apart from `steps`, which ran.
    pub skipped: Vec<(String, String)>,
    /// `STEP_RETRY` lines, in log order. Empty unless the run used `--retry-flakes`. Not part of `to_json`.
    pub retries: Vec<Retry>,
    /// `CHAIN_RETRIES <step>=green|red ...`: how each retried step ended; None when nothing was retried.
    pub retry_summary: Option<String>,
}

/// `STEP_RETRY <step> attempt=<n> rc=<rc> class=<cls> via=quarantine:FINDING test=<text>`: attempt `n` of a step was red and
/// the step was re-run once on the same sha (D446). The step's own `STEP_EXIT` is the last attempt's.
#[derive(Debug, Clone, PartialEq)]
pub struct Retry {
    pub step: String,
    pub attempt: i64,
    pub rc: i64,
    pub class: String,
    pub via: String,
    pub test: String,
}

/// The part of a `STEP_RETRY` line after the keyword; None when it lacks the step or one of the four `key=value` words.
fn parse_retry(rest: &str) -> Option<Retry> {
    let (head, test) = rest.split_once(" test=").unwrap_or((rest, ""));
    let mut w = head.split_whitespace();
    let step = w.next()?.to_string();
    let get = |w: &mut std::str::SplitWhitespace, key: &str| w.next()?.strip_prefix(key).map(str::to_string);
    let attempt = get(&mut w, "attempt=")?.parse().ok()?;
    let rc = get(&mut w, "rc=")?.parse().ok()?;
    let class = get(&mut w, "class=")?;
    let via = get(&mut w, "via=")?;
    Some(Retry { step, attempt, rc, class, via, test: test.trim().to_string() })
}

impl ParsedLog {
    /// Retries per test text (the `test=` field), most retried first, then by text: the count a flake rate is built from.
    pub fn retries_by_test(&self) -> Vec<(String, usize)> {
        let mut m: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for r in &self.retries {
            for t in r.test.split(" ;; ") {
                *m.entry(t).or_default() += 1;
            }
        }
        let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    fn timestamps(&self) -> Vec<Ts> {
        let mut ts = Vec::new();
        for s in &self.steps {
            ts.extend(s.start);
            ts.extend(s.end());
        }
        ts.extend(self.red_now.iter().filter_map(|r| r.2));
        ts
    }

    pub fn last_ts(&self) -> Option<Ts> {
        self.timestamps().into_iter().max_by_key(|t| t.instant())
    }

    pub fn first_ts(&self) -> Option<Ts> {
        self.steps.iter().filter_map(|s| s.start).min_by_key(|t| t.instant())
    }

    /// The JSON the parity corpus compares (keys sorted by serde_json's map; no whitespace).
    pub fn to_json(&self) -> Value {
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "start": s.start.map(|t| t.iso()),
                    "end": s.end().map(|t| t.iso()),
                    "duration": s.duration,
                    "exit_code": s.exit_code,
                    "cached": s.cached,
                })
            })
            .collect();
        let red_now: Vec<Value> = self.red_now.iter().map(|(n, rc, at)| json!({"step": n, "rc": rc, "at": at.map(|t| t.iso())})).collect();
        let mut m = Map::new();
        m.insert("branch".into(), json!(self.branch));
        m.insert("commit".into(), json!(self.commit));
        m.insert("host".into(), json!(self.host));
        m.insert("mode".into(), json!(self.mode));
        m.insert("parallel".into(), json!(self.parallel));
        m.insert("gates_done".into(), json!(self.gates_done));
        m.insert("red_summary".into(), json!(self.red_summary));
        m.insert("first_ts".into(), json!(self.first_ts().map(|t| t.iso())));
        m.insert("last_ts".into(), json!(self.last_ts().map(|t| t.iso())));
        m.insert("steps".into(), Value::Array(steps));
        m.insert("red_now".into(), Value::Array(red_now));
        if !self.skipped.is_empty() {
            // only when present, so every log without the marker prints exactly what it printed before
            m.insert("skipped".into(), Value::Array(self.skipped.iter().map(|(n, w)| json!({"step": n, "reason": w})).collect()));
        }
        if !self.retries.is_empty() {
            // only when present: a log without retries prints exactly what it printed before
            m.insert("retries".into(), Value::Array(self.retries.iter().map(|r| json!({"step": r.step, "attempt": r.attempt, "rc": r.rc, "class": r.class, "via": r.via, "test": r.test})).collect()));
            m.insert("retry_summary".into(), json!(self.retry_summary));
        }
        Value::Object(m)
    }
}

struct Re {
    branch: Regex,
    start: Regex,
    time: Regex,
    exit: Regex,
    cached: Regex,
    red: Regex,
    summary: Regex,
    parallel: Regex,
}

fn re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re {
        branch: Regex::new(r"^branch (\S+) at (\S+) on (\S+)").unwrap(),
        start: Regex::new(r"^STEP_START (\S+) (\S+)").unwrap(),
        time: Regex::new(r"^STEP_TIME (\S+) (\d+(?:\.\d+)?)s").unwrap(),
        exit: Regex::new(r"^STEP_EXIT (\S+) (-?\d+)").unwrap(),
        cached: Regex::new(r"^STEP_CACHED (\S+)").unwrap(),
        red: Regex::new(r"^RED_NOW (\S+) rc=(-?\d+)(?: at (\S+))?").unwrap(),
        summary: Regex::new(r"^RED_SUMMARY:\s*(.*)").unwrap(),
        parallel: Regex::new(r"^CHAIN_PARALLEL\s+(\d+)").unwrap(),
    })
}

/// Python's `str.splitlines()` line boundaries.
pub(crate) fn splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let brk = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if brk {
            out.push(&text[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(_, '\n')) = it.peek() {
                    it.next();
                    end += 1;
                }
            }
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn parse_int(s: &str) -> Result<i64> {
    s.parse().map_err(|_| Error::Timestamp(format!("integer out of range: {s}")))
}

pub fn parse_log(text: &str) -> Result<ParsedLog> {
    let r = re();
    let mut p = ParsedLog::default();
    let mut by: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut shard_open: std::collections::HashMap<(String, String), Ts> = std::collections::HashMap::new();
    fn step<'a>(p: &'a mut ParsedLog, by: &mut std::collections::HashMap<String, usize>, name: &str) -> &'a mut Step {
        let i = *by.entry(name.to_string()).or_insert_with(|| {
            p.steps.push(Step { name: name.to_string(), ..Default::default() });
            p.steps.len() - 1
        });
        &mut p.steps[i]
    }
    for raw in splitlines(text) {
        let line = raw.trim_end();
        if line.starts_with("STEP_START") {
            if let Some(m) = r.start.captures(line) {
                let t = parse_iso(&m[2])?;
                step(&mut p, &mut by, &m[1]).start = Some(t);
            }
        } else if line.starts_with("STEP_TIME") {
            if let Some(m) = r.time.captures(line) {
                let d: f64 = m[2].parse().map_err(|_| Error::Timestamp(m[2].to_string()))?;
                step(&mut p, &mut by, &m[1]).duration = Some(d);
            }
        } else if line.starts_with("STEP_EXIT") {
            if let Some(m) = r.exit.captures(line) {
                let rc = parse_int(&m[2])?;
                step(&mut p, &mut by, &m[1]).exit_code = Some(rc);
            }
        } else if line.starts_with("STEP_CACHED") {
            if let Some(m) = r.cached.captures(line) {
                step(&mut p, &mut by, &m[1]).cached = true;
            }
        } else if let Some(rest) = line.strip_prefix("STEP_SHARD ") {
            // `STEP_SHARD <step> <i>/<n> START <time>` / `... EXIT <rc> <N>s`: a slot of a sharded step; the step's own lines stay the truth
            let w: Vec<&str> = rest.split_whitespace().collect();
            match w.as_slice() {
                [name, frac, "START", t] => {
                    if let Ok(t) = parse_iso(t) {
                        shard_open.insert((name.to_string(), frac.to_string()), t);
                    }
                }
                [name, frac, "EXIT", _rc, secs] => {
                    if let (Some(t), Ok(d)) = (shard_open.remove(&(name.to_string(), frac.to_string())), secs.trim_end_matches('s').parse::<f64>()) {
                        step(&mut p, &mut by, name).shard_spans.push((t, d));
                    }
                }
                _ => {}
            }
        } else if let Some(rest) = line.strip_prefix("STEP_SKIPPED ") {
            // `STEP_SKIPPED <step> <reason>`: never ran, so it is no Step (no start, no exit) and counts in no total
            let (name, why) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if !name.is_empty() {
                p.skipped.push((name.to_string(), why.trim().to_string()));
            }
        } else if let Some(rest) = line.strip_prefix("STEP_RETRY ") {
            if let Some(r) = parse_retry(rest) {
                p.retries.push(r);
            }
        } else if let Some(rest) = line.strip_prefix("CHAIN_RETRIES ") {
            p.retry_summary = Some(rest.trim().to_string());
        } else if line.starts_with("RED_NOW") {
            if let Some(m) = r.red.captures(line) {
                let at = m.get(3).map(|a| parse_iso(a.as_str())).transpose()?;
                p.red_now.push((m[1].to_string(), parse_int(&m[2])?, at));
            }
        } else if line.starts_with("RED_SUMMARY") {
            // `RED_SUMMARY: none; retried: a(quarantine)`: the reds stay what they were; the suffix names a retried green and is read by `retries`
            let s = r.summary.captures(line).map(|m| m[1].trim().to_string()).unwrap_or_default();
            p.red_summary = Some(s.split_once("; retried:").map_or(s.clone(), |(head, _)| head.trim().to_string()));
        } else if line.starts_with("GATES_DONE") {
            p.gates_done = true;
        } else if line.starts_with("CHAIN_PARALLEL") {
            if let Some(m) = r.parallel.captures(line) {
                p.parallel = Some(parse_int(&m[1])?);
            }
        } else if line.starts_with("CHAIN_MODE") {
            // Python: line.split(None, 1); mode = parts[1].strip() when there is a second part
            p.mode = line.find(char::is_whitespace).map(|i| line[i..].trim().to_string());
        } else if line.starts_with("branch ") && p.branch.is_none() {
            if let Some(m) = r.branch.captures(line) {
                p.branch = Some(m[1].to_string());
                p.commit = Some(m[2].to_string());
                p.host = Some(m[3].to_string());
            }
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip_python_style() {
        for (i, o) in [
            ("2026-11-11T09:00:48+07:00", "2026-11-11T09:00:48+07:00"),
            ("2026-11-11T09:00:48Z", "2026-11-11T09:00:48+00:00"),
            ("2026-11-11 09:00:48.5-0330", "2026-11-11T09:00:48.500000-03:30"),
            ("2026-11-11T09:00", "2026-11-11T09:00:00"),
            ("2026-11-11", "2026-11-11T00:00:00"),
        ] {
            assert_eq!(parse_iso(i).unwrap().iso(), o, "{i}");
        }
        assert!(parse_iso("2026-13-01T00:00:00").is_err());
        assert!(parse_iso("nope").is_err());
    }

    #[test]
    fn end_crosses_midnight() {
        let t = parse_iso("2026-02-28T23:59:30Z").unwrap().plus_seconds(45.5);
        assert_eq!(t.iso(), "2026-03-01T00:00:15.500000+00:00");
    }

    #[test]
    fn micros_round_half_even_on_the_exact_value() {
        assert_eq!(micros_exact(1.9999995), 1_999_999); // the float is just below the tie
        assert_eq!(micros_exact(0.5), 500_000);
        assert_eq!(micros_exact(0.0000005), 0); // exact value is below 0.5 us
        assert_eq!(micros_exact(0.0000015), 2); // python: timedelta(seconds=1.5e-6).microseconds == 2
        assert_eq!(micros_exact(86400.0), 86_400_000_000);
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\r\nb\rc\n"), vec!["a", "b", "c"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("x\n\ny"), vec!["x", "", "y"]);
    }

    #[test]
    fn markers_in_order() {
        let p = parse_log("branch b at abc on h\nCHAIN_MODE full\nCHAIN_PARALLEL 4\nSTEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 2.5s\nSTEP_EXIT a 0\nRED_SUMMARY: none\nGATES_DONE\n").unwrap();
        assert_eq!((p.branch.as_deref(), p.mode.as_deref(), p.parallel), (Some("b"), Some("full"), Some(4)));
        assert!(p.gates_done);
        assert_eq!(p.steps[0].end().unwrap().iso(), "2026-11-11T09:00:02.500000+00:00");
    }
}
