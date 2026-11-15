//! The hardening spec of `check` / `tidy`, ported from tests/test_tidy_harden.py (lessons L60-L66): injection, deletion order, path
//! safety, merge proof, links, timeouts. Each group is one review finding. The fake host (`common::Sim`) models the real failure (an
//! unquoted name reaching a shell, a recursive remove that follows a link); the real-git and real-disk tests are in tidy_real.rs.
mod common;
use common::*;
use gatetrain_core::config::ConfigError;
use gatetrain_core::tidy::apply::apply_plan;
use gatetrain_core::tidy::config::{build_cfg, TidyCfg};
use gatetrain_core::tidy::inventory::{gather, Extra, Inventory, Item};
use gatetrain_core::tidy::ops::{norm, safe_path};
use gatetrain_core::tidy::{Fatal, Ops, RunOut, Runner};
use serde_json::{json, Value};
use std::path::Path;

fn is_guard<T: std::fmt::Debug>(r: Result<T, Fatal>) -> String {
    match r {
        Err(Fatal::Guard(m)) | Err(Fatal::Item(m)) => m,
        other => panic!("expected a guard violation, got {other:?}"),
    }
}

fn merge(a: Value, b: Value) -> Value {
    let mut a = a;
    for (k, v) in b.as_object().unwrap() {
        a.as_object_mut().unwrap().insert(k.clone(), v.clone());
    }
    a
}

/// The Windows config of the fake host plus `extra`.
fn wx(extra: Value) -> Value {
    merge(win_extra(), extra)
}

fn with_log(extra: Value, log: &Path) -> Value {
    merge(extra, json!({"log": log.to_string_lossy()}))
}

fn apply(sim: &Shared, classes: &[&str], extra: Value, win: bool, log: &Path) -> Result<(usize, usize), Fatal> {
    let extra = with_log(if win { wx(extra) } else { extra }, log);
    let (h, t, inv) = gather_sim(sim, extra, win, 50.0);
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    apply_plan(&h, sim, &t, &inv, &classes, Some(NOW as f64), None, false)
}

fn plan(sim: &Shared, log: &Path, items: Vec<Item>, classes: &[&str]) -> Result<(usize, usize), Fatal> {
    let (h, t, mut inv) = gather_sim(sim, with_log(json!({}), log), false, 50.0);
    inv.items = items;
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    apply_plan(&h, sim, &t, &inv, &classes, Some(NOW as f64), None, false)
}

fn wt_item(path: &str, branch: &str) -> Item {
    let mut it = Item::new("worktrees", "remove-worktree", path, Some(0), "x");
    it.extra = Extra::Worktree { branch: branch.into(), links: 0 };
    it
}

fn ops_run<R>(sim: &Shared, extra: Value, win: bool, f: impl FnOnce(&Ops) -> R) -> R {
    let (cfg, h) = hostcfg(extra, win, 50.0);
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, sim, &t);
    f(&ops)
}

fn winsim() -> Shared {
    let sim = win_sim();
    {
        let s = &mut *sim.s();
        s.dir("C:/trash", 0);
    }
    sim
}

fn log_in(d: &tempfile::TempDir) -> std::path::PathBuf {
    d.path().join("log.jsonl")
}

fn gather_try(sim: &Shared, extra: Value, win: bool) -> Result<Inventory, Fatal> {
    let (cfg, h) = hostcfg(extra, win, 50.0);
    let t: TidyCfg = build_cfg(&cfg, &h).unwrap();
    gather(&h, sim, &t, Some(NOW as f64))
}

const EVIL: [&str; 7] = ["x;touch pwned", "x&calc", "x$(id)", "x`id`", "x|id", "-oops", "a b"];

// ---------------------------------------------------------------- 1. branch names never reach a shell

#[test]
fn unsafe_branch_is_kept_at_inventory() {
    for bad in EVIL {
        let sim = linux_sim();
        {
            let s = &mut *sim.s();
            s.worktrees[1] = ("/repo/wt/lane-a".into(), bad.into());
            s.branches.push(bad.into());
            s.ancestors.insert(bad.into());
        }
        let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
        let w = inv.worktrees.iter().find(|x| x.path == "/repo/wt/lane-a").unwrap();
        assert_eq!(w.keep.as_deref(), Some("unsafe name"), "{bad}");
        assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty(), "{bad}");
    }
}

#[test]
fn apply_refuses_an_unsafe_branch_even_if_planned() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let r = plan(&sim, &log_in(&d), vec![wt_item("/repo/wt/lane-a", "x;touch pwned")], &["worktrees"]);
    assert_eq!(r.unwrap(), (0, 1), "refused and reported, never run");
    assert_eq!(records(&log_in(&d))[0]["status"], "refused");
    assert!(!sim.calls().iter().any(|c| c.contains("pwned") && (c.contains("tag") || c.contains("update-ref"))));
}

#[test]
fn git_never_goes_through_a_shell_string() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    let git_calls: Vec<String> = sim.calls().into_iter().filter(|c| c.starts_with("git")).collect();
    assert!(!git_calls.is_empty());
    let joined: Vec<String> = sim.s().argvs.iter().map(|a| a.join(" ")).collect();
    for c in &git_calls {
        assert!(joined.contains(c), "{c}");
    }
}

#[test]
fn docker_and_cargo_run_as_argv() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["containers", "targets"], json!({}), false, &log_in(&d)).unwrap();
    let flat: Vec<String> = sim.s().argvs.iter().map(|a| a.join(" ")).collect();
    assert!(flat.contains(&"docker rm gate-1".to_string()), "{flat:?}");
    assert!(flat.iter().any(|c| c.starts_with("cargo clean")));
}

// ---------------------------------------------------------------- 2. quoting on each kind of host

#[test]
fn windows_refuses_shell_metacharacters_in_interpolated_values() {
    let sim = winsim();
    ops_run(&sim, win_extra(), true, |ops| {
        for ch in "\"%^!&|<>".chars() {
            is_guard(ops.q(&format!("C:/a{ch}b")));
            is_guard(ops.psq(&format!("C:/a{ch}b")));
        }
        assert!(ops.q("C:/ok dir/x-1_2.txt").is_ok());
    });
}

#[test]
fn windows_git_takes_the_repo_as_an_argument_not_a_command_line() {
    let sim = winsim();
    ops_run(&sim, win_extra(), true, |ops| {
        let _ = ops.git("C:/repo&calc/main", &["status", "--porcelain"]);
    });
    let last = sim.s().argvs.last().cloned().unwrap();
    assert_eq!(last, ["git", "-C", "C:/repo&calc/main", "status", "--porcelain"]);
}

#[test]
fn local_runner_argv_has_no_shell() {
    let py = if cfg!(windows) { "python" } else { "python3" };
    let argv: Vec<String> = [py, "-c", "import sys;print(sys.argv[1])", "a&b|c;d$(e)"].iter().map(|s| s.to_string()).collect();
    match gatetrain_core::tidy::LocalRunner.run_argv_to(&argv, Some(30.0)).unwrap() {
        RunOut::Done { rc, out } => assert_eq!((rc, out.trim()), (0, "a&b|c;d$(e)")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn ssh_argv_is_posix_single_quoted() {
    let argv: Vec<String> = ["git", "-C", "/a b", "tag", "x';id;'"].iter().map(|s| s.to_string()).collect();
    let fake = gatetrain_core::runner::FakeRunner::default(); // the default `run_argv_to` is what ssh uses
    let _ = fake.run_argv_to(&argv, None);
    assert_eq!(fake.calls.borrow().last().unwrap(), &gatetrain_core::pycompat::shlex_join(&argv));
    assert_eq!(fake.calls.borrow().last().unwrap(), "git -C '/a b' tag 'x'\"'\"';id;'\"'\"''");
}

// ---------------------------------------------------------------- 3. the branch is deleted only after a verified archive

#[test]
fn failed_tag_keeps_branch_and_worktree() {
    let sim = linux_sim();
    sim.s().tag_fails = true;
    let d = tempfile::tempdir().unwrap();
    is_guard(apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)));
    assert!(sim.s().deleted_branches.is_empty());
    assert!(sim.s().branches.iter().any(|b| b == "line/lane-a"));
    assert!(sim.s().has("/repo/wt/lane-a"));
}

#[test]
fn tag_that_exists_elsewhere_gets_a_suffixed_name() {
    let sim = linux_sim();
    sim.s().tag_refs.insert("archive/line/lane-a".into(), "someothersha".into());
    let d = tempfile::tempdir().unwrap();
    let (_done, failed) = apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    assert_eq!(failed, 0);
    let sha = sim.s().sha("line/lane-a");
    let name = format!("archive/line/lane-a-{}", &sha[..8]);
    assert_eq!(sim.s().tag_refs[&name], sha);
    assert_eq!(sim.s().tag_refs["archive/line/lane-a"], "someothersha"); // the old tag is untouched
    assert!(sim.s().deleted_branches.iter().any(|b| b == "line/lane-a"));
}

#[test]
fn tag_that_points_elsewhere_is_a_violation() {
    let sim = linux_sim();
    sim.s().wrong_tag_sha = Some("WRONG".into());
    let d = tempfile::tempdir().unwrap();
    is_guard(apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)));
    assert!(sim.s().deleted_branches.is_empty());
}

#[test]
fn branch_that_moved_after_archiving_is_not_deleted() {
    let sim = linux_sim();
    sim.s().move_branch_on_tag = Some(("line/lane-a".into(), "newer".into())); // someone commits to the branch in between
    let d = tempfile::tempdir().unwrap();
    is_guard(apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)));
    assert!(sim.s().deleted_branches.is_empty());
    assert!(sim.s().branches.iter().any(|b| b == "line/lane-a"));
}

#[test]
fn branch_is_deleted_by_ref_and_sha() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    let want: Vec<String> = ["git", "-C", "/repo/main", "update-ref", "-d", "refs/heads/line/lane-a"].iter().map(|s| s.to_string()).chain([sim.s().sha("line/lane-a")]).collect();
    assert!(sim.s().argvs.contains(&want));
    assert!(!sim.calls().iter().any(|c| c.contains("branch -D")));
}

// ---------------------------------------------------------------- 4. paths

#[test]
fn safe_path_rules() {
    safe_path("/build/targets/lane-a", None).unwrap();
    safe_path("C:/build/x", None).unwrap();
    for bad in ["", "relative/x", "/a/../b", "/a/./b", "/a\nb", "/a\x00b", "/a\x1bb", "C:", "//server/share/x"] {
        is_guard(safe_path(bad, None));
    }
}

#[test]
fn trash_entry_is_exactly_one_component_below() {
    safe_path("/trash/old", Some("/trash")).unwrap();
    for bad in ["/trash", "/trash/old/x", "/trash/../x", "/other/old", "/trashy/old"] {
        is_guard(safe_path(bad, Some("/trash")));
    }
}

#[test]
fn remove_dir_refuses_dot_dot() {
    let sim = linux_sim();
    ops_run(&sim, json!({}), false, |ops| {
        is_guard(ops.remove_dir("/build/targets/../../repo/main/node_modules"));
    });
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"));
}

#[test]
fn trash_remove_of_a_nested_path_is_refused() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    assert_eq!(plan(&sim, &log_in(&d), vec![Item::new("trash", "remove", "/trash/old/x", Some(0), "x")], &["trash"]).unwrap(), (0, 1));
    assert_eq!(records(&log_in(&d))[0]["status"], "refused");
    assert!(sim.s().has("/trash/old/x"));
}

#[test]
fn trash_that_resolves_onto_a_kept_path_is_refused() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.pop("/trash");
        s.add("/trash", "link", 0, 10 * DAY, Some("/repo/main"));
    }
    let d = tempfile::tempdir().unwrap();
    is_guard(plan(&sim, &log_in(&d), vec![Item::new("trash", "remove", "/trash/node_modules", Some(0), "x")], &["trash"]));
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"));
}

#[test]
fn config_refuses_a_dangerous_trash_dir() {
    for bad in ["/", "C:/", "C:", "/repo", "/repo/main", "/build", "/build/targets", "/repo/main/sub", "relative", "/a/../trash"] {
        let (cfg, h) = hostcfg(json!({"trash_dir": bad}), false, 50.0);
        assert!(matches!(build_cfg(&cfg, &h), Err(ConfigError::Invalid(_))), "{bad}");
    }
}

#[test]
fn config_accepts_a_separate_trash_dir() {
    let (cfg, h) = hostcfg(json!({"trash_dir": "/var/trash"}), false, 50.0);
    assert_eq!(build_cfg(&cfg, &h).unwrap().trash_dir.as_deref(), Some("/var/trash"));
}

#[test]
fn a_name_with_a_newline_is_refused_not_split() {
    let sim = linux_sim();
    sim.s().dir("/trash/a\nb", 10 * DAY);
    is_guard(gather_try(&sim, json!({}), false));
}

// ---------------------------------------------------------------- 5. merged means: merging changes nothing

#[test]
fn fake_host_never_shows_files_or_diffs() {
    let sim = linux_sim();
    gather_sim(&sim, json!({}), false, 50.0);
    let calls = sim.calls();
    assert!(!calls.iter().any(|c| c.contains(" show ") || c.contains(" diff ")));
    assert!(calls.iter().any(|c| c.contains("merge-tree --write-tree")));
}

#[test]
fn conflict_or_old_git_means_not_merged() {
    let sim = linux_sim();
    sim.s().conflict.insert("line/lane-a".into());
    ops_run(&sim, json!({}), false, |ops| assert_eq!(ops.merged("line/lane-a").unwrap(), Some(false)));
}

// ---------------------------------------------------------------- 6. links and the main worktree

#[test]
fn trash_entry_that_is_a_link_is_unlinked_not_walked() {
    let sim = linux_sim();
    sim.s().add("/trash/j", "link", 0, 10 * DAY, Some("/repo/main/node_modules"));
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["trash"], json!({}), false, &log_in(&d)).unwrap();
    assert!(!sim.s().has("/trash/j"));
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"));
    let calls = sim.calls();
    assert!(!calls.iter().any(|c| c.starts_with("find") && c.contains("/trash/j"))); // never listed inside
    assert!(!calls.iter().any(|c| c.starts_with("rm -r") && c.contains("/trash/j"))); // never a recursive remove
}

#[test]
fn windows_junction_item_is_removed_as_a_link_only() {
    let sim = winsim();
    sim.s().add("C:/trash/j", "link", 0, 10 * DAY, Some("C:/repo/main/node_modules"));
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["trash"], json!({}), true, &log_in(&d)).unwrap();
    assert!(!sim.s().has("C:/trash/j"));
    assert!(sim.s().has("C:/repo/main/node_modules/pkg.js"));
    let calls = sim.calls();
    assert!(!calls.iter().any(|c| c.starts_with("dir /AL /S /B") && c.contains("trash") && !c.contains("canary")));
    assert!(!calls.iter().any(|c| c.replace('\\', "/").contains("rmdir /S") && c.replace('\\', "/").contains("trash/j")));
}

#[test]
fn worktree_that_is_a_link_is_kept() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        let e = s.get_mut("/repo/wt/lane-a").unwrap();
        e.typ = "link";
        e.target = Some("/trash/new".into());
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let w = inv.worktrees.iter().find(|x| x.path == "/repo/wt/lane-a").unwrap();
    assert_eq!(w.keep.as_deref(), Some("is a link"));
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty());
}

#[test]
fn target_that_is_a_link_is_kept() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        let e = s.get_mut("/build/targets/lane-a").unwrap();
        e.typ = "link";
        e.target = Some("/trash/new".into());
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(by_key(&inv.items, "/build/targets/lane-a").is_empty());
    assert_eq!(inv.targets.iter().find(|x| x.path == "/build/targets/lane-a").unwrap().keep.as_deref(), Some("is a link"));
}

#[test]
fn first_worktree_entry_is_always_kept() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.dir("/repo/alias", DAY);
        s.worktrees[0] = ("/repo/alias".into(), "line/lane-z".into()); // the first entry, not spelled like the configured main checkout, on a merged branch
        s.branches.push("line/lane-z".into());
        s.ancestors.insert("line/lane-z".into());
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(by_key(&inv.items, "/repo/alias").is_empty());
    assert_eq!(inv.worktrees.iter().find(|x| x.path == "/repo/alias").unwrap().keep.as_deref(), Some("main worktree"));
}

#[test]
fn a_worktree_that_resolves_to_main_is_kept() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.add("/repo/wt/alias", "link", 0, 10 * DAY, Some("/repo/main"));
        s.worktrees.push(("/repo/wt/alias".into(), "line/lane-q".into()));
        s.branches.push("line/lane-q".into());
        s.ancestors.insert("line/lane-q".into());
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(by_key(&inv.items, "/repo/wt/alias").is_empty());
}

#[test]
fn locked_worktree_is_kept() {
    let sim = linux_sim();
    sim.s().locked.insert("/repo/wt/lane-a".into());
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert_eq!(inv.worktrees.iter().find(|x| x.path == "/repo/wt/lane-a").unwrap().keep.as_deref(), Some("locked"));
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty());
}

// ---------------------------------------------------------------- 7. move

fn move_sim() -> Shared {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.dir("/build/targets/lane-m", 10 * DAY);
        s.file("/build/targets/lane-m/x.bin", 5 * MB, 10 * DAY);
    }
    sim
}

fn rec_for(log: &Path, item: &str) -> Value {
    records(log).into_iter().find(|r| r["item"] == item).unwrap()
}

#[test]
fn failed_move_is_reported_not_ok() {
    let sim = move_sim();
    sim.s().mv_fails = true;
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["targets"], json!({"orphan_targets": "clean"}), false, &log_in(&d)).unwrap();
    assert_eq!(rec_for(&log_in(&d), "/build/targets/lane-m")["status"], "failed");
    assert!(sim.s().has("/build/targets/lane-m/x.bin"));
}

#[test]
fn good_move_is_verified() {
    let sim = move_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["targets"], json!({"orphan_targets": "clean"}), false, &log_in(&d)).unwrap();
    let r = rec_for(&log_in(&d), "/build/targets/lane-m");
    assert_eq!(r["status"], "ok");
    assert!(!sim.s().has("/build/targets/lane-m"));
    let dest = r["note"].as_str().unwrap().split("moved to ").nth(1).unwrap().to_string();
    assert!(sim.s().has(&dest), "{dest}");
}

#[test]
fn trash_age_comes_from_the_timestamp_in_the_name() {
    use chrono::TimeZone;
    let stamp = |secs: i64| chrono::Local.timestamp_opt(secs, 0).single().unwrap().format("%Y%m%d%H%M%S").to_string();
    let sim = linux_sim();
    let fresh = format!("lane-q-{}", stamp(NOW - 60));
    let stale = format!("lane-r-{}", stamp(NOW - 10 * DAY));
    {
        let s = &mut *sim.s();
        s.dir(&format!("/trash/{fresh}"), 10 * DAY); // moved just now, but the folder keeps its old mtime
        s.file(&format!("/trash/{fresh}/x"), 5, 10 * DAY);
        s.dir(&format!("/trash/{stale}"), 10 * DAY);
        s.file(&format!("/trash/{stale}/x"), 5, 10 * DAY);
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(by_key(&inv.items, &format!("/trash/{fresh}")).is_empty());
    assert_eq!(by_key(&inv.items, &format!("/trash/{stale}")).len(), 1);
}

// ---------------------------------------------------------------- 8, 9. timeouts, unknown states, intent

#[test]
fn removal_timeout_is_stopped_never_done() {
    let sim = linux_sim();
    sim.s().rm_timeout = true;
    let d = tempfile::tempdir().unwrap();
    let (done, failed) = apply(&sim, &["trash"], json!({}), false, &log_in(&d)).unwrap();
    assert_eq!((done, failed), (0, 1));
    assert_eq!(records(&log_in(&d))[0]["status"], "stopped");
    assert!(sim.s().has("/trash/old/x"));
}

#[test]
fn removal_timeout_is_its_own_configurable_key() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["trash"], json!({"remove_timeout_s": 1234}), false, &log_in(&d)).unwrap();
    let seen: Vec<Option<f64>> = sim.s().call_timeouts.iter().filter(|(c, _)| c.starts_with("rm -r")).map(|(_, t)| *t).collect();
    assert_eq!(seen, [Some(1234.0)]);
    assert_eq!(TidyCfg::default().remove_timeout_s, 1800.0);
}

#[test]
fn unknown_existence_after_removal_is_a_violation() {
    let sim = linux_sim();
    sim.s().unknown_exists = vec!["/trash/old".into()];
    let d = tempfile::tempdir().unwrap();
    is_guard(apply(&sim, &["trash"], json!({}), false, &log_in(&d)));
}

#[test]
fn unknown_existence_at_inventory_is_reported_and_skipped() {
    let sim = linux_sim();
    sim.s().unknown_exists = vec!["/build/targets/lane-a".into()];
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(by_key(&inv.items, "/build/targets/lane-a").is_empty());
    assert!(inv.breaches.iter().any(|b| b.contains("cannot tell") && b.contains("/build/targets/lane-a")));
}

#[test]
fn intent_is_logged_before_the_action() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["containers"], json!({}), false, &log_in(&d)).unwrap();
    let rs = all_records(&log_in(&d));
    assert_eq!([rs[0]["status"].as_str().unwrap(), rs[1]["status"].as_str().unwrap()], ["intent", "ok"]);
    assert_eq!(rs[0]["item"], rs[1]["item"]);
}

#[test]
fn intent_survives_a_crash() {
    let sim = linux_sim();
    sim.s().crash_on = Some("docker rm".into());
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(apply(&sim, &["containers"], json!({}), false, &log_in(&d)), Err(Fatal::Other(_))));
    let st: Vec<String> = all_records(&log_in(&d)).iter().map(|r| r["status"].as_str().unwrap().to_string()).collect();
    assert_eq!(st, ["intent"]);
}

// ---------------------------------------------------------------- 10. small ones

#[test]
fn refs_keep_their_slashes_on_windows() {
    let sim = winsim();
    let d = tempfile::tempdir().unwrap();
    assert_eq!(apply(&sim, &["worktrees"], json!({}), true, &log_in(&d)).unwrap(), (1, 0));
    assert!(!sim.calls().iter().any(|c| c.starts_with("git") && c.contains('\\')));
    assert!(sim.s().argvs.iter().any(|a| a.join(" ").contains("refs/heads/line/lane-a")));
}

#[test]
fn fake_git_rejects_a_backslash_in_a_ref() {
    let sim = linux_sim();
    let argv: Vec<String> = ["git", "-C", "/r", "rev-parse", "refs/heads/a\\b"].iter().map(|s| s.to_string()).collect();
    assert_eq!(sim.run_argv_to(&argv, None).unwrap(), RunOut::done(128, ""));
}

#[test]
fn unlink_script_picks_rmdir_or_del_by_type() {
    let sim = winsim();
    {
        let s = &mut *sim.s();
        s.link("C:/repo/wt/lane-a/note.txt", "C:/repo/main/a.rs");
        s.get_mut("C:/repo/wt/lane-a/note.txt").unwrap().ltype = Some("file");
    }
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), true, &log_in(&d)).unwrap();
    let script = sim.s().scripts["C:/tmp/gatetrain-unlink.cmd"].clone();
    assert!(!script.contains("||"));
    assert!(script.contains("rmdir \"C:\\repo\\wt\\lane-a\\node_modules\""), "{script}");
    assert!(script.contains("del \"C:\\repo\\wt\\lane-a\\note.txt\""), "{script}");
    assert!(!script.contains("rmdir \"C:\\repo\\wt\\lane-a\\note.txt\""));
}

#[test]
fn link_of_unknown_type_is_a_violation() {
    let sim = winsim();
    sim.pre(Box::new(|_s, c| if c.contains("PSIsContainer") { Some(RunOut::done(1, "")) } else { None }));
    let d = tempfile::tempdir().unwrap();
    is_guard(apply(&sim, &["worktrees"], json!({}), true, &log_in(&d)));
}

#[test]
fn windows_listing_needs_the_canary() {
    let sim = winsim();
    let (cfg, h) = hostcfg(win_extra(), true, 50.0);
    let mut t = build_cfg(&cfg, &h).unwrap();
    t.link_canary = None; // config load refuses this; the runtime check is the second lock
    let ops = Ops::new(&h, &sim, &t);
    is_guard(ops.list_links("C:/repo/wt/lane-a"));
}

#[test]
fn short_cache_age_is_at_least_an_hour() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["cache"], json!({"cache_days": 0.01}), false, &log_in(&d)).unwrap();
    let prunes: Vec<String> = sim.calls().into_iter().filter(|c| c.starts_with("docker builder prune")).collect();
    assert_eq!(prunes.len(), 1);
    assert!(prunes[0].contains("until=1h") && !prunes[0].contains("until=0h"));
}

#[test]
fn cargo_clean_rechecks_the_folder_is_a_cargo_target() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.dir("/build/targets/plain", 10 * DAY);
        s.file("/build/targets/plain/notes.txt", 10, 10 * DAY);
    }
    let d = tempfile::tempdir().unwrap();
    let mut it = Item::new("targets", "clean", "/build/targets/plain", Some(10), "x");
    it.extra = Extra::Target { cargo: true, tag: Some(true), branch: None };
    let (done, failed) = plan(&sim, &log_in(&d), vec![it], &["targets"]).unwrap();
    assert!(!sim.calls().iter().any(|c| c.starts_with("cargo clean")));
    assert_eq!((done, failed), (0, 1));
    assert!(sim.s().has("/build/targets/plain/notes.txt"));
}

// ---------------------------------------------------------------- review round 2

#[test]
fn listdir_strips_the_find_prefix() {
    let sim = linux_sim();
    ops_run(&sim, json!({}), false, |ops| {
        for p in ["/trash", "/trash/"] {
            let mut v: Vec<String> = ops.listdir(p).unwrap().into_iter().filter(|x| x != gatetrain_core::tidy::ops::SEEN).collect();
            v.sort();
            assert_eq!(v, ["new", "old"], "{p}");
        }
    });
}

#[test]
fn listdir_refuses_a_line_without_the_prefix() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.starts_with("find") { Some(RunOut::done(0, "/elsewhere/x\0")) } else { None }));
    ops_run(&sim, json!({}), false, |ops| {
        is_guard(ops.listdir("/trash"));
    });
}

#[test]
fn cargo_target_is_recognised_through_listdir() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(inv.targets.iter().find(|x| x.path == "/build/targets/lane-a").unwrap().cargo);
}

#[cfg(unix)]
#[test]
fn real_disk_listdir_and_inventory() {
    use gatetrain_core::tidy::LocalRunner;
    let d = tempfile::tempdir().unwrap();
    let (roots, trash) = (d.path().join("targets"), d.path().join("trash"));
    for lane in ["lane-a", "lane-b"] {
        std::fs::create_dir_all(roots.join(lane).join("debug")).unwrap();
        std::fs::create_dir_all(roots.join(lane).join("tmp")).unwrap();
        std::fs::write(roots.join(lane).join("debug/x.o"), "x".repeat(1000)).unwrap();
    }
    std::fs::create_dir_all(trash.join("old-20200101000000")).unwrap();
    std::fs::write(trash.join("old-20200101000000/f"), "y").unwrap();
    std::fs::create_dir_all(roots.join("weird\nname")).unwrap();
    let mut h = gatetrain_core::config::Host { name: "h".into(), os: "linux".into(), ..Default::default() };
    h.tidy = to_table(&json!({"target_roots": [roots.to_string_lossy()], "trash_dir": trash.to_string_lossy(), "log": d.path().join("log").to_string_lossy()}));
    let cfg = gatetrain_core::config::Config { hosts: vec![h.clone()], ..Default::default() };
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &LocalRunner, &t);
    is_guard(ops.listdir(&roots.to_string_lossy())); // the newline name is refused, not split
    std::fs::remove_dir(roots.join("weird\nname")).unwrap();
    let mut names = ops.listdir(&roots.to_string_lossy()).unwrap();
    names.sort();
    assert_eq!(names, ["lane-a", "lane-b"]);
    // tidy saw that entry long ago (a name stamp alone no longer sets an age: the later of it and the first sighting counts)
    std::fs::write(trash.join(".gatetrain-seen.json"), r#"{"old-20200101000000": {"t": 1577836800, "m": null}}"#).unwrap();
    let inv = gather(&h, &LocalRunner, &t, None).unwrap();
    let mut paths: Vec<String> = inv.targets.iter().map(|x| x.path.clone()).collect();
    paths.sort();
    assert_eq!(paths, [roots.join("lane-a").to_string_lossy(), roots.join("lane-b").to_string_lossy()]);
    assert!(inv.targets.iter().all(|x| x.cargo));
    let keys: Vec<&str> = inv.items.iter().filter(|i| i.cls == "trash").map(|i| i.key.as_str()).collect();
    assert_eq!(keys, [trash.join("old-20200101000000").to_string_lossy()]);
}

#[test]
fn typographic_quotes_are_refused() {
    let sim = winsim();
    let evil = "C:/Windows\u{2019}$(Write-Host INJECTED)\u{2018}x";
    let before = sim.calls().len();
    ops_run(&sim, win_extra(), true, |ops| {
        is_guard(ops.is_link(evil));
        is_guard(ops.link_type(evil));
        is_guard(ops.size(evil));
        is_guard(ops.mtime(evil));
    });
    assert_eq!(sim.calls().len(), before); // nothing reached a shell
}

#[test]
fn git_worktree_remove_timeout_is_stopped() {
    let sim = linux_sim();
    sim.s().rm_timeout = true; // the folder removal is what can hang: the worktree goes by a non-traversing removal, then `git worktree prune`
    let d = tempfile::tempdir().unwrap();
    let (done, _failed) = apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    assert_eq!(done, 0);
    let rs = records(&log_in(&d));
    assert!(!rs.is_empty() && rs.iter().all(|r| r["status"] == "stopped"));
    assert!(sim.s().deleted_branches.is_empty());
}

#[test]
fn worktree_remove_gets_the_removal_timeout() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({"remove_timeout_s": 4321}), false, &log_in(&d)).unwrap();
    let seen: Vec<Option<f64>> = sim.s().call_timeouts.iter().filter(|(c, _)| c.starts_with("rm -r")).map(|(_, t)| *t).collect();
    assert!(!seen.is_empty() && seen.iter().all(|t| *t == Some(4321.0)), "{seen:?}");
}

#[test]
fn merge_is_proved_on_the_resolved_sha() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.shas.insert("line/lane-a".into(), "abc123".into());
        s.ancestors.insert("abc123".into());
    }
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    let argvs = sim.s().argvs.clone();
    let want: Vec<String> = ["git", "-C", "/repo/main", "rev-parse", "--verify", "--quiet", "refs/heads/line/lane-a^{commit}"].iter().map(|s| s.to_string()).collect();
    let i = argvs.iter().position(|a| *a == want).unwrap();
    let later: Vec<&Vec<String>> = argvs[i..].iter().filter(|a| a.iter().any(|x| x == "--is-ancestor")).take(1).collect();
    assert!(!later.is_empty());
    for a in later {
        assert!(a.iter().all(|x| x != "line/lane-a") && a.iter().any(|x| x == "abc123"), "{a:?}");
    }
}

#[test]
fn win_names_with_metacharacters_are_kept_not_fatal() {
    let sim = winsim();
    {
        let s = &mut *sim.s();
        s.dir("C:/targets", 0);
        s.dir("C:/targets/a&b", DAY);
        s.dir("C:/trash/x%y", 10 * DAY);
    }
    let (_h, _t, inv) = gather_sim(&sim, wx(json!({"target_roots": ["C:/targets"]})), true, 50.0);
    let tg = inv.targets.iter().find(|x| x.path.ends_with("a&b")).unwrap();
    assert!(tg.keep.as_deref().unwrap().contains("unsafe"));
    assert!(by_key(&inv.items, "C:/trash/x%y").is_empty());
    assert!(inv.trash.iter().any(|x| x.path.ends_with("x%y")));
}

#[test]
fn windows_without_canary_is_a_config_error() {
    let mut v = win_extra();
    v.as_object_mut().unwrap().retain(|k, _| !k.starts_with("link_canary"));
    let (cfg, h) = hostcfg(v, true, 50.0);
    assert!(matches!(build_cfg(&cfg, &h), Err(ConfigError::Invalid(_))));
}

#[test]
fn trash_dir_holding_a_worktree_is_refused() {
    let sim = linux_sim();
    sim.s().worktrees.push(("/trash/old".into(), "line/lane-t".into()));
    is_guard(gather_try(&sim, json!({}), false));
}

#[test]
fn a_link_under_another_link_is_not_listed_for_unlinking() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.add("/repo/wt/lane-a/L", "link", 0, 10 * DAY, Some("/repo/main/node_modules"));
        s.add("/repo/wt/lane-a/L/inner", "link", 0, 10 * DAY, Some("/repo/main/a.rs"));
    }
    ops_run(&sim, json!({}), false, |ops| {
        let mut v: Vec<String> = ops.list_links("/repo/wt/lane-a").unwrap().iter().map(|x| n(x)).collect();
        v.sort();
        assert_eq!(v, ["/repo/wt/lane-a/L", "/repo/wt/lane-a/node_modules"]);
    });
}

#[test]
fn keep_lanes_match_case_insensitively_on_windows() {
    let sim = winsim();
    {
        let s = &mut *sim.s();
        s.dir("C:/repo/wt/Lane-A", DAY);
        s.file("C:/repo/wt/Lane-A/src.rs", 5, DAY);
        s.worktrees[1] = ("C:/repo/wt/Lane-A".into(), "line/lane-a".into());
    }
    let (_h, _t, inv) = gather_sim(&sim, wx(json!({"keep_lanes": ["Lane-A"]})), true, 50.0);
    assert_eq!(inv.worktrees.iter().find(|x| x.path == "C:/repo/wt/Lane-A").unwrap().keep.as_deref(), Some("keep list"));
    assert!(by_key(&inv.items, "C:/repo/wt/Lane-A").is_empty());
}

#[test]
fn keep_lanes_also_protect_a_target_with_a_capital() {
    let sim = winsim();
    {
        let s = &mut *sim.s();
        s.dir("C:/targets", 0);
        cargo_target(s, "C:/targets/Lane-B", 100 * MB, true, 10 * DAY);
    }
    let (_h, _t, inv) = gather_sim(&sim, wx(json!({"keep_lanes": ["lane-b"], "target_roots": ["C:/targets"]})), true, 50.0);
    assert!(by_key(&inv.items, "C:/targets/Lane-B").is_empty());
}

#[test]
fn norm_keeps_case_on_posix_paths_only() {
    assert_eq!(norm("/Srv/Repo/"), "/Srv/Repo");
    assert_eq!(norm("C:\\Srv\\Repo"), "c:/srv/repo");
}


// ---------------------------------------------------------------- review of e2c543b (inherited holes)

#[test]
fn a_worktree_prefix_with_a_separator_is_a_config_error() {
    for bad in ["C:/wt/lane-", "/srv/wt/lane-", "wt\\lane-"] {
        let (cfg, h) = hostcfg(json!({"worktree_prefix": bad}), false, 50.0);
        assert!(matches!(build_cfg(&cfg, &h), Err(ConfigError::Invalid(_))), "{bad}");
    }
}

#[test]
fn a_keep_lanes_entry_that_matches_nothing_is_reported() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({"keep_lanes": ["lane-c", "no-such-lane"]}), false, 50.0);
    assert!(inv.breaches.iter().any(|b| b.contains("no-such-lane") && b.contains("protects nothing")), "{:?}", inv.breaches);
    assert!(!inv.breaches.iter().any(|b| b.contains("'lane-c'")));
}

#[test]
fn unc_and_extended_length_keep_paths_are_refused() {
    for bad in ["\\\\?\\C:\\repo\\keep", "\\\\server\\share\\keep", "//server/share/keep"] {
        let (cfg, h) = hostcfg(json!({"keep_paths": [bad]}), false, 50.0);
        assert!(matches!(build_cfg(&cfg, &h), Err(ConfigError::Invalid(_))), "{bad}");
    }
}

#[test]
fn the_merge_proof_names_branches_by_refs_heads() {
    let sim = linux_sim();
    gather_sim(&sim, json!({}), false, 50.0);
    let argvs = sim.s().argvs.clone();
    let proofs: Vec<&Vec<String>> = argvs.iter().filter(|a| a.iter().any(|x| x == "--is-ancestor" || x == "--write-tree")).collect();
    assert!(!proofs.is_empty());
    for a in proofs {
        let i = a.iter().position(|x| x == "--is-ancestor" || x == "--write-tree").unwrap();
        for x in &a[i + 1..] {
            assert!(x.starts_with("refs/heads/") || x.starts_with("sha-"), "{a:?}");
        }
    }
}

#[test]
fn an_unreadable_git_is_never_read_as_no_branches_or_no_worktrees() {
    for part in ["worktree list", "branch --list"] {
        let sim = linux_sim();
        let p = format!("git -C /repo/main {part}");
        sim.pre(Box::new(move |_s, c| if c.starts_with(&p) { Some(RunOut::done(128, "")) } else { None }));
        is_guard(gather_try(&sim, json!({}), false));
    }
}

#[test]
fn a_failed_listing_is_not_an_empty_folder() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.starts_with("find") { Some(RunOut::done(1, "")) } else { None }));
    ops_run(&sim, json!({}), false, |ops| {
        is_guard(ops.listdir("/trash"));
    });
}

#[test]
fn a_link_target_that_vanishes_during_the_removal_stops_the_run() {
    let sim = linux_sim();
    sim.post(Box::new(|s, c| {
        if c.starts_with("rm -r") && c.contains("/repo/wt/lane-a") {
            for k in s.under("/repo/main/node_modules") {
                s.pop(&k); // a traversing removal emptied what the link pointed to
            }
        }
    }));
    let d = tempfile::tempdir().unwrap();
    let m = is_guard(apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)));
    assert!(m.contains("is gone after removing"), "{m}");
}

#[test]
fn a_worktree_is_removed_by_a_plain_removal_then_pruned() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    let calls = sim.calls();
    let i_rm = calls.iter().position(|c| c.starts_with("rm -r") && c.contains("/repo/wt/lane-a")).unwrap();
    let i_unreg = calls.iter().position(|c| c.contains("worktree remove --force /repo/wt/lane-a")).unwrap();
    assert!(i_rm < i_unreg);
    assert!(!calls.iter().any(|c| c.ends_with("worktree prune")), "never the repo-wide prune");
}

#[test]
fn cargo_clean_with_a_non_zero_exit_is_a_failure_not_freed_unknown() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.starts_with("cargo clean") { Some(RunOut::done(101, "error")) } else { None }));
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["targets"], json!({}), false, &log_in(&d)).unwrap();
    let rs: Vec<Value> = records(&log_in(&d)).into_iter().filter(|r| r["action"] == "clean").collect();
    assert!(!rs.is_empty() && rs.iter().all(|r| r["status"] == "failed" && r["note"].as_str().unwrap().contains("exited 101")), "{rs:?}");
}


// ---------------------------------------------------------------- review of e2c543b, round 2

fn gather_at(sim: &Shared, extra: Value, lock: Option<Value>, now: f64) -> (gatetrain_core::config::Host, TidyCfg, Inventory) {
    let (cfg, mut h) = hostcfg(with_log(extra, Path::new("log.jsonl")), false, 50.0);
    if let Some(l) = lock {
        h.lock_file = Some("/run/h.lock".into());
        sim.s().scripts.insert("/run/h.lock.info".into(), l.to_string());
    }
    let t = build_cfg(&cfg, &h).unwrap();
    let inv = gather(&h, sim, &t, Some(now)).unwrap();
    (h, t, inv)
}

fn target<'a>(inv: &'a Inventory, path: &str) -> &'a gatetrain_core::tidy::inventory::TargetInfo {
    inv.targets.iter().find(|x| x.path == path).unwrap()
}

#[test]
fn a_target_with_a_file_changed_inside_is_kept_though_its_folder_is_old() {
    let sim = linux_sim();
    sim.s().add("/build/targets/lane-a/debug/fresh.o", "file", 5, 120, None); // the folder's own time is 10 days old; a file inside is 2 minutes old
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, NOW as f64);
    assert!(target(&inv, "/build/targets/lane-a").keep.as_deref().unwrap().contains("a file inside changed"));
    assert!(by_key(&inv.items, "/build/targets/lane-a").is_empty());
}

#[test]
fn an_activity_probe_that_cannot_answer_means_not_idle() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.contains("-mmin") { Some(RunOut::done(1, "")) } else { None }));
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, NOW as f64);
    assert!(by_key(&inv.items, "/build/targets/lane-a").is_empty());
    assert!(target(&inv, "/build/targets/lane-a").keep.as_deref().unwrap().contains("cannot be read"));
}

#[test]
fn a_target_named_by_a_running_hold_is_kept_and_a_released_one_is_not() {
    let held = json!({"run_id": "r7", "state": "held", "targets": ["/build/targets/lane-a/"]});
    let (_h, _t, inv) = gather_at(&linux_sim(), json!({}), Some(held.clone()), NOW as f64);
    assert!(by_key(&inv.items, "/build/targets/lane-a").is_empty());
    assert!(target(&inv, "/build/targets/lane-a").keep.as_deref().unwrap().contains("named by the running hold r7"));
    let mut released = held;
    released["state"] = json!("released");
    let (_h, _t, inv) = gather_at(&linux_sim(), json!({}), Some(released), NOW as f64);
    assert_eq!(by_key(&inv.items, "/build/targets/lane-a").len(), 1);
    let (_h, _t, inv) = gather_at(&linux_sim(), json!({}), Some(json!({"run_id": "r8", "state": "held", "targets": ["/elsewhere/Lane-B"]})), NOW as f64);
    assert!(target(&inv, "/build/targets/lane-b").keep.as_deref().unwrap().contains("named by the running hold r8"));
}

#[test]
fn a_worktree_with_recent_files_is_kept_until_it_is_old_enough() {
    let sim = linux_sim(); // lane-a: merged, clean, its files are a day old
    let (_h, _t, inv) = gather_at(&sim, json!({"worktree_min_idle_days": 2}), None, NOW as f64);
    let w = inv.worktrees.iter().find(|x| x.path == "/repo/wt/lane-a").unwrap();
    assert!(w.keep.as_deref().unwrap().contains("used in the last 2 days"), "{:?}", w.keep);
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty());
    sim.s().add("/repo/wt/lane-a/src.rs", "file", 5, 3 * DAY, None);
    sim.s().add("/repo/wt/lane-a", "dir", 0, 3 * DAY, None);
    let (_h, _t, inv) = gather_at(&sim, json!({"worktree_min_idle_days": 2}), None, NOW as f64);
    assert_eq!(by_key(&inv.items, "/repo/wt/lane-a").len(), 1);
    assert_eq!(TidyCfg::default().worktree_min_idle_days, 2.0);
}

#[test]
fn a_lane_that_is_no_longer_proven_is_not_cleaned_even_from_an_old_plan() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    let it = inv.items.iter().find(|i| i.key == "/build/targets/lane-a").unwrap();
    assert!(matches!(&it.extra, Extra::Target { branch: Some(b), .. } if b == "line/lane-a"));
    sim.s().mtree.clear(); // main moved on: merging lane-a would now change something
    let r = apply_plan(&h, &sim, &t, &inv, &["targets".to_string()], Some(NOW as f64), None, false).unwrap();
    assert_eq!(r, (0, 1));
    let rec = records(&log_in(&d)).into_iter().find(|r| r["item"] == "/build/targets/lane-a").unwrap();
    assert_eq!(rec["status"], "failed");
    assert!(rec["note"].as_str().unwrap().contains("not proven"));
    assert!(!sim.calls().iter().any(|c| c.starts_with("cargo clean") && c.contains("lane-a")));
    assert!(sim.s().has("/build/targets/lane-a/debug/big.o"));
}

#[test]
fn a_bad_item_is_reported_and_skipped_and_the_rest_carries_on() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, mut inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    inv.items = vec![wt_item("/repo/wt/lane-a", "x;touch pwned"), Item::new("containers", "rm-container", "gate-1", Some(0), "x")];
    let classes = vec!["worktrees".to_string(), "containers".to_string()];
    assert_eq!(apply_plan(&h, &sim, &t, &inv, &classes, Some(NOW as f64), None, false).unwrap(), (1, 1));
    let st: Vec<String> = records(&log_in(&d)).iter().map(|r| r["status"].as_str().unwrap().to_string()).collect();
    assert_eq!(st, ["refused", "ok"]);
}

#[test]
fn a_plain_file_in_the_trash_is_removed_and_a_half_removed_folder_is_reported_loudly() {
    let sim = linux_sim();
    sim.s().add("/trash/stray.log", "file", 40, 10 * DAY, None);
    let d = tempfile::tempdir().unwrap();
    let (_done, failed) = apply(&sim, &["trash"], json!({}), false, &log_in(&d)).unwrap();
    assert_eq!(failed, 0);
    assert!(!sim.s().has("/trash/stray.log"));
    let sim = linux_sim();
    sim.s().rm_noop = true; // a long path or a locked file: the removal "succeeds" and nothing goes
    let d2 = tempfile::tempdir().unwrap();
    apply(&sim, &["trash", "containers"], json!({}), false, &log_in(&d2)).unwrap();
    let rs = records(&log_in(&d2));
    let r = rs.iter().find(|r| r["class"] == "trash").unwrap();
    assert_eq!(r["status"], "failed");
    assert!(r["note"].as_str().unwrap().contains("PARTLY REMOVED"));
    assert!(rs.iter().any(|r| r["class"] == "containers" && r["status"] == "ok"), "the run carried on");
}

#[test]
fn a_half_removed_worktree_keeps_its_branch() {
    let sim = linux_sim();
    sim.s().rm_noop = true;
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    assert!(sim.s().deleted_branches.is_empty());
    let rs = records(&log_in(&d));
    assert!(rs.iter().filter(|r| r["status"] == "failed").all(|r| r["note"].as_str().unwrap().contains("PARTLY REMOVED")));
}

#[test]
fn a_windows_worktree_path_with_a_metacharacter_is_kept_not_fatal() {
    let sim = winsim();
    {
        let s = &mut *sim.s();
        s.dir("C:/repo/wt/a&b", DAY);
        s.worktrees.push(("C:/repo/wt/a&b".into(), "line/lane-x".into()));
        s.branches.push("line/lane-x".into());
        s.ancestors.insert("line/lane-x".into());
    }
    let (_h, _t, inv) = gather_sim(&sim, win_extra(), true, 50.0);
    assert_eq!(inv.worktrees.iter().find(|x| x.path.ends_with("a&b")).unwrap().keep.as_deref(), Some("unsafe name"));
}

#[test]
fn an_unstamped_trash_entry_ages_from_when_tidy_first_saw_it() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        // an old folder time is no age: the entries are put straight into the file system, not through the fixture helper
        s.fs.push(("/trash/zz".into(), Entry { typ: "dir", size: 0, mtime: NOW - 30 * DAY, target: None, ltype: None }));
        s.fs.push(("/trash/zz/f".into(), Entry { typ: "file", size: 5, mtime: NOW - 30 * DAY, target: None, ltype: None }));
    }
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, NOW as f64);
    assert!(by_key(&inv.items, "/trash/zz").is_empty(), "first seen now: age 0");
    assert!(sim.s().scripts["/trash/.gatetrain-seen.json"].contains("\"zz\""));
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, (NOW + 3 * DAY) as f64);
    assert!(by_key(&inv.items, "/trash/zz").is_empty(), "3 days: under trash_days");
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, (NOW + 8 * DAY) as f64);
    assert_eq!(by_key(&inv.items, "/trash/zz").len(), 1, "seen 8 days ago");
    assert!(!inv.items.iter().any(|i| i.key.ends_with(".gatetrain-seen.json")));
}


// ---------------------------------------------------------------- second review (ce417a5)

#[test]
fn a_link_that_points_inside_the_removed_folder_goes_with_it() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.dir("/repo/wt/lane-a/inner", DAY);
        s.file("/repo/wt/lane-a/inner/f", 3, DAY);
        s.add("/repo/wt/lane-a/ws-link", "link", 0, DAY, Some("/repo/wt/lane-a/inner")); // an npm workspace junction, a relative symlink
    }
    let d = tempfile::tempdir().unwrap();
    let (_done, failed) = apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    assert_eq!(failed, 0);
    assert!(!sim.s().has("/repo/wt/lane-a"));
}

#[test]
fn only_this_worktrees_registration_goes_and_never_a_repo_wide_prune() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.worktrees.push(("/repo/wt/lane-away".into(), "line/lane-away".into())); // registered, its folder only temporarily missing
        s.branches.push("line/lane-away".into());
    }
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    let r = records(&log_in(&d)).into_iter().find(|r| r["item"] == "/repo/wt/lane-a").unwrap();
    assert_eq!(r["status"], "ok", "{r}");
    assert!(!sim.calls().iter().any(|c| c.ends_with("worktree prune")), "the repo-wide prune never ran");
    assert!(sim.s().worktrees.iter().any(|w| w.0 == "/repo/wt/lane-away"), "the other registration is untouched");
    assert!(!sim.s().worktrees.iter().any(|w| w.0 == "/repo/wt/lane-a"), "this one is unregistered");
    assert!(sim.s().deleted_branches.iter().any(|b| b == "line/lane-a"));
}

#[test]
fn a_failed_unregistration_is_reported_with_the_branch_kept() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.contains(" worktree remove ") { Some(RunOut::done(128, "")) } else { None }));
    let d = tempfile::tempdir().unwrap();
    apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    let rs = records(&log_in(&d));
    assert!(!rs.is_empty() && rs.iter().all(|r| r["status"] == "failed" && r["note"].as_str().unwrap().contains("STALE REGISTRATION")), "{rs:?}");
    assert!(sim.s().deleted_branches.is_empty());
}

#[test]
fn a_link_listing_that_times_out_is_a_timeout_and_an_item_failure_not_a_run_stop() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    assert!(!by_key(&inv.items, "/repo/wt/lane-a").is_empty());
    sim.pre(Box::new(|_s, c| if c.starts_with("find") && c.contains("-type l") { Some(RunOut::TimedOut) } else { None }));
    let (done, _failed) = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    assert_eq!(done, 0);
    let rs = records(&log_in(&d));
    assert!(!rs.is_empty() && rs.iter().all(|r| r["status"] == "refused" && r["note"].as_str().unwrap().contains("timed out")), "{rs:?}");
    let seen: Vec<Option<f64>> = sim.s().call_timeouts.iter().filter(|(c, _)| c.starts_with("find") && c.contains("-type l")).map(|(_, t)| *t).collect();
    assert!(seen.iter().all(|t| *t == Some(300.0)), "its own, longer timeout: {seen:?}");
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0); // and `check` keeps the worktree instead of dying
    assert!(inv.worktrees.iter().find(|x| x.path == "/repo/wt/lane-a").unwrap().keep.as_deref().unwrap().contains("cannot list its links"));
}

#[test]
fn a_link_target_that_cannot_be_used_refuses_the_item_before_anything_is_removed() {
    let sim = winsim();
    sim.s().add("C:/repo/wt/lane-a/odd", "link", 0, 10 * DAY, Some("C:/repo/main/a&b"));
    let d = tempfile::tempdir().unwrap();
    assert_eq!(apply(&sim, &["worktrees"], json!({}), true, &log_in(&d)).unwrap(), (0, 1));
    assert_eq!(records(&log_in(&d))[0]["status"], "refused");
    assert!(sim.s().has("C:/repo/wt/lane-a/src.rs"), "the folder is untouched");
    assert!(!sim.calls().iter().any(|c| c.contains("rmdir /S") || c.ends_with("gatetrain-unlink.cmd\"")));
}

#[test]
fn an_old_format_first_seen_record_is_first_seen_now_and_upgraded() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.fs.push(("/trash/zz".into(), Entry { typ: "dir", size: 0, mtime: NOW - 30 * DAY, target: None, ltype: None }));
        s.fs.push(("/trash/zz/f".into(), Entry { typ: "file", size: 5, mtime: NOW - 30 * DAY, target: None, ltype: None }));
        s.scripts.insert("/trash/.gatetrain-seen.json".into(), json!({"zz": NOW - 20 * DAY, "old": NOW - 10 * DAY, "new": NOW - DAY}).to_string()); // bare numbers
    }
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, NOW as f64);
    assert!(by_key(&inv.items, "/trash/zz").is_empty(), "a bare 20-day-old number is not trusted");
    let rec: Value = serde_json::from_str(&sim.s().scripts["/trash/.gatetrain-seen.json"]).unwrap();
    assert_eq!(rec["zz"]["t"].as_f64().unwrap(), NOW as f64);
    assert_eq!(rec["zz"]["m"].as_i64().unwrap(), NOW - 30 * DAY);
}

#[test]
fn a_worktree_with_no_git_root_of_its_own_is_unverifiable() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.starts_with("git -C /repo/wt/lane-a rev-parse --show-toplevel") { Some(RunOut::done(0, "/repo/main\n")) } else { None }));
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let w = inv.worktrees.iter().find(|x| x.path == "/repo/wt/lane-a").unwrap();
    assert!(w.keep.as_deref().unwrap().contains("unverifiable"));
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty());
}

#[test]
fn a_reused_trash_name_is_a_new_entry_and_records_of_gone_entries_are_dropped() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.scripts.insert(
            "/trash/.gatetrain-seen.json".into(),
            json!({"zz": {"t": NOW - 10 * DAY, "m": NOW - 30 * DAY}, "gone": {"t": NOW - 9 * DAY, "m": null}, "old": {"t": NOW - 10 * DAY, "m": null}, "new": {"t": NOW - DAY, "m": null}}).to_string(),
        );
        s.fs.push(("/trash/zz".into(), Entry { typ: "dir", size: 0, mtime: NOW - DAY, target: None, ltype: None })); // a different folder now carries the name
        s.fs.push(("/trash/zz/f".into(), Entry { typ: "file", size: 5, mtime: NOW - DAY, target: None, ltype: None }));
    }
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, NOW as f64);
    assert!(by_key(&inv.items, "/trash/zz").is_empty(), "the old record does not delete the new entry on its first day");
    let rec: Value = serde_json::from_str(&sim.s().scripts["/trash/.gatetrain-seen.json"]).unwrap();
    assert!(rec.get("gone").is_none());
    assert_eq!(rec["zz"]["t"].as_f64().unwrap(), NOW as f64);
    assert_eq!(by_key(&inv.items, "/trash/old").len(), 1);
}

#[test]
fn a_stamped_name_ages_from_the_later_of_its_stamp_and_the_first_sighting() {
    use chrono::TimeZone;
    let stamp = chrono::Local.timestamp_opt(NOW - 20 * DAY, 0).single().unwrap().format("%Y%m%d%H%M%S").to_string();
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.fs.push((format!("/trash/lane-s-{stamp}"), Entry { typ: "dir", size: 0, mtime: NOW - 20 * DAY, target: None, ltype: None }));
        s.fs.push((format!("/trash/lane-s-{stamp}/f"), Entry { typ: "file", size: 5, mtime: NOW - 20 * DAY, target: None, ltype: None }));
    }
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, NOW as f64);
    assert!(by_key(&inv.items, &format!("/trash/lane-s-{stamp}")).is_empty(), "first seen now: a name stamp alone does not delete it");
    let (_h, _t, inv) = gather_at(&sim, json!({}), None, (NOW + 8 * DAY) as f64);
    assert_eq!(by_key(&inv.items, &format!("/trash/lane-s-{stamp}")).len(), 1);
}

#[test]
fn a_glob_character_in_one_name_refuses_that_item_not_the_run() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, mut inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    inv.items = vec![Item::new("trash", "remove", "/trash/a*b", Some(0), "x"), Item::new("containers", "rm-container", "gate-1", Some(0), "x")];
    let classes = vec!["trash".to_string(), "containers".to_string()];
    assert_eq!(apply_plan(&h, &sim, &t, &inv, &classes, Some(NOW as f64), None, false).unwrap(), (1, 1));
    let st: Vec<String> = records(&log_in(&d)).iter().map(|r| r["status"].as_str().unwrap().to_string()).collect();
    assert_eq!(st, ["ok", "refused"], "containers run before the trash class");
}

#[test]
fn the_idle_walk_depth_is_configurable_with_a_default_of_twelve() {
    let sim = linux_sim();
    gather_at(&sim, json!({}), None, NOW as f64);
    assert!(sim.calls().iter().any(|c| c.starts_with("find") && c.contains("-mmin") && c.contains("-maxdepth 12 ")));
    let sim = linux_sim();
    gather_at(&sim, json!({"idle_walk_depth": 3}), None, NOW as f64);
    assert!(sim.calls().iter().any(|c| c.starts_with("find") && c.contains("-mmin") && c.contains("-maxdepth 3 ")));
}

#[test]
fn link_state_tells_missing_from_unknown() {
    use gatetrain_core::tidy::ops::LinkState;
    let sim = linux_sim();
    ops_run(&sim, json!({}), false, |ops| {
        assert_eq!(ops.link_state("/nope/at/all").unwrap(), LinkState::Missing);
        assert_eq!(ops.link_state("/repo/main").unwrap(), LinkState::Real);
        assert_eq!(ops.link_state("/repo/wt/lane-a/node_modules").unwrap(), LinkState::Link);
    });
    sim.pre(Box::new(|_s, c| if c.starts_with("test -L") { Some(RunOut::done(1, "")) } else { None }));
    ops_run(&sim, json!({}), false, |ops| assert_eq!(ops.link_state("/repo/main").unwrap(), LinkState::Unknown));
}

#[test]
fn a_windows_listing_failure_is_not_an_empty_folder() {
    let sim = winsim();
    sim.s().dir("C:/empty", DAY);
    sim.pre(Box::new(|_s, c| if c.starts_with("dir /B") { Some(RunOut::done(1, "")) } else { None }));
    ops_run(&sim, win_extra(), true, |ops| {
        assert_eq!(ops.listdir("C:/empty").unwrap(), Vec::<String>::new()); // rc 1 and the folder is there: an empty folder
        is_guard(ops.listdir("C:/not-there")); // rc 1 and the folder cannot be shown: a failed listing
    });
}


// ---------------------------------------------------------------- fourth review

#[test]
fn a_dangling_junction_inside_a_removed_folder_is_not_a_missing_target() {
    let sim = linux_sim();
    sim.s().add("/repo/wt/lane-a/dead", "link", 0, 10 * DAY, Some("/gone/away")); // its target was already gone before tidy came
    let d = tempfile::tempdir().unwrap();
    let (_done, failed) = apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)).unwrap();
    assert_eq!(failed, 0);
    assert!(!sim.s().has("/repo/wt/lane-a"));
}

#[test]
fn a_target_that_existed_and_went_with_the_removal_still_stops_the_run() {
    let sim = linux_sim();
    sim.post(Box::new(|s, c| {
        if c.starts_with("rm -r") && c.contains("/repo/wt/lane-a") {
            for k in s.under("/repo/main/node_modules") {
                s.pop(&k);
            }
        }
    }));
    let d = tempfile::tempdir().unwrap();
    is_guard(apply(&sim, &["worktrees"], json!({}), false, &log_in(&d)));
}

#[test]
fn a_worktree_locked_after_the_plan_is_refused() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    assert!(!by_key(&inv.items, "/repo/wt/lane-a").is_empty());
    sim.s().locked.insert("/repo/wt/lane-a".into()); // someone locked it between the plan and the action
    apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    let r = records(&log_in(&d)).into_iter().find(|r| r["item"] == "/repo/wt/lane-a").unwrap();
    assert_eq!(r["status"], "refused");
    assert!(r["note"].as_str().unwrap().contains("locked now"));
    assert!(sim.s().has("/repo/wt/lane-a/src.rs"));
    assert!(!sim.s().tags.iter().any(|t| t == "archive/line/lane-a"));
}

#[test]
fn the_archive_tag_is_made_only_after_the_link_check_passed() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    sim.pre(Box::new(|_s, c| if c.starts_with("find") && c.contains("-type l") { Some(RunOut::TimedOut) } else { None }));
    apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    assert!(sim.s().tags.is_empty(), "a refused item leaves no tag behind");
}

#[test]
fn obsidian_style_rewrites_inside_main_do_not_stop_the_run_but_other_changes_do() {
    let run = |extra: Value| {
        let sim = linux_sim();
        sim.s().add("/repo/main/.obsidian/app.json", "file", 10, 0, None);
        sim.post(Box::new(|s, c| {
            if c.starts_with("rm -r") && c.contains("/repo/wt/lane-a") {
                s.pop("/repo/main/.obsidian/app.json"); // the editor rewrote its settings while tidy ran
            }
        }));
        let d = tempfile::tempdir().unwrap();
        apply(&sim, &["worktrees"], extra, false, &log_in(&d))
    };
    assert_eq!(run(json!({})).unwrap().1, 0, "the default volatile path is .obsidian/");
    is_guard(run(json!({"main_volatile_paths": []})));
}

#[test]
fn the_summary_counts_bytes_actually_reclaimed() {
    use gatetrain_core::tidy::apply::apply_plan_report;
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    let r = apply_plan_report(&h, &sim, &t, &inv, &["trash".to_string()], Some(NOW as f64), None, false).unwrap();
    assert_eq!((r.0, r.1, r.2), (1, 0, 50 * MB));
    let sim = linux_sim();
    sim.s().rm_noop = true;
    let (h, t, inv) = gather_sim(&sim, with_log(json!({}), &log_in(&d)), false, 50.0);
    assert_eq!(apply_plan_report(&h, &sim, &t, &inv, &["trash".to_string()], Some(NOW as f64), None, false).unwrap().2, 0, "nothing was reclaimed");
}

// ---------------------------------------------------------------- macOS: realpath has no -m

#[cfg(unix)]
#[test]
fn the_shell_fallback_resolves_links_and_missing_tails_like_realpath_m() {
    use gatetrain_core::tidy::ops::RESOLVE_SH;
    let d = tempfile::tempdir().unwrap();
    let real = d.path().canonicalize().unwrap();
    std::fs::create_dir_all(real.join("a/b")).unwrap();
    std::os::unix::fs::symlink(real.join("a"), real.join("lnk")).unwrap();
    let run = |p: &str| {
        let o = std::process::Command::new("sh").args(["-c", RESOLVE_SH, "sh", p]).output().unwrap();
        (o.status.code(), String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let r = real.to_string_lossy().to_string();
    assert_eq!(run(&format!("{r}/lnk/b")), (Some(0), format!("{r}/a/b")));
    assert_eq!(run(&format!("{r}/lnk/nope/x")), (Some(0), format!("{r}/a/nope/x")));
    assert_eq!(run(&format!("{r}/a/b/")), (Some(0), format!("{r}/a/b")));
    assert_eq!(run("/"), (Some(0), "/".to_string()));
}
