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
fn every_tier_stamp_stands_alone_is_partial_for_old_consumers_and_names_the_steps_it_ran_for_its_closure() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let sha = git(r.path(), &["rev-parse", "HEAD"]);
    // the drive's order, fast then slow, on one commit
    let fast = chain(r.path(), w.path(), &["--tier", "fast", "--run-id", "f1"]);
    assert_eq!(fast.code(), 0, "{}{}", fast.stdout, fast.stderr);
    assert!(fast.has("STEP_EXIT A 0") && !fast.stdout.contains("STEP_START S"), "only the fast steps run: {}", fast.stdout);
    let st = fast.stamp(&format!("stamp-{sha}-fast.json"));
    assert_eq!((st["tiers"].clone(), st["partial"].clone(), st["partial_reason"].clone()), (serde_json::json!(["fast"]), serde_json::json!(true), serde_json::json!("tier")));
    assert_eq!(st["plan_tiers"], serde_json::json!(["fast", "slow"]));
    assert_eq!(st["ran_for_closure"], serde_json::json!([]));
    assert!(st.get("depends_on").is_none());
    // the slow tier needs B (fast): it runs B itself, in the same run, and does not count it as slow proof
    let slow = chain(r.path(), w.path(), &["--tier", "slow", "--run-id", "s1"]);
    assert_eq!(slow.code(), 0, "{}{}", slow.stdout, slow.stderr);
    assert!(slow.has("STEP_EXIT B 0") && slow.has("STEP_EXIT S 0") && slow.has("STEP_EXIT T 0") && !slow.stdout.contains("STEP_START A"), "{}", slow.stdout);
    let st = slow.stamp(&format!("stamp-{sha}-slow.json"));
    assert_eq!((st["tiers"].clone(), st["partial"].clone(), st["partial_reason"].clone()), (serde_json::json!(["slow"]), serde_json::json!(true), serde_json::json!("tier")));
    let names = |k: &str| {
        let mut v = st[k].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        v.sort();
        v
    };
    assert_eq!((names("steps"), names("ran_for_closure")), (vec!["S".to_string(), "T".to_string()], vec!["B".to_string()]));
    assert!(w.path().join("out").join(format!("stamp-{sha}-fast.json")).exists(), "the slow run did not overwrite the fast stamp");
    assert!(!w.path().join("out").join(format!("stamp-{sha}.json")).exists(), "no single full stamp from two tier runs");
    // both tiers named together are a whole run: the ordinary stamp
    let both = chain(r.path(), w.path(), &["--tier", "fast,slow", "--run-id", "b1"]);
    assert_eq!(both.stamp(&format!("stamp-{sha}.json"))["tiers"], serde_json::json!(["fast", "slow"]));
}

#[test]
fn a_slow_run_after_a_fast_run_on_the_same_commit_reuses_the_fast_steps_it_needs_under_since() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let sha1 = git(r.path(), &["rev-parse", "HEAD"]);
    assert_eq!(chain(r.path(), w.path(), &["--tier", "fast", "--run-id", "f1"]).code(), 0);
    // a commit that touches no step input: B (needed by S) is unchanged, so the slow run takes it from the fast run: no double work
    commit_file(r.path(), "docs/n.txt", "n");
    let slow = chain(r.path(), w.path(), &["--tier", "slow", "--since", &sha1, "--run-id", "s1"]);
    assert_eq!(slow.code(), 0, "{}{}", slow.stdout, slow.stderr);
    assert!(slow.stdout.contains(&format!("STEP_REUSED B f1 {sha1}")), "{}", slow.stdout);
    assert!(slow.has("STEP_EXIT S 0") && !slow.has("STEP_REUSED S"), "S is new work: {}", slow.stdout);
    // the same fast tier again reuses its own
    let again = chain(r.path(), w.path(), &["--tier", "fast", "--since", &sha1, "--run-id", "f2"]);
    assert!(again.stdout.contains(&format!("STEP_REUSED A f1 {sha1}")), "{}", again.stdout);
    // a changed input of B re-runs B, and S with it
    let sha2 = commit_file(r.path(), "b/y.txt", "2");
    let _ = sha2;
    let slow2 = chain(r.path(), w.path(), &["--tier", "slow", "--since", &sha1, "--run-id", "s2"]);
    assert!(!slow2.has("STEP_REUSED B") && slow2.stdout.contains("STEP_RERUN B its inputs changed"), "{}", slow2.stdout);
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
