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
