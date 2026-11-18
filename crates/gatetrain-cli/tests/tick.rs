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
        std::fs::write(root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n[tick]\nenabled = true\n").unwrap();
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
    assert!(free_soon(&gatetrain_core::tick::lock_path(Path::new(&fx.db))));
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
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\nstorm_reds = 0\n[tick]\nenabled = true\n").unwrap();
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
    let wd = fwd(&fx.root.join("repo"));
    // every subcommand that writes a batch, so leaving any one out of the lock goes red
    for args in [
        vec!["batch", "new", "b2", "--base", "main", "--workdir", &wd, "lane1"],
        vec!["batch", "advance", "b1"],
        vec!["batch", "hold", "b1", "--reason", "x"],
        vec!["batch", "release", "b1"],
        vec!["batch", "restack", "b1"],
        vec!["batch", "retry", "b1"],
        vec!["batch", "note-cause", "b1", "flake", "x"],
        vec!["batch", "close", "b1"],
        vec!["batch", "abandon", "b1", "--reason", "x"],
        vec!["batch", "advance", "b1", "--wait", "1"],
    ] {
        let mut a = args.clone();
        let c = fx.cfg();
        a.extend(["--db", &fx.db, "--config", &c]);
        let o = gt(&a);
        assert_eq!(o.status.code(), Some(73), "{args:?}: {}", out(&o));
        assert!(out(&o).contains("a tick is running (pid ") && out(&o).contains(&me), "{}", out(&o));
    }
    assert_eq!(fx.rows(), before, "nothing was written");
    assert!(gt(&["batch", "status", "--db", &fx.db]).status.success(), "a read needs no lock");
    drop(held);
    assert!(free_soon(&lock_of(&fx)));
    let c = fx.cfg();
    assert_eq!(gt(&["batch", "advance", "b1", "--db", &fx.db, "--config", &c]).status.code(), Some(0), "free again");
}

#[test]
fn minor4_the_refusal_names_a_batch_holder_too() {
    let fx = Fx::new(true);
    let _held = match gatetrain_core::tick::take(&lock_of(&fx), "batch").unwrap() {
        gatetrain_core::tick::Taken::Got(l) => l,
        gatetrain_core::tick::Taken::Held(_) => panic!("the lock was free"),
    };
    let c = fx.cfg();
    let o = gt(&["batch", "advance", "b1", "--db", &fx.db, "--config", &c]);
    assert_eq!(o.status.code(), Some(73), "{}", out(&o));
    assert!(out(&o).contains("a `batch` command is running (pid "), "{}", out(&o));
}

fn sidecar(fx: &Fx, pid: u32, started: &str) {
    std::fs::write(gatetrain_core::tick::info_path(&lock_of(fx)), format!("{{\"pid\": {pid}, \"started\": \"{started}\", \"by\": \"tick\"}}")).unwrap();
}

#[test]
fn minor1_a_tick_finding_the_lock_held_too_long_says_so_and_exits_1() {
    let fx = Fx::new(true);
    let held = gatetrain_core::tick::try_lock(&lock_of(&fx)).unwrap();
    assert!(held.is_some());
    let me = std::process::id();
    sidecar(&fx, me, "2026-01-01T00:00:00Z");
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    let t = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(t.starts_with(&format!("tick: the ledger lock is held by pid {me} (tick) since 2026-01-01T00:00:00Z")), "{t}");
    assert!(t.contains("[tick] stuck_minutes = 60"), "{t}");
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n[tick]\nenabled = true\nstuck_minutes = 999999999\nmax_hold_minutes = 999999999\n").unwrap();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), format!("tick: another tick holds the lock (pid {me}, started 2026-01-01T00:00:00Z)"));
}

#[test]
fn minor3_a_sidecar_of_a_dead_pid_is_ignored_and_no_sidecar_falls_back_to_the_lock_file_age() {
    let fx = Fx::new(true);
    let lp = lock_of(&fx);
    let held = gatetrain_core::tick::try_lock(&lp).unwrap();
    assert!(held.is_some());
    // a pid that has exited: its old sidecar describes nobody
    let mut child = Command::new("git").arg("--version").stdout(std::process::Stdio::null()).spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    sidecar(&fx, dead, "2026-01-01T00:00:00Z");
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(0), "a fresh lock file with a stale sidecar is a running tick: {}", out(&o));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "tick: another tick holds the lock");
    // no sidecar to trust and a lock file untouched for two hours: stuck
    std::fs::remove_file(gatetrain_core::tick::info_path(&lp)).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&lp).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600)).unwrap();
    drop(f);
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("tick: the ledger lock is held by an unknown holder since "), "{}", out(&o));
}

#[test]
fn minor3_the_sidecar_is_written_whole() {
    let fx = Fx::new(true);
    let lp = lock_of(&fx);
    let _held = match gatetrain_core::tick::take(&lp, "tick").unwrap() {
        gatetrain_core::tick::Taken::Got(l) => l,
        gatetrain_core::tick::Taken::Held(_) => panic!("the lock was free"),
    };
    let i = gatetrain_core::tick::read_info(&lp).expect("a readable sidecar");
    assert_eq!(i.pid, i64::from(std::process::id()));
    let dir: Vec<String> = std::fs::read_dir(lp.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!dir.iter().any(|n| n.ends_with(".tmp")), "no temp file is left: {dir:?}");
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

/// A released lock is free at once, except that on Linux a child forked by a concurrent test in the instant before its exec shares
/// the open file (flock belongs to the open file description) until it execs. Poll briefly instead of failing on that window.
fn free_soon(lp: &Path) -> bool {
    for _ in 0..50 {
        if gatetrain_core::tick::try_lock(lp).unwrap().is_some() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

// ---------------------------------------------------------------- round-3 review fixes

#[test]
fn rev3_minor2_stuck_is_judged_by_the_heartbeat_not_the_start() {
    let fx = Fx::new(true);
    let lp = lock_of(&fx);
    let held = gatetrain_core::tick::try_lock(&lp).unwrap();
    assert!(held.is_some());
    let me = std::process::id();
    let beat = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    // started two hours ago (past stuck_minutes = 60, within max_hold_minutes) and beating now
    let started = chrono_like_iso(2 * 3600);
    std::fs::write(gatetrain_core::tick::info_path(&lp), format!("{{\"pid\": {me}, \"started\": \"{started}\", \"by\": \"tick\", \"beat_ms\": {beat}}}")).unwrap();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(0), "started long ago but beating now: a long deploy, not a stuck holder: {}", out(&o));
}

#[test]
fn rev3_minor4_the_wait_help_names_exit_73() {
    let o = gt(&["batch", "--help"]);
    let t = out(&o);
    assert!(t.contains("exit 73") && !t.contains("exit 75"), "{t}");
}

// ---------------------------------------------------------------- the failure-model rebuild (S0..S8)

#[test]
fn s0_tick_is_off_unless_the_config_turns_it_on() {
    let fx = Fx::new(true);
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n").unwrap();
    let before = fx.rows();
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(2), "{}", out(&o));
    assert!(out(&o).contains("[tick] enabled = true"), "{}", out(&o));
    assert_eq!(fx.rows(), before, "nothing was written");
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n[tick]\nenabled = false\n").unwrap();
    assert_eq!(fx.tick(&[]).status.code(), Some(2));
    assert!(gatetrain_core::batchcfg::BatchConfig::parse("[tick]\nenabled = \"yes\"\n").is_err(), "enabled is a boolean");
}

#[test]
fn s2_batch_advance_after_a_failed_attempt_is_refused_until_retry_with_a_reason() {
    let fx = Fx::new(true);
    std::fs::write(fx.root.join("gatetrain.toml"), "[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n[stack]\ncommand = [\"sh\", \"-c\", \"exit 3\"]\n[tick]\nenabled = true\n").unwrap();
    let c = fx.cfg();
    let adv = |extra: &[&str]| {
        let mut a = vec!["batch", "advance", "b1", "--db", &fx.db, "--config", &c];
        a.extend(extra);
        gt(&a)
    };
    let o = adv(&[]);
    assert_eq!(o.status.code(), Some(1), "the first attempt fails: {}", out(&o));
    let o = adv(&[]);
    assert_eq!(o.status.code(), Some(3), "{}", out(&o));
    assert!(out(&o).contains("rule    batch.failed-needs-person"), "{}", out(&o));
    let o = adv(&["--retry"]);
    assert_eq!(o.status.code(), Some(2), "--retry needs --reason: {}", out(&o));
    let o = adv(&["--retry", "--reason", "the stack script was fixed"]);
    assert_eq!(o.status.code(), Some(1), "one more attempt ran (and failed again): {}", out(&o));
    assert!(out(&o).contains("one more attempt of T1 is allowed"), "{}", out(&o));
}

/// An ISO time `secs` ago, in UTC.
fn chrono_like_iso(secs: u64) -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() - secs;
    gatetrain_core::batch::iso_of(t as i64)
}

fn beating_sidecar(fx: &Fx, started: &str, extra: &str) {
    let me = std::process::id();
    let beat = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    std::fs::write(gatetrain_core::tick::info_path(&lock_of(fx)), format!("{{\"pid\": {me}, \"started\": \"{started}\", \"by\": \"tick\", \"beat_ms\": {beat}{extra}}}")).unwrap();
}

#[test]
fn s6_a_beating_holder_past_its_step_deadline_makes_the_tick_exit_1() {
    let fx = Fx::new(true);
    let held = gatetrain_core::tick::try_lock(&lock_of(&fx)).unwrap();
    assert!(held.is_some());
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    beating_sidecar(&fx, &chrono_like_iso(60), &format!(", \"effect\": \"b1 T8a deploy staging\", \"deadline_ms\": {}", now_ms - 10 * 60_000));
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("b1 T8a deploy staging") && out(&o).contains("past its deadline"), "{}", out(&o));
    // the same step within its deadline is a busy holder, exit 0
    beating_sidecar(&fx, &chrono_like_iso(60), &format!(", \"effect\": \"b1 T8a deploy staging\", \"deadline_ms\": {}", now_ms + 10 * 60_000));
    assert_eq!(fx.tick(&[]).status.code(), Some(0));
}

#[test]
fn s6_a_holder_beating_for_longer_than_max_hold_minutes_makes_the_tick_exit_1() {
    let fx = Fx::new(true);
    let held = gatetrain_core::tick::try_lock(&lock_of(&fx)).unwrap();
    assert!(held.is_some());
    beating_sidecar(&fx, &chrono_like_iso(13 * 3600), "");
    let o = fx.tick(&[]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("[tick] max_hold_minutes = 720"), "{}", out(&o));
}

/// B1 and MINOR 1 (round 2), as commands: a retry or a `clear-record` against a live deploy record is refused with exit 3 and clears
/// nothing; an unverifiable record needs `--clear-unverifiable-record`; `clear-record` works on an abandoned batch.
#[test]
fn b1_minor1_a_live_record_refuses_retry_and_clear_record_with_exit_3() {
    let fx = Fx::new(true);
    let running = fx.root.join(".gatetrain").join("running");
    std::fs::create_dir_all(&running).unwrap();
    let rec = running.join("staging.json");
    let mut live = Command::new("sh").args(["-c", "sleep 60"]).spawn().unwrap();
    let pid = live.id();
    let token = gatetrain_core::runner::process_token(pid);
    std::fs::write(&rec, serde_json::json!({"batch": "b1", "step": "deploy staging", "target": "staging", "pid": pid, "token": token}).to_string()).unwrap();
    let c = fx.cfg();
    let retry = gt(&["batch", "advance", "b1", "--retry", "--reason", "gone?", "--db", &fx.db, "--config", &c]);
    let clear = gt(&["batch", "clear-record", "b1", "--reason", "gone?", "--db", &fx.db, "--config", &c]);
    let kept = rec.exists();
    let _ = live.kill();
    let _ = live.wait();
    assert_eq!(retry.status.code(), Some(3), "{}", out(&retry));
    assert!(out(&retry).contains("deploy.record-live"), "{}", out(&retry));
    assert_eq!(clear.status.code(), Some(3), "{}", out(&clear));
    assert!(kept, "nothing cleared while it ran");
    // unverifiable: only with the flag (the flag needs --retry on advance)
    std::fs::write(&rec, serde_json::json!({"batch": "b1", "step": "deploy staging", "target": "staging"}).to_string()).unwrap();
    assert!(!gt(&["batch", "advance", "b1", "--clear-unverifiable-record", "--db", &fx.db, "--config", &c]).status.success(), "the flag needs --retry");
    assert!(gt(&["batch", "abandon", "b1", "--db", &fx.db, "--config", &c]).status.success());
    let o = gt(&["batch", "clear-record", "b1", "--reason", "checked", "--db", &fx.db, "--config", &c]);
    assert_eq!(o.status.code(), Some(3), "{}", out(&o));
    let o = gt(&["batch", "clear-record", "b1", "--reason", "checked", "--clear-unverifiable-record", "--db", &fx.db, "--config", &c]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("a second deploy may start beside an unseen one"), "{}", out(&o));
    assert!(!rec.exists());
}
