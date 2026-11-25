//! `gatetrain tidy --force`: the order of archive then removal, the STOP rules, and what always wins (the main checkout, the keep list,
//! the protect list). Every effect is a stub that records; nothing here touches a disk.
use gatetrain_core::force::tidyforce::*;
use gatetrain_core::tidy::apply::cargo_clean_argv;
use gatetrain_core::tidy::config::TidyCfg;
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashSet;

fn wt(name: &str, dirty: &[&str]) -> WtState {
    WtState { path: format!("F:/repos/proj-wt-{name}"), name: format!("proj-wt-{name}"), branch: Some(name.into()), head: format!("head-{name}"), locked: false, dirty: dirty.iter().map(|s| s.to_string()).collect(), ignored: vec![], ignored_skipped: vec![], is_main: false }
}

#[derive(Default)]
struct Stub {
    wts: Vec<WtState>,
    on_remote: HashSet<String>,
    protected: Vec<String>,
    fail_push: HashSet<String>,
    fail_commit: HashSet<String>,
    fail_contains: bool,
    fail_remove: HashSet<String>,
    sweep: Vec<SweepItem>,
    fail_sweep: bool,
    calls: RefCell<Vec<String>>,
}

impl Stub {
    fn log(&self, s: String) {
        self.calls.borrow_mut().push(s);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
    fn mutated(&self) -> Vec<String> {
        self.calls().into_iter().filter(|c| ["commit ", "push ", "remove ", "sweep "].iter().any(|p| c.starts_with(p))).collect()
    }
}

impl ForceHost for Stub {
    fn fetch(&self) -> Result<(), String> {
        self.log("fetch".into());
        Ok(())
    }
    fn list(&self) -> Result<Vec<WtState>, String> {
        Ok(self.wts.clone())
    }
    fn kept_by_config(&self, path: &str) -> Option<String> {
        self.protected.iter().find(|p| path.starts_with(p.as_str())).map(|p| format!("protected by {p}"))
    }
    fn remote_contains(&self, head: &str) -> Result<bool, String> {
        if self.fail_contains {
            return Err("cannot read the remote refs".into());
        }
        Ok(self.on_remote.contains(head))
    }
    fn commit_paths(&self, w: &WtState, msg: &str) -> Result<String, String> {
        self.log(format!("commit {} {} paths [{}]", w.name, w.dirty.len(), msg.chars().take(12).collect::<String>()));
        if self.fail_commit.contains(&w.name) {
            return Err("hook refused the commit".into());
        }
        Ok(format!("wip-{}", w.name))
    }
    fn push_archive(&self, w: &WtState, head: &str, r: &str) -> Result<(), String> {
        self.log(format!("push {} {head} -> {r}", w.name));
        if self.fail_push.contains(&w.name) {
            return Err("remote refused".into());
        }
        Ok(())
    }
    fn remove(&self, w: &WtState) -> Result<String, String> {
        self.log(format!("remove {}", w.name));
        if self.fail_remove.contains(&w.name) {
            return Err("main checkout changed".into());
        }
        Ok("removed".into())
    }
    fn sweep_plan(&self) -> Result<Vec<SweepItem>, String> {
        Ok(self.sweep.clone())
    }
    fn sweep_apply(&self, it: &SweepItem) -> Result<String, String> {
        self.log(format!("sweep {} {}", it.class, it.key));
        if self.fail_sweep {
            return Err("cargo clean failed".into());
        }
        Ok("cleaned".into())
    }
}

fn main_wt() -> WtState {
    WtState { path: "F:/repos/proj".into(), name: "proj".into(), branch: Some("main".into()), head: "head-main".into(), locked: false, dirty: vec![], ignored: vec![], ignored_skipped: vec![], is_main: true }
}

fn base() -> Stub {
    Stub { wts: vec![main_wt(), wt("a", &[]), wt("b", &["x.txt", "dir/"]), wt("live1", &[]), wt("c", &[])], on_remote: ["head-a".to_string()].into(), ..Default::default() }
}

const DATE: &str = "2026-11-25";

fn keep(r: &str) -> Option<Regex> {
    Some(Regex::new(r).unwrap())
}

#[test]
fn a_dry_run_prints_the_plan_and_changes_nothing() {
    let h = Stub { sweep: vec![SweepItem { class: "targets".into(), key: "F:/build/t1".into(), bytes: Some(5), note: "finished".into() }], ..base() };
    let o = run(&h, keep("live").as_ref(), false, DATE);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(h.mutated().is_empty(), "{:?}", h.mutated());
    for w in ["proj-wt-a", "proj-wt-b", "main checkout", "kept (--keep", "F:/build/t1", "dry run", "archive/proj-wt-b"] {
        assert!(o.text.contains(w), "{w} missing from\n{}", o.text);
    }
    assert!(o.text.contains("already on the remote") && o.text.contains("2 uncommitted"), "{}", o.text);
}

#[test]
fn apply_archives_then_removes_in_that_order_and_skips_what_must_stay() {
    let h = base();
    let o = run(&h, keep("live").as_ref(), true, DATE);
    assert_eq!(o.rc, 0, "{}", o.text);
    let c = h.mutated();
    // a: already on the remote, so no push; b: dirty, so commit, push, remove; c: push, remove
    assert_eq!(
        c.iter().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "remove proj-wt-a",
            "commit proj-wt-b 2 paths [wip(archive)]",
            "push proj-wt-b wip-proj-wt-b -> archive/proj-wt-b",
            "remove proj-wt-b",
            "push proj-wt-c head-c -> archive/proj-wt-c",
            "remove proj-wt-c",
        ]
    );
}

#[test]
fn the_main_checkout_and_the_protected_and_locked_always_win() {
    let mut h = base();
    h.protected = vec!["F:/repos/proj-wt-c".into()];
    h.wts[1].locked = true;
    let o = run(&h, None, true, DATE);
    let removed: Vec<String> = h.mutated().into_iter().filter(|c| c.starts_with("remove ")).collect();
    assert_eq!(removed.iter().map(String::as_str).collect::<Vec<_>>(), vec!["remove proj-wt-b", "remove proj-wt-live1"]);
    assert!(o.text.contains("protected by F:/repos/proj-wt-c") && o.text.contains("locked"), "{}", o.text);
    // even a --keep regex that matches nothing, or an empty stub list, never lets the main checkout through
    let h = Stub { wts: vec![main_wt()], ..Default::default() };
    run(&h, keep("^$").as_ref(), true, DATE);
    assert!(h.mutated().is_empty());
}

#[test]
fn a_push_that_fails_stops_everything_with_that_worktree_intact() {
    let mut h = base();
    h.fail_push = ["proj-wt-b".to_string()].into();
    let o = run(&h, None, true, DATE);
    assert_eq!(o.rc, 6, "{}", o.text);
    assert!(o.text.contains("STOP proj-wt-b"), "{}", o.text);
    let c = h.mutated();
    assert!(!c.iter().any(|x| x == "remove proj-wt-b"), "removed despite an unproven archive: {c:?}");
    assert!(!c.iter().any(|x| x.contains("proj-wt-c") || x.contains("live1")), "carried on after a STOP: {c:?}");
    assert!(!c.iter().any(|x| x.starts_with("sweep ")));
}

#[test]
fn a_commit_that_fails_or_an_unreadable_remote_also_stops() {
    let mut h = base();
    h.fail_commit = ["proj-wt-b".to_string()].into();
    let o = run(&h, None, true, DATE);
    assert_eq!(o.rc, 6);
    assert!(!h.mutated().iter().any(|x| x == "remove proj-wt-b" || x.starts_with("push proj-wt-b")));
    let h = Stub { fail_contains: true, ..base() };
    let o = run(&h, None, true, DATE);
    assert_eq!(o.rc, 6, "{}", o.text);
    assert!(h.mutated().is_empty(), "nothing may be removed when the remote cannot be proven: {:?}", h.mutated());
}

#[test]
fn a_main_guard_failure_after_a_removal_stops_the_run() {
    let mut h = base();
    h.fail_remove = ["proj-wt-a".to_string()].into();
    let o = run(&h, None, true, DATE);
    assert_eq!(o.rc, 6, "{}", o.text);
    assert_eq!(h.mutated().iter().map(String::as_str).collect::<Vec<_>>(), vec!["remove proj-wt-a"]);
}

#[test]
fn finished_targets_and_containers_are_swept_after_the_worktrees() {
    let items = vec![SweepItem { class: "targets".into(), key: "F:/build/t1".into(), bytes: Some(80), note: "finished".into() }, SweepItem { class: "containers".into(), key: "mr-gate-1".into(), bytes: None, note: "exited".into() }];
    let h = Stub { sweep: items, ..base() };
    let o = run(&h, None, true, DATE);
    assert_eq!(o.rc, 0, "{}", o.text);
    let c = h.mutated();
    let last_remove = c.iter().rposition(|x| x.starts_with("remove ")).unwrap();
    assert_eq!(c[last_remove + 1..].iter().map(String::as_str).collect::<Vec<_>>(), ["sweep targets F:/build/t1", "sweep containers mr-gate-1"]);
    // a failed sweep item is reported and gives rc 1, but is not a STOP
    let h = Stub { fail_sweep: true, sweep: vec![SweepItem { class: "targets".into(), key: "F:/build/t1".into(), bytes: None, note: String::new() }], ..base() };
    let o = run(&h, None, true, DATE);
    assert_eq!(o.rc, 1, "{}", o.text);
}

#[test]
fn cargo_clean_runs_with_the_rustc_wrapper_unset_when_forced() {
    let mut t = TidyCfg::default();
    let plain = cargo_clean_argv(&t, Some("F:/repos/proj/Cargo.toml".into()), "F:/build/t1");
    assert_eq!(plain, ["cargo", "clean", "--manifest-path", "F:/repos/proj/Cargo.toml", "--target-dir", "F:/build/t1"]);
    t.cargo_unset_wrapper = true;
    let forced = cargo_clean_argv(&t, None, "F:/build/t1");
    assert_eq!(forced, ["cargo", "clean", "--config", "build.rustc-wrapper=''", "--target-dir", "F:/build/t1"]);
}

#[test]
fn skipped_build_output_is_printed_in_the_plan_never_silent() {
    use gatetrain_core::force::tidyforce::parse_ignored_split;
    let out = "!! node_modules/\0!! dist/x.js\0!! .env.local\0!! docs/notes.md\0?? other\0";
    let (blocking, skipped) = parse_ignored_split(out);
    assert_eq!(blocking, vec![".env.local".to_string(), "docs/notes.md".to_string()]);
    assert_eq!(skipped, vec!["node_modules".to_string(), "dist/x.js".to_string()]);
    let mut w = wt("a", &[]);
    w.ignored_skipped = skipped;
    let h = Stub { wts: vec![main_wt(), w], ..Default::default() };
    let o = run(&h, None, false, DATE);
    assert!(o.text.contains("build output") && o.text.contains("node_modules") && o.text.contains("not archived"), "{}", o.text);
}
