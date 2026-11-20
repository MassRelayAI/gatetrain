//! Retry eligibility (D446, ADR 003). This is its OWN strict rule: it never reads the `diagnose` rule table, whose classes advise a
//! person and were not written to decide a re-run. When in doubt, never retry.
//!
//! A red step may be re-run once only when ONE of these holds:
//! 1. **Quarantine.** Every failing test id in its output (vitest `FAIL <file> > <name>`, cargo `test <name> ... FAILED`, node
//!    `not ok N - <name>`) is the EXACT `test` of a live quarantine row for this step, and there is at least one id.
//! 2. **Infra.** The output shows an infra signature ([`INFRA`]) and nothing of the deny set ([`DENY`]): no assertion, panic, failing
//!    test line, compile error or timeout anywhere in it, in or outside a block.
//!
//! Never retried: a step killed by its timeout (rc 124), whose worker died (125) or that could not start (127), and any output over
//! [`MAX_OUTPUT_BYTES`] (it cannot be read whole, so it cannot be judged).
use regex::Regex;
use std::sync::OnceLock;

/// A step's output larger than this is never judged: the deny set must see all of it.
pub const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
/// The first line a quarantine file must carry.
pub const QUARANTINE_HEADER: &str = "test,step,finding,added,expires";
/// Longest life of a quarantine row, in days (`expires` minus `added`).
pub const MAX_QUARANTINE_DAYS: i64 = 14;

/// What an infra red looks like: (class, pattern). The class is what the log and the stamp print.
pub const INFRA: &[(&str, &str)] = &[
    ("infra-docker", r"(?i)Cannot connect to the Docker daemon|docker daemon is not running|error during connect: .*docker"),
    ("infra-port", r"(?i)address already in use|\bEADDRINUSE\b|port is already allocated|failed to bind host port"),
    ("infra-dns", r"(?i)Temporary failure in name resolution|\bEAI_AGAIN\b"),
];

/// Anything here, anywhere in a step's output, means the red may be a real defect: no infra retry. (name, pattern)
pub const DENY: &[(&str, &str)] = &[
    ("assertion", r"(?i)AssertionError|ERR_ASSERTION|\bassertion\b|\bexpected\b[^\n]*\bto\b"),
    ("panic", r"panicked at|(?m)^\s*panic:"),
    ("failing-test", r"(?m)^\s*(?:FAIL|FAILED)\b|\bFAILED\b|(?m)^\s*not ok\b|[✖×]"),
    ("compile", r"error\[E\d+\]|could not compile|error TS\d+|\bSyntaxError\b"),
    ("timeout", r"(?i)\btime[d ]?-? ?out\b|chain: timed out"),
];

/// Always fatal to a retry, quarantine or not: a compile error or a timeout is never a flake.
const NEVER: &[&str] = &["compile", "timeout"];

type Table = &'static [(&'static str, &'static str)];
type Cell = OnceLock<Vec<(&'static str, Regex)>>;

fn compiled(table: Table, cell: &'static Cell) -> &'static [(&'static str, Regex)] {
    cell.get_or_init(|| table.iter().map(|(n, p)| (*n, Regex::new(p).expect("retry table regex"))).collect())
}

fn infra() -> &'static [(&'static str, Regex)] {
    static C: Cell = OnceLock::new();
    compiled(INFRA, &C)
}

fn deny() -> &'static [(&'static str, Regex)] {
    static C: Cell = OnceLock::new();
    compiled(DENY, &C)
}

/// The names of the deny rows an output trips (empty = none).
pub fn deny_hits(output: &str) -> Vec<&'static str> {
    deny().iter().filter(|(_, r)| r.is_match(output)).map(|(n, _)| *n).collect()
}

fn id_res() -> &'static [Regex] {
    static C: OnceLock<Vec<Regex>> = OnceLock::new();
    C.get_or_init(|| {
        [r"(?m)^\s*FAIL\s+(\S.*?)\s*$", r"(?m)^test (\S.*?) \.\.\. FAILED\s*$", r"(?m)^\s*not ok \d+ - (.+?)\s*$"].iter().map(|p| Regex::new(p).unwrap()).collect()
    })
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The failing test ids of an output, in order of first appearance, whitespace-normalised, without repeats.
pub fn failing_ids(output: &str) -> Vec<String> {
    let mut hits: Vec<(usize, String)> = Vec::new();
    for r in id_res() {
        for c in r.captures_iter(output) {
            hits.push((c.get(0).unwrap().start(), norm(&c[1])));
        }
    }
    hits.sort();
    let mut out: Vec<String> = Vec::new();
    for (_, id) in hits {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// One live row of a quarantine file.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineRow {
    pub test: String,
    pub step: String,
    pub finding: String,
    pub added: String,
    pub expires: String,
}

/// Days since 1970-01-01 of a real calendar date `YYYY-MM-DD` (None for 2026-02-31, a wrong shape or an impossible month or day).
pub fn days_of(date: &str) -> Option<i64> {
    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' || !date.bytes().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (date[..4].parse().ok()?, date[5..7].parse().ok()?, date[8..].parse().ok()?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + i64::from(leap),
        _ => return None,
    };
    if !(1..=dim).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Split one CSV line (double quotes may hold commas; `""` is a quote).
fn csv_fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match (c, quoted) {
            ('"', true) if it.peek() == Some(&'"') => {
                cur.push('"');
                it.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// The live rows and the dropped ones as `(line number, why)`.
pub type Parsed = (Vec<QuarantineRow>, Vec<(usize, String)>);

/// The live rows of a quarantine file on `today` (`YYYY-MM-DD`), and the rows not honoured as `(line number, why)`.
/// Err when the file has no header row [`QUARANTINE_HEADER`] or `today` is not a date. A row is honoured only when it has five fields,
/// a finding id without spaces, real dates, `added <= today <= expires` and `expires - added <= 14` days.
pub fn parse_quarantine(text: &str, today: &str) -> Result<Parsed, String> {
    let now = days_of(today).ok_or_else(|| format!("today is not a date: {today:?}"))?;
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, h)) if h.trim().trim_start_matches('\u{feff}') == QUARANTINE_HEADER => {}
        _ => return Err(format!("the quarantine file must start with the header row {QUARANTINE_HEADER}")),
    }
    let (mut live, mut dropped) = (Vec::new(), Vec::new());
    for (n, line) in lines {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let f = csv_fields(line);
        let n = n + 1;
        if f.len() != 5 || f[..3].iter().any(String::is_empty) {
            dropped.push((n, "expected test,step,finding,added,expires with the first three filled".into()));
            continue;
        }
        if f[2].contains(char::is_whitespace) {
            dropped.push((n, "a finding id has no spaces".into()));
            continue;
        }
        let (Some(a), Some(e)) = (days_of(&f[3]), days_of(&f[4])) else {
            dropped.push((n, "added and expires must be real YYYY-MM-DD dates".into()));
            continue;
        };
        if e < a || e - a > MAX_QUARANTINE_DAYS {
            dropped.push((n, format!("expires must be 0 to {MAX_QUARANTINE_DAYS} days after added")));
        } else if now < a {
            dropped.push((n, format!("not yet live: added {}", f[3])));
        } else if now > e {
            dropped.push((n, format!("expired on {}", f[4])));
        } else {
            live.push(QuarantineRow { test: norm(&f[0]), step: f[1].clone(), finding: f[2].clone(), added: f[3].clone(), expires: f[4].clone() });
        }
    }
    Ok((live, dropped))
}

/// Why a red step may be re-run once.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryCause {
    /// `quarantine` or an [`INFRA`] class
    pub cls: String,
    /// `quarantine:<finding>` or `infra:<class>`
    pub via: String,
    /// every cause named: all failing test ids (quarantine) or the infra class (infra)
    pub tests: Vec<String>,
}

/// The strict rule. `output` is the step's whole output; None = the step may not be re-run.
pub fn retry_cause(step: &str, rc: i64, output: &str, quarantine: &[QuarantineRow]) -> Option<RetryCause> {
    if rc == 0 || matches!(rc, 124 | 125 | 127) {
        return None;
    }
    let hits = deny_hits(output);
    if hits.iter().any(|h| NEVER.contains(h)) {
        return None;
    }
    let ids = failing_ids(output);
    if !ids.is_empty() {
        let mut finding = None;
        for id in &ids {
            let row = quarantine.iter().find(|q| q.step == step && q.test == *id)?;
            finding.get_or_insert(row.finding.clone());
        }
        return Some(RetryCause { cls: "quarantine".into(), via: format!("quarantine:{}", finding?), tests: ids });
    }
    if !hits.is_empty() {
        return None;
    }
    let (cls, _) = infra().iter().find(|(_, r)| r.is_match(output))?;
    Some(RetryCause { cls: (*cls).into(), via: format!("infra:{cls}"), tests: vec![(*cls).into()] })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HDR: &str = "test,step,finding,added,expires\n";
    fn rows(body: &str, today: &str) -> Parsed {
        parse_quarantine(&format!("{HDR}{body}"), today).unwrap()
    }
    fn q(test: &str) -> Vec<QuarantineRow> {
        vec![QuarantineRow { test: test.into(), step: "s".into(), finding: "M1".into(), added: "2026-11-01".into(), expires: "2026-11-10".into() }]
    }

    #[test]
    fn dates_are_real_calendar_dates() {
        assert!(days_of("2026-02-31").is_none() && days_of("2026-13-01").is_none() && days_of("2026-1-01").is_none());
        assert!(days_of("2024-02-29").is_some() && days_of("2026-02-29").is_none());
        assert_eq!(days_of("1970-01-02"), Some(1));
    }

    #[test]
    fn a_row_is_live_only_between_added_and_expires_and_within_14_days() {
        let row = "a > b,s,M1,2026-11-01,2026-11-10\n";
        assert_eq!(rows(row, "2026-11-01").0.len(), 1);
        assert_eq!(rows(row, "2026-11-10").0.len(), 1);
        assert!(rows(row, "2026-10-31").0.is_empty(), "not yet live");
        assert!(rows(row, "2026-11-11").0.is_empty(), "expired");
        assert!(rows("a > b,s,M1,2090-01-01,2090-01-10\n", "2026-11-20").0.is_empty(), "a row dated in 2090 is not honoured today");
        assert!(rows("a > b,s,M1,2026-11-01,2026-11-16\n", "2026-11-02").0.is_empty(), "15 days is too long");
        assert_eq!(rows("a > b,s,M1,2026-11-01,2026-11-15\n", "2026-11-02").0.len(), 1, "14 days is the limit");
        assert!(rows("a > b,s,M1,2026-02-31,2026-03-02\n", "2026-03-01").0.is_empty(), "an impossible date");
        assert!(rows("a > b,s,M 1,2026-11-01,2026-11-10\n", "2026-11-02").0.is_empty(), "a finding id with a space");
        let (_, dropped) = rows("a > b,s,M1,2026-11-01,2026-11-16\n", "2026-11-02");
        assert_eq!(dropped[0].0, 2, "the dropped row is named by its line");
    }

    #[test]
    fn a_missing_header_is_an_error_not_a_skipped_row() {
        assert!(parse_quarantine("a > b,s,M1,2026-11-01,2026-11-10\n", "2026-11-02").is_err());
        assert!(parse_quarantine("", "2026-11-02").is_err());
        assert!(parse_quarantine(HDR, "not-a-date").is_err());
    }

    #[test]
    fn failing_ids_come_from_vitest_cargo_and_node() {
        let out = "FAIL  a.test.ts > pays\ntest m::t ... FAILED\nnot ok 3 - the thing\nFAIL  a.test.ts > pays\n";
        assert_eq!(failing_ids(out), vec!["a.test.ts > pays", "m::t", "the thing"]);
    }

    #[test]
    fn the_quarantine_match_is_the_exact_id_never_a_substring() {
        assert!(retry_cause("s", 1, "FAIL  a.test.ts > pays\n", &q("a.test.ts > pays")).is_some());
        assert!(retry_cause("s", 1, "FAIL  a.test.ts > pays twice\n", &q("a.test.ts > pays")).is_none(), "a longer id is another test");
        assert!(retry_cause("s", 1, "FAIL  a.test.ts > pays\n", &q("e")).is_none(), "a one-letter row covers nothing");
        assert!(retry_cause("other", 1, "FAIL  a.test.ts > pays\n", &q("a.test.ts > pays")).is_none(), "another step's row");
    }

    #[test]
    fn a_mixed_red_is_not_retried_and_every_cause_is_recorded() {
        let out = "FAIL  a > one\nFAIL  a > two\n";
        let both = vec![q("a > one")[0].clone(), q("a > two")[0].clone()];
        let c = retry_cause("s", 1, out, &both).unwrap();
        assert_eq!(c.tests, vec!["a > one", "a > two"]);
        assert!(retry_cause("s", 1, out, &both[..1]).is_none());
    }

    #[test]
    fn infra_is_retried_only_when_nothing_in_the_deny_set_appears() {
        let docker = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?\n";
        assert_eq!(retry_cause("s", 1, docker, &[]).unwrap().cls, "infra-docker");
        assert_eq!(retry_cause("s", 1, "bind: address already in use\n", &[]).unwrap().via, "infra:infra-port");
        for bad in [
            "thread 'x' panicked at src/a.rs:1:1:\nConnectionRefused\naddress already in use\n",
            "error[E0432]: unresolved import\naddress already in use\n",
            "AssertionError: expected EADDRINUSE to be undefined\n",
            "assertion `left == right` failed\nconnection refused\naddress already in use\n",
            "address already in use\nFAIL  a > b\n",
            "address already in use\nchain: timed out after 5 s\n",
            "error TS2345: bad\naddress already in use\n",
        ] {
            assert!(retry_cause("s", 1, bad, &[]).is_none(), "{bad}");
        }
        assert!(retry_cause("s", 1, "no signature at all\n", &[]).is_none(), "an unclassified red");
    }

    #[test]
    fn a_timeout_or_a_dead_worker_is_never_retried() {
        for rc in [124, 125, 127, 0] {
            assert!(retry_cause("s", rc, "address already in use\n", &[]).is_none(), "rc {rc}");
        }
        assert!(retry_cause("s", 1, "FAIL  a > b\nerror[E0599]: x\n", &q("a > b")).is_none(), "a compile error beats a quarantine row");
    }
}
