//! Check and tidy against a real file system, real git and the real local shell: temp directories and temp git repositories.
//! The junction (Windows) and symlink (Unix) tests prove that a link inside a removed folder is unlinked first and its
//! target's files survive (L12, L44).
mod common;
use common::to_table;
use gatetrain_core::tidy::apply::apply_plan;
use gatetrain_core::config::{Config, Host};
use gatetrain_core::tidy::config::build_cfg;
use gatetrain_core::tidy::inventory::{gather, Extra};
use gatetrain_core::tidy::{Fatal, LocalRunner, Ops, RunOut, Runner};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn fwd(p: &Path) -> String {
    let s = p.canonicalize().unwrap().to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").map(String::from).unwrap_or(s)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write(p: impl AsRef<Path>, text: &str) {
    if let Some(d) = p.as_ref().parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, text).unwrap();
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("gt-real-").tempdir().unwrap()
}

/// A Windows host must prove its link listing works: a folder holding one known junction (the canary).
fn canary(tidy: &mut Value) {
    if !cfg!(windows) {
        return;
    }
    let d = tmp();
    let (target, c) = (d.path().join("target"), d.path().join("canary"));
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&c).unwrap();
    dir_link(&c.join("l"), &target);
    let o = tidy.as_object_mut().unwrap();
    o.insert("link_canary".into(), json!(fwd(&c)));
    o.insert("link_canary_count".into(), json!(1));
    // dropped when the test's thread ends: nothing is left in the temp folder
    CANARIES.with(|c| c.borrow_mut().push(d));
}

thread_local! {
    static CANARIES: std::cell::RefCell<Vec<tempfile::TempDir>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn real_host(mut tidy: Value) -> (Config, Host) {
    canary(&mut tidy);
    let h = Host { name: "real".into(), tidy: to_table(&tidy), ..Default::default() };
    (Config { hosts: vec![h.clone()], ..Default::default() }, h)
}

/// A main checkout with one tracked file in `shared/` and a `.gitignore` for the link names.
fn main_repo(root: &Path) -> PathBuf {
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    write(main.join("shared/keep.txt"), "the shared file\n");
    write(main.join("f.txt"), "one\n");
    write(main.join(".gitignore"), "jlink\nlink\n");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "base"]);
    main
}

fn add_worktree(main: &Path, wt: &Path, branch: &str) {
    git(main, &["worktree", "add", "-q", "-b", branch, &wt.to_string_lossy()]);
}

fn tracked(main: &Path) -> (usize, String) {
    (git(main, &["ls-files"]).lines().count(), git(main, &["status", "--porcelain"]))
}

#[test]
fn merged_by_content_against_real_git() {
    let d = tmp();
    let p = d.path();
    git(p, &["init", "-q", "-b", "main"]);
    write(p.join("f.txt"), "one\n");
    git(p, &["add", "f.txt"]);
    git(p, &["commit", "-q", "-m", "base"]);
    git(p, &["checkout", "-q", "-b", "lane-x"]);
    write(p.join("f.txt"), "one\ntwo\nthree\n");
    git(p, &["commit", "-q", "-am", "x"]);
    git(p, &["checkout", "-q", "-b", "lane-y", "main"]);
    write(p.join("f.txt"), "one\nfour\n");
    git(p, &["commit", "-q", "-am", "y"]);
    git(p, &["checkout", "-q", "main"]);
    write(p.join("f.txt"), "one\ntwo\nthree\nfive\n");
    git(p, &["commit", "-q", "-am", "squash of x, plus more"]);
    let (cfg, h) = real_host(json!({"main_checkout": fwd(p)}));
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &LocalRunner, &t);
    assert_eq!(ops.merged("lane-x").unwrap(), Some(false)); // not an ancestor, and merging it conflicts with main: never "merged by content"
    assert_eq!(ops.merged("lane-y").unwrap(), Some(false));
    assert_eq!(ops.merged("main").unwrap(), Some(true));
}

#[test]
fn merge_proof_real_git_cases() {
    // merged means: merging the branch into main changes nothing (git merge-tree --write-tree, git 2.38+)
    let d = tmp();
    let p = d.path();
    git(p, &["init", "-q", "-b", "main"]);
    write(p.join("f.txt"), "a
}
");
    write(p.join("g.txt"), "keep
");
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "base"]);
    git(p, &["checkout", "-q", "-b", "dup-line"]);
    write(p.join("f.txt"), "a
}
}
");
    git(p, &["commit", "-q", "-am", "adds a closing brace"]);
    git(p, &["checkout", "-q", "-b", "same-change", "main"]);
    write(p.join("g.txt"), "keep
more
");
    git(p, &["commit", "-q", "-am", "x"]);
    git(p, &["checkout", "-q", "-b", "deletion", "main"]);
    std::fs::remove_file(p.join("g.txt")).unwrap();
    git(p, &["commit", "-q", "-am", "drop g"]);
    git(p, &["checkout", "-q", "main"]);
    write(p.join("g.txt"), "keep
more
");
    git(p, &["commit", "-q", "-am", "same change made separately"]);
    let (cfg, h) = real_host(json!({"main_checkout": fwd(p)}));
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &LocalRunner, &t);
    assert_eq!(ops.merged("dup-line").unwrap(), Some(false)); // every added line "exists" in main, yet the change is not there
    assert_eq!(ops.merged("same-change").unwrap(), Some(true)); // not an ancestor, but merging it changes nothing
    assert_eq!(ops.merged("deletion").unwrap(), Some(false));
    assert_eq!(ops.merged("main").unwrap(), Some(true));
}

#[test]
fn a_taken_archive_tag_name_gets_a_suffix_and_the_branch_goes_by_sha() {
    let d = tmp();
    let root = d.path();
    let main = main_repo(root);
    let wt = root.join("wt/lane-t");
    add_worktree(&main, &wt, "line/lane-t");
    write(wt.join("g.txt"), "2
");
    git(&wt, &["add", "g.txt"]);
    git(&wt, &["commit", "-q", "-m", "lane"]);
    git(&main, &["merge", "-q", "--no-edit", "line/lane-t"]);
    let sha = git(&main, &["rev-parse", "line/lane-t"]).trim().to_string();
    let other = git(&main, &["rev-parse", "main~1"]).trim().to_string();
    git(&main, &["tag", "archive/line/lane-t", &other]); // the name is taken by another commit
    let (cfg, h) = real_host(json!({"main_checkout": fwd(&main), "log": fwd(root) + "/tidy.jsonl"}));
    let t = build_cfg(&cfg, &h).unwrap();
    let inv = gather(&h, &LocalRunner, &t, None).unwrap();
    assert_eq!(inv.items.len(), 1);
    let (done, failed) = apply_plan(&h, &LocalRunner, &t, &inv, &["worktrees".to_string()], None, None, false).unwrap();
    assert_eq!((done, failed), (1, 0));
    assert!(!wt.exists());
    assert!(!git(&main, &["branch", "--list"]).contains("line/lane-t"));
    assert_eq!(git(&main, &["rev-parse", &format!("archive/line/lane-t-{}", &sha[..8])]).trim(), sha);
    assert_eq!(git(&main, &["rev-parse", "archive/line/lane-t"]).trim(), other, "the old tag is untouched");
}

#[cfg(windows)]
#[test]
fn a_real_junction_is_a_link_and_removed_as_one() {
    let d = tmp();
    let root = d.path();
    let main = main_repo(root);
    let trash = root.join("trash");
    std::fs::create_dir_all(&trash).unwrap();
    let j = trash.join("j");
    dir_link(&j, &main.join("shared"));
    let (cfg, h) = real_host(json!({"main_checkout": fwd(&main), "trash_dir": fwd(&trash)}));
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &LocalRunner, &t);
    let jp = format!("{}/j", fwd(&trash)); // not canonicalised: that would follow the junction
    assert_eq!(ops.is_link(&jp).unwrap(), Some(true));
    assert_eq!(ops.is_link(&fwd(&main)).unwrap(), Some(false));
    assert_eq!(ops.remove_dir(&jp).unwrap().0, "ok");
    assert!(!j.exists());
    assert!(main.join("shared/keep.txt").exists());
}

#[test]
fn a_real_merged_clean_worktree_is_tagged_then_removed_and_main_is_untouched() {
    let d = tmp();
    let root = d.path();
    let main = main_repo(root);
    let (a, b, c) = (root.join("wt/lane-a"), root.join("wt/lane-b"), root.join("wt/lane-c"));
    add_worktree(&main, &a, "lane-a"); // merged: same commit as main
    add_worktree(&main, &b, "lane-b");
    write(b.join("extra.txt"), "work not on main\n");
    git(&b, &["add", "extra.txt"]);
    git(&b, &["commit", "-q", "-m", "unmerged work"]);
    add_worktree(&main, &c, "lane-c"); // merged but dirty
    write(c.join("scratch.txt"), "uncommitted\n");
    let before = tracked(&main);
    let log = root.join("tidy.jsonl");
    let (cfg, h) = real_host(json!({"main_checkout": fwd(&main), "root_checkout": fwd(root) + "/root", "log": fwd(root) + "/tidy.jsonl"}));
    let t = build_cfg(&cfg, &h).unwrap();
    let inv = gather(&h, &LocalRunner, &t, None).unwrap();
    let keep = |p: &Path| inv.worktrees.iter().find(|w| w.path.to_lowercase() == fwd(p).to_lowercase()).unwrap().keep.clone();
    assert_eq!(keep(&a), None);
    assert_eq!(keep(&b).as_deref(), Some("not merged"));
    assert_eq!(keep(&c).as_deref(), Some("uncommitted work"));
    assert_eq!(inv.items.len(), 1);
    // gathering changed nothing
    assert!(a.exists() && b.exists() && c.exists() && !log.exists());
    let (done, failed) = apply_plan(&h, &LocalRunner, &t, &inv, &["worktrees".to_string()], None, None, false).unwrap();
    assert_eq!((done, failed), (1, 0));
    assert!(!a.exists(), "the merged clean worktree is gone");
    assert!(b.exists() && c.exists(), "the unmerged and the dirty worktree stay");
    assert!(git(&main, &["tag", "--list", "archive/*"]).contains("archive/lane-a"), "archive tag first (L43)");
    let branches = git(&main, &["branch", "--list"]);
    assert!(!branches.contains("lane-a") && branches.contains("lane-b") && branches.contains("lane-c"));
    assert_eq!(tracked(&main), before, "the main checkout is unchanged");
    assert!(main.join("shared/keep.txt").exists());
    assert!(std::fs::read_to_string(&log).unwrap().contains("\"status\": \"ok\""));
}

#[test]
fn the_main_checkout_and_its_parent_are_never_removed_on_a_real_disk() {
    let d = tmp();
    let main = main_repo(d.path());
    let (cfg, h) = real_host(json!({"main_checkout": fwd(&main), "root_checkout": fwd(d.path()) + "/root"}));
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &LocalRunner, &t);
    for p in [fwd(&main), fwd(d.path()), fwd(d.path()) + "/root"] {
        assert!(matches!(ops.remove_dir(&p), Err(Fatal::Guard(_))), "{p}");
    }
    assert!(main.join("shared/keep.txt").exists() && main.join(".git").exists());
}

/// Make a link to a directory: a junction on Windows (`mklink /J`, no privilege needed), a symlink elsewhere.
fn dir_link(link: &Path, target: &Path) {
    if cfg!(windows) {
        let o = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link.to_string_lossy().replace('/', "\\")).arg(target.to_string_lossy().replace('/', "\\")).output().unwrap();
        assert!(o.status.success(), "mklink /J failed: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    } else {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

#[test]
fn a_junction_inside_a_removed_worktree_is_unlinked_and_never_followed_into_its_target() {
    let d = tmp();
    let root = d.path();
    let main = main_repo(root);
    let wt = root.join("wt/lane-j");
    add_worktree(&main, &wt, "lane-j");
    dir_link(&wt.join("jlink"), &main.join("shared")); // a link into the main checkout, like a node_modules junction (L44)
    assert!(wt.join("jlink/keep.txt").exists(), "the link works: it reaches main's file");
    let before = tracked(&main);
    let (cfg, h) = real_host(json!({"main_checkout": fwd(&main), "log": fwd(root) + "/tidy.jsonl"}));
    let t = build_cfg(&cfg, &h).unwrap();
    // the listing sees the link
    let ops = Ops::new(&h, &LocalRunner, &t);
    let links = ops.list_links(&fwd(&wt)).unwrap();
    assert_eq!(links.len(), 1, "{links:?}");
    assert!(links[0].replace('\\', "/").ends_with("lane-j/jlink"));
    let inv = gather(&h, &LocalRunner, &t, None).unwrap();
    let it = inv.items.iter().find(|i| i.key.to_lowercase() == fwd(&wt).to_lowercase()).expect("the clean merged worktree is planned");
    assert!(matches!(&it.extra, Extra::Worktree { links: 1, .. }));
    let (done, failed) = apply_plan(&h, &LocalRunner, &t, &inv, &["worktrees".to_string()], None, None, false).unwrap();
    assert_eq!((done, failed), (1, 0));
    assert!(!wt.exists(), "the worktree is removed");
    // the target's files survive
    assert_eq!(std::fs::read_to_string(main.join("shared/keep.txt")).unwrap(), "the shared file\n");
    assert_eq!(tracked(&main), before);
    assert!(git(&main, &["tag", "--list"]).contains("archive/lane-j"));
}

#[test]
fn a_junction_inside_a_recycle_bin_entry_is_unlinked_and_its_target_survives() {
    let d = tmp();
    let root = d.path();
    let main = main_repo(root);
    let outside = root.join("outside");
    write(outside.join("precious.txt"), "do not delete\n");
    write(outside.join("sub/deep.txt"), "nor this\n");
    let entry = root.join("trash/old-entry");
    write(entry.join("junk.bin"), "junk\n");
    dir_link(&entry.join("link"), &outside);
    let (cfg, h) = real_host(json!({"main_checkout": fwd(&main), "trash_dir": fwd(&root.join("trash"))}));
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &LocalRunner, &t);
    ops.remove_dir(&fwd(&entry)).unwrap();
    assert!(!entry.exists());
    assert_eq!(std::fs::read_to_string(outside.join("precious.txt")).unwrap(), "do not delete\n");
    assert_eq!(std::fs::read_to_string(outside.join("sub/deep.txt")).unwrap(), "nor this\n");
}

#[test]
fn a_command_that_outlives_its_timeout_is_cut_off_and_reported_timed_out() {
    let cmd = if cfg!(windows) { "ping -n 8 127.0.0.1 >nul" } else { "sleep 8" };
    let t0 = Instant::now();
    let r = LocalRunner.run_for(cmd, Some(0.4)).unwrap();
    assert_eq!(r, RunOut::TimedOut);
    assert!(t0.elapsed().as_secs_f64() < 6.0, "returned in {:?}", t0.elapsed());
}

#[test]
fn sizing_is_best_effort_a_timeout_is_unknown_not_zero() {
    let d = tmp();
    write(d.path().join("x.bin"), "12345");
    let (cfg, h) = real_host(json!({"size_timeout_s": 0.0}));
    let t = build_cfg(&cfg, &h).unwrap();
    assert_eq!(Ops::new(&h, &LocalRunner, &t).size(&fwd(d.path())).unwrap(), None);
    let (cfg, h) = real_host(json!({"size_timeout_s": 60}));
    let t = build_cfg(&cfg, &h).unwrap();
    let n = Ops::new(&h, &LocalRunner, &t).size(&fwd(d.path())).unwrap().expect("a real size");
    assert!(n >= 5, "{n}");
}
