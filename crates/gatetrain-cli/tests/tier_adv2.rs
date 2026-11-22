//! Adversarial: the artifact on disk may come from another commit between a fast and a slow run; a tier run executes the closure of its steps, so it never reads it.
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?} {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}
fn steps(repo: &Path, work: &Path, body: &str) -> PathBuf {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{body}", repo.display())).unwrap();
    f
}
fn chain(f: &Path, work: &Path, extra: &[&str]) -> (i32, String) {
    let out = work.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()]).args(extra).current_dir(work).output().unwrap();
    (o.status.code().unwrap(), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
}
fn json(p: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok())
}
const BODY: &str = "[[step]]\nname = 'build'\ntier = 'fast'\nrun = 'mkdir -p build && cp a/x.txt build/out'\n\n[[step]]\nname = 'e2e'\ntier = 'slow'\nneeds = ['build']\nrun = 'sleep 1.5; grep -q good build/out'\n";

fn repo() -> (tempfile::TempDir, String, String) {
    let r = tempfile::tempdir().unwrap();
    git(r.path(), &["init", "-q"]);
    std::fs::create_dir_all(r.path().join("a")).unwrap();
    std::fs::write(r.path().join(".gitignore"), "build/\n").unwrap();
    std::fs::write(r.path().join("a/x.txt"), "good\n").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-q", "-m", "good"]);
    let good = git(r.path(), &["rev-parse", "HEAD"]);
    std::fs::write(r.path().join("a/x.txt"), "bad\n").unwrap();
    git(r.path(), &["commit", "-q", "-am", "bad"]);
    let bad = git(r.path(), &["rev-parse", "HEAD"]);
    (r, good, bad)
}

/// P1: the fast stamp of BAD is real and earlier, but the artifact on disk came from a later fast run of GOOD (checkout in between).
#[test]
fn p1_artifact_from_another_commit_between_fast_and_slow() {
    let (r, good, bad) = repo();
    let w = tempfile::tempdir().unwrap();
    let f = steps(r.path(), w.path(), BODY);
    assert_eq!(chain(&f, w.path(), &["--tier", "fast"]).0, 0); // bad: build/out = bad, stamp-bad-fast
    git(r.path(), &["checkout", "-q", &good]);
    assert_eq!(chain(&f, w.path(), &["--tier", "fast"]).0, 0); // good: build/out = good
    git(r.path(), &["checkout", "-q", &bad]);
    let (cs, os) = chain(&f, w.path(), &["--tier", "slow"]);
    let out = w.path().join("out");
    let fast = out.join(format!("stamp-{bad}-fast.json"));
    let slow = json(&out.join(format!("stamp-{bad}-slow.json")));
    let _ = (&fast, &slow);
    eprintln!("P1 slow rc={cs} {os}");
    assert_ne!(cs, 0, "FALSE GREEN: the slow tier of BAD must re-run build and go red: {os}");
    assert!(slow.is_none(), "no slow stamp for a red run");
}

/// P3: after slow, a RED fast run on the same commit: the fast stamp must be gone (the pair cannot count).
#[test]
fn p3_red_fast_rerun_breaks_the_pair() {
    let (r, _good, _bad) = repo();
    std::fs::write(r.path().join("a/x.txt"), "good\n").unwrap();
    git(r.path(), &["commit", "-q", "-am", "good again"]);
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    let w = tempfile::tempdir().unwrap();
    let f = steps(r.path(), w.path(), BODY);
    assert_eq!(chain(&f, w.path(), &["--tier", "fast"]).0, 0);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(chain(&f, w.path(), &["--tier", "slow"]).0, 0);
    std::fs::remove_file(r.path().join("a/x.txt")).unwrap(); // the build step now fails (cp of a missing file); tree dirty is irrelevant to a red run
    let (cf, _) = chain(&f, w.path(), &["--tier", "fast"]);
    let fast = w.path().join("out").join(format!("stamp-{sha}-fast.json"));
    eprintln!("P3 fast rc={cf} fast_stamp_exists={}", fast.exists());
    assert_ne!(cf, 0);
    assert!(!fast.exists(), "a red fast run left the old green fast stamp in place");
}
