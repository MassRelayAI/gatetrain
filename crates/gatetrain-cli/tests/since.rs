//! `gatetrain chain --since`: incremental re-run end to end (docs/since.md). The real binary over `gt-testchild` steps and temp git repos.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn child() -> String {
    // the child sits next to the test binary's target folder; newer cargo keeps test binaries in a deeper build folder, so look upward
    let exe = std::env::current_exe().unwrap();
    let name = format!("gt-testchild{}", std::env::consts::EXE_SUFFIX);
    let p = exe.ancestors().skip(1).take(6).map(|d| d.join(&name)).find(|p| p.is_file());
    p.unwrap_or_else(|| panic!("{name} is not built: run `cargo test --workspace`")).to_string_lossy().into_owned()
}

fn step(name: &str, tier: &str, extra: &str, args: &[&str]) -> String {
    let mut argv = vec![format!("'{}'", child())];
    argv.extend(args.iter().map(|a| format!("'{a}'")));
    format!("[[step]]\nname = '{name}'\ntier = '{tier}'\n{extra}\nargv = [{}]\n\n", argv.join(", "))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn commit_file(repo: &Path, rel: &str, text: &str) -> String {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    git(repo, &["add", rel]);
    git(repo, &["commit", "-q", "-m", rel]);
    git(repo, &["rev-parse", "HEAD"])
}

fn repo() -> tempfile::TempDir {
    let r = tempfile::tempdir().unwrap();
    git(r.path(), &["init", "-q"]);
    commit_file(r.path(), "a/x.txt", "1");
    commit_file(r.path(), "b/y.txt", "1");
    r
}

struct Run {
    o: Output,
    out: PathBuf,
    stdout: String,
}

impl Run {
    fn code(&self) -> i32 {
        self.o.status.code().unwrap()
    }
    fn has(&self, prefix: &str) -> bool {
        self.stdout.lines().any(|l| l.starts_with(prefix))
    }
    fn stamp(&self, sha: &str) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.out.join(format!("stamp-{sha}.json"))).unwrap()).unwrap()
    }
}

fn chain(repo: &Path, work: &Path, body: &str, extra: &[&str]) -> Run {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{body}", repo.display())).unwrap();
    let out = work.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()]).args(extra).current_dir(work).output().unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
    Run { o, out, stdout }
}

fn body() -> String {
    step("A", "fast", "inputs = ['a/**']", &["pass"]) + &step("B", "fast", "inputs = ['b/**']", &["pass"]) + &step("S", "slow", "inputs = ['b/**']\nneeds = ['B']", &["pass"]) + &step("Z", "slow", "", &["pass"])
}

#[test]
fn a_reused_step_is_labelled_and_a_step_whose_input_changed_runs() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let db = w.path().join("l.db");
    let first = chain(r.path(), w.path(), &body(), &["--run-id", "run-1", "--db", db.to_str().unwrap()]);
    assert_eq!(first.code(), 0, "{}", first.stdout);
    assert!(!first.has("STEP_REUSED"), "no --since, no reuse");
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    commit_file(r.path(), "b/y.txt", "2");
    let sha2 = git(r.path(), &["rev-parse", "HEAD"]);
    let second = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--run-id", "run-2", "--db", db.to_str().unwrap()]);
    assert_eq!(second.code(), 0, "{}", second.stdout);
    assert!(second.stdout.contains(&format!("STEP_REUSED A run-1 {sha1}")), "A is carried and labelled with where it really ran: {}", second.stdout);
    assert!(!second.has("STEP_REUSED B"), "B's input changed: it must run: {}", second.stdout);
    assert!(second.stdout.contains("STEP_RERUN B its inputs changed"), "{}", second.stdout);
    assert!(!second.has("STEP_REUSED S"), "S needs B, which runs: {}", second.stdout);
    assert!(second.stdout.contains("STEP_RERUN Z no declared inputs"), "an undeclared step always runs: {}", second.stdout);
    let st = second.stamp(&sha2);
    let fresh = |n: &str| st["steps"].as_array().unwrap().iter().find(|s| s["name"] == n).unwrap()["freshness"].as_str().unwrap().to_string();
    assert_eq!((fresh("A").as_str(), fresh("B").as_str(), fresh("S").as_str(), fresh("Z").as_str()), ("reused", "fresh", "fresh", "fresh"));
    assert_eq!(st["reused"], serde_json::json!(["A"]));
    assert_eq!(st["steps"][0]["reused_from"]["run"], "run-1");
    let l = rusqlite::Connection::open(&db).unwrap();
    let cf: String = l.query_row("select cached_from from steps where run_id='run-2' and name='A'", [], |r| r.get(0)).unwrap();
    assert_eq!(cf, format!("reused-from run-1 {sha1}"), "the ledger says reused, never silently green");
}

#[test]
fn a_chain_of_reuse_keeps_the_origin_and_counts_its_age() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    chain(r.path(), w.path(), &body(), &["--run-id", "run-1"]);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    let sha2 = commit_file(r.path(), "b/y.txt", "2");
    chain(r.path(), w.path(), &body(), &["--since", &sha1, "--run-id", "run-2"]);
    commit_file(r.path(), "b/y.txt", "3");
    let third = chain(r.path(), w.path(), &body(), &["--since", &sha2, "--run-id", "run-3", "--since-max-age", "1"]);
    assert!(!third.has("STEP_REUSED A"), "carried twice with max age 1: {}", third.stdout);
    assert!(third.stdout.contains("STEP_RERUN A its result is older than 1 runs"), "{}", third.stdout);
}

#[test]
fn a_red_step_runs_again_even_when_nothing_it_reads_changed() {
    let (r, w, flag) = (repo(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let f = flag.path().join("ok");
    let b = || step("A", "fast", "inputs = ['a/**']", &["pass"]) + &step("B", "fast", "inputs = ['b/**']", &["exists", f.to_str().unwrap()]);
    let first = chain(r.path(), w.path(), &b(), &["--run-id", "run-1"]);
    assert_eq!(first.code(), 1, "{}", first.stdout);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    std::fs::write(&f, "x").unwrap();
    commit_file(r.path(), "docs/n.txt", "n");
    let second = chain(r.path(), w.path(), &b(), &["--since", &sha1, "--run-id", "run-2"]);
    assert_eq!(second.code(), 0, "{}", second.stdout);
    assert!(second.stdout.contains("STEP_RERUN B it was red in the previous run (rc=1)"), "{}", second.stdout);
    assert!(second.stdout.contains(&format!("STEP_REUSED A run-1 {sha1}")), "{}", second.stdout);
}

#[test]
fn a_toolchain_change_runs_everything() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    chain(r.path(), w.path(), &body(), &["--run-id", "run-1", "--toolchain-probe", "echo one"]);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    commit_file(r.path(), "docs/n.txt", "n");
    let same = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--run-id", "run-2", "--toolchain-probe", "echo one"]);
    assert!(same.has("STEP_REUSED A"), "same toolchain: {}", same.stdout);
    let other = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--run-id", "run-3", "--toolchain-probe", "echo two"]);
    assert!(!other.has("STEP_REUSED"), "a toolchain change reuses nothing: {}", other.stdout);
    assert!(other.stdout.contains("STEP_RERUN A the toolchain fingerprint differs"), "{}", other.stdout);
}

#[test]
fn a_lockfile_change_runs_everything() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    chain(r.path(), w.path(), &body(), &["--run-id", "run-1"]);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    commit_file(r.path(), "Cargo.lock", "x");
    let second = chain(r.path(), w.path(), &body(), &["--since", &sha1]);
    assert!(!second.has("STEP_REUSED") && second.stdout.contains("a lockfile or toolchain pin changed: Cargo.lock"), "{}", second.stdout);
}

#[test]
fn full_runs_everything_and_a_partial_previous_run_is_refused() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    chain(r.path(), w.path(), &body(), &["--run-id", "run-1"]);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    commit_file(r.path(), "docs/n.txt", "n");
    let full = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--full"]);
    assert!(!full.has("STEP_REUSED") && full.stdout.contains("STEP_RERUN A --full"), "{}", full.stdout);
    let w2 = tempfile::tempdir().unwrap();
    let r2 = repo();
    let partial = chain(r2.path(), w2.path(), &body(), &["--skip", "Z", "--mode", "preflight"]);
    assert_eq!(partial.code(), 0, "{}", partial.stdout);
    let sha = git(r2.path(), &["rev-parse", "HEAD"]);
    commit_file(r2.path(), "docs/n.txt", "n");
    let after = chain(r2.path(), w2.path(), &body(), &["--since", &sha]);
    assert!(!after.has("STEP_REUSED"), "a partial run's result is refused: {}", after.stdout);
}

#[test]
fn a_missing_previous_result_runs_everything_and_says_so() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    let run = chain(r.path(), w.path(), &body(), &["--since", &sha]);
    assert_eq!(run.code(), 0, "{}", run.stdout);
    assert!(run.has("CHAIN_SINCE refused") && !run.has("STEP_REUSED"), "{}", run.stdout);
}

#[test]
fn a_stamp_with_reused_steps_is_v2_and_partial_and_a_full_run_stays_v1() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let first = chain(r.path(), w.path(), &body(), &["--run-id", "run-1"]);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    assert_eq!((first.stamp(&sha1)["v"].clone(), first.stamp(&sha1)["partial"].clone()), (serde_json::json!(1), serde_json::Value::Null));
    let sha2 = commit_file(r.path(), "b/y.txt", "2");
    let second = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--run-id", "run-2"]);
    let st = second.stamp(&sha2);
    assert_eq!((st["v"].clone(), st["partial"].clone(), st["partial_reason"].clone()), (serde_json::json!(2), serde_json::json!(true), serde_json::json!("reused")));
    // nothing reused (--full): an ordinary full stamp
    let third = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--full", "--run-id", "run-3"]);
    assert_eq!((third.stamp(&sha2)["v"].clone(), third.stamp(&sha2)["partial"].clone()), (serde_json::json!(1), serde_json::Value::Null));
}

#[test]
fn an_environment_variable_in_the_default_set_changes_the_fingerprint() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let run = |extra: &[&str], flags: &str| {
        let f = w.path().join("steps.toml");
        std::fs::write(&f, format!("cwd = '{}'\n\n{}", r.path().display(), body())).unwrap();
        let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", w.path().join("out").to_str().unwrap()]).args(extra).env("RUSTFLAGS", flags).current_dir(w.path()).output().unwrap();
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    run(&["--run-id", "run-1"], "-Cdebuginfo=0");
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    commit_file(r.path(), "docs/n.txt", "n");
    assert!(run(&["--since", &sha1], "-Cdebuginfo=0").contains("STEP_REUSED A"));
    let other = run(&["--since", &sha1], "-Copt-level=3");
    assert!(!other.contains("STEP_REUSED") && other.contains("the environment fingerprint differs"), "{other}");
}

#[test]
fn an_edited_result_file_or_a_ledger_that_disagrees_is_refused() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let db = w.path().join("l.db");
    let d = db.to_str().unwrap();
    chain(r.path(), w.path(), &body(), &["--run-id", "run-1", "--db", d]);
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    commit_file(r.path(), "docs/n.txt", "n");
    let file = w.path().join("out").join(format!("result-{sha1}-fast+slow.json"));
    let good = std::fs::read_to_string(&file).unwrap();
    // an untouched result with its ledger row: reused
    assert!(chain(r.path(), w.path(), &body(), &["--since", &sha1, "--db", d, "--run-id", "run-2"]).has("STEP_REUSED A"));
    // a step's rc edited in the file (a red turned green): the seal fails
    let mut v: serde_json::Value = serde_json::from_str(&good).unwrap();
    v["steps"][0]["rc"] = serde_json::json!(1);
    std::fs::write(&file, v.to_string()).unwrap();
    let a = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--run-id", "run-3"]);
    assert!(a.has("CHAIN_SINCE refused") && a.stdout.contains("seal") && !a.has("STEP_REUSED"), "{}", a.stdout);
    // a file whose run is not in the ledger (or whose ledger row disagrees)
    std::fs::write(&file, &good).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("update runs set log_sha256='0' where id='run-1'", []).unwrap();
    let b = chain(r.path(), w.path(), &body(), &["--since", &sha1, "--db", d, "--run-id", "run-4"]);
    assert!(b.stdout.contains("CHAIN_SINCE refused the ledger's row") && !b.has("STEP_REUSED"), "{}", b.stdout);
}
