//! Adversarial: cross-tier `--since` must never reuse a red step's old green (P5) nor a need whose output a running step reads from disk (P4).
use std::path::{Path, PathBuf};
use std::process::Command;

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let name = format!("gt-testchild{}", std::env::consts::EXE_SUFFIX);
    let p = exe.ancestors().skip(1).take(6).map(|d| d.join(&name)).find(|p| p.is_file());
    p.unwrap_or_else(|| panic!("{name} is not built: run `cargo test --workspace`")).to_string_lossy().into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?} {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// One chain run; `--db` is always the same ledger in `work` (--since needs it). Returns (exit code, stdout + stderr).
fn chain(repo: &Path, work: &Path, body: &str, extra: &[&str]) -> (i32, String) {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{body}", repo.display())).unwrap();
    let out: PathBuf = work.join("out");
    let db = work.join("l.db");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap(), "--db", db.to_str().unwrap()])
        .args(extra)
        .current_dir(work)
        .output()
        .unwrap();
    (o.status.code().unwrap(), String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr))
}

/// P5: `build` is green in a `--tier fast` run and RED in a later full run of the same commit; `--since` must take the NEWEST result, the red one.
#[test]
fn p5_the_newest_result_of_a_step_wins_a_later_red_is_not_overridden_by_an_older_green() {
    let (r, w, flagdir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    std::fs::create_dir_all(r.path().join("a")).unwrap();
    std::fs::write(r.path().join("a/x.txt"), "1").unwrap();
    git(r.path(), &["add", "a/x.txt"]);
    git(r.path(), &["commit", "-q", "-m", "init"]);
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    let flag = flagdir.path().join("ok");
    std::fs::write(&flag, "x").unwrap();
    let body = format!(
        "[[step]]\nname = 'build'\ntier = 'fast'\ninputs = ['a/**']\nargv = ['{c}', 'exists', '{f}']\n\n[[step]]\nname = 'e2e'\ntier = 'slow'\nneeds = ['build']\nargv = ['{c}', 'pass']\n",
        c = child(),
        f = flag.display()
    );
    assert_eq!(chain(r.path(), w.path(), &body, &["--tier", "fast"]).0, 0, "build is green while the flag exists");
    std::fs::remove_file(&flag).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_ne!(chain(r.path(), w.path(), &body, &[]).0, 0, "the full run of the same commit is red");
    let (rc, out) = chain(r.path(), w.path(), &body, &["--tier", "fast", "--since", &sha]);
    assert_ne!(rc, 0, "FALSE GREEN: the older green of build was reused over the newer red: {out}");
}

/// P4: `build` was green for BAD, then a run on GOOD left GOOD's output on disk; `e2e` runs now and reads it, so `build` must run again, not be reused.
#[test]
fn p4_a_need_of_a_running_step_is_never_reused() {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    std::fs::create_dir_all(r.path().join("a")).unwrap();
    std::fs::write(r.path().join(".gitignore"), "build/\n").unwrap();
    std::fs::write(r.path().join("a/x.txt"), "good\n").unwrap();
    git(r.path(), &["add", "-A", "--", ".gitignore", "a"]);
    git(r.path(), &["commit", "-q", "-m", "good"]);
    let good = git(r.path(), &["rev-parse", "HEAD"]);
    std::fs::write(r.path().join("a/x.txt"), "bad\n").unwrap();
    git(r.path(), &["commit", "-q", "-am", "bad"]);
    let bad = git(r.path(), &["rev-parse", "HEAD"]);
    let body = "[[step]]\nname = 'build'\ntier = 'fast'\ninputs = ['a/**']\nrun = 'mkdir -p build && cp a/x.txt build/out'\n\n[[step]]\nname = 'e2e'\ntier = 'slow'\nneeds = ['build']\nrun = 'grep -q good build/out'\n";
    assert_eq!(chain(r.path(), w.path(), body, &["--tier", "fast"]).0, 0); // BAD: build/out = bad
    git(r.path(), &["checkout", "-q", &good]);
    assert_eq!(chain(r.path(), w.path(), body, &["--tier", "fast"]).0, 0); // GOOD: build/out = good
    git(r.path(), &["checkout", "-q", &bad]);
    let (rc, out) = chain(r.path(), w.path(), body, &["--tier", "slow", "--since", &bad]);
    assert_ne!(rc, 0, "FALSE GREEN: build was reused for BAD while e2e read GOOD's output: {out}");
}

/// P6 (replay): an old green result file is restored and re-sealed with a far-future `at`; the ledger's own order says the later run was red, so the step must run again.
#[test]
fn p6_a_replayed_old_green_re_sealed_with_a_newer_at_does_not_outrank_a_later_red_in_the_ledger() {
    let (r, w, flagdir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    std::fs::create_dir_all(r.path().join("a")).unwrap();
    std::fs::write(r.path().join("a/x.txt"), "1").unwrap();
    git(r.path(), &["add", "a/x.txt"]);
    git(r.path(), &["commit", "-q", "-m", "init"]);
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    let flag = flagdir.path().join("ok");
    std::fs::write(&flag, "x").unwrap();
    let body = format!("[[step]]\nname = 'build'\ntier = 'fast'\ninputs = ['a/**']\nargv = ['{c}', 'exists', '{f}']\n", c = child(), f = flag.display());
    assert_eq!(chain(r.path(), w.path(), &body, &[]).0, 0);
    let file = w.path().join("out").join(format!("result-{sha}-fast.json"));
    let green = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_file(&flag).unwrap();
    assert_ne!(chain(r.path(), w.path(), &body, &[]).0, 0, "the later run of the same commit is red");
    // the attacker restores the old green (its ledger row exists) and re-seals it as newer
    let mut res: gatetrain_core::since::RunResult = serde_json::from_str(&green).unwrap();
    res.at = "9999-12-31T00:00:00+00:00".into();
    res.seal = res.compute_seal();
    std::fs::write(&file, serde_json::to_string_pretty(&res).unwrap()).unwrap();
    let (rc, out) = chain(r.path(), w.path(), &body, &["--since", &sha]);
    assert_ne!(rc, 0, "FALSE GREEN: a replayed older green outranked the ledger's later red: {out}");
}
