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
    let line = format!("( sleep 3; echo late >> '{}' ); true", late.to_string_lossy().replace('\\', "/"));
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
            let r = runner::run_isolated_with(c, 60.0, runner::Opts { survives_parent: false, on_spawn: Some(&mut die) });
            assert!(r.is_err());
        });
    });
    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(!late.exists(), "the grandchild outlived its parent");
}
