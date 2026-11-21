//! The process scan on the REAL shell (Linux): a fixture `/proc` with real symlinks, run by `sh`. Skipped where there is no `sh`/`stat`.
#![cfg(unix)]
use gatetrain_core::tidy::ops::{parse_holders, proc_scan_script};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

fn proc(root: &Path, pid: &str, state: &str, cwd: Option<&str>, fd: Option<&str>) {
    let d = root.join(pid);
    std::fs::create_dir_all(d.join("fd")).unwrap();
    std::fs::write(d.join("status"), format!("Name:\tx\nState:\t{state}\n")).unwrap();
    if let Some(c) = cwd {
        symlink(c, d.join("cwd")).unwrap();
    }
    if let Some(f) = fd {
        symlink(f, d.join("fd/3")).unwrap();
    }
}

fn scan(root: &Path, path: &str) -> Option<bool> {
    let o = Command::new("sh").arg("-c").arg(proc_scan_script(&root.to_string_lossy())).env("PATH", "/usr/bin:/bin").output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    parse_holders(&String::from_utf8_lossy(&o.stdout), path)
}

#[test]
fn the_real_scan_finds_a_cwd_an_open_file_and_reports_unreadable_as_unknown() {
    let d = tempfile::tempdir().unwrap();
    let r = d.path().join("proc");
    proc(&r, "1", "S (sleeping)", Some("/elsewhere"), None);
    assert_eq!(scan(&r, "/work/chain-1"), Some(false));
    proc(&r, "2", "S (sleeping)", Some("/work/chain-1/target"), None);
    assert_eq!(scan(&r, "/work/chain-1"), Some(true), "a cwd inside");
    std::fs::remove_dir_all(r.join("2")).unwrap();
    proc(&r, "3", "S (sleeping)", Some("/"), Some("/work/chain-1/lock"));
    assert_eq!(scan(&r, "/work/chain-1"), Some(true), "an open file inside");
    std::fs::remove_dir_all(r.join("3")).unwrap();
    proc(&r, "4", "S (sleeping)", None, None); // owned by us, cwd cannot be read
    assert_eq!(scan(&r, "/work/chain-1"), None, "unreadable is unknown, never free");
    proc(&r, "4", "Z (zombie)", None, None); // a zombie has no readable cwd and is skipped
    assert_eq!(scan(&r, "/work/chain-1"), Some(false), "a zombie is not an unknown");
}

/// The REAL `/proc` of this machine (not a fixture): the user's own `systemd --user` and `(sd-pam)` have an unreadable cwd there.
#[test]
fn the_real_proc_gives_a_definite_answer_for_a_scratch_folder() {
    if !Path::new("/proc/self/cwd").exists() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let path = d.path().canonicalize().unwrap();
    let ps = path.to_string_lossy().into_owned();
    assert_eq!(scan(Path::new("/proc"), &ps), Some(false), "no process is in the scratch folder");
    let mut child = Command::new("sleep").arg("30").current_dir(&path).spawn().unwrap();
    let r = scan(Path::new("/proc"), &ps);
    child.kill().unwrap();
    let _ = child.wait();
    assert_eq!(r, Some(true), "a sleep started inside it holds it");
}
