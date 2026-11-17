//! The holder, end to end: runs the built `gatetrain` binary with synthetic children (the gt-testchild program).
use gatetrain_core::{declog, ledger::Ledger};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

fn gt_cmd(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gatetrain"));
    c.current_dir(dir).env("GATETRAIN_BOOT_ID", "boot-A").env_remove("GATETRAIN_PARALLEL").env_remove("GATETRAIN_CORES");
    c
}
/// The synthetic child process (crates/gatetrain-testchild), built next to the gatetrain binary.
fn child() -> String {
    let b = std::path::Path::new(env!("CARGO_BIN_EXE_gatetrain"));
    b.with_file_name(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX)).to_string_lossy().into_owned()
}
fn lock(d: &Path) -> PathBuf {
    d.join("host.lock")
}
fn hold(d: &Path, run: &str, extra: &[&str], code: &str) -> Command {
    let mut c = gt_cmd(d);
    c.args(["hold", run, "--lock", lock(d).to_str().unwrap(), "--db", d.join("g.db").to_str().unwrap()]).args(extra).arg("--").arg(child()).args(code.split(' '));
    c
}
fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn kinds(d: &Path) -> Vec<(String, String, String)> {
    let l = Ledger::open(&d.join("g.db")).unwrap();
    let mut st = l.conn.prepare("SELECT kind, subject, outcome FROM decisions ORDER BY seq").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|r| r.unwrap()).collect()
}
/// Start a holder whose child sleeps, and wait until it prints HOLDS.
fn start_holder(d: &Path, run: &str, secs: u32) -> Child {
    let mut ch = hold(d, run, &[], &format!("sleep {secs}")).stdout(Stdio::piped()).spawn().unwrap();
    let mut r = BufReader::new(ch.stdout.take().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), format!("HOLDS {run}"));
    std::mem::forget(r); // keep the pipe open so the holder is not killed by a closed stdout
    ch
}

#[test]
fn the_child_exit_code_passes_through_and_holds_and_released_are_printed() {
    let d = tempfile::tempdir().unwrap();
    let o = hold(d.path(), "r1", &[], "exit 7").output().unwrap();
    assert_eq!(o.status.code(), Some(7), "{}", String::from_utf8_lossy(&o.stderr));
    let s = out(&o);
    assert!(s.lines().next().unwrap() == "HOLDS r1", "{s}");
    assert!(s.contains("RELEASED r1 rc=7"), "{s}");
}

#[test]
fn every_hold_and_release_writes_one_decision_log_entry_and_the_chain_verifies() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(hold(d.path(), "r1", &[], "pass").output().unwrap().status.code(), Some(0));
    let k = kinds(d.path());
    assert_eq!(k, vec![("host.hold".into(), "r1".into(), "held".into()), ("host.release".into(), "r1".into(), "released".into())]);
    let v = declog::verify(&Ledger::open(&d.path().join("g.db")).unwrap()).unwrap();
    assert_eq!(v.unwrap().entries, 2);
}

#[test]
fn a_second_hold_is_refused_while_the_first_runs_and_status_reads_the_lock() {
    let d = tempfile::tempdir().unwrap();
    let mut a = start_holder(d.path(), "ra", 4);
    let b = hold(d.path(), "rb", &[], "pass").output().unwrap();
    assert_eq!(b.status.code(), Some(75), "{}{}", out(&b), String::from_utf8_lossy(&b.stderr));
    assert!(out(&b).contains("REFUSED rb"), "{}", out(&b));
    assert!(out(&b).contains("ra"), "names the holder: {}", out(&b));
    let st = gt_cmd(d.path()).args(["hold", "--status", "--lock", lock(d.path()).to_str().unwrap()]).output().unwrap();
    assert_eq!(st.status.code(), Some(0));
    assert!(out(&st).starts_with("HELD ra"), "{}", out(&st));
    let run = gt_cmd(d.path()).args(["running", "--lock", lock(d.path()).to_str().unwrap()]).output().unwrap();
    assert_eq!(run.status.code(), Some(0), "{}", out(&run));
    a.wait().unwrap();
    let run = gt_cmd(d.path()).args(["running", "--lock", lock(d.path()).to_str().unwrap()]).output().unwrap();
    assert_eq!(run.status.code(), Some(1), "{}", out(&run));
}

#[test]
fn with_wait_the_second_hold_waits_for_the_first() {
    let d = tempfile::tempdir().unwrap();
    let mut a = start_holder(d.path(), "ra", 2);
    let b = hold(d.path(), "rb", &["--wait-secs", "30"], "pass").output().unwrap();
    assert_eq!(b.status.code(), Some(0), "{}{}", out(&b), String::from_utf8_lossy(&b.stderr));
    assert!(out(&b).contains("HOLDS rb"));
    a.wait().unwrap();
}

fn write_stale(d: &Path, boot: &str) {
    std::fs::write(lock(d), b"").unwrap();
    let info = format!(r#"{{"run_id":"old","pid":999999,"boot_id":"{boot}","started_at":"2026-11-01T00:00:00Z","state":"held"}}"#);
    std::fs::write(d.join("host.lock.info"), info).unwrap();
}

#[test]
fn a_stale_lock_from_a_dead_pid_is_taken_over_and_logged() {
    let d = tempfile::tempdir().unwrap();
    write_stale(d.path(), "boot-A");
    let st = gt_cmd(d.path()).args(["hold", "--status", "--lock", lock(d.path()).to_str().unwrap()]).output().unwrap();
    assert_eq!(st.status.code(), Some(1));
    assert!(out(&st).starts_with("STALE old"), "{}", out(&st));
    let o = hold(d.path(), "new", &[], "pass").output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let k: Vec<String> = kinds(d.path()).into_iter().map(|x| x.0).collect();
    assert_eq!(k, vec!["host.takeover", "host.hold", "host.release"]);
}

#[test]
fn a_boot_id_change_marks_the_old_run_stopped_not_red() {
    let d = tempfile::tempdir().unwrap();
    write_stale(d.path(), "boot-OLD"); // gt_cmd runs as boot-A
    let st = gt_cmd(d.path()).args(["hold", "--status", "--lock", lock(d.path()).to_str().unwrap()]).output().unwrap();
    assert!(out(&st).starts_with("STOPPED old reboot"), "{}", out(&st));
    let o = hold(d.path(), "new", &[], "pass").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    let k = kinds(d.path());
    assert_eq!(k[0], ("run.stopped".into(), "old".into(), "stopped".into()));
    assert!(!k.iter().any(|x| x.2 == "red"));
}

#[test]
fn parallelism_comes_from_hosts_toml_only() {
    let d = tempfile::tempdir().unwrap();
    let hosts = d.path().join("hosts.toml");
    std::fs::write(&hosts, "[[hosts]]\nname = \"h\"\ncores = 16\nreserve_cores = 4\nmax_parallel_steps = 20\nmemory_check = false\n").unwrap();
    let code = "env P=GATETRAIN_PARALLEL C=GATETRAIN_CORES";
    let o = hold(d.path(), "r1", &["--hosts", hosts.to_str().unwrap(), "--host", "h"], code).output().unwrap();
    assert!(out(&o).contains("P=12 C=12"), "{}{}", out(&o), String::from_utf8_lossy(&o.stderr));
    let o = hold(d.path(), "r2", &[], code).output().unwrap();
    assert!(out(&o).contains("P=unset C=unset"), "no hard-coded number: {}", out(&o));
}

#[test]
fn the_holder_samples_into_log_samples_jsonl_tells_its_child_and_ingests_the_samples() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("holder.log");
    let o = hold(d.path(), "hs1", &["--log", log.to_str().unwrap(), "--samples", "1"], "env S=GATETRAIN_SAMPLED").output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(out(&o).contains("S=1"), "the child is told the holder samples, so chain does not: {}", out(&o));
    let o = hold(d.path(), "hs2", &["--log", log.to_str().unwrap(), "--samples", "1"], "sleep 3").output().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let text = std::fs::read_to_string(d.path().join("holder.log.samples.jsonl")).expect("the samples file next to the log");
    let rows: Vec<serde_json::Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).filter(|v: &serde_json::Value| v.get("host").is_some()).collect();
    assert!(rows.len() >= 2 && rows.iter().all(|r| r["run_id"] == "hs2" && r["running_steps"].is_null()), "{text}");
    let l = Ledger::open(&d.path().join("g.db")).unwrap();
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM host_samples WHERE run_id='hs2'", [], |r| r.get(0)).unwrap();
    assert!(n >= 2, "ingested into the ledger the holder writes");
    // off: no samples
    let o = hold(d.path(), "hs3", &["--log", log.to_str().unwrap(), "--samples", "0"], "sleep 2").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM host_samples WHERE run_id='hs3'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}
