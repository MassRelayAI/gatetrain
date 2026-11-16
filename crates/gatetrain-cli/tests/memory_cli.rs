//! Memory admission and recording through the binary. Synthetic fixtures only.
use gatetrain_core::ledger::Ledger;
use std::path::Path;
use std::process::{Command, Output};

fn gt(d: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).current_dir(d).env_remove("GATETRAIN_PARALLEL").args(args).output().unwrap()
}
fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
/// The synthetic child process (crates/gatetrain-testchild), built next to the gatetrain binary.
fn child() -> String {
    let b = std::path::Path::new(env!("CARGO_BIN_EXE_gatetrain"));
    b.with_file_name(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX)).to_string_lossy().into_owned()
}

const HOSTS: &str = "[[hosts]]\nname = \"pc\"\ncores = 16\nreserve_cores = 4\nmax_parallel_steps = 12\nmemory_gb = 31\nmemory_reserve_gb = 12\nmemory_per_step_gb = 4\nmemory_per_cargo_job_gb = 2\n";

fn seed(d: &Path) {
    for (step, gb) in [("big", 15), ("mid", 9), ("small", 2)] {
        let o = gt(d, &["memory", "record", "--run", "r0", "--step", step, "--peak-mb", &(gb * 1024).to_string()]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}

#[test]
fn hold_refuses_a_chain_that_does_not_fit_and_logs_the_numbers_and_the_cheap_set() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("hosts.toml"), HOSTS).unwrap();
    seed(d.path());
    let o = gt(d.path(), &["hold", "r1", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--steps", "big,mid,small", "--", child().as_str(), "print", "RAN"]);
    assert_eq!(o.status.code(), Some(76), "{}{}", out(&o), String::from_utf8_lossy(&o.stderr));
    let s = out(&o);
    assert!(s.contains("predicted 26.0 GB") && s.contains("available 19.0 GB"), "{s}");
    assert!(s.contains("CHEAP-SET small,mid"), "{s}");
    assert!(!s.contains("RAN") && !s.contains("HOLDS"), "the chain must not start: {s}");
    let l = Ledger::open(&d.path().join("gatetrain.db")).unwrap();
    let (kind, ev): (String, String) = l.conn.query_row("SELECT kind, evidence FROM decisions", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(kind, "host.refuse-memory");
    assert!(ev.contains("\"available_mb\":19456") && ev.contains("cheap_set"), "{ev}");
    // the cheap set is admitted and runs, with memory-based parallelism and cargo jobs exported
    let o = gt(d.path(), &["hold", "r2", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--steps", "small,mid", "--", child().as_str(), "env", "P=GATETRAIN_PARALLEL", "J=CARGO_BUILD_JOBS"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), String::from_utf8_lossy(&o.stderr));
    assert!(out(&o).contains("P=4 J=9"), "{}", out(&o));
}

#[test]
fn memory_admit_prints_the_rule_without_launching() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("hosts.toml"), HOSTS).unwrap();
    seed(d.path());
    let o = gt(d.path(), &["memory", "admit", "--hosts", "hosts.toml", "--host", "pc", "--steps", "big,mid,small"]);
    assert_eq!(o.status.code(), Some(76));
    assert!(out(&o).contains("memory_gb - memory_reserve_gb"));
    let o = gt(d.path(), &["memory", "admit", "--hosts", "hosts.toml", "--host", "pc", "--steps", "small"]);
    assert_eq!(o.status.code(), Some(0));
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn the_holder_records_the_chain_peak_memory_in_the_ledger() {
    let d = tempfile::tempdir().unwrap();
    let o = gt(d.path(), &["hold", "r1", "--lock", "h.lock", "--", child().as_str(), "alloc", "80", "2.5"]);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let h = Ledger::open(&d.path().join("gatetrain.db")).unwrap().step_memory_history().unwrap();
    let mb = h["(chain)"][0];
    assert!(mb > 60.0 && mb < 4000.0, "peak {mb} MB; {}", out(&o));
}
