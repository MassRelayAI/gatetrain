//! A forged result file (a red step edited to green, the unkeyed seal recomputed) must never be reused: --since needs the ledger, and the ledger row disagrees.
use std::path::{Path, PathBuf};
use std::process::Command;

fn child() -> String {
    // the child sits next to the test binary's target folder; newer cargo keeps test binaries in a deeper build folder, so look upward
    let exe = std::env::current_exe().unwrap();
    let name = format!("gt-testchild{}", std::env::consts::EXE_SUFFIX);
    let p = exe.ancestors().skip(1).take(6).map(|d| d.join(&name)).find(|p| p.is_file());
    p.unwrap_or_else(|| panic!("{name} is not built: run `cargo test --workspace`")).to_string_lossy().into_owned()
}
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

fn setup(with_db: bool) -> (i32, String) {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    std::fs::create_dir_all(r.path().join("a")).unwrap();
    std::fs::write(r.path().join("a/x.txt"), "1").unwrap();
    git(r.path(), &["add", "-A"]);
    git(r.path(), &["commit", "-q", "-m", "i"]);
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    let body = format!("[[step]]\nname = 'A'\ntier = 'fast'\ninputs = ['a/**']\nargv = ['{}', 'exit', '1']\n", child());
    let db = w.path().join("l.db");
    let mut ex = vec!["--run-id", "run-1"];
    let dbs = db.to_str().unwrap().to_string();
    if with_db {
        ex.extend(["--db", &dbs]);
    }
    let (c1, _, out) = chain(r.path(), w.path(), &body, &ex);
    assert_eq!(c1, 1, "A is red");
    let p = out.join(format!("result-{sha}-fast.json"));
    let mut res: gatetrain_core::since::RunResult = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    res.steps[0].rc = Some(0); // forge: red -> green
    res.seal = res.compute_seal(); // anyone can recompute an unkeyed sha-256
    std::fs::write(&p, serde_json::to_string_pretty(&res).unwrap()).unwrap();
    let mut ex2 = vec!["--since", sha.as_str(), "--run-id", "run-2"];
    if with_db {
        ex2.extend(["--db", &dbs]);
    }
    let (c2, o2, _) = chain(r.path(), w.path(), &body, &ex2);
    (c2, o2)
}

#[test]
fn forged_result_without_db() {
    let (c, o) = setup(false);
    eprintln!("NO-DB rc={c} reused={}", o.contains("STEP_REUSED A"));
    assert_ne!(c, 0, "FALSE GREEN: forged result with a recomputed seal was accepted: {o}");
}

#[test]
fn forged_result_with_db() {
    let (c, o) = setup(true);
    eprintln!("DB rc={c} refused={}", o.contains("CHAIN_SINCE refused"));
    assert_ne!(c, 0, "FALSE GREEN with --db: {o}");
}
