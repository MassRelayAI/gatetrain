//! Adversarial: a slow step needing a fast step output, run as two tier runs, against a full run of the same commit (the tier order must be proven).
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}
fn chain(repo: &Path, work: &Path, body: &str, extra: &[&str]) -> (i32, String, PathBuf) {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{body}", repo.display())).unwrap();
    let out = work.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()]).args(extra).current_dir(work).output().unwrap();
    (o.status.code().unwrap(), String::from_utf8_lossy(&o.stdout).into_owned(), out)
}

const BODY: &str = "[[step]]\nname = 'build'\ntier = 'fast'\nrun = 'mkdir -p build && cp a/x.txt build/out'\n\n[[step]]\nname = 'e2e'\ntier = 'slow'\nneeds = ['build']\nrun = 'grep -q good build/out'\n";

#[test]
fn tier_union_of_a_cross_tier_need_vs_a_full_run() {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    std::fs::create_dir_all(r.path().join("a")).unwrap();
    std::fs::write(r.path().join(".gitignore"), "build/\n").unwrap();
    std::fs::write(r.path().join("a/x.txt"), "good\n").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-q", "-m", "good"]);
    let (c0, o0, _) = chain(r.path(), w.path(), BODY, &[]);
    assert_eq!(c0, 0, "{o0}");
    std::fs::write(r.path().join("a/x.txt"), "bad\n").unwrap();
    git(r.path(), &["commit", "-q", "-am", "bad"]);
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    // slow first (stale build/out from the previous commit), then fast
    let (cs, os, out) = chain(r.path(), w.path(), BODY, &["--tier", "slow"]);
    let (cf, of, _) = chain(r.path(), w.path(), BODY, &["--tier", "fast"]);
    let slow = std::fs::read_to_string(out.join(format!("stamp-{sha}-slow.json"))).ok();
    let fast = std::fs::read_to_string(out.join(format!("stamp-{sha}-fast.json"))).ok();
    let w2 = tempfile::tempdir().unwrap();
    let (cfull, _, _) = chain(r.path(), w2.path(), BODY, &[]);
    eprintln!("TIER slow rc={cs} fast rc={cf} slow_stamp={} fast_stamp={} full rc={cfull}", slow.is_some(), fast.is_some());
    if let Some(s) = &slow {
        let v: serde_json::Value = serde_json::from_str(s).unwrap();
        eprintln!("TIER slow stamp partial={:?} tiers={} plan_tiers={} digest={}", v["partial"], v["tiers"], v["plan_tiers"], v["digest"]);
    }
    let _ = (os, of);
    assert_ne!(cfull, 0, "a full run of the bad commit is red");
    assert!(!(slow.is_some() && fast.is_some()), "FALSE GREEN: both tier stamps written for a commit whose full run is red");
}
