//! The admission guard end to end (docs/admission.md): fake /proc readings (`--guard-proc`), a real disk floor set above the real free
//! space, a real slow HTTP service, and a hosts file edited while the run waits. Orders are read from the log, never from wall clocks.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

fn steps(work: &Path) -> PathBuf {
    let c = child();
    let body = format!("[[step]]\nname = 'a'\ntier = 'fast'\nargv = ['{c}', 'sleep', '0.3']\n\n[[step]]\nname = 'b'\ntier = 'fast'\nargv = ['{c}', 'sleep', '0.3']\n");
    let f = work.join("steps.toml");
    std::fs::write(&f, body.replace('\\', "/")).unwrap();
    f
}

fn fake_proc(dir: &Path, mem_full: f64, io_full: f64, cpu_some: f64, load: f64) -> PathBuf {
    let p = dir.join("proc");
    std::fs::create_dir_all(p.join("pressure")).unwrap();
    let psi = |s: f64, f: f64| format!("some avg10={s:.2} avg60=0.00 avg300=0.00 total=1\nfull avg10={f:.2} avg60=0.00 avg300=0.00 total=1\n");
    std::fs::write(p.join("pressure/memory"), psi(mem_full, mem_full)).unwrap();
    std::fs::write(p.join("pressure/io"), psi(io_full, io_full)).unwrap();
    std::fs::write(p.join("pressure/cpu"), psi(cpu_some, 0.0)).unwrap();
    std::fs::write(p.join("loadavg"), format!("{load:.2} 1.00 1.00 3/900 12345\n")).unwrap();
    p
}

fn chain(work: &Path, extra: &[&str]) -> Output {
    let out = work.join(format!("out-{}", std::fs::read_dir(work).unwrap().count()));
    Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", steps(work).to_str().unwrap(), "--out", out.to_str().unwrap(), "--samples", "0", "--par", "2", "--order", "file"])
        .args(extra)
        .current_dir(work)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

fn at(o: &Output, prefix: &str) -> usize {
    text(o).lines().position(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no {prefix} line in:\n{}", text(o)))
}

/// both steps started before either ended (they ran side by side)
fn side_by_side(o: &Output) -> bool {
    at(o, "STEP_START b") < at(o, "STEP_EXIT a")
}

fn dir_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn severe_memory_pressure_holds_new_steps_then_goes_on_at_par_1() {
    let w = tempfile::tempdir().unwrap();
    let proc = fake_proc(w.path(), 35.0, 0.0, 0.0, 1.0);
    let o = chain(w.path(), &["--max-psi-mem", "20", "--max-hold", "1", "--guard-every", "0.2", "--guard-proc", &dir_str(&proc)]);
    assert_eq!(o.status.code(), Some(0), "a hold is never a red:\n{}", text(&o));
    assert!(at(&o, "GUARD_HOLD psi memory full avg10 35 > 20") < at(&o, "STEP_START a"), "{}", text(&o));
    assert!(at(&o, "GUARD_HOLD_TIMEOUT") < at(&o, "STEP_START a"));
    assert!(!side_by_side(&o), "after the timeout the run goes on one step at a time:\n{}", text(&o));
}

#[test]
fn normal_high_load_and_cpu_and_io_pressure_do_not_hold() {
    let w = tempfile::tempdir().unwrap();
    let proc = fake_proc(w.path(), 2.0, 80.0, 99.0, 999.0);
    let cfg = w.path().join("hosts.toml");
    std::fs::write(&cfg, format!("[guard]\nmax_psi_mem_full = 20\nmonitor_every_s = 0.1\n[[guard.disk]]\npath = '{}'\nmin_free_gb = 0\n", dir_str(w.path()).replace('\\', "/"))).unwrap();
    let o = chain(w.path(), &["--guard-config", &dir_str(&cfg), "--guard-proc", &dir_str(&proc)]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(!text(&o).contains("GUARD_HOLD"), "load 999, cpu 99, io 80 only monitor:\n{}", text(&o));
    assert!(text(&o).lines().any(|l| l.starts_with("GUARD_MONITOR ") && l.contains("load1=999") && l.contains("io_full10=80")), "{}", text(&o));
    assert!(side_by_side(&o), "{}", text(&o));
}

#[test]
fn a_near_full_disk_holds_and_an_edit_to_the_hosts_file_releases_it() {
    let w = tempfile::tempdir().unwrap();
    let proc = fake_proc(w.path(), 0.0, 0.0, 0.0, 1.0);
    let cfg = w.path().join("hosts.toml");
    let path = dir_str(w.path()).replace('\\', "/");
    // a floor far above the real free space: this disk is "near full" for the guard
    let full = format!("profile = 'mixed'\n[guard]\nevery_s = 0.2\nmax_hold_s = 600\n[[guard.disk]]\npath = '{path}'\nmin_free_gb = 1000000\n");
    std::fs::write(&cfg, &full).unwrap();
    let cfg2 = cfg.clone();
    let editor = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        std::fs::write(&cfg2, full.replace("min_free_gb = 1000000", "min_free_gb = 0")).unwrap();
    });
    let o = chain(w.path(), &["--guard-config", &dir_str(&cfg), "--guard-proc", &dir_str(&proc)]);
    editor.join().unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(at(&o, &format!("GUARD_HOLD disk {path} free")) < at(&o, "GUARD_CONFIG changed: disk["), "{}", text(&o));
    assert!(text(&o).contains("min_free_gb: 1000000 -> 0"), "{}", text(&o));
    assert!(at(&o, "GUARD_CONFIG changed") < at(&o, "GUARD_RELEASE"));
    assert!(at(&o, "GUARD_RELEASE") < at(&o, "STEP_START a"));
    assert!(!text(&o).contains("GUARD_HOLD_TIMEOUT"), "released by the edit, not by the timeout");
    assert!(side_by_side(&o), "released: back to par 2:\n{}", text(&o));
}

/// A one-thread HTTP service that answers 200 after `delay_ms`.
fn slow_service(delay_ms: u64) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                std::thread::sleep(Duration::from_millis(delay_ms));
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            });
        }
    });
    format!("http://{addr}/")
}

fn have_curl() -> bool {
    Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn a_slow_protected_service_holds_new_steps() {
    if !have_curl() {
        eprintln!("SKIP: no curl on this host");
        return;
    }
    let w = tempfile::tempdir().unwrap();
    let proc = fake_proc(w.path(), 0.0, 0.0, 0.0, 1.0);
    let url = slow_service(600);
    let o = chain(w.path(), &["--probe", &url, "--probe-max-ms", "150", "--probe-fails", "1", "--probe-every", "0.1", "--guard-every", "0.1", "--max-hold", "2", "--guard-proc", &dir_str(&proc)]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(at(&o, &format!("GUARD_HOLD probe {url} bad 1x in a row")) < at(&o, "STEP_START a"), "{}", text(&o));
    // and a fast one does not
    let fast = slow_service(0);
    let o = chain(w.path(), &["--probe", &fast, "--probe-max-ms", "1500", "--probe-fails", "1", "--guard-proc", &dir_str(&proc)]);
    assert!(!text(&o).contains("GUARD_HOLD"), "{}", text(&o));
}

#[test]
fn guard_wait_returns_at_once_on_a_quiet_host_and_waits_while_held() {
    let w = tempfile::tempdir().unwrap();
    let gt = env!("CARGO_BIN_EXE_gatetrain");
    let quiet = fake_proc(&w.path().join("q"), 0.0, 0.0, 0.0, 1.0);
    let o = Command::new(gt).args(["guard", "wait", "--max-psi-mem", "20", "--guard-proc", &dir_str(&quiet)]).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o).trim().is_empty(), "a quiet wait prints nothing: {}", text(&o));
    let loud = fake_proc(&w.path().join("l"), 50.0, 0.0, 0.0, 1.0);
    let o = Command::new(gt).args(["guard", "wait", "--label", "gate-x", "--max-psi-mem", "20", "--max-hold", "0.5", "--guard-every", "0.1", "--guard-proc", &dir_str(&loud)]).output().unwrap();
    assert_eq!(o.status.code(), Some(0), "a wait never fails: {}", text(&o));
    assert!(text(&o).contains("gate-x GUARD_HOLD psi memory full avg10 50 > 20") && text(&o).contains("gate-x GUARD_WAIT_END"), "{}", text(&o));
    let o = Command::new(gt).args(["guard", "status", "--max-psi-mem", "20", "--guard-proc", &dir_str(&loud)]).output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("GUARD_DECISION hold"));
}

#[test]
fn guard_env_prints_the_opt_in_caps_and_a_bad_config_is_refused() {
    let w = tempfile::tempdir().unwrap();
    let gt = env!("CARGO_BIN_EXE_gatetrain");
    let cfg = w.path().join("hosts.toml");
    std::fs::write(&cfg, "[guard]\n").unwrap();
    let o = Command::new(gt).args(["guard", "env", "--guard-config", &dir_str(&cfg)]).output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o).contains("GUARD_SCOPE=0\n") && text(&o).contains("GUARD_CONTAINER_MEMORY=\n"), "off by default: {}", text(&o));
    std::fs::write(&cfg, "[guard.caps]\nscope = true\ncpu_quota = '2400%'\ncontainer_cpus = '2'\n").unwrap();
    let o = Command::new(gt).args(["guard", "env", "--guard-config", &dir_str(&cfg)]).output().unwrap();
    assert!(text(&o).contains("GUARD_SCOPE=1\n") && text(&o).contains("GUARD_CPU_QUOTA=2400%\n") && text(&o).contains("GUARD_CONTAINER_CPUS=2\n"), "{}", text(&o));
    std::fs::write(&cfg, "[guard]\nmax_psi_memory = 20\n").unwrap();
    let o = chain(w.path(), &["--guard-config", &dir_str(&cfg)]);
    assert_eq!(o.status.code(), Some(2), "a typo is a config error at the start: {}", text(&o));
}
