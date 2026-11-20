//! `gatetrain chain --retry-flakes` (D446, ADR 003): the ONLY retry path is the quarantine. A red step is re-run once, on the same sha, only
//! when the step declares its runner, the runner's own failed count equals the failing ids parsed, every id is the exact `test` of a live
//! row read from a trusted ref, and the exit code is the runner's test-failure code. There is no infra retry. A green retry is never
//! reported as a first-try green.
use gatetrain_core::logfmt::parse_log;
use std::path::{Path, PathBuf};
use std::process::Command;

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

fn step_with(name: &str, extra_toml: &str, args: &[&str]) -> String {
    let mut argv = vec![format!("'{}'", child())];
    argv.extend(args.iter().map(|a| format!("'{a}'")));
    format!("[[step]]\nname = '{name}'\ntier = 'fast'\n{extra_toml}argv = [{}]\n\n", argv.join(", "))
}

/// A step that declares `runner` (None = undeclared).
fn step(name: &str, runner: Option<&str>, args: &[&str]) -> String {
    let r = runner.map(|r| format!("runner = '{r}'\n")).unwrap_or_default();
    step_with(name, &r, args)
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

struct Run {
    code: i32,
    stdout: String,
    out: PathBuf,
    _dir: Option<tempfile::TempDir>,
}

const HDR: &str = "test,step,finding,added,expires\n";

/// A committed repo holding the steps file and `quarantine.csv`; the branch `trusted` is that commit, and HEAD is one empty commit
/// later (the change under test), so `trusted:quarantine.csv` is a trusted ref that is not the commit under test.
fn repo(steps: &str, quarantine: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    std::fs::write(d.join("steps.toml"), steps).unwrap();
    std::fs::write(d.join(".gitignore"), "out/\n").unwrap();
    std::fs::write(d.join("quarantine.csv"), quarantine).unwrap();
    git(d, &["add", "steps.toml", ".gitignore", "quarantine.csv"]);
    git(d, &["commit", "-q", "-m", "init"]);
    git(d, &["branch", "trusted"]);
    git(d, &["commit", "-q", "--allow-empty", "-m", "under-test"]);
    dir
}

fn run_in(d: &Path, extra: &[&str]) -> Run {
    let out = d.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", d.join("steps.toml").to_str().unwrap(), "--out", out.to_str().unwrap()])
        .args(extra)
        .current_dir(d)
        .output()
        .unwrap();
    Run { code: o.status.code().unwrap(), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), out, _dir: None }
}

fn chain(steps: &str, quarantine: &str, extra: &[&str]) -> Run {
    let dir = repo(steps, quarantine);
    let mut r = run_in(dir.path(), extra);
    r._dir = Some(dir);
    r
}

const QREF: &[&str] = &["--retry-flakes", "--quarantine-ref", "trusted:quarantine.csv"];

/// One step `a` of `runner` that is red the first time (printing `lines`, exiting `rc`) and green after. The marker lives outside the repo.
fn flaky(runner: Option<&str>, rc: i64, lines: &[&str], quarantine: &str, extra: &[&str]) -> Run {
    let m = tempfile::tempdir().unwrap();
    let mk = m.path().join("a.marker").to_string_lossy().into_owned();
    let rc = rc.to_string();
    let mut args = vec!["flaky", mk.as_str(), rc.as_str()];
    args.extend(lines);
    chain(&step("a", runner, &args), quarantine, extra)
}

fn marker() -> String {
    tempfile::tempdir().unwrap().keep().join("m").to_string_lossy().into_owned()
}

fn retries(r: &Run) -> usize {
    r.stdout.lines().filter(|l| l.starts_with("STEP_RETRY ")).count()
}

/// Today (UTC) plus `n` days as `YYYY-MM-DD`.
fn day(n: i64) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let z = secs.div_euclid(86400) + n + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A live quarantine file with one row per `(test, step)`.
fn live(rows: &[(&str, &str)]) -> String {
    let mut s = HDR.to_string();
    for (test, st) in rows {
        s += &format!("{test},{st},M1234,{},{}\n", day(0), day(10));
    }
    s
}

fn csv_row(test: &str, added: &str, expires: &str) -> String {
    format!("{HDR}{test},a,M1234,{added},{expires}\n")
}

// the runners' own outputs, ANSI colour included where a real run has it
const VITEST: &[&str] = &["\\e[31m FAIL \\e[39m  a.test.ts > pays", "AssertionError: boom", "\\e[2m      Tests \\e[22m \\e[1m\\e[31m1 failed\\e[39m | 3 passed (4)"];
const VID: &str = "a.test.ts > pays";

#[test]
fn a_quarantined_failure_is_retried_once_then_green_and_never_reads_as_a_first_try_green() {
    let r = flaky(Some("vitest"), 1, VITEST, &live(&[(VID, "a")]), QREF);
    assert_eq!(r.code, 0, "{}", r.stdout);
    let lines: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("STEP_RETRY ")).collect();
    assert_eq!(lines.len(), 1, "{}", r.stdout);
    assert!(lines[0].contains("a attempt=1 rc=1") && lines[0].contains("class=quarantine") && lines[0].contains("via=quarantine:M1234") && lines[0].contains("test=a.test.ts > pays"), "{}", lines[0]);
    assert!(r.stdout.contains("CHAIN_RETRIES a=green"), "{}", r.stdout);
    assert!(r.stdout.contains("RED_SUMMARY: none; retried: a(quarantine)"), "a retried green is never a bare none: {}", r.stdout);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.red_summary.as_deref(), Some("none"), "the reds stay what they were");
    assert_eq!((p.retries.len(), p.steps.len(), p.steps[0].exit_code), (1, 1, Some(0)));
    assert_eq!(p.to_json()["retries"][0]["step"], "a", "retries are in the parser's JSON");
    assert!(r.out.join("a.attempt1.full").exists());
    let stamp = std::fs::read_dir(&r.out).unwrap().flatten().map(|e| e.path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with("stamp-")).expect("a stamp");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stamp).unwrap()).unwrap();
    assert_eq!((v["retried"][0]["name"].as_str(), v["retried"][0]["first_rc"].as_i64(), v["retried"][0]["final_rc"].as_i64()), (Some("a"), Some(1), Some(0)), "{v}");
}

#[test]
fn every_supported_runner_has_a_positive_control() {
    let cases: Vec<(&str, i64, Vec<&str>, &str)> = vec![
        ("vitest", 1, VITEST.to_vec(), VID),
        ("node", 1, vec!["not ok 1 - pays", "# tests 4", "# pass 3", "# fail 1"], "pays"),
        ("cargo", 101, vec!["test m::pays ... FAILED", "test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"], "m::pays"),
        ("playwright", 1, vec!["  1) [chromium] › a.spec.ts:3:1 › pays ──────", "  1 failed", "  3 passed (2s)"], "[chromium] › a.spec.ts:3:1 › pays"),
        ("pytest", 1, vec!["=== short test summary info ===", "FAILED tests/a.py::test_pays - assert 1 == 2", "=== 1 failed, 3 passed in 0.12s ==="], "tests/a.py::test_pays"),
    ];
    for (runner, rc, lines, id) in cases {
        let r = flaky(Some(runner), rc, &lines, &live(&[(id, "a")]), QREF);
        assert_eq!((r.code, retries(&r)), (0, 1), "{runner}: {}", r.stdout);
    }
}

#[test]
fn the_parse_view_names_the_retries() {
    let r = flaky(Some("vitest"), 1, VITEST, &live(&[(VID, "a")]), QREF);
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("run.log");
    std::fs::write(&log, &r.stdout).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["parse", log.to_str().unwrap()]).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("retried (never a first-try green): a=green") && text.contains("1x a.test.ts > pays"), "{text}");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["parse", "--json", log.to_str().unwrap()]).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("\"retries\""));
}

#[test]
fn a_retry_that_is_red_again_stays_red() {
    let mut lines = vec!["redprint", "1"];
    lines.extend(VITEST);
    let r = chain(&step("a", Some("vitest"), &lines), &live(&[(VID, "a")]), QREF);
    assert_eq!((r.code, retries(&r)), (1, 1), "re-run once, never twice: {}", r.stdout);
    assert!(r.stdout.contains("CHAIN_RETRIES a=red") && r.stdout.contains("RED_SUMMARY: a"), "{}", r.stdout);
}

#[test]
fn the_probes_that_were_once_green_are_never_retried_by_construction() {
    let mut g = vec!["error TS2345: Argument of type x is not assignable".to_string()];
    g.extend((0..60).map(|i| format!("line {i} of a long build log")));
    g.push("bind: address already in use".into());
    let g: Vec<&str> = g.iter().map(String::as_str).collect();
    let none = HDR.to_string();
    let pw = ["  1) [chromium] › a.spec.ts:3:1 › pays", "  2) [chromium] › a.spec.ts:4:1 › refunds", "  2 failed"];
    // (name, runner, rc, lines, quarantine file)
    let cases: Vec<(&str, Option<&str>, i64, Vec<&str>, String)> = vec![
        ("G: the real error above the tail", Some("vitest"), 1, g, none.clone()),
        ("N1 playwright failure marks", Some("playwright"), 1, vec!["  ✘  1 [chromium] › a.spec.ts:3:1 › pays (1s)", "  1 failed", "  1) [chromium] › a.spec.ts:3:1 › pays"], none.clone()),
        ("N2 pytest ERROR and E lines", Some("pytest"), 1, vec!["ERROR tests/a.py::test_x - fixture", "E   AssertionError", "=== 1 error in 0.1s ==="], live(&[("tests/a.py::test_x", "a")])),
        ("N3 Go --- FAIL with no runner", None, 1, vec!["--- FAIL: TestX (0.00s)", "FAIL"], live(&[("TestX", "a")])),
        ("N4 TypeError", Some("vitest"), 1, vec!["TypeError: x is not a function"], none.clone()),
        ("N5 npm ERR!", Some("vitest"), 1, vec!["npm ERR! code ELIFECYCLE"], none.clone()),
        ("N6 a segfault at rc 139", Some("vitest"), 139, VITEST.to_vec(), live(&[(VID, "a")])),
        ("N7 an OOM kill at rc 137", Some("vitest"), 137, VITEST.to_vec(), live(&[(VID, "a")])),
        ("N8 a cargo SIGABRT", Some("cargo"), 101, vec!["process didnt exit successfully (signal: 6, SIGABRT: process abort signal)"], none.clone()),
        ("N9 ANSI FAIL that is not quarantined", Some("vitest"), 1, vec!["\\e[31m FAIL \\e[39m  a.test.ts > other", "      Tests  1 failed (1)"], live(&[(VID, "a")])),
        ("Q1 a quarantined id plus Unhandled Errors", Some("vitest"), 1, vec!["FAIL  a.test.ts > pays", " Unhandled Errors ", "Error: boom", "      Tests  1 failed (1)"], live(&[(VID, "a")])),
        ("Q2 a quarantined id plus another playwright failure", Some("playwright"), 1, pw.to_vec(), live(&[("[chromium] › a.spec.ts:3:1 › pays", "a")])),
    ];
    for (name, runner, rc, lines, q) in cases {
        let r = flaky(runner, rc, &lines, &q, QREF);
        assert_eq!((r.code, retries(&r)), (if rc == 0 { 0 } else { 1 }, 0), "{name}: {}", r.stdout);
    }
}

#[test]
fn an_undeclared_runner_is_never_retried_even_with_a_matching_row() {
    let r = flaky(None, 1, VITEST, &live(&[(VID, "a")]), QREF);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
}

#[test]
fn an_unknown_runner_is_a_config_error() {
    let r = chain(&step("a", Some("jest"), &["pass"]), HDR, &[]);
    assert_eq!(r.code, 2, "{}", r.stdout);
}

#[test]
fn an_expired_future_or_too_long_row_does_not_qualify() {
    for (what, q) in [
        ("expired", csv_row(VID, &day(-30), &day(-20))),
        ("a 2090 row", csv_row(VID, "2090-01-01", "2090-01-10")),
        ("a row living 15 days", csv_row(VID, &day(0), &day(15))),
        ("an impossible date", csv_row(VID, "2026-02-31", &day(5))),
    ] {
        let r = flaky(Some("vitest"), 1, VITEST, &q, QREF);
        assert_eq!((r.code, retries(&r)), (1, 0), "{what}: {}", r.stdout);
    }
}

#[test]
fn a_row_covers_only_its_exact_test_never_a_substring() {
    for (row, failing) in [("e", "a.test.ts > pays"), ("a.test.ts > pays", "a.test.ts > pays twice")] {
        let out = [format!("FAIL  {failing}"), "      Tests  1 failed (1)".to_string()];
        let lines: Vec<&str> = out.iter().map(String::as_str).collect();
        let r = flaky(Some("vitest"), 1, &lines, &live(&[(row, "a")]), QREF);
        assert_eq!((r.code, retries(&r)), (1, 0), "row {row:?} must not cover {failing:?}: {}", r.stdout);
    }
}

#[test]
fn a_mixed_red_is_not_retried() {
    let lines = ["FAIL  a > one", "FAIL  a > two", "      Tests  2 failed (2)"];
    let r = flaky(Some("vitest"), 1, &lines, &live(&[("a > one", "a")]), QREF);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
}

#[test]
fn the_quarantine_is_read_from_the_trusted_ref_never_the_working_tree() {
    let dir = repo(&step("a", Some("vitest"), &["flaky", &marker(), "1", VITEST[0], VITEST[2]]), &csv_row(VID, &day(-30), &day(-20)));
    // the working tree under test carries a live row; the trusted ref does not
    std::fs::write(dir.path().join("quarantine.csv"), live(&[(VID, "a")])).unwrap();
    let r = run_in(dir.path(), QREF);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
}

#[test]
fn the_head_ref_and_the_index_form_are_refused() {
    for bad in ["HEAD:quarantine.csv", ":quarantine.csv"] {
        let r = flaky(Some("vitest"), 1, VITEST, &live(&[(VID, "a")]), &["--retry-flakes", "--quarantine-ref", bad]);
        assert_eq!((r.code, retries(&r)), (2, 0), "{bad}: {}", r.stdout);
    }
}

#[test]
fn a_quarantine_file_without_its_header_row_is_a_config_error() {
    let r = flaky(Some("vitest"), 1, VITEST, &format!("{VID},a,M1,{},{}\n", day(0), day(5)), QREF);
    assert_eq!(r.code, 2, "{}", r.stdout);
}

#[test]
fn a_third_flaky_step_is_not_retried() {
    let d = tempfile::tempdir().unwrap();
    let mut steps = String::new();
    for n in ["a", "b", "c"] {
        steps += &step(n, Some("vitest"), &["flaky", &d.path().join(format!("{n}.marker")).to_string_lossy(), "1", VITEST[0], VITEST[2]]);
    }
    let r = chain(&steps, &live(&[(VID, "a"), (VID, "b"), (VID, "c")]), &["--retry-flakes", "--quarantine-ref", "trusted:quarantine.csv", "--par", "1"]);
    assert_eq!((r.code, retries(&r)), (1, 2), "{}", r.stdout);
    assert!(r.stdout.contains("RED_SUMMARY: c"), "{}", r.stdout);
}

#[test]
fn without_the_flag_nothing_changes() {
    let r = flaky(Some("vitest"), 1, VITEST, &live(&[(VID, "a")]), &[]);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
    assert!(!r.stdout.contains("CHAIN_RETRIES") && !r.stdout.contains("retried:"), "{}", r.stdout);
    assert!(!r.out.join("a.attempt1.full").exists());
}

#[test]
fn a_retried_green_writes_no_cache_entry() {
    let extra = "runner = 'vitest'\ninputs = ['steps.toml']\n";
    let dir = repo(&step_with("a", extra, &["flaky", &marker(), "1", VITEST[0], VITEST[2]]), &live(&[(VID, "a")]));
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path().to_str().unwrap();
    let first = run_in(dir.path(), &["--retry-flakes", "--quarantine-ref", "trusted:quarantine.csv", "--cache", c]);
    assert_eq!((first.code, retries(&first)), (0, 1), "{}", first.stdout);
    let second = run_in(dir.path(), &["--retry-flakes", "--quarantine-ref", "trusted:quarantine.csv", "--cache", c]);
    assert_eq!(second.code, 0, "{}", second.stdout);
    assert!(!second.stdout.contains("STEP_CACHED"), "a retried green must not be served as a cached first-try green: {}", second.stdout);
}

#[test]
fn a_retried_green_does_not_feed_the_ledger_medians() {
    let steps = step("b", None, &["pass"]) + &step("a", Some("vitest"), &["flaky", &marker(), "1", VITEST[0], VITEST[2]]);
    let dir = repo(&steps, &live(&[(VID, "a")]));
    let db = tempfile::tempdir().unwrap().keep().join("g.db");
    let r = run_in(dir.path(), &["--retry-flakes", "--quarantine-ref", "trusted:quarantine.csv", "--db", db.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.stdout);
    let h = gatetrain_core::history::history_durations(&db, "").expect("history");
    assert!(h.iter().any(|(n, _)| n == "b"), "an ordinary green is a median input: {h:?}");
    assert!(!h.iter().any(|(n, _)| n == "a"), "the summed-duration retried green is not: {h:?}");
}
