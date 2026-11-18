//! Liveness of a recorded plugin process and of its process group (enable-readiness review, minors 1, 3 and 7).
//! The child is crates/gatetrain-testchild; run these with `cargo test --workspace` so it is built.
use gatetrain_core::runner;
use std::path::PathBuf;

fn child() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap(); // target/<profile>/deps/<test> -> target/<profile>
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p
}

/// MINOR 7 (Windows): a process that ended with exit code 259 (`STILL_ACTIVE`) is not alive.
#[test]
fn minor7_a_process_that_exited_with_code_259_is_not_alive() {
    let mut c = std::process::Command::new(child()).args(["exit", "259"]).spawn().unwrap();
    let pid = c.id();
    let token = runner::process_token(pid);
    let st = c.wait().unwrap();
    assert_eq!(st.code(), Some(if cfg!(windows) { 259 } else { 259 & 0xff }));
    // `c` still holds the process handle, so the pid cannot be reused while we look
    assert!(!runner::process_alive(pid, None), "an ended process read as alive");
    assert!(!runner::process_alive(pid, token), "an ended process read as alive (with its token)");
    drop(c);
}

/// A live process is alive, and is not alive under another start token.
#[test]
fn a_live_process_is_alive_only_under_its_own_token() {
    let mut c = std::process::Command::new(child()).args(["sleep", "5"]).spawn().unwrap();
    let pid = c.id();
    let token = runner::process_token(pid);
    assert!(runner::process_alive(pid, None));
    if let Some(t) = token {
        assert!(runner::process_alive(pid, Some(t)));
        assert!(!runner::process_alive(pid, Some(t.wrapping_add(1))), "another start token is another process");
    }
    let _ = c.kill();
    let _ = c.wait();
}

/// MINOR 1: `group_alive` sees a group whose leader is gone while a member lives, and not once the group is gone.
#[cfg(unix)]
#[test]
fn minor1_group_alive_sees_a_member_after_its_leader_exited() {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new("/bin/sh");
    c.args(["-c", "sleep 30 & exit 0"]).process_group(0);
    let mut leader = c.spawn().unwrap();
    let pgid = leader.id();
    leader.wait().unwrap();
    assert!(runner::group_alive(pgid), "the member `sleep 30` of group {pgid} lives");
    let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pgid}")]).status();
    let t0 = std::time::Instant::now();
    while runner::group_alive(pgid) && t0.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!runner::group_alive(pgid), "group {pgid} was killed");
}

/// MINOR 1: a plugin's grandchild dies with gatetrain too, not only its leader. The run happens on a thread of its own that ends with
/// an abandoned spawn, as a dead parent does (`PR_SET_PDEATHSIG` fires when the spawning thread ends).
#[test]
fn minor1_a_grandchild_of_a_plugin_dies_with_its_parent() {
    let t = tempfile::tempdir().unwrap();
    let late = t.path().join("late.txt");
    // TERM, INT and HUP ignored (the subshell and its sleep inherit that): only the watchdog's SIGKILL of the group ends them on
    // Linux, so the test fails if that path is gone (round 3: a forwarded TERM alone kept it green)
    let line = format!("trap '' TERM INT HUP; ( sleep 3; echo late >> '{}' ); true", late.to_string_lossy().replace('\\', "/"));
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", &line]);
            // the parent dies a moment later, once the shell has started its subshell (a parent that dies first kills the shell
            // before it forks, which proves nothing)
            let mut die = |_pid: u32| -> Result<(), String> {
                std::thread::sleep(std::time::Duration::from_millis(700));
                Err("the parent died".into())
            };
            let r = runner::run_isolated_with(c, 60.0, runner::Opts { survives_parent: false, on_spawn: Some(&mut die), ..Default::default() });
            assert!(r.is_err());
        });
    });
    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(!late.exists(), "the grandchild outlived its parent");
}

// ---------------------------------------------------------------- the watchdog (enable-readiness round 2, MINORS 4 and 5; Linux)

#[cfg(target_os = "linux")]
fn sh(line: &str) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.args(["-c", line]);
    c
}

#[cfg(target_os = "linux")]
fn bash(line: &str) -> std::process::Command {
    let mut c = std::process::Command::new("bash");
    c.args(["-c", line]);
    c
}

#[cfg(target_os = "linux")]
fn run_watched(c: std::process::Command) -> runner::IsolatedRaw {
    runner::run_isolated_with(c, 120.0, runner::Opts { survives_parent: false, ..Default::default() }).unwrap()
}

#[cfg(target_os = "linux")]
fn rc_out(r: &runner::IsolatedRaw) -> (i32, String) {
    match r {
        runner::IsolatedRaw::Done { rc, stdout, .. } => (*rc, String::from_utf8_lossy(stdout).trim().to_string()),
        runner::IsolatedRaw::TimedOut => (-999, "timed out".into()),
    }
}

/// MINOR 4: a plugin that signals its own process group (HUP, USR1, USR2, ALRM, PIPE: `kill -HUP 0`, the reviewer's repro) does not
/// kill its watchdog: the run waits for the plugin and returns the plugin's own exit code.
#[cfg(target_os = "linux")]
#[test]
fn minor4_signals_a_plugin_sends_its_group_leave_the_watchdog_standing_and_the_exit_code_right() {
    let t = tempfile::tempdir().unwrap();
    let done = t.path().join("done");
    let line = format!("trap '' HUP USR1 USR2 ALRM PIPE; for s in HUP USR1 USR2 ALRM PIPE; do kill -$s 0; done; sleep 1; echo ok > '{}'; exit 7", done.display());
    let r = run_watched(sh(&line));
    assert_eq!(rc_out(&r).0, 7, "the plugin's own exit code: {r:?}");
    assert!(done.exists(), "the run returned before the plugin ended");
    // the plain repro
    let r = run_watched(sh("trap '' HUP; kill -HUP 0; sleep 1; exit 3"));
    assert_eq!(rc_out(&r).0, 3, "{r:?}");
}

/// MINOR 4: a plugin that ends by a signal reads as 128 + the signal, as a shell reports it; a plain exit code passes through.
#[cfg(target_os = "linux")]
#[test]
fn minor4_the_watchdog_returns_the_plugins_exit_code_or_128_plus_its_signal() {
    assert_eq!(rc_out(&run_watched(sh("exit 0"))).0, 0);
    assert_eq!(rc_out(&run_watched(sh("exit 42"))).0, 42);
    assert_eq!(rc_out(&run_watched(sh("kill -TERM $$"))).0, 128 + 15);
    assert_eq!(rc_out(&run_watched(sh("kill -KILL $$"))).0, 128 + 9);
    // the `trap 'kill 0' EXIT` idioms of deploy scripts: the TERM they send their group on the way out is not taken for a parent's
    // death (nothing is SIGKILLed): bash ends by its own TERM, as it does without a watchdog (where the bare status reads -1, a signal)
    for line in ["trap 'kill 0' EXIT; echo work; exit 0", "trap 'trap - TERM; kill 0' EXIT; echo work; exit 0"] {
        let bare = runner::run_isolated_with(bash(line), 120.0, runner::Opts { survives_parent: true, ..Default::default() }).unwrap();
        assert_eq!(rc_out(&bare), (-1, "work".to_string()), "{line}: without a watchdog");
        assert_eq!(rc_out(&run_watched(bash(line))), (128 + 15, "work".to_string()), "{line}");
    }
}

/// MINOR 5: the plugin starts with SIGINT (and every other signal) at its default disposition, so a bash plugin's `trap ... INT`
/// fires (a background job of `sh` started with SIGINT and SIGQUIT ignored, which bash cannot trap).
#[cfg(target_os = "linux")]
#[test]
fn minor5_a_bash_plugins_int_trap_fires() {
    let r = run_watched(bash("trap 'echo trapped; exit 5' INT; kill -INT $$; sleep 5; exit 0"));
    assert_eq!(rc_out(&r), (5, "trapped".to_string()), "{r:?}");
    let r = run_watched(bash("trap 'exit 6' QUIT; kill -QUIT $$; sleep 5; exit 0"));
    assert_eq!(rc_out(&r).0, 6, "{r:?}");
}

/// MINOR 5: an INT or TERM sent to the watchdog from outside (a person's `kill <pid>`, `timeout -s INT`) is forwarded to the plugin's
/// group, so its trap runs and its exit code comes back; the watchdog does not die alone.
#[cfg(target_os = "linux")]
#[test]
fn minor5_int_and_term_sent_to_the_watchdog_reach_the_plugin() {
    for (sig, code) in [("INT", 6), ("TERM", 8)] {
        let t = tempfile::tempdir().unwrap();
        let ready = t.path().join("ready");
        let line = format!("trap 'exit {code}' {sig}; touch '{}'; while :; do sleep 0.1; done", ready.display());
        let leader = std::sync::Mutex::new(0u32);
        let r = std::thread::scope(|s| {
            let runner = s.spawn(|| {
                let mut seen = |pid: u32| -> Result<(), String> {
                    *leader.lock().unwrap() = pid;
                    Ok(())
                };
                runner::run_isolated_with(bash(&line), 60.0, runner::Opts { survives_parent: false, on_start: Some(&mut seen), ..Default::default() }).unwrap()
            });
            let t0 = std::time::Instant::now();
            while !ready.exists() && t0.elapsed() < std::time::Duration::from_secs(30) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let pid = *leader.lock().unwrap();
            assert!(ready.exists() && pid > 0, "the plugin started");
            let _ = std::process::Command::new("kill").args([&format!("-{sig}"), &pid.to_string()]).status();
            let r = runner.join().unwrap();
            let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
            r
        });
        assert_eq!(rc_out(&r).0, code, "{sig}: {r:?}");
    }
}

/// The watchdog still kills the whole group when its parent dies, even when the plugin ignores TERM and INT.
#[cfg(target_os = "linux")]
#[test]
fn a_plugin_that_ignores_term_and_int_still_dies_with_its_parent() {
    let t = tempfile::tempdir().unwrap();
    let late = t.path().join("late.txt");
    let ready = t.path().join("ready");
    let line = format!("trap '' TERM INT HUP; touch '{}'; sleep 3; echo late >> '{}'", ready.display(), late.display());
    std::thread::scope(|s| {
        s.spawn(|| {
            let ready = ready.clone();
            let mut die = move |_pid: u32| -> Result<(), String> {
                let t0 = std::time::Instant::now();
                while !ready.exists() && t0.elapsed() < std::time::Duration::from_secs(30) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err("the parent died".into())
            };
            assert!(runner::run_isolated_with(sh(&line), 60.0, runner::Opts { survives_parent: false, on_spawn: Some(&mut die), ..Default::default() }).is_err());
        });
    });
    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(!late.exists(), "the plugin outlived its parent");
}

/// MINOR 7 (round 2, Windows): a plugin that cannot be put in its job object would run without kill-on-close and without a job to end
/// its tree at a timeout. That is a failure of the step: the process, still suspended, is ended and never runs its command.
#[cfg(windows)]
#[test]
fn minor7_a_job_assignment_that_fails_fails_the_step_and_nothing_runs() {
    let t = tempfile::tempdir().unwrap();
    let marker = t.path().join("ran.txt");
    for survives_parent in [false, true] {
        let mut c = std::process::Command::new(child());
        c.args(["marker", &marker.to_string_lossy(), "0"]);
        runner::job_seam::fail_next_assign();
        let r = runner::run_isolated_with(c, 30.0, runner::Opts { survives_parent, ..Default::default() });
        std::thread::sleep(std::time::Duration::from_millis(500));
        match r {
            Err(why) => assert!(why.contains("job object"), "{why}"),
            Ok(o) => panic!("survives_parent {survives_parent}: the plugin ran outside its job: {o:?}"),
        }
        assert!(!marker.exists(), "survives_parent {survives_parent}: the plugin ran its command");
    }
}

/// Round 3, A: the watchdog takes every catchable signal the plugin may send its group, not a list: IO, PWR, PROF, a real-time
/// signal, XCPU, ABRT and WINCH sent with `kill -X 0` leave it standing, the run waits for the plugin and returns its exit code.
#[cfg(target_os = "linux")]
#[test]
fn round3_a_every_signal_a_plugin_sends_its_group_leaves_the_watchdog_standing() {
    let mut died = vec![];
    for sig in ["IO", "PWR", "PROF", "RTMIN", "RTMAX", "XCPU", "XFSZ", "SYS", "TRAP", "STKFLT", "ABRT", "WINCH"] {
        let r = run_watched(bash(&format!("trap '' {sig}; kill -{sig} 0; sleep 1.5; exit 0")));
        if rc_out(&r).0 != 0 {
            died.push(format!("SIG{sig} rc {}", rc_out(&r).0));
        }
    }
    assert!(died.is_empty(), "the watchdog died alone on: {died:?}");
}

/// Round 3, C: a TERM a member of the plugin's group sends the group (`kill -TERM 0`) reaches the plugin once. The watchdog gets it
/// too, from a sender inside the group, and does not forward it a second time (terraform and docker compose read a second interrupt
/// as a force-quit).
#[cfg(target_os = "linux")]
#[test]
fn round3_c_a_term_a_member_sends_its_group_runs_the_plugins_trap_once() {
    let t = tempfile::tempdir().unwrap();
    let hits = t.path().join("hits");
    let line = format!("trap 'echo hit >> \"{}\"' TERM; kill -TERM 0; for i in 1 2 3 4 5 6 7 8 9 10; do sleep 0.1; done; exit 0", hits.display());
    let r = run_watched(bash(&line));
    assert_eq!(rc_out(&r).0, 0, "{r:?}");
    assert_eq!(std::fs::read_to_string(&hits).unwrap_or_default().lines().count(), 1, "the trap ran once");
}

/// Round 3, D: where `close_range` is missing, the watchdog closes every descriptor up to the soft `RLIMIT_NOFILE`, not up to 4095:
/// a descriptor above 4095 (std's exec-handshake pipe can be one) is not held by the watchdog while the plugin runs.
#[cfg(target_os = "linux")]
#[test]
fn round3_d_the_close_loop_reaches_the_descriptor_limit() {
    const HIGH: i32 = 5000;
    // SAFETY: plain syscalls on this test process; the descriptor is ours and closed at the end
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl), 0);
        if rl.rlim_max != libc::RLIM_INFINITY && rl.rlim_max <= HIGH as libc::rlim_t {
            eprintln!("skipped: the hard RLIMIT_NOFILE {} is below {HIGH}", rl.rlim_max);
            return;
        }
        if rl.rlim_cur <= HIGH as libc::rlim_t {
            rl.rlim_cur = (HIGH as libc::rlim_t + 1000).min(rl.rlim_max);
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &rl), 0);
        }
        let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        assert!(fd >= 0);
        assert_eq!(libc::dup3(fd, HIGH, libc::O_CLOEXEC), HIGH);
        libc::close(fd);
    }
    let t = tempfile::tempdir().unwrap();
    let ready = t.path().join("ready");
    let line = format!("touch '{}'; sleep 3", ready.display());
    let leader = std::sync::Mutex::new(0u32);
    runner::fd_seam::force_close_loop(true);
    let held = std::thread::scope(|s| {
        let run = s.spawn(|| {
            let mut seen = |pid: u32| -> Result<(), String> {
                *leader.lock().unwrap() = pid;
                Ok(())
            };
            runner::run_isolated_with(bash(&line), 60.0, runner::Opts { survives_parent: false, on_start: Some(&mut seen), ..Default::default() }).unwrap()
        });
        let t0 = std::time::Instant::now();
        while !ready.exists() && t0.elapsed() < std::time::Duration::from_secs(30) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let pid = *leader.lock().unwrap();
        let held = std::path::Path::new(&format!("/proc/{pid}/fd/{HIGH}")).exists();
        run.join().unwrap();
        held
    });
    runner::fd_seam::force_close_loop(false);
    // SAFETY: our own descriptor
    unsafe {
        libc::close(HIGH);
    }
    assert!(!held, "the watchdog still holds descriptor {HIGH}");
}
