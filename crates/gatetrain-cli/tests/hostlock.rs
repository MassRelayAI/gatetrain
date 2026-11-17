//! The batch engine's seams against the REAL holder, lock and memory admission (not the fakes).
use gatetrain_core::batch::{Admission, HostLock, LaunchRequest};
use gatetrain_core::hold::{self, Acquire, Stale, State};
use gatetrain_core::hostlock::{MemoryAdmission, RealLock};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn real(dir: &Path) -> RealLock {
    RealLock { lock: dir.join("host.lock"), db: dir.join("g.db"), hosts: None, holder: vec![env!("CARGO_BIN_EXE_gatetrain").into(), "hold".into()] }
}

/// The synthetic child process (crates/gatetrain-testchild), built next to the gatetrain binary.
fn child() -> String {
    let b = std::path::Path::new(env!("CARGO_BIN_EXE_gatetrain"));
    b.with_file_name(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX)).to_string_lossy().into_owned()
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
        command: vec![child(), "chain".into()],
        log: log.clone(),
        workdir: d.path().to_path_buf(),
        env: vec![],
        role: "proof".into(),
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
fn admission_refuses_proof_and_light_on_a_held_lock() {
    let d = tempfile::tempdir().unwrap();
    let a = MemoryAdmission::new(d.path().join("host.lock"), d.path().join("g.db"), None, vec![]);
    assert_eq!(a.admit("proof", "b").unwrap(), None);
    let Acquire::Got(held) = hold::try_acquire(&a.lock, "other").unwrap() else { panic!() };
    let why = a.admit("proof", "b").unwrap().expect("refused");
    assert!(why.contains("other"), "{why}");
    // F6 (engine-v2 E5): a light run no longer starts on a host that is proving
    assert!(a.admit("light", "b").unwrap().expect("refused").contains("proving"));
    held.release().unwrap();
}

#[test]
fn admission_applies_the_memory_rule_to_the_chain_steps() {
    let d = tempfile::tempdir().unwrap();
    let hosts = d.path().join("hosts.toml");
    std::fs::write(&hosts, "[[hosts]]\nname = \"host-a\"\nmemory_gb = 8\nmemory_reserve_gb = 2\nstep_default_gb = 4\n").unwrap();
    let mk = |steps: &[&str]| MemoryAdmission::new(d.path().join("host.lock"), d.path().join("g.db"), Some((hosts.clone(), "host-a".into())), steps.iter().map(|s| s.to_string()).collect());
    assert_eq!(mk(&["lint"]).admit("proof", "b").unwrap(), None, "4 GB fits in 6 GB");
    let why = mk(&["lint", "test"]).admit("proof", "b").unwrap().expect("8 GB does not fit in 6 GB");
    assert!(why.contains("REFUSED"), "{why}");
}

#[test]
fn a_light_run_is_refused_on_a_host_whose_proof_lock_is_held_unless_the_host_says_otherwise() {
    let d = tempfile::tempdir().unwrap();
    let mut a = MemoryAdmission::new(d.path().join("host.lock"), d.path().join("g.db"), None, vec![]);
    assert_eq!(a.admit_rule("light", "b").unwrap(), None, "a free host admits a light run");
    let Acquire::Got(held) = hold::try_acquire(&a.lock, "proof-run").unwrap() else { panic!() };
    let (rule, why) = a.admit_rule("light", "b").unwrap().expect("refused: the host is proving");
    assert_eq!(rule, "host.refuse-proof-running");
    assert!(why.contains("proof-run"), "{why}");
    a.light_while_proving = true;
    assert_eq!(a.admit_rule("light", "b").unwrap(), None, "light_while_proving = true is the host's own word");
    held.release().unwrap();
}

#[test]
fn a_light_run_is_admitted_on_the_steps_of_the_preflight_not_the_chain() {
    let d = tempfile::tempdir().unwrap();
    let hosts = d.path().join("hosts.toml");
    std::fs::write(&hosts, "[[hosts]]\nname = \"host-a\"\nmemory_gb = 8\nmemory_reserve_gb = 2\nstep_default_gb = 4\n").unwrap();
    let mut a = MemoryAdmission::new(d.path().join("host.lock"), d.path().join("g.db"), Some((hosts, "host-a".into())), vec!["lint".into(), "test".into()]);
    assert!(a.admit("proof", "b").unwrap().is_some(), "8 GB of chain steps do not fit in 6 GB");
    assert!(a.admit("light", "b").unwrap().is_some(), "without the pre-flight's own steps the chain's are used");
    a.preflight_steps = vec!["lint".into()];
    assert_eq!(a.admit("light", "b").unwrap(), None, "4 GB of pre-flight steps fit");
    assert!(a.admit("proof", "b").unwrap().is_some(), "the proof is still judged on the chain's steps");
}

fn floor_adm(d: &Path, uses: &str, min_free: &str) -> MemoryAdmission {
    let hosts = d.join("hosts-floor.toml");
    let disk = d.to_string_lossy().replace('\\', "/");
    std::fs::write(&hosts, format!("[[hosts]]\nname = \"host-a\"\nmemory_gb = 64\nmemory_reserve_gb = 2\nstep_default_gb = 1\n[[hosts.disks]]\npath = \"{disk}\"\nmin_free_gb = {min_free}\nuse = [\"{uses}\"]\n")).unwrap();
    let mut a = MemoryAdmission::new(d.join("host.lock"), d.join("g.db"), Some((hosts, "host-a".into())), vec![]);
    a.disk_floor = true;
    a
}

#[test]
fn the_disk_floor_refuses_an_unknown_growth_and_admits_a_known_one_that_fits() {
    let d = tempfile::tempdir().unwrap();
    let mut a = floor_adm(d.path(), "chain", "0");
    let (rule, why) = a.admit_rule("proof", "b").unwrap().expect("unknown growth refuses");
    assert_eq!(rule, "host.refuse-disk");
    assert!(why.contains("unknown"), "{why}");
    a.needs_chain_gb = Some(0.001);
    assert_eq!(a.admit_rule("proof", "b").unwrap(), None, "a tiny growth fits");
}

#[test]
fn the_disk_floor_refuses_when_free_minus_growth_is_below_the_minimum() {
    let d = tempfile::tempdir().unwrap();
    let mut a = floor_adm(d.path(), "chain", "0");
    a.needs_chain_gb = Some(1.0e9);
    let (rule, why) = a.admit_rule("proof", "b").unwrap().expect("refused");
    assert_eq!(rule, "host.refuse-disk");
    assert!(why.contains("min_free_gb") && why.contains("predicted growth"), "{why}");
    // a floor that is off changes nothing
    a.disk_floor = false;
    assert_eq!(a.admit_rule("proof", "b").unwrap(), None);
}

#[test]
fn the_disk_floor_looks_only_at_the_disk_whose_use_covers_the_run() {
    let d = tempfile::tempdir().unwrap();
    // the only disk is for the chain: a light run (preflight) is not judged by it
    let mut a = floor_adm(d.path(), "chain", "0");
    a.needs_preflight_gb = Some(1.0e9);
    assert_eq!(a.admit_rule("light", "b").unwrap(), None);
    // the same disk for preflight: now it is
    let mut a = floor_adm(d.path(), "preflight", "0");
    a.needs_preflight_gb = Some(1.0e9);
    assert!(a.admit_rule("light", "b").unwrap().is_some());
}

#[test]
fn light_while_proving_is_a_known_host_key_and_is_read_from_the_file() {
    let text = "[[hosts]]\nname = \"h\"\nlight_while_proving = true\n[[hosts]]\nname = \"g\"\n";
    gatetrain_core::config::validate_text(text).unwrap();
    assert!(gatetrain_core::hostlock::host_flag(text, "h", "light_while_proving"));
    assert!(!gatetrain_core::hostlock::host_flag(text, "g", "light_while_proving"));
}
