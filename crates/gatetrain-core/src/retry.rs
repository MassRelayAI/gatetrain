//! Retry eligibility (D446, ADR 003). **The only path is the quarantine**: a red step is re-run once only when a live, tracked
//! quarantine row names every test that failed. There is no infra auto-retry and no list of "real failure" wording to keep up to date:
//! a rule that must recognise every way a run can go wrong loses to the next runner. This rule instead needs the run to *prove* that it
//! failed only in quarantined tests, by construction:
//!
//! 1. the step declares its test runner (`runner = "vitest"|"node"|"cargo"|"playwright"|"pytest"`); an undeclared runner is never retried;
//! 2. ANSI colour is stripped, then the runner's own result lines are parsed;
//! 3. the runner's own failed count equals the number of failing test ids parsed (a missing or different count, an unhandled error, a
//!    setup or collection error, a worker crash: no retry);
//! 4. every parsed id is the EXACT `test` of a live quarantine row for this step;
//! 5. the exit code is the runner's documented test-failure code (1; cargo 101). 0, other codes, anything 128 or above (a signal: segfault,
//!    OOM kill, abort) and the timeout codes 124, 125, 127 never retry;
//! 6. as an extra belt, no compile error and no gatetrain timeout note in the output.
use regex::Regex;
use std::sync::OnceLock;

/// A step's output larger than this is never judged.
pub const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
/// The first line a quarantine file must carry.
pub const QUARANTINE_HEADER: &str = "test,step,finding,added,expires";
/// Longest life of a quarantine row, in days (`expires` minus `added`).
pub const MAX_QUARANTINE_DAYS: i64 = 14;
/// The runners a step may declare.
pub const RUNNERS: &[&str] = &["vitest", "node", "cargo", "playwright", "pytest"];

/// Extra belt: a compile error anywhere, or gatetrain's own timeout note, is never a flake.
const BELT: &str = r"error\[E\d+\]|could not compile|error TS\d+|\bSyntaxError\b|chain: timed out";

fn rx(cell: &'static OnceLock<Regex>, p: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(p).expect("retry regex"))
}

/// Remove ANSI escape sequences (colours, cursor moves).
pub fn strip_ansi(s: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    rx(&R, r"\x1b\[[0-9;?]*[ -/]*[@-~]").replace_all(s, "").into_owned()
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a runner's output proves: the failing ids it names, the failed count it states, and whether anything else went wrong.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub ids: Vec<String>,
    /// the runner's own number of failed tests; None when it printed none
    pub failed: Option<usize>,
    /// a setup, collection, unhandled or crash signal the ids do not cover
    pub extra: bool,
}

fn ids_of(re: &Regex, out: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for c in re.captures_iter(out) {
        let id = norm(&c[1]);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

fn sum_failed(re: &Regex, out: &str) -> Option<usize> {
    let all: Vec<usize> = re.captures_iter(out).filter_map(|c| c[1].parse().ok()).collect();
    (!all.is_empty()).then(|| all.iter().sum())
}

/// Parse an ANSI-stripped output with one runner's rules. None = not a known runner.
pub fn parse(runner: &str, out: &str) -> Option<Outcome> {
    static VID: OnceLock<Regex> = OnceLock::new();
    static VCOUNT: OnceLock<Regex> = OnceLock::new();
    static VX: OnceLock<Regex> = OnceLock::new();
    static NID: OnceLock<Regex> = OnceLock::new();
    static NCOUNT: OnceLock<Regex> = OnceLock::new();
    static NX: OnceLock<Regex> = OnceLock::new();
    static CID: OnceLock<Regex> = OnceLock::new();
    static CCOUNT: OnceLock<Regex> = OnceLock::new();
    static PID: OnceLock<Regex> = OnceLock::new();
    static PCOUNT: OnceLock<Regex> = OnceLock::new();
    static PX: OnceLock<Regex> = OnceLock::new();
    static YID: OnceLock<Regex> = OnceLock::new();
    static YFINAL: OnceLock<Regex> = OnceLock::new();
    static YERR: OnceLock<Regex> = OnceLock::new();
    static YX: OnceLock<Regex> = OnceLock::new();
    static CX: OnceLock<Regex> = OnceLock::new();
    static CRES: OnceLock<Regex> = OnceLock::new();
    static PXE: OnceLock<Regex> = OnceLock::new();
    Some(match runner {
        "vitest" => {
            let ids = ids_of(rx(&VID, r"(?m)^\s*FAIL\s+(\S.*?)\s*$"), out);
            // a suite-level failure (`FAIL file [ file ]`) names no test: it counts as an error with no id
            let extra = rx(&VX, r"Unhandled (?:Errors|Rejection)|Vitest caught \d+ unhandled").is_match(out) || ids.iter().any(|i| !i.contains(" > "));
            Outcome { failed: sum_failed(rx(&VCOUNT, r"(?m)^\s*Tests\s+(\d+) failed"), out), ids, extra }
        }
        "node" => {
            let ids = ids_of(rx(&NID, r"(?m)^\s*not ok \d+ - (.+?)\s*$"), out);
            Outcome { failed: sum_failed(rx(&NCOUNT, r"(?m)^(?:#|ℹ) fail (\d+)\s*$"), out), ids, extra: rx(&NX, r"(?m)^(?:#|ℹ) cancelled [1-9]").is_match(out) }
        }
        "cargo" => {
            let ids = ids_of(rx(&CID, r"(?m)^test (\S.*?) \.\.\. FAILED\s*$"), out);
            // a test binary that dies (SIGSEGV, abort) prints no `test result:` line: it shows as `process didn't exit successfully`, or as an
            // `error: test failed` line with no matching `test result: FAILED` (real `cargo test --no-fail-fast` output has one per failing binary)
            let died = out.contains("process didn't exit successfully");
            let failed_runs = rx(&CX, r"(?m)^error: test failed").find_iter(out).count();
            let result_lines = rx(&CRES, r"(?m)^test result: FAILED\.").find_iter(out).count();
            Outcome { failed: sum_failed(rx(&CCOUNT, r"(?m)^test result: FAILED\. \d+ passed; (\d+) failed;"), out), ids, extra: died || failed_runs != result_lines }
        }
        "playwright" => {
            let ids = ids_of(rx(&PID, r"(?m)^\s*\d+\)\s+(.+?)(?:\s+─+)?\s*$"), out);
            // `N error(s) was/were not a part of any test` is playwright's own wording for a failure outside every test (setup, teardown, a worker crash)
            let extra = rx(&PX, r"(?m)^\s*\d+ (?:interrupted|did not run)\s*$").is_match(out) || rx(&PXE, r"\d+ errors? (?:was|were) not a part of any test").is_match(out);
            Outcome { failed: sum_failed(rx(&PCOUNT, r"(?m)^\s*(\d+) failed\s*$"), out), ids, extra }
        }
        "pytest" => {
            // `FAILED <nodeid> - <message>`: the id runs to ` - ` or the end of the line (a parametrized id may hold spaces)
            let ids = ids_of(rx(&YID, r"(?m)^FAILED (.+?)(?: - .*)?\s*$"), out);
            let fin = rx(&YFINAL, r"(?m)^=+ (.* in [\d.]+s.*?) =+\s*$").captures_iter(out).last().map(|c| c[1].to_string());
            let num = |what: &str| fin.as_deref().and_then(|f| Regex::new(&format!(r"(\d+) {what}")).ok()?.captures(f)?[1].parse::<usize>().ok());
            let errors = num("errors?").unwrap_or(0);
            let extra = errors > 0 || rx(&YX, r"(?m)^ERROR ").is_match(out) || rx(&YERR, r"(?m)^=+ ERRORS =+").is_match(out);
            Outcome { failed: num("failed"), ids, extra }
        }
        _ => return None,
    })
}

/// The runner's own final summary line must be the LAST non-blank line of the (ANSI-stripped) output. Anything after it (another command's
/// output, a drift check, a second tool) means the runner was not the step's whole outcome: no retry. `runner` may therefore only be declared
/// on a step whose command is a single runner invocation.
pub fn final_line_ok(runner: &str, out: &str) -> bool {
    static V: OnceLock<Regex> = OnceLock::new();
    static N: OnceLock<Regex> = OnceLock::new();
    static C: OnceLock<Regex> = OnceLock::new();
    static P: OnceLock<Regex> = OnceLock::new();
    static Y: OnceLock<Regex> = OnceLock::new();
    let Some(last) = out.lines().rev().find(|l| !l.trim().is_empty()) else { return false };
    let re = match runner {
        // vitest ends with `Duration  1.2s (...)` after the `Tests` line
        "vitest" => rx(&V, r"^\s*Duration\s+\S"),
        // node: TAP `# duration_ms 12` or the spec reporter's `ℹ duration_ms 12`
        "node" => rx(&N, r"^(?:#|ℹ) duration_ms \d"),
        // cargo: `error: test failed, to rerun pass ...`, or the indented target list of `error: N targets failed:`
        "cargo" => rx(&C, r"^error: test failed, to rerun pass |^\s+`.+`\s*$"),
        // playwright's summary ends with a count line (`N passed (2s)`, `N failed`, ...) or a failed-test header under one
        "playwright" => rx(&P, r"^\s*\d+ (?:passed|failed|flaky|skipped|interrupted|did not run)\b|^\s+\S.* › .*$"),
        // pytest: the `=== 1 failed, 3 passed in 0.12s ===` line
        "pytest" => rx(&Y, r"^=+ .* in [\d.]+s.*=+\s*$"),
        _ => return false,
    };
    re.is_match(last)
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
    /// always `quarantine`
    pub cls: String,
    /// `quarantine:<finding>`
    pub via: String,
    /// every failing test id, all of them quarantined
    pub tests: Vec<String>,
}

/// The documented test-failure exit code of a runner.
fn failure_code(runner: &str) -> i64 {
    if runner == "cargo" {
        101
    } else {
        1
    }
}

/// The strict rule. `output` is the step's whole output; None = the step may not be re-run.
pub fn retry_cause(runner: Option<&str>, step: &str, rc: i64, output: &str, quarantine: &[QuarantineRow]) -> Option<RetryCause> {
    let runner = runner.filter(|r| RUNNERS.contains(r))?;
    if rc != failure_code(runner) {
        return None;
    }
    let out = strip_ansi(output);
    static BELT_RE: OnceLock<Regex> = OnceLock::new();
    if rx(&BELT_RE, BELT).is_match(&out) {
        return None;
    }
    if !final_line_ok(runner, &out) {
        return None;
    }
    let p = parse(runner, &out)?;
    if p.extra || p.ids.is_empty() || p.failed != Some(p.ids.len()) {
        return None;
    }
    let mut finding = None;
    for id in &p.ids {
        let row = quarantine.iter().find(|q| q.step == step && q.test == *id)?;
        finding.get_or_insert(row.finding.clone());
    }
    Some(RetryCause { cls: "quarantine".into(), via: format!("quarantine:{}", finding?), tests: p.ids })
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
    fn go(runner: &str, rc: i64, out: &str, test: &str) -> Option<RetryCause> {
        retry_cause(Some(runner), "s", rc, out, &q(test))
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
        assert!(rows("a > b,s,M1,2090-01-01,2090-01-10\n", "2026-11-20").0.is_empty());
        assert!(rows("a > b,s,M1,2026-11-01,2026-11-16\n", "2026-11-02").0.is_empty(), "15 days is too long");
        assert_eq!(rows("a > b,s,M1,2026-11-01,2026-11-15\n", "2026-11-02").0.len(), 1, "14 days is the limit");
        assert!(rows("a > b,s,M1,2026-02-31,2026-03-02\n", "2026-03-01").0.is_empty());
        assert!(rows("a > b,s,M 1,2026-11-01,2026-11-10\n", "2026-11-02").0.is_empty(), "a finding id with a space");
        assert_eq!(rows("a > b,s,M1,2026-11-01,2026-11-16\n", "2026-11-02").1[0].0, 2, "a dropped row is named by its line");
    }

    #[test]
    fn a_missing_header_is_an_error_not_a_skipped_row() {
        assert!(parse_quarantine("a > b,s,M1,2026-11-01,2026-11-10\n", "2026-11-02").is_err());
        assert!(parse_quarantine("", "2026-11-02").is_err());
        assert!(parse_quarantine(HDR, "not-a-date").is_err());
    }

    #[test]
    fn ansi_is_stripped_before_parsing() {
        assert_eq!(strip_ansi("\u{1b}[31m FAIL \u{1b}[39m  a > b"), " FAIL   a > b");
    }

    const VITEST: &str = "\u{1b}[31m FAIL \u{1b}[39m  a.test.ts > pays\nAssertionError: x\n\u{1b}[2m      Tests \u{1b}[22m \u{1b}[1m\u{1b}[31m1 failed\u{1b}[39m | 3 passed (4)\n   Duration  1.2s (transform 1ms)\n";
    const NODE: &str = "not ok 1 - pays\n# tests 4\n# pass 3\n# fail 1\n# duration_ms 12.3\n";
    const CARGO: &str = "test m::pays ... FAILED\ntest result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\nerror: test failed, to rerun pass `--test a`\n";
    const PLAY: &str = "  1) [chromium] › a.spec.ts:3:1 › pays ──────────\n  1 failed\n    [chromium] › a.spec.ts:3:1 › pays\n  3 passed (2s)\n";
    const PYTEST: &str = "=== short test summary info ===\nFAILED tests/a.py::test_pays - assert 1 == 2\n=== 1 failed, 3 passed in 0.12s ===\n";
    const DUR: &str = "   Duration  1s\n";

    /// Real `cargo test --no-fail-fast` output (cargo 1.98, paths shortened): binary `a` has a failing test, binary `b` dies with SIGSEGV
    /// and prints no `test result:` line.
    const CARGO_X1: &str = r#"   Compiling cap v0.1.0 (/tmp/cap)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.97s
     Running unittests src/lib.rs (target/debug/deps/cap-a4e771e757e5195c)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/a.rs (target/debug/deps/a-1016cff4e1fb7a79)

running 2 tests
test ok ... ok
test pays ... FAILED

failures:

---- pays stdout ----

thread 'pays' (3851872) panicked at tests/a.rs:2:13:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    pays

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--test a`
     Running tests/b.rs (target/debug/deps/b-8e92244dbb1a6189)

running 1 test
error: test failed, to rerun pass `--test b`

Caused by:
  process didn't exit successfully: `target/debug/deps/b-8e92244dbb1a6189` (signal: 11, SIGSEGV: invalid memory reference)
   Doc-tests cap

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: 2 targets failed:
    `--test a`
    `--test b`
"#;

    #[test]
    fn a_quarantined_failure_with_a_matching_count_is_retried_for_every_runner() {
        for (runner, rc, out, id) in [
            ("vitest", 1, VITEST, "a.test.ts > pays"),
            ("node", 1, NODE, "pays"),
            ("cargo", 101, CARGO, "m::pays"),
            ("playwright", 1, PLAY, "[chromium] › a.spec.ts:3:1 › pays"),
            ("pytest", 1, PYTEST, "tests/a.py::test_pays"),
        ] {
            let c = go(runner, rc, out, id).unwrap_or_else(|| panic!("{runner} was not retried"));
            assert_eq!((c.cls.as_str(), c.via.as_str(), c.tests), ("quarantine", "quarantine:M1", vec![id.to_string()]), "{runner}");
        }
    }

    #[test]
    fn nothing_is_retried_without_a_declared_runner_or_a_row() {
        assert!(retry_cause(None, "s", 1, VITEST, &q("a.test.ts > pays")).is_none(), "an undeclared runner");
        assert!(retry_cause(Some("go"), "s", 1, VITEST, &q("a.test.ts > pays")).is_none(), "an unknown runner");
        assert!(retry_cause(Some("vitest"), "s", 1, VITEST, &[]).is_none(), "no row, no retry");
        assert!(retry_cause(Some("vitest"), "other", 1, VITEST, &q("a.test.ts > pays")).is_none(), "another step's row");
    }

    #[test]
    fn the_row_is_the_exact_id_never_a_substring() {
        assert!(go("vitest", 1, VITEST, "a.test.ts > pays twice").is_none());
        assert!(go("vitest", 1, VITEST, "e").is_none());
        assert!(go("vitest", 1, &VITEST.replace("> pays", "> pays twice"), "a.test.ts > pays").is_none());
    }

    #[test]
    fn a_count_that_is_missing_or_differs_is_never_retried() {
        let two = format!("FAIL  a.test.ts > pays\nFAIL  a.test.ts > refunds\n      Tests  1 failed | 3 passed (4)\n{DUR}");
        assert!(go("vitest", 1, &two, "a.test.ts > pays").is_none(), "two ids, one failed");
        assert!(go("vitest", 1, &format!("FAIL  a.test.ts > pays\n{DUR}"), "a.test.ts > pays").is_none(), "no count at all");
        let more = format!("FAIL  a.test.ts > pays\n      Tests  2 failed | 3 passed (5)\n{DUR}");
        assert!(go("vitest", 1, &more, "a.test.ts > pays").is_none(), "a failure with no id");
        assert!(go("vitest", 1, &format!("      Tests  1 failed (1)\nTypeError: x is not a function\n{DUR}"), "a > b").is_none(), "no id parsed");
    }

    #[test]
    fn a_setup_or_collection_error_beside_a_quarantined_failure_is_never_retried() {
        let q1 = format!("{VITEST}\n Unhandled Errors \nError: boom\n{DUR}");
        assert!(go("vitest", 1, &q1, "a.test.ts > pays").is_none(), "Q1");
        let collect = format!("FAIL  a.test.ts [ a.test.ts ]\n      Tests  1 failed (1)\n{DUR}");
        assert!(go("vitest", 1, &collect, "a.test.ts [ a.test.ts ]").is_none(), "a suite failure is not a test id");
        let py = format!("{PYTEST}ERROR tests/b.py - ImportError\n=== 1 failed, 1 error in 0.1s ===\n");
        assert!(go("pytest", 1, &py, "tests/a.py::test_pays").is_none());
        let pw = PLAY.replace("1 failed", "1 failed\n  1 interrupted");
        assert!(go("playwright", 1, &pw, "[chromium] › a.spec.ts:3:1 › pays").is_none());
    }

    #[test]
    fn only_the_runners_failure_code_retries_never_a_signal_or_a_timeout() {
        for rc in [0, 2, 124, 125, 127, 128, 134, 137, 139, 255] {
            assert!(go("vitest", rc, VITEST, "a.test.ts > pays").is_none(), "rc {rc}");
        }
        assert!(go("cargo", 1, CARGO, "m::pays").is_none(), "cargo's code is 101");
        assert!(go("vitest", 101, VITEST, "a.test.ts > pays").is_none());
    }

    #[test]
    fn a_compile_error_or_a_timeout_note_beside_a_quarantined_failure_is_never_retried() {
        assert!(go("cargo", 101, &format!("error[E0599]: no method\n{CARGO}"), "m::pays").is_none());
        assert!(go("vitest", 1, &format!("chain: timed out after 5 s\n{VITEST}"), "a.test.ts > pays").is_none());
    }

    #[test]
    fn x1_a_cargo_binary_that_dies_beside_a_quarantined_failure_is_never_retried() {
        // the real output parses to one quarantined id and a count of 1, rc 101: only the dead binary tells it apart
        let p = parse("cargo", CARGO_X1).unwrap();
        assert_eq!((p.ids.as_slice(), p.failed), (["pays".to_string()].as_slice(), Some(1)));
        assert!(p.extra, "a SIGSEGV binary is an error with no id");
        assert!(go("cargo", 101, CARGO_X1, "pays").is_none());
        // without the `process didn't exit successfully` line, the unmatched `error: test failed` still gives it away
        let no_cause = CARGO_X1.replace("process didn't exit successfully", "process ended");
        assert!(go("cargo", 101, &no_cause, "pays").is_none(), "2 `error: test failed` lines against 1 `test result: FAILED`");
    }

    #[test]
    fn x2_a_playwright_error_outside_every_test_is_never_retried() {
        // playwright's own wording (lib/runner/index.js generateSummaryMessage): `1 error was not a part of any test, see above for details`
        let one = format!("{PLAY}  1 error was not a part of any test, see above for details\n");
        assert!(go("playwright", 1, &one, "[chromium] › a.spec.ts:3:1 › pays").is_none());
        assert!(parse("playwright", &one).unwrap().extra);
        let two = PLAY.replace("  3 passed (2s)", "  3 passed (2s)\n  2 errors were not a part of any test, see above for details");
        assert!(parse("playwright", &two).unwrap().extra);
    }

    #[test]
    fn x3_the_runner_must_be_the_whole_outcome_the_last_line_is_its_own_summary() {
        let drift = format!("{VITEST}ledger drift: 1000 != 999\n");
        assert!(go("vitest", 1, &drift, "a.test.ts > pays").is_none(), "something ran after the runner");
        assert!(go("vitest", 1, &format!("{VITEST}\n\n"), "a.test.ts > pays").is_some(), "trailing blank lines are not output");
        for (runner, out) in [("vitest", VITEST), ("node", NODE), ("cargo", CARGO), ("playwright", PLAY), ("pytest", PYTEST)] {
            assert!(final_line_ok(runner, out), "{runner}");
            assert!(!final_line_ok(runner, &format!("{out}extra line\n")), "{runner}: a line after the summary");
        }
        assert!(final_line_ok("cargo", CARGO_X1), "the target list is cargo's own last line");
        assert!(!final_line_ok("vitest", ""));
    }

    #[test]
    fn x8_a_pytest_id_runs_to_the_dash_separator_or_the_end_of_line() {
        let out = "FAILED tests/a.py::test_total[eur 1000] - assert 1 == 2\n=== 1 failed in 0.1s ===\n";
        assert_eq!(parse("pytest", out).unwrap().ids, vec!["tests/a.py::test_total[eur 1000]"]);
        assert!(go("pytest", 1, out, "tests/a.py::test_total[eur").is_none(), "a prefix row does not cover it");
        assert!(go("pytest", 1, out, "tests/a.py::test_total[eur 1000]").is_some());
        let bare = "FAILED tests/a.py::test_total[eur 1000]\n=== 1 failed in 0.1s ===\n";
        assert_eq!(parse("pytest", bare).unwrap().ids, vec!["tests/a.py::test_total[eur 1000]"]);
    }
}
