//! The local runner's timeout ends the call even when the command's own child keeps the output pipe open.
//!
//! On Windows a shell that is killed on a timeout leaves a grandchild (the PowerShell that sizes a folder) holding the pipes, and
//! a runner that waits for them never times out: a dry run then waits for the whole recursive walk. This is why `tidy` on a
//! workstation with large worktrees once printed nothing for 300 s. (Ported from tests/test_runner_timeout.py.)
//! The child is crates/gatetrain-testchild; run these with `cargo test --workspace` so it is built.
use gatetrain_core::runner::{LocalRunner, RunOut, Runner};
use std::path::PathBuf;
use std::time::Instant;

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap(); // target/<profile>/deps/<test> -> target/<profile>
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

/// A shell line: the child started by the shell, with a trailing command so `sh` does not exec it in place.
fn shell_line(mode_args: &str) -> String {
    let line = format!("\"{}\" {mode_args}", child());
    if cfg!(windows) {
        line
    } else {
        format!("{line}; true")
    }
}

#[test]
fn a_shell_command_whose_child_holds_the_pipe_times_out_on_time() {
    let t0 = Instant::now();
    let r = LocalRunner.run_for(&shell_line("grandchild 8"), Some(1.0)).unwrap();
    let took = t0.elapsed().as_secs_f64();
    assert_eq!(r, RunOut::TimedOut);
    assert!(took < 5.0, "the 1 s timeout took {took:.1} s: the runner waited for the grandchild");
}

#[test]
fn an_argv_command_times_out_on_time() {
    let t0 = Instant::now();
    let r = LocalRunner.run_argv_to(&[child(), "grandchild".into(), "8".into()], Some(1.0)).unwrap();
    assert_eq!(r, RunOut::TimedOut);
    assert!(t0.elapsed().as_secs_f64() < 5.0);
}

#[test]
fn output_and_return_code_are_unchanged() {
    let r = LocalRunner.run_for(&format!("\"{}\" print 42", child()), Some(30.0)).unwrap();
    assert_eq!(r, RunOut::done(0, "42\n"));
    let r = LocalRunner.run_argv_to(&[child(), "exit".into(), "3".into()], Some(30.0)).unwrap();
    assert_eq!(r, RunOut::done(3, ""));
    let r = LocalRunner.run_argv_to(&[child(), "args".into(), "a&b|c;d$(e)".into()], Some(30.0)).unwrap();
    assert_eq!(r, RunOut::done(0, "a&b|c;d$(e)\n"));
}
