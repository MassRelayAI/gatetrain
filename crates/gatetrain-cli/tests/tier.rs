//! `gatetrain chain --tier`: a complete run of one tier (docs/since.md, docs/chain.md). The real binary over `gt-testchild` steps and temp git repos.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let name = format!("gt-testchild{}", std::env::consts::EXE_SUFFIX);
    let p = exe.ancestors().skip(1).take(6).map(|d| d.join(&name)).find(|p| p.is_file());
    p.unwrap_or_else(|| panic!("{name} is not built: run `cargo test --workspace`")).to_string_lossy().into_owned()
}

fn step(name: &str, tier: &str, extra: &str) -> String {
    format!("[[step]]\nname = '{name}'\ntier = '{tier}'\n{extra}\nargv = ['{}', 'pass']\n\n", child())
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

fn body() -> String {
    step("A", "fast", "inputs = ['a/**']") + &step("B", "fast", "inputs = ['b/**']") + &step("S", "slow", "inputs = ['b/**']\nneeds = ['B']") + &step("T", "slow", "inputs = ['a/**']")
}

struct Run {
    o: Output,
    out: PathBuf,
    stdout: String,
    stderr: String,
}

impl Run {
    fn code(&self) -> i32 {
        self.o.status.code().unwrap()
    }
    fn has(&self, p: &str) -> bool {
        self.stdout.lines().any(|l| l.starts_with(p))
    }
    fn stamp(&self, file: &str) -> serde_json::Value {
        let names: Vec<_> = std::fs::read_dir(&self.out).unwrap().flatten().map(|e| e.file_name()).collect();
        serde_json::from_str(&std::fs::read_to_string(self.out.join(file)).unwrap_or_else(|_| panic!("no {file} in {names:?}"))).unwrap()
    }
}

fn chain(repo: &Path, work: &Path, extra: &[&str]) -> Run {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{}", repo.display(), body())).unwrap();
    let out = work.join("out");
    let db = work.join("l.db");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap(), "--db", db.to_str().unwrap()]).args(extra).current_dir(work).output().unwrap();
    Run { stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned(), o, out }
}

#[test]
fn a_tier_run_is_complete_for_its_tier_names_its_tiers_and_does_not_collide_with_the_other_tier() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    // the slow tier needs the fast tier (S needs B): with no fast stamp for this commit it is refused
    let early = chain(r.path(), w.path(), &["--tier", "slow", "--run-id", "s0"]);
    assert_eq!(early.code(), 2, "{}{}", early.stdout, early.stderr);
    assert!(early.stderr.contains("fast-tier stamp"), "{}", early.stderr);
    assert!(!w.path().join("out").join(format!("stamp-{sha}-slow.json")).exists());
    let fast = chain(r.path(), w.path(), &["--tier", "fast", "--run-id", "f1"]);
    assert_eq!(fast.code(), 0, "{}{}", fast.stdout, fast.stderr);
    assert!(fast.has("STEP_EXIT A 0") && !fast.stdout.contains("STEP_START S"), "only the fast steps run: {}", fast.stdout);
    let st = fast.stamp(&format!("stamp-{sha}-fast.json"));
    assert_eq!((st["tiers"].clone(), st["partial"].clone(), st["partial_reason"].clone()), (serde_json::json!(["fast"]), serde_json::json!(true), serde_json::json!("tier")));
    assert_eq!(st["plan_tiers"], serde_json::json!(["fast", "slow"]));
    let slow = chain(r.path(), w.path(), &["--tier", "slow", "--run-id", "s1"]);
    assert_eq!(slow.code(), 0, "{}{}", slow.stdout, slow.stderr);
    assert!(slow.has("STEP_EXIT S 0") && slow.has("STEP_EXIT T 0") && !slow.stdout.contains("STEP_START A"), "{}", slow.stdout);
    let st = slow.stamp(&format!("stamp-{sha}-slow.json"));
    assert_eq!((st["tiers"].clone(), st["partial"].clone(), st["partial_reason"].clone()), (serde_json::json!(["slow"]), serde_json::json!(true), serde_json::json!("tier")));
    let fast_hash = gatetrain_core::chain::digest_of(&std::fs::read_to_string(w.path().join("out").join(format!("stamp-{sha}-fast.json"))).unwrap());
    assert_eq!(st["depends_on"][0]["hash"], serde_json::json!(fast_hash), "the slow stamp names the fast stamp it ran on");
    assert!(w.path().join("out").join(format!("stamp-{sha}-fast.json")).exists(), "the slow run did not overwrite the fast stamp");
    assert!(!w.path().join("out").join(format!("stamp-{sha}.json")).exists(), "no single full stamp from two tier runs");
    // both tiers named together are a whole run: the ordinary stamp
    let both = chain(r.path(), w.path(), &["--tier", "fast,slow", "--run-id", "b1"]);
    assert_eq!(both.stamp(&format!("stamp-{sha}.json"))["tiers"], serde_json::json!(["fast", "slow"]));
}

#[test]
fn since_reuses_from_a_previous_run_of_the_same_tier_and_a_different_tier_set_runs_everything() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    assert_eq!(chain(r.path(), w.path(), &["--tier", "fast", "--run-id", "f1"]).code(), 0);
    let sha2 = commit_file(r.path(), "b/y.txt", "2");
    let again = chain(r.path(), w.path(), &["--tier", "fast", "--since", &sha1, "--run-id", "f2"]);
    assert!(again.stdout.contains(&format!("STEP_REUSED A f1 {sha1}")) && !again.has("STEP_REUSED B"), "{}", again.stdout);
    // the slow tier has no previous slow run: refused, everything runs
    let slow = chain(r.path(), w.path(), &["--tier", "slow", "--since", &sha1, "--run-id", "s1"]);
    assert!(!slow.has("STEP_REUSED") && slow.has("CHAIN_SINCE refused"), "{}", slow.stdout);
    assert_eq!(slow.stamp(&format!("stamp-{sha2}-slow.json"))["partial_reason"], "reused", "built on an incremental fast stamp: incremental too");
    // a previous fast run does not feed a fast+slow run
    let all = chain(r.path(), w.path(), &["--since", &sha1, "--run-id", "all1"]);
    assert!(!all.has("STEP_REUSED") && all.stdout.contains("no previous result"), "{}", all.stdout);
}

#[test]
fn a_tier_cannot_be_combined_with_only_skip_or_a_full_mode_and_must_exist() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    for extra in [vec!["--tier", "fast", "--only", "A"], vec!["--tier", "fast", "--skip", "B"], vec!["--tier", "fast", "--mode", "full"], vec!["--tier", "medium"]] {
        let run = chain(r.path(), w.path(), &extra);
        assert_eq!(run.code(), 2, "{extra:?}: {}{}", run.stdout, run.stderr);
    }
}

#[test]
fn a_red_run_deletes_the_older_green_stamp_of_the_same_tiers() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let fd = tempfile::tempdir().unwrap();
    let flag = fd.path().join("ok");
    std::fs::write(&flag, "x").unwrap();
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    let body = format!("[[step]]\nname = 'A'\ntier = 'fast'\nargv = ['{}', 'exists', '{}']\n", child(), flag.display());
    let go = |extra: &[&str]| {
        let f = w.path().join("steps.toml");
        std::fs::write(&f, format!("cwd = '{}'\n\n{body}", r.path().display())).unwrap();
        let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", w.path().join("out").to_str().unwrap()]).args(extra).current_dir(w.path()).output().unwrap();
        o.status.code().unwrap()
    };
    assert_eq!(go(&[]), 0);
    let stamp = w.path().join("out").join(format!("stamp-{sha}.json"));
    assert!(stamp.exists());
    std::fs::remove_file(&flag).unwrap();
    assert_eq!(go(&[]), 1, "the same commit is now red");
    assert!(!stamp.exists(), "the older green stamp no longer describes the tree");
}
