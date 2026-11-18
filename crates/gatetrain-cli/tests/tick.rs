//! `gatetrain tick` as a command: the ledger lock, the exit codes, and that a tick changes no config file. Synthetic fixtures only:
//! a temp git repository and hosts whose reach has no runner, so no host is contacted.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn gt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).env_remove("GATETRAIN_PARALLEL").args(args).output().unwrap()
}
fn out(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
fn sha256_of(p: &Path) -> String {
    gatetrain_core::chain::digest_of(&String::from_utf8_lossy(&std::fs::read(p).unwrap()))
}

const HOSTS: &str = "[[hosts]]\nname = \"alpha\"\nreach = \"none\"\n\n[[hosts]]\nname = \"beta\"\nreach = \"none\"\n\n[[hosts]]\nname = \"gamma\"\nreach = \"none\"\n";

/// A repository with one lane and a ledger holding batch `b1` (open). gatetrain.toml has no `[preflight] command`, so a tick stacks and
/// settles b1 and then stops on a rule a person must answer.
struct Fx {
    _t: tempfile::TempDir,
    root: PathBuf,
    db: String,
}

impl Fx {
    fn new(with_batch: bool) -> Fx {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        let r = root.join("repo");
        std::fs::create_dir_all(&r).unwrap();
        let g = |a: &[&str]| {
            let o = Command::new("git").arg("-C").arg(&r).args(a).output().unwrap();
            assert!(o.status.success(), "{a:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        std::fs::write(r.join("a"), "a").unwrap();
        g(&["add", "--", "a"]);
        g(&["commit", "-q", "-m", "init"]);
        g(&["switch", "-q", "-c", "lane1"]);
        std::fs::write(r.join("b"), "b").unwrap();
        g(&["add", "--", "b"]);
        g(&["commit", "-q", "-m", "feat: b"]);
        g(&["switch", "-q", "main"]);
        std::fs::write(root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n").unwrap();
        std::fs::write(root.join("hosts.toml"), HOSTS).unwrap();
        let fx = Fx { db: fwd(&root.join("g.db")), root, _t: t };
        assert!(gt(&["ledger", "migrate", "--db", &fx.db]).status.success());
        if with_batch {
            let o = gt(&["batch", "new", "b1", "--base", "main", "--workdir", &fwd(&r), "lane1", "--db", &fx.db, "--config", &fx.cfg()]);
            assert!(o.status.success(), "{}", out(&o));
        }
        fx
    }
    fn cfg(&self) -> String {
        fwd(&self.root.join("gatetrain.toml"))
    }
    fn hosts(&self) -> String {
        fwd(&self.root.join("hosts.toml"))
    }
    fn tick(&self, extra: &[&str]) -> Output {
        let (c, h) = (self.cfg(), self.hosts());
        let mut a = vec!["tick", "--db", &self.db, "--config", &c, "--hosts", &h, "--host", "alpha"];
        a.extend(extra);
        gt(&a)
    }
    fn rows(&self) -> (i64, i64) {
        let c = rusqlite::Connection::open(&self.db).unwrap();
        let n = |q: &str| c.query_row(q, [], |r| r.get::<_, i64>(0)).unwrap();
        (n("SELECT COUNT(*) FROM batch_events"), n("SELECT COUNT(*) FROM decisions"))
    }
}

#[test]
fn a_tick_advances_until_a_rule_needs_a_person_and_exits_1() {
    let fx = Fx::new(true);
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    assert_eq!(
        text.trim_end(),
        "b1: open -> stacked -> settled, BLOCKED config.preflight-command: gatetrain.toml has no [preflight] command\n\
         hosts: alpha no runner (reach 'none'), tidy skipped; beta no runner (reach 'none'), tidy skipped; gamma no runner (reach 'none'), tidy skipped\n\
         advice: none"
    );
    assert!(out(&gt(&["batch", "status", "b1", "--db", &fx.db])).contains("b1: settled"));
    // the same refusal on the next tick is not written again
    let before = fx.rows();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("b1: settled, BLOCKED config.preflight-command"), "{}", out(&o));
    assert_eq!(fx.rows(), before);
}

#[test]
fn a_tick_changes_no_config_file() {
    let fx = Fx::new(true);
    let files = [fx.root.join("gatetrain.toml"), fx.root.join("hosts.toml")];
    let before: Vec<String> = files.iter().map(|f| sha256_of(f)).collect();
    fx.tick(&[]);
    fx.tick(&["--json"]);
    let after: Vec<String> = files.iter().map(|f| sha256_of(f)).collect();
    assert_eq!(before, after);
}

#[test]
fn a_second_tick_while_one_holds_the_ledger_lock_does_nothing_and_exits_0() {
    let fx = Fx::new(true);
    let before = fx.rows();
    let held = gatetrain_core::tick::try_lock(&gatetrain_core::tick::lock_path(Path::new(&fx.db))).unwrap();
    assert!(held.is_some());
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "tick: another tick holds the lock");
    assert_eq!(fx.rows(), before, "nothing was written");
    drop(held);
    assert_eq!(fx.tick(&[]).status.code(), Some(1), "the lock is free again");
}

#[test]
fn no_open_batch_is_exit_0_and_a_bad_config_is_exit_2() {
    let fx = Fx::new(false);
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("batches: none open"), "{}", out(&o));
    assert_eq!(fx.rows().0, 0, "tick never opens a batch");
    std::fs::write(fx.root.join("hosts.toml"), "[[hosts]]\nnam = \"x\"\n").unwrap();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(2), "{}", out(&o));
    std::fs::write(fx.root.join("hosts.toml"), HOSTS).unwrap();
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\nstorm_reds = 0\n").unwrap();
    assert_eq!(fx.tick(&[]).status.code(), Some(2));
}

#[test]
fn json_carries_the_lines_and_the_exit_code() {
    let fx = Fx::new(true);
    let o = fx.tick(&["--json"]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["exit"], 1);
    assert_eq!(v["lines"].as_array().unwrap().len(), 3);
    assert_eq!(v["batches"][0]["batch"], "b1");
    assert_eq!(v["batches"][0]["end"], "blocked");
}

// ---------------------------------------------------------------- review fixes

fn lock_of(fx: &Fx) -> PathBuf {
    gatetrain_core::tick::lock_path(Path::new(&fx.db))
}

#[test]
fn b1_a_writing_batch_command_refuses_while_a_tick_holds_the_ledger_lock() {
    let fx = Fx::new(true);
    let before = fx.rows();
    let held = match gatetrain_core::tick::take(&lock_of(&fx), "tick").unwrap() {
        gatetrain_core::tick::Taken::Got(l) => l,
        gatetrain_core::tick::Taken::Held(_) => panic!("the lock was free"),
    };
    let me = std::process::id().to_string();
    for args in [
        vec!["batch", "advance", "b1"],
        vec!["batch", "hold", "b1", "--reason", "x"],
        vec!["batch", "abandon", "b1", "--reason", "x"],
        vec!["batch", "advance", "b1", "--wait", "1"],
    ] {
        let mut a = args.clone();
        let c = fx.cfg();
        a.extend(["--db", &fx.db, "--config", &c]);
        let o = gt(&a);
        assert_eq!(o.status.code(), Some(75), "{args:?}: {}", out(&o));
        assert!(out(&o).contains("a tick is running (pid ") && out(&o).contains(&me), "{}", out(&o));
    }
    assert_eq!(fx.rows(), before, "nothing was written");
    assert!(gt(&["batch", "status", "--db", &fx.db]).status.success(), "a read needs no lock");
    drop(held);
    let c = fx.cfg();
    assert_eq!(gt(&["batch", "advance", "b1", "--db", &fx.db, "--config", &c]).status.code(), Some(0), "free again");
}

#[test]
fn minor1_a_tick_finding_the_lock_held_too_long_says_so_and_exits_1() {
    let fx = Fx::new(true);
    let lp = lock_of(&fx);
    let held = gatetrain_core::tick::try_lock(&lp).unwrap();
    assert!(held.is_some());
    std::fs::write(gatetrain_core::tick::info_path(&lp), "{\"pid\": 4242, \"started\": \"2026-01-01T00:00:00Z\", \"by\": \"tick\"}").unwrap();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let t = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(t.starts_with("tick: the ledger lock is held by pid 4242 (tick) since 2026-01-01T00:00:00Z"), "{t}");
    assert!(t.contains("[tick] stuck_minutes = 60"), "{t}");
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n[tick]\nstuck_minutes = 999999999\n").unwrap();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "tick: another tick holds the lock (pid 4242, started 2026-01-01T00:00:00Z)");
}

#[test]
fn minor2_hosts_without_a_host_or_a_proof_host_is_exit_2_and_lock_is_accepted() {
    let fx = Fx::new(true);
    let (c, h) = (fx.cfg(), fx.hosts());
    let o = gt(&["tick", "--db", &fx.db, "--config", &c, "--hosts", &h]);
    assert_eq!(o.status.code(), Some(2), "{}", out(&o));
    assert!(out(&o).contains("--host"), "{}", out(&o));
    let lock = fwd(&fx.root.join("other.lock"));
    let o = fx.tick(&["--lock", &lock]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
}
