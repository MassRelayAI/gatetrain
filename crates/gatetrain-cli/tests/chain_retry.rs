//! `gatetrain chain --retry-flakes` (D446): a red step is re-run ONCE on the same sha only when `diagnose` classes it `flake-*`/`infra-*`
//! or a live row of the quarantine file names its test. An unclassified red is never re-run; a green retry is never a first-try green.
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

fn step(name: &str, args: &[&str]) -> String {
    let mut argv = vec![format!("'{}'", child())];
    argv.extend(args.iter().map(|a| format!("'{a}'")));
    format!("[[step]]\nname = '{name}'\ntier = 'fast'\nargv = [{}]\n\n", argv.join(", "))
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

struct Run {
    code: i32,
    stdout: String,
    out: PathBuf,
    _dir: tempfile::TempDir,
}

/// A committed repo holding the steps file (and the quarantine file, when given), so the tree is clean and a green run writes a stamp.
fn chain(steps: &str, quarantine: Option<&str>, extra: &[&str]) -> Run {
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
    let out = d.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", d.join("steps.toml").to_str().unwrap(), "--out", out.to_str().unwrap()])
        .args(extra)
        .current_dir(d)
        .output()
        .unwrap();
    Run { code: o.status.code().unwrap(), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), out, _dir: dir }
}

const PORT: &str = "bind: address already in use";

/// One step `a` that is red the first time it runs (printing `text`) and green after. The marker lives in its own temp dir, outside the repo.
fn with_marker(text: &str, quarantine: Option<&str>, extra: &[&str]) -> Run {
    let m = tempfile::tempdir().unwrap();
    let mk = m.path().join("a.marker");
    chain(&step("a", &["flaky", &mk.to_string_lossy(), text]), quarantine, extra)
}

#[test]
fn a_flake_classed_red_is_retried_once_then_green() {
    let r = with_marker(PORT, None, &["--retry-flakes"]);
    assert_eq!(r.code, 0, "{}", r.stdout);
    let retries: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("STEP_RETRY ")).collect();
    assert_eq!(retries.len(), 1, "{}", r.stdout);
    assert!(retries[0].contains("a attempt=1 rc=1") && retries[0].contains("class=infra-port"), "{}", retries[0]);
    assert!(r.stdout.contains("CHAIN_RETRIES a=green"), "the summary must show the retry: {}", r.stdout);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.retries.len(), 1);
    assert_eq!(p.retries[0].step, "a");
    assert_eq!(p.retries[0].rc, 1);
    assert_eq!(p.steps.len(), 1, "one logical step");
    assert_eq!(p.steps[0].exit_code, Some(0), "the step's own exit is the retry's");
    // both attempts' outputs are kept
    assert!(r.out.join("a.attempt1.full").exists());
    // the stamp lists the retry, so a green retry is never a first-try green
    let stamp = std::fs::read_dir(&r.out).unwrap().flatten().map(|e| e.path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with("stamp-")).expect("a stamp");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stamp).unwrap()).unwrap();
    assert_eq!(v["retried"][0]["name"], "a", "{v}");
    assert_eq!(v["retried"][0]["first_rc"], 1);
    assert_eq!(v["retried"][0]["final_rc"], 0);
}

#[test]
fn an_unclassified_red_is_not_retried() {
    let r = with_marker("assertion failed: left != right", None, &["--retry-flakes"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(!r.stdout.contains("STEP_RETRY"), "{}", r.stdout);
    assert!(!r.stdout.contains("CHAIN_RETRIES"), "{}", r.stdout);
}

#[test]
fn a_retry_that_is_red_again_stays_red() {
    let r = chain(&step("a", &["failprint", PORT]), None, &["--retry-flakes"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert_eq!(r.stdout.lines().filter(|l| l.starts_with("STEP_RETRY ")).count(), 1, "re-run once, never twice: {}", r.stdout);
    assert!(r.stdout.contains("CHAIN_RETRIES a=red"), "{}", r.stdout);
    assert!(r.stdout.contains("RED_SUMMARY: a"), "{}", r.stdout);
}

#[test]
fn a_live_quarantine_row_qualifies_and_an_expired_one_does_not() {
    let text = "FAIL  a.test.ts > findBy under load";
    let q = ["--retry-flakes", "--quarantine", "quarantine.csv"];
    let live = "test,step,finding,added,expires\na.test.ts > findBy under load,a,M1234,2099-01-01,2099-01-10\n";
    let r = with_marker(text, Some(live), &q);
    assert_eq!(r.code, 0, "{}", r.stdout);
    assert!(r.stdout.contains("via=quarantine:M1234"), "{}", r.stdout);
    let expired = "test,step,finding,added,expires\na.test.ts > findBy under load,a,M1234,2020-01-01,2020-01-10\n";
    let r = with_marker(text, Some(expired), &q);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(!r.stdout.contains("STEP_RETRY"), "{}", r.stdout);
    // a row that outlives its 14 days is refused, not honoured
    let long = "test,step,finding,added,expires\na.test.ts > findBy under load,a,M1234,2099-01-01,2099-03-01\n";
    let r = with_marker(text, Some(long), &q);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(!r.stdout.contains("STEP_RETRY"), "{}", r.stdout);
}

#[test]
fn a_third_flaky_step_is_not_retried() {
    let d = tempfile::tempdir().unwrap();
    let mut steps = String::new();
    for n in ["a", "b", "c"] {
        steps += &step(n, &["flaky", &d.path().join(format!("{n}.marker")).to_string_lossy(), PORT]);
    }
    let r = chain(&steps, None, &["--retry-flakes", "--par", "1"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert_eq!(r.stdout.lines().filter(|l| l.starts_with("STEP_RETRY ")).count(), 2, "{}", r.stdout);
    assert!(r.stdout.contains("RED_SUMMARY: c"), "{}", r.stdout);
}

#[test]
fn without_the_flag_nothing_changes() {
    let r = with_marker(PORT, None, &[]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(!r.stdout.contains("STEP_RETRY") && !r.stdout.contains("CHAIN_RETRIES"), "{}", r.stdout);
    assert!(!r.out.join("a.attempt1.full").exists());
}
