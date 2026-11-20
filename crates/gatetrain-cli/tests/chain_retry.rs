//! `gatetrain chain --retry-flakes` (D446, ADR 003): a red step is re-run ONCE on the same sha only under the strict rule of
//! `gatetrain_core::retry` (a live quarantine row naming the exact failing test, or a pure infra failure). It is never the `diagnose` classes.
//! An unclassified red is never re-run; a green retry is never reported as a first-try green.
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

fn step(name: &str, args: &[&str]) -> String {
    step_with(name, "", args)
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

struct Run {
    code: i32,
    stdout: String,
    out: PathBuf,
}

/// A committed repo holding the steps file and, when given, `quarantine.csv`, so the tree is clean and a green run writes a stamp.
fn repo(steps: &str, quarantine: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q"]);
    std::fs::write(d.join("steps.toml"), steps).unwrap();
    std::fs::write(d.join(".gitignore"), "out/\n").unwrap();
    let mut add = vec!["add", "steps.toml", ".gitignore"];
    if let Some(q) = quarantine {
        std::fs::write(d.join("quarantine.csv"), q).unwrap();
        add.push("quarantine.csv");
    }
    git(d, &add);
    git(d, &["commit", "-q", "-m", "init"]);
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
    Run { code: o.status.code().unwrap(), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), out }
}

fn chain(steps: &str, quarantine: Option<&str>, extra: &[&str]) -> Run {
    let dir = repo(steps, quarantine);
    run_in(dir.path(), extra)
}

const DOCKER: &str = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";
const FLAG: &[&str] = &["--retry-flakes"];
const QREF: &[&str] = &["--retry-flakes", "--quarantine-ref", "HEAD:quarantine.csv"];

/// One step `a` that is red the first time it runs (printing `lines`) and green after. The marker lives outside the repo.
fn flaky(lines: &[&str], quarantine: Option<&str>, extra: &[&str]) -> Run {
    let m = tempfile::tempdir().unwrap();
    let mk = m.path().join("a.marker").to_string_lossy().into_owned();
    let mut args = vec!["flaky", mk.as_str()];
    args.extend(lines);
    chain(&step("a", &args), quarantine, extra)
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

fn csv(test: &str, added: &str, expires: &str) -> String {
    format!("test,step,finding,added,expires\n{test},a,M1234,{added},{expires}\n")
}

#[test]
fn a_pure_infra_red_is_retried_once_then_green_and_never_reads_as_a_first_try_green() {
    let r = flaky(&[DOCKER], None, FLAG);
    assert_eq!(r.code, 0, "{}", r.stdout);
    let lines: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("STEP_RETRY ")).collect();
    assert_eq!(lines.len(), 1, "{}", r.stdout);
    assert!(lines[0].contains("a attempt=1 rc=1") && lines[0].contains("class=infra-docker"), "{}", lines[0]);
    assert!(r.stdout.contains("CHAIN_RETRIES a=green"), "{}", r.stdout);
    assert!(r.stdout.contains("RED_SUMMARY: none; retried: a(infra-docker)"), "a retried green is never a bare none: {}", r.stdout);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.red_summary.as_deref(), Some("none"), "the reds stay what they were");
    assert_eq!(p.retries.len(), 1);
    assert_eq!(p.steps.len(), 1, "one logical step");
    assert_eq!(p.steps[0].exit_code, Some(0));
    let j = p.to_json();
    assert_eq!(j["retries"][0]["step"], "a", "retries are in the parser's JSON: {j}");
    assert!(r.out.join("a.attempt1.full").exists());
    let stamp = std::fs::read_dir(&r.out).unwrap().flatten().map(|e| e.path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with("stamp-")).expect("a stamp");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stamp).unwrap()).unwrap();
    assert_eq!((v["retried"][0]["name"].as_str(), v["retried"][0]["first_rc"].as_i64(), v["retried"][0]["final_rc"].as_i64()), (Some("a"), Some(1), Some(0)), "{v}");
}

#[test]
fn the_parse_view_names_the_retries() {
    let r = flaky(&[DOCKER], None, FLAG);
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("run.log");
    std::fs::write(&log, &r.stdout).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["parse", log.to_str().unwrap()]).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("retried (never a first-try green): a=green") && text.contains("1x infra-docker"), "{text}");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["parse", "--json", log.to_str().unwrap()]).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("\"retries\""));
}

#[test]
fn an_unclassified_red_is_not_retried() {
    let r = flaky(&["something broke"], None, FLAG);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
    assert!(!r.stdout.contains("CHAIN_RETRIES") && !r.stdout.contains("retried:"), "{}", r.stdout);
}

#[test]
fn a_retry_that_is_red_again_stays_red() {
    let r = chain(&step("a", &["redprint", DOCKER]), None, FLAG);
    assert_eq!((r.code, retries(&r)), (1, 1), "re-run once, never twice: {}", r.stdout);
    assert!(r.stdout.contains("CHAIN_RETRIES a=red") && r.stdout.contains("RED_SUMMARY: a"), "{}", r.stdout);
}

#[test]
fn an_infra_signature_beside_anything_real_is_never_retried() {
    let mut g = vec!["error TS2345: Argument of type 'x' is not assignable".to_string()];
    g.extend((0..60).map(|i| format!("line {i} of a long build log")));
    g.push("bind: address already in use".into());
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("A: a panic on ConnectionRefused", vec!["thread 'x' panicked at src/a.rs:1:1:".into(), "ConnectionRefused".into(), "bind: address already in use".into()]),
        ("C: a compile error outside any block", vec!["error[E0432]: unresolved import `x`".into(), "bind: address already in use".into()]),
        ("D: an assertion mentioning EADDRINUSE", vec!["AssertionError: expected EADDRINUSE to be undefined".into()]),
        ("D2: a Rust assertion and connection refused", vec!["assertion `left == right` failed".into(), "connection refused".into(), "bind: address already in use".into()]),
        ("G: the real error above the tail", g),
    ];
    for (name, lines) in cases {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let r = flaky(&refs, None, FLAG);
        assert_eq!((r.code, retries(&r)), (1, 0), "{name}: {}", r.stdout);
    }
}

#[test]
fn a_timeout_is_never_retried() {
    // H: the step prints an infra line, then outlives its timeout_s and is killed (rc 124)
    let toml = step_with("a", "timeout_s = 1\n", &["printsleep", "bind: address already in use", "5"]);
    let r = chain(&toml, None, FLAG);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
}

#[test]
fn a_live_quarantine_row_naming_the_exact_test_qualifies() {
    let q = csv("a.test.ts > findBy under load", &day(0), &day(10));
    let r = flaky(&["FAIL  a.test.ts > findBy under load"], Some(&q), QREF);
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(r.stdout.contains("via=quarantine:M1234") && r.stdout.contains("test=a.test.ts > findBy under load"), "{}", r.stdout);
}

#[test]
fn an_expired_future_or_too_long_row_does_not_qualify() {
    let fail = ["FAIL  a.test.ts > findBy under load"];
    for (what, q) in [
        ("expired", csv("a.test.ts > findBy under load", &day(-30), &day(-20))),
        ("a 2090 row", csv("a.test.ts > findBy under load", "2090-01-01", "2090-01-10")),
        ("a row living 15 days", csv("a.test.ts > findBy under load", &day(0), &day(15))),
        ("an impossible date", csv("a.test.ts > findBy under load", "2026-02-31", &day(5))),
    ] {
        let r = flaky(&fail, Some(&q), QREF);
        assert_eq!((r.code, retries(&r)), (1, 0), "{what}: {}", r.stdout);
    }
}

#[test]
fn a_row_covers_only_its_exact_test_never_a_substring() {
    for (row, failing) in [("e", "FAIL  a.test.ts > pays"), ("a.test.ts > pays", "FAIL  a.test.ts > pays twice")] {
        let r = flaky(&[failing], Some(&csv(row, &day(0), &day(10))), QREF);
        assert_eq!((r.code, retries(&r)), (1, 0), "row {row:?} must not cover {failing:?}: {}", r.stdout);
    }
}

#[test]
fn a_mixed_red_is_not_retried() {
    let q = csv("a > one", &day(0), &day(10));
    let r = flaky(&["FAIL  a > one", "FAIL  a > two"], Some(&q), QREF);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
}

#[test]
fn the_quarantine_is_read_from_the_ref_never_the_working_tree() {
    let dir = repo(&step("a", &["flaky", &marker(), "FAIL  a > one"]), Some(&csv("a > one", &day(-30), &day(-20))));
    // the working tree under test carries a live row; the committed ref does not
    std::fs::write(dir.path().join("quarantine.csv"), csv("a > one", &day(0), &day(10))).unwrap();
    let r = run_in(dir.path(), QREF);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
}

#[test]
fn a_quarantine_file_without_its_header_row_is_a_config_error() {
    let r = flaky(&["FAIL  a > one"], Some(&format!("a > one,a,M1,{},{}\n", day(0), day(5))), QREF);
    assert_eq!(r.code, 2, "{}", r.stdout);
}

#[test]
fn a_third_flaky_step_is_not_retried() {
    let d = tempfile::tempdir().unwrap();
    let mut steps = String::new();
    for n in ["a", "b", "c"] {
        steps += &step(n, &["flaky", &d.path().join(format!("{n}.marker")).to_string_lossy(), DOCKER]);
    }
    let r = chain(&steps, None, &["--retry-flakes", "--par", "1"]);
    assert_eq!((r.code, retries(&r)), (1, 2), "{}", r.stdout);
    assert!(r.stdout.contains("RED_SUMMARY: c"), "{}", r.stdout);
}

#[test]
fn without_the_flag_nothing_changes() {
    let r = flaky(&[DOCKER], None, &[]);
    assert_eq!((r.code, retries(&r)), (1, 0), "{}", r.stdout);
    assert!(!r.stdout.contains("CHAIN_RETRIES") && !r.stdout.contains("retried:"), "{}", r.stdout);
    assert!(!r.out.join("a.attempt1.full").exists());
}

#[test]
fn a_retried_green_writes_no_cache_entry() {
    let dir = repo(&step_with("a", "inputs = ['steps.toml']\n", &["flaky", &marker(), DOCKER]), None);
    let cache = tempfile::tempdir().unwrap();
    let c = cache.path().to_str().unwrap();
    let first = run_in(dir.path(), &["--retry-flakes", "--cache", c]);
    assert_eq!((first.code, retries(&first)), (0, 1), "{}", first.stdout);
    let second = run_in(dir.path(), &["--retry-flakes", "--cache", c]);
    assert_eq!(second.code, 0, "{}", second.stdout);
    assert!(!second.stdout.contains("STEP_CACHED"), "a retried green must not be served as a cached first-try green: {}", second.stdout);
}

#[test]
fn a_retried_green_does_not_feed_the_ledger_medians() {
    let dir = repo(&(step("b", &["pass"]) + &step("a", &["flaky", &marker(), DOCKER])), None);
    let db = tempfile::tempdir().unwrap().keep().join("g.db");
    let r = run_in(dir.path(), &["--retry-flakes", "--db", db.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.stdout);
    let h = gatetrain_core::history::history_durations(&db, "").expect("history");
    assert!(h.iter().any(|(n, _)| n == "b"), "an ordinary green is a median input: {h:?}");
    assert!(!h.iter().any(|(n, _)| n == "a"), "the summed-duration retried green is not: {h:?}");
}
