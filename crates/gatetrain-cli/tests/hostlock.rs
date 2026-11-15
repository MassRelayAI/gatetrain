//! The batch engine's seams against the REAL holder, lock and memory admission (not the fakes).
use gatetrain_core::batch::{Admission, HostLock, LaunchRequest};
use gatetrain_core::hold::{self, Acquire, Stale, State};
use gatetrain_core::hostlock::{MemoryAdmission, RealLock};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn real(dir: &Path) -> RealLock {
    RealLock { lock: dir.join("host.lock"), db: dir.join("g.db"), hosts: None, holder: vec![env!("CARGO_BIN_EXE_gatetrain").into(), "hold".into()] }
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

#[test]
fn a_dead_chain_without_gates_done_is_stale_not_running() {
    let d = tempfile::tempdir().unwrap();
    let l = real(d.path());
    // what a holder that was killed leaves: a sidecar still saying `held`, nobody holding the OS lock
    std::fs::write(&l.lock, "").unwrap();
    let info = format!(r#"{{"run_id":"run-1","pid":999999,"boot_id":"{}","started_at":"2026-11-15T00:00:00Z","state":"held"}}"#, hold::boot_id());
    std::fs::write(hold::info_path(&l.lock), info).unwrap();
    assert!(matches!(hold::status(&l.lock).unwrap(), State::Stale(Stale::DeadHolder(_))), "the dead holder is STALE");
    assert!(!l.names_run("run-1").unwrap(), "a stale lock does not name a running run");
}

#[test]
fn a_live_holder_names_only_its_own_run() {
    let d = tempfile::tempdir().unwrap();
    let l = real(d.path());
    assert!(!l.names_run("run-1").unwrap(), "no lock file: nobody runs");
    let Acquire::Got(held) = hold::try_acquire(&l.lock, "run-1").unwrap() else { panic!("free lock expected") };
    assert!(l.names_run("run-1").unwrap());
    assert!(!l.names_run("run-2").unwrap());
    held.release().unwrap();
    assert!(!l.names_run("run-1").unwrap(), "released: not running");
}

#[test]
fn launch_starts_the_real_holder_which_holds_runs_the_chain_and_releases() {
    let d = tempfile::tempdir().unwrap();
    let l = real(d.path());
    let log: PathBuf = d.path().join("run-7.log");
    let req = LaunchRequest {
        run_id: "run-7".into(),
        batch: "b".into(),
        sha: "x".into(),
        base: "y".into(),
        command: vec![python().into(), "-c".into(), "import os; print('CHAIN ' + os.environ['GATETRAIN_RUN_ID'] + ' ' + os.environ.get('GATETRAIN_PARALLEL', 'unset'))".into()],
        log: log.clone(),
        workdir: d.path().to_path_buf(),
        env: vec![],
    };
    l.launch(&req).unwrap();
    let t = Instant::now();
    let text = loop {
        let s = std::fs::read_to_string(&log).unwrap_or_default();
        if s.contains("RELEASED run-7") || t.elapsed() > Duration::from_secs(60) {
            break s;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(text.contains("HOLDS run-7"), "{text}");
    assert!(text.contains("CHAIN run-7 unset"), "the engine no longer forces GATETRAIN_PARALLEL: {text}");
    assert!(text.contains("RELEASED run-7 rc=0"), "{text}");
    assert!(!l.names_run("run-7").unwrap());
}

#[test]
fn admission_refuses_proof_on_a_held_lock_and_light_still_passes() {
    let d = tempfile::tempdir().unwrap();
    let a = MemoryAdmission { lock: d.path().join("host.lock"), db: d.path().join("g.db"), hosts: None, chain_steps: vec![] };
    assert_eq!(a.admit("proof", "b").unwrap(), None);
    let Acquire::Got(held) = hold::try_acquire(&a.lock, "other").unwrap() else { panic!() };
    let why = a.admit("proof", "b").unwrap().expect("refused");
    assert!(why.contains("other"), "{why}");
    assert_eq!(a.admit("light", "b").unwrap(), None);
    held.release().unwrap();
}

#[test]
fn admission_applies_the_memory_rule_to_the_chain_steps() {
    let d = tempfile::tempdir().unwrap();
    let hosts = d.path().join("hosts.toml");
    std::fs::write(&hosts, "[[hosts]]\nname = \"host-a\"\nmemory_gb = 8\nmemory_reserve_gb = 2\nstep_default_gb = 4\n").unwrap();
    let mk = |steps: &[&str]| MemoryAdmission { lock: d.path().join("host.lock"), db: d.path().join("g.db"), hosts: Some((hosts.clone(), "host-a".into())), chain_steps: steps.iter().map(|s| s.to_string()).collect() };
    assert_eq!(mk(&["lint"]).admit("proof", "b").unwrap(), None, "4 GB fits in 6 GB");
    let why = mk(&["lint", "test"]).admit("proof", "b").unwrap().expect("8 GB does not fit in 6 GB");
    assert!(why.contains("REFUSED"), "{why}");
}
