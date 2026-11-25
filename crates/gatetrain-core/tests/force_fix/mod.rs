//! Real git in throwaway folders (a bare origin, a clone, branches). Nothing here removes anything outside the temp folder.
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub fn sh_dated(dir: &Path, args: &[&str], unix: i64) -> String {
    let d = format!("{unix} +0000");
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_DATE", &d)
        .env("GIT_COMMITTER_DATE", &d)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub struct Repo {
    pub tmp: tempfile::TempDir,
    pub origin: PathBuf,
    pub work: PathBuf,
}

impl Repo {
    pub fn p(&self) -> String {
        self.work.to_string_lossy().into_owned()
    }
}

pub fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// origin (bare) + work (clone on main with one commit, pushed).
pub fn repo() -> Repo {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    sh(&origin, &["init", "--bare", "-b", "main"]);
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(&work, &["init", "-b", "main"]);
    sh(&work, &["config", "user.name", "T"]);
    sh(&work, &["config", "user.email", "t@example.invalid"]);
    sh(&work, &["config", "core.autocrlf", "false"]);
    sh(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
    write(&work, "README.md", "hello\n");
    sh(&work, &["add", "README.md"]);
    sh(&work, &["commit", "-m", "init"]);
    sh(&work, &["push", "-u", "origin", "main"]);
    Repo { tmp, origin, work }
}

/// A branch off main with one commit changing `files`; leaves main checked out.
pub fn branch(r: &Repo, name: &str, files: &[&str]) -> String {
    sh(&r.work, &["checkout", "-q", "-b", name, "main"]);
    for f in files {
        write(&r.work, f, &format!("{name} {f}\n"));
        sh(&r.work, &["add", f]);
    }
    sh(&r.work, &["commit", "-m", &format!("work on {name}")]);
    let tip = sh(&r.work, &["rev-parse", "HEAD"]);
    sh(&r.work, &["checkout", "-q", "main"]);
    tip
}
