//! The safety spec of tidy, ported from tests/test_tidy.py (every test there has its Rust twin here, same fixtures) plus the
//! guards Python enforces without a test of their own. Tidy deletes things: each protection has a test showing the Rust tool refuses.
mod common;
use common::*;
use gatetrain_core::tidy::apply::{apply_plan, cargo_tag, clean_target, do_item, CARGO_TAG_SIG};
use gatetrain_core::config;
use gatetrain_core::tidy::config::{self as tidycfg, build_cfg, CLASSES};
use gatetrain_core::tidy::inventory::{gather, human, Extra, Item};
use gatetrain_core::tidy::report::format_check;
use gatetrain_core::tidy::{Fatal, Ops, RunOut, Runner};
use regex::Regex;
use serde_json::json;
use std::path::Path;

fn is_guard<T: std::fmt::Debug>(r: Result<T, Fatal>) -> String {
    match r {
        Err(Fatal::Guard(m)) => m,
        other => panic!("expected a guard violation, got {other:?}"),
    }
}

fn applied(sim: &Shared, classes: &[&str], extra: serde_json::Value, win: bool, log: &Path) -> (Result<(usize, usize), Fatal>, tidycfg::TidyCfg) {
    let mut extra = extra;
    extra.as_object_mut().unwrap().insert("log".into(), json!(log.to_string_lossy()));
    let (h, t, inv) = gather_sim(sim, extra, win, 50.0);
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    (apply_plan(&h, sim, &t, &inv, &classes, Some(NOW as f64), None, false), t)
}

// ---------------------------------------------------------------- unknown sizes (L51)

#[test]
fn du_timeout_does_not_crash_check_and_shows_question_mark() {
    let sim = linux_sim();
    sim.s().slow = vec!["/trash/old".into()];
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let old = inv.trash.iter().find(|x| x.path == "/trash/old").unwrap();
    assert_eq!(old.bytes, None); // unknown, never a fake 0
    assert_eq!(by_key(&inv.items, "/trash/old").len(), 1); // age still decides removal
    assert_eq!(by_key(&inv.items, "/trash/old")[0].bytes, None);
    let out = format_check(&inv);
    assert!(out.lines().find(|l| l.starts_with("/trash/old")).unwrap().contains('?'));
    assert!(out.contains("item(s) of unknown size"));
}

#[test]
fn ops_size_is_none_on_timeout_and_error() {
    let sim = linux_sim();
    sim.s().slow = vec!["/trash/old".into()];
    let (cfg, h) = hostcfg(json!({}), false, 50.0);
    let t = build_cfg(&cfg, &h).unwrap();
    let ops = Ops::new(&h, &sim, &t);
    assert_eq!(ops.size("/trash/old").unwrap(), None);
    assert_eq!(ops.size("/trash/new").unwrap(), Some(5 * MB));
    assert_eq!(human(None), "?");
}

#[test]
fn everything_slow_check_still_completes() {
    let sim = linux_sim();
    sim.s().slow = vec!["".into()];
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(inv.worktrees.iter().all(|w| w.bytes.is_none()));
    assert!(!format_check(&inv).is_empty());
}

#[test]
fn age_only_disk_is_never_sized() {
    let sim = linux_sim();
    let (cfg, mut h) = hostcfg(json!({}), false, 50.0);
    h.disks.push(config::Disk { path: "/trash".into(), measure: "age-only".into(), ..Default::default() });
    let t = build_cfg(&cfg, &h).unwrap();
    let inv = gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
    let calls = sim.calls();
    assert!(!calls.iter().any(|c| c.starts_with("du -sk") && c.contains("/trash")));
    assert!(calls.iter().any(|c| c.starts_with("df -Pk") && c.contains("/trash")));
    let old = inv.trash.iter().find(|x| x.path == "/trash/old").unwrap();
    assert_eq!(old.bytes, None);
    assert_eq!(by_key(&inv.items, "/trash/old").len(), 1);
}

#[test]
fn measure_config_is_validated() {
    let ok: toml::Table = r#"[[hosts]]
name = "h"
[[hosts.disks]]
path = "/x"
measure = "age-only"
"#
    .parse()
    .unwrap();
    assert_eq!(config::parse(&ok).unwrap().hosts[0].disks[0].measure, "age-only");
    let bad: toml::Table = "[[hosts]]\nname = \"h\"\n[[hosts.disks]]\npath = \"/x\"\nmeasure = \"guess\"\n".parse().unwrap();
    let e = config::parse(&bad).unwrap_err();
    assert!(e.to_string().contains("hosts[0].disks[0].measure: must be du or age-only, got 'guess'"), "{e}");
}

#[test]
fn size_timeout_is_configurable_and_passed_to_the_runner() {
    let sim = linux_sim();
    gather_sim(&sim, json!({"size_timeout_s": 7}), false, 50.0);
    let seen = sim.0.st.borrow().du_timeouts.clone();
    assert!(!seen.is_empty() && seen.iter().all(|t| *t == Some(7.0)), "{seen:?}");
    let (cfg, h) = hostcfg(json!({}), false, 50.0);
    assert_eq!(build_cfg(&cfg, &h).unwrap().size_timeout_s, 30.0);
}

#[test]
fn unknown_size_target_over_budget_is_not_judged_nor_cleaned() {
    let sim = linux_sim();
    sim.s().slow = vec!["/build/chain".into()];
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let chain = inv.targets.iter().find(|x| x.path == "/build/chain").unwrap();
    assert_eq!(chain.bytes, None);
    assert!(!chain.over);
    assert!(by_key(&inv.items, "/build/chain").is_empty()); // never planned on budget grounds
    assert!(!inv.breaches.iter().any(|b| b.contains("/build/chain")));
    assert!(format_check(&inv).contains("size unknown"));
}

#[test]
fn clean_with_unknown_before_uses_df_delta() {
    let sim = linux_sim();
    sim.s().clean_needs_tag = false;
    sim.s().slow = vec!["/build/targets/lane-a".into()];
    sim.pre(Box::new(|st, cmd| {
        if cmd.starts_with("cargo clean") {
            st.free += 10.0;
        }
        None
    }));
    let tmp = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, json!({"log": tmp.path().join("l.jsonl").to_string_lossy()}), false, 50.0);
    let it = by_key(&inv.items, "/build/targets/lane-a")[0].clone();
    assert_eq!(it.bytes, None);
    let ops = Ops::new(&h, &sim, &t);
    let (_after, status, note) = clean_target(&ops, &t, &it).unwrap();
    assert_eq!(status, "ok");
    assert!(note.contains("df"), "{note}");
}

#[test]
fn clean_unmeasurable_never_claims_freed_nothing() {
    let sim = linux_sim();
    sim.s().clean_needs_tag = false;
    sim.s().slow = vec!["/build/targets/lane-a".into()];
    sim.pre(Box::new(|_st, cmd| if cmd.starts_with("df -Pk") { Some(RunOut::done(1, "")) } else { None }));
    let (h, t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let it = by_key(&inv.items, "/build/targets/lane-a")[0].clone();
    let (_after, status, note) = clean_target(&Ops::new(&h, &sim, &t), &t, &it).unwrap();
    assert_eq!(status, "ok");
    assert!(note.contains("freed: unknown"));
    assert!(!note.contains("nothing"));
}

// ---------------------------------------------------------------- candidate rules (check)

#[test]
fn inventory_and_plan() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let tg = |p: &str| inv.targets.iter().find(|x| x.path == p).unwrap();
    let a = tg("/build/targets/lane-a");
    assert_eq!(a.tag, Some(false)); // missing CACHEDIR.TAG is detected
    assert_eq!(a.merged, Some(true)); // squash-merged: added lines are present in main
    assert_eq!(tg("/build/targets/lane-b").merged, Some(false));
    assert_eq!(by_key(&inv.items, "/build/targets/lane-a").len(), 1);
    assert!(by_key(&inv.items, "/build/targets/lane-b").is_empty()); // unmerged lane is kept
    assert!(by_key(&inv.items, "/build/targets/lane-c").is_empty()); // keep list
    assert!(by_key(&inv.items, "/build/targets/lane-new").is_empty()); // used a minute ago
    assert!(tg("/build/targets/lane-b").keep.as_deref().unwrap().contains("not merged"));
    let chain = by_key(&inv.items, "/build/chain")[0];
    assert!(chain.attended_only); // over budget: attended only
    assert!(inv.breaches.iter().any(|b| b.contains("over budget")));
}

#[test]
fn worktrees_containers_images_trash() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let wt = |p: &str| inv.worktrees.iter().find(|w| w.path == p).unwrap();
    assert_eq!(wt("/repo/wt/lane-a").links, 1);
    assert_eq!(wt("/repo/wt/lane-d").keep.as_deref(), Some("uncommitted work"));
    assert_eq!(wt("/repo/wt/lane-b").keep.as_deref(), Some("not merged"));
    assert_eq!(wt("/repo/wt/lane-c").keep.as_deref(), Some("keep list"));
    assert_eq!(wt("/repo/main").keep.as_deref(), Some("main/root/keep list"));
    let keys = |cls: &str| inv.items.iter().filter(|i| i.cls == cls).map(|i| i.key.clone()).collect::<Vec<_>>();
    assert_eq!(keys("worktrees"), ["/repo/wt/lane-a", "/repo/wt/lane-new"]);
    assert_eq!(keys("containers"), ["gate-1"]); // running and other-prefix kept
    assert_eq!(keys("images"), ["gate-img:1"]); // in use, unmatched, <none> kept
    assert_eq!(keys("trash"), ["/trash/old"]);
}

#[test]
fn threshold_exit_code_and_table() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let p = write_conf(d.path(), json!({}), true);
    let o = cli(&sim, &["check"], &p);
    assert_eq!(o.rc, 1);
    assert!(o.stdout.contains("would tidy"));
    assert!(o.stdout.contains("THRESHOLD: disk /build: 5 GB free < 50"));
    {
        let mut s = sim.s();
        s.free = 500.0;
        s.get_mut("/build/chain/debug/big.o").unwrap().size = MB;
    }
    let o = cli(&sim, &["check"], &p);
    assert_eq!(o.rc, 0, "{}", o.stdout);
}

#[test]
fn profile_in_evidence_dir_is_a_breach_and_never_removed() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, cmd| if cmd.starts_with("find /evidence") { Some(RunOut::done(0, "/evidence/profile/SingletonLock\n")) } else { None }));
    let (_h, _t, inv) = gather_sim(&sim, json!({"evidence_dirs": ["/evidence"]}), false, 1.0);
    assert_eq!(inv.profiles, ["/evidence/profile/SingletonLock"]);
    assert!(inv.breaches.iter().any(|b| b.contains("browser-profile")));
    assert!(!inv.items.iter().any(|i| i.key.contains("Singleton"))); // reported, never removed
}

#[test]
fn orphan_target_is_kept_unless_configured_clean() {
    let sim = linux_sim();
    {
        let mut s = sim.s();
        cargo_target(&mut s, "/build/targets/orphan", 10 * MB, true, 10 * DAY);
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    assert!(by_key(&inv.items, "/build/targets/orphan").is_empty());
    assert!(inv.targets.iter().find(|t| t.path == "/build/targets/orphan").unwrap().keep.as_deref().unwrap().contains("orphan_targets = keep"));
    let (_h, _t, inv) = gather_sim(&sim, json!({"orphan_targets": "clean"}), false, 50.0);
    assert_eq!(by_key(&inv.items, "/build/targets/orphan").len(), 1);
}

#[test]
fn keep_branches_and_dirty_worktree_keep_their_targets() {
    let sim = linux_sim();
    {
        let mut s = sim.s();
        cargo_target(&mut s, "/build/targets/lane-d", 10 * MB, true, 10 * DAY); // merged branch, dirty worktree
    }
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let d = inv.targets.iter().find(|t| t.path == "/build/targets/lane-d").unwrap();
    assert_eq!(d.keep.as_deref(), Some("its worktree has uncommitted work"));
    assert!(by_key(&inv.items, "/build/targets/lane-d").is_empty());
    let (_h, _t, inv) = gather_sim(&sim, json!({"keep_branches": ["line/lane-a"]}), false, 50.0);
    assert!(by_key(&inv.items, "/build/targets/lane-a").is_empty()); // a kept branch keeps its target and its worktree
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty());
}

// ---------------------------------------------------------------- apply

#[test]
fn quiet_clean_refusal_is_detected_and_fixed() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("log.jsonl");
    let before = sim.s().total("/build/targets/lane-a");
    let (r, _) = applied(&sim, &["targets"], json!({}), false, &log);
    assert_eq!(r.unwrap(), (1, 0));
    assert_eq!(sim.s().total("/build/targets/lane-a"), cargo_tag().len() as i64);
    assert!(sim.s().has("/build/targets/lane-a/CACHEDIR.TAG"));
    assert!(sim.s().scripts["/build/targets/lane-a/CACHEDIR.TAG"].contains(CARGO_TAG_SIG));
    let calls = sim.calls();
    let cleans: Vec<&String> = calls.iter().filter(|c| c.starts_with("cargo clean")).collect();
    assert_eq!(cleans.len(), 2); // refused once, tagged, re-run
    let quiet = Regex::new(r"(^| )(-q|--quiet)( |$)").unwrap();
    assert!(!cleans.iter().any(|c| quiet.is_match(c)), "cargo clean must never run quiet (L43)");
    let r0 = &records(&log)[0];
    assert_eq!((r0["before_bytes"].as_i64().unwrap(), r0["after_bytes"].as_i64().unwrap()), (before, 0));
    assert!(r0["note"].as_str().unwrap().contains("refused silently"));
}

#[test]
fn clean_that_frees_nothing_is_reported_failed() {
    let sim = linux_sim();
    sim.s().clean_needs_tag = false;
    sim.pre(Box::new(|_s, cmd| if cmd.starts_with("cargo clean") { Some(RunOut::done(0, "")) } else { None })); // a clean that does nothing, with a tag present
    sim.s().file("/build/targets/lane-a/CACHEDIR.TAG", 100, 10 * DAY);
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("log.jsonl");
    let (r, _) = applied(&sim, &["targets"], json!({}), false, &log);
    assert_eq!(r.unwrap(), (0, 1));
    assert_eq!(records(&log)[0]["status"], "failed");
}

#[test]
fn untouched_targets_stay() {
    let sim = linux_sim();
    let keep = ["/build/targets/lane-b", "/build/targets/lane-c", "/build/targets/lane-new", "/build/chain"];
    let snap: Vec<(String, Entry)> = sim.s().snapshot().into_iter().filter(|(k, _)| keep.iter().any(|p| k.starts_with(p))).collect();
    let d = tempfile::tempdir().unwrap();
    applied(&sim, &["targets"], json!({}), false, &d.path().join("l")).0.unwrap();
    for (k, v) in snap {
        assert_eq!(sim.s().get(&k), Some(&v), "{k}");
    }
}

fn win_cfg(log: &Path) -> serde_json::Value {
    let mut v = win_extra();
    v.as_object_mut().unwrap().insert("log".into(), json!(log.to_string_lossy()));
    v
}

#[test]
fn windows_worktree_links_unlinked_and_main_untouched() {
    let sim = win_sim();
    let naive = win_sim(); // the danger is real in the simulation: a naive removal empties main
    naive.0.run_for("rmdir /S /Q \"C:\\repo\\wt\\lane-a\"", None).unwrap();
    assert!(!naive.s().has("C:/repo/main/node_modules/pkg.js"));
    let main_of = |s: &Shared| -> Vec<(String, Entry)> { s.s().snapshot().into_iter().filter(|(k, _)| k.starts_with("C:/repo/main")).collect() };
    let main_before = main_of(&sim);
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, win_cfg(&d.path().join("l.jsonl")), true, 50.0);
    let r = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    assert_eq!(r, (1, 0));
    assert_eq!(main_of(&sim), main_before);
    assert!(!sim.s().has("C:/repo/wt/lane-a"));
    let calls = sim.calls();
    let i_unlink = calls.iter().position(|c| c.ends_with("gatetrain-unlink.cmd\"")).unwrap();
    let i_remove = calls.iter().position(|c| c.contains("rmdir /S /Q")).unwrap();
    assert!(i_unlink < i_remove, "links are unlinked before the worktree is removed");
    assert!(calls.iter().any(|c| c.starts_with("dir /AL /S /B")));
    assert!(sim.s().tags.contains(&"archive/line/lane-a".to_string()));
    assert!(sim.s().deleted_branches.contains(&"line/lane-a".to_string()));
    let script = sim.s().scripts["C:/tmp/gatetrain-unlink.cmd"].clone();
    assert!(script.contains("rmdir \"C:\\repo\\wt\\lane-a\\node_modules\""), "written as a file, backslashes intact: {script}");
    assert!(!calls.iter().any(|c| c.contains("echo") && c.contains("rmdir")), "never an inline echo/printf script");
}

#[test]
fn archive_tag_comes_before_worktree_remove_and_branch_delete() {
    let sim = win_sim();
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, win_cfg(&d.path().join("l.jsonl")), true, 50.0);
    apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    let calls = sim.calls();
    let pos = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap_or_else(|| panic!("no call with {needle}"));
    assert!(pos("tag archive/line/lane-a") < pos("rmdir /S /Q"));
    assert!(pos("rmdir /S /Q") < pos("worktree prune"));
    assert!(pos("worktree prune") < pos("update-ref -d refs/heads/line/lane-a"));
    assert!(!calls.iter().any(|c| c.contains("branch -D")), "the branch is deleted by ref and sha, never by name");
}

#[test]
fn linux_worktree_symlinks_are_unlinked_by_exact_path_before_removal() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (r, _) = applied(&sim, &["worktrees"], json!({}), false, &d.path().join("l.jsonl"));
    assert_eq!(r.unwrap(), (2, 0));
    let calls = sim.calls();
    let i_unlink = calls.iter().position(|c| c == "unlink /repo/wt/lane-a/node_modules").expect("unlink by exact path");
    let i_remove = calls.iter().position(|c| c.starts_with("rm -r -- /repo/wt/lane-a")).unwrap();
    assert!(i_unlink < i_remove);
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"));
}

#[test]
fn stuck_link_stops_before_anything_is_removed() {
    let sim = win_sim();
    sim.s().unlink_noop = true;
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, win_cfg(&log), true, 50.0);
    let r = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false);
    assert!(is_guard(r).contains("link(s) remain"));
    assert!(sim.s().has("C:/repo/main/node_modules/pkg.js"));
    assert!(!sim.calls().iter().any(|c| c.contains("worktree remove")));
    assert_eq!(records(&log).last().unwrap()["status"], "guard-violation");
}

#[test]
fn listing_that_did_not_run_is_not_trusted() {
    let sim = win_sim();
    sim.s().broken_links = true;
    let (cfg, h) = hostcfg(win_extra(), true, 50.0);
    let t = build_cfg(&cfg, &h).unwrap();
    assert!(is_guard(gather(&h, &sim, &t, Some(NOW as f64))).contains("no end marker"));
}

#[test]
fn canary_must_return_expected_count() {
    let sim = win_sim();
    {
        let mut s = sim.s();
        s.dir("C:/canary", 0);
        s.link("C:/canary/l1", "C:/repo/main/a.rs");
    }
    let mut x = win_extra();
    x.as_object_mut().unwrap().insert("link_canary".into(), json!("C:/canary"));
    x["link_canary_count"] = json!(2);
    let (cfg, h) = hostcfg(x.clone(), true, 50.0);
    let t = build_cfg(&cfg, &h).unwrap();
    assert!(is_guard(gather(&h, &sim, &t, Some(NOW as f64))).contains("the listing command is not trusted"));
    x["link_canary_count"] = json!(1);
    let (cfg, h) = hostcfg(x, true, 50.0);
    let t = build_cfg(&cfg, &h).unwrap();
    gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
}

#[test]
fn main_checkout_change_is_caught() {
    let sim = win_sim();
    sim.post(Box::new(|st, cmd| {
        if cmd.contains("rmdir /S") {
            st.pop("C:/repo/main/a.rs"); // something deletes a tracked file meanwhile
        }
    }));
    let d = tempfile::tempdir().unwrap();
    let (h, t, inv) = gather_sim(&sim, win_cfg(&d.path().join("l.jsonl")), true, 50.0);
    let r = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false);
    assert!(is_guard(r).contains("main checkout changed"));
}

#[test]
fn keep_list_path_is_never_removed() {
    let sim = linux_sim();
    let (h, t, _inv) = gather_sim(&sim, json!({}), false, 50.0);
    let ops = Ops::new(&h, &sim, &t);
    for p in ["/repo/main", "/repo/root", "/repo", "/", "/build/*"] {
        is_guard(ops.remove_dir(p));
    }
    assert!(sim.s().has("/repo/main/a.rs"));
    assert!(!sim.calls().iter().any(|c| c.starts_with("rm -r")), "no removal was even attempted");
}

#[test]
fn refused_paths_each_have_their_own_reason() {
    let sim = linux_sim();
    let (h, t, _inv) = gather_sim(&sim, json!({"keep_paths": ["/keep/this"]}), false, 50.0);
    let ops = Ops::new(&h, &sim, &t);
    for p in ["", "/", "C:", "c:/", "/build/targets/*", "/build/lane-?", "/build/[ab]", "/repo/main", "/repo/main/", "relative/x", "/a/../b", "/repo", "/keep/this", "/keep"] {
        is_guard(ops.check_path(p));
    }
    ops.check_path("/build/targets/lane-a").unwrap();
}

#[test]
fn containers_images_cache_trash_exact_and_safe() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (r, _) = applied(&sim, &["containers", "images", "cache", "trash"], json!({}), false, &d.path().join("l"));
    assert_eq!(r.unwrap().1, 0);
    assert_eq!(sim.s().containers_exited, ["gate-9", "other-1"]); // running gate-9 and other-prefix stay
    let names: Vec<String> = sim.s().images.iter().map(|i| i.split('|').next().unwrap().to_string()).collect();
    assert_eq!(names, ["gate-img:2", "keepme:1", "gate-img:<none>"]);
    assert!(!sim.s().has("/trash/old"));
    assert!(sim.s().has("/trash/new"));
    assert!(sim.s().cache_row.is_none());
    let calls = sim.calls();
    assert!(calls.iter().any(|c| c.contains("until=72h")));
    assert!(!calls.iter().any(|c| c.contains("system prune")));
    let bulk = Regex::new(r"rm -r\w* .*[*?]").unwrap();
    assert!(!calls.iter().any(|c| bulk.is_match(c)));
    assert!(calls.iter().any(|c| c == "docker rm gate-1"), "removed by exact name");
    assert!(calls.iter().any(|c| c == "docker rmi gate-img:1"), "removed by exact name");
}

#[test]
fn bulk_prune_is_refused_by_the_wrapper() {
    let sim = linux_sim();
    let (h, t, _inv) = gather_sim(&sim, json!({}), false, 50.0);
    let ops = Ops::new(&h, &sim, &t);
    let before = sim.calls().len();
    is_guard(ops.run0("docker system prune -af"));
    is_guard(ops.run0("rm -rf /build/*"));
    is_guard(ops.run0("rm -r -f /build/lane-?"));
    assert_eq!(sim.calls().len(), before, "a refused command never reaches the host");
}

#[test]
fn action_log_is_append_only_jsonl() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("log.jsonl");
    applied(&sim, &["containers"], json!({}), false, &log).0.unwrap();
    let first = std::fs::read_to_string(&log).unwrap();
    applied(&sim, &["trash"], json!({}), false, &log).0.unwrap();
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.starts_with(&first)); // appended, never rewritten
    let recs = records(&log);
    assert_eq!(recs.iter().map(|r| r["class"].as_str().unwrap()).collect::<Vec<_>>(), ["containers", "trash"]);
    for r in &recs {
        for k in ["ts", "host", "action", "item", "before_bytes", "after_bytes", "status"] {
            assert!(r.get(k).is_some(), "{k}");
        }
    }
    assert_eq!(recs[1]["before_bytes"], 50 * MB);
    assert_eq!(recs[1]["after_bytes"], 0);
}

#[test]
fn first_violation_stops_the_run() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, mut inv) = gather_sim(&sim, json!({"log": log.to_string_lossy()}), false, 50.0);
    inv.items.insert(0, Item::new("targets", "clean", "/repo/main", Some(1), "poisoned"));
    let classes = ["targets".to_string(), "containers".to_string()];
    is_guard(apply_plan(&h, &sim, &t, &inv, &classes, Some(NOW as f64), None, false));
    assert_eq!(sim.s().containers_exited, ["gate-1", "gate-9", "other-1"]); // nothing after it ran
    assert_eq!(records(&log).len(), 1);
}

// ---------------------------------------------------------------- guards at apply time (the plan is not trusted)

fn poisoned(sim: &Shared, extra: serde_json::Value, it: Item) -> Result<(), Fatal> {
    let (h, t, _inv) = gather_sim(sim, extra, false, 50.0);
    let ops = Ops::new(&h, sim, &t);
    do_item(&ops, &t, &it, None).map(|_| ())
}

fn wt_item(path: &str, branch: &str) -> Item {
    let mut it = Item::new("worktrees", "remove-worktree", path, Some(0), "poisoned");
    it.extra = Extra::Worktree { branch: branch.into(), links: 0 };
    it
}

#[test]
fn apply_refuses_a_worktree_with_uncommitted_work() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({"keep_lanes": []}), wt_item("/repo/wt/lane-d", "line/lane-d")));
    assert!(m.contains("uncommitted work"), "{m}");
    assert!(!sim.calls().iter().any(|c| c.contains("worktree remove") || c.contains("branch -D")));
}

#[test]
fn apply_refuses_a_worktree_whose_branch_is_not_merged() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({}), wt_item("/repo/wt/lane-b", "line/lane-b")));
    assert!(m.contains("is not on main"), "{m}");
    assert!(!sim.calls().iter().any(|c| c.contains("worktree remove") || c.contains("branch -D")));
}

#[test]
fn apply_refuses_the_main_checkout_and_the_root_checkout_as_a_worktree() {
    let sim = linux_sim();
    is_guard(poisoned(&sim, json!({}), wt_item("/repo/main", "main")));
    is_guard(poisoned(&sim, json!({}), wt_item("/repo/root", "main")));
    assert!(!sim.calls().iter().any(|c| c.contains("worktree remove")));
}

#[test]
fn apply_refuses_worktree_removal_without_a_main_checkout() {
    let sim = linux_sim();
    let (cfg, h) = hostcfg(json!({}), false, 50.0);
    let mut t = build_cfg(&cfg, &h).unwrap();
    t.main_checkout = None;
    let ops = Ops::new(&h, &sim, &t);
    let m = is_guard(do_item(&ops, &t, &wt_item("/repo/wt/lane-a", "line/lane-a"), None));
    assert!(m.contains("needs main_checkout"));
}

#[test]
fn trash_removal_refuses_a_path_outside_the_recycle_bin() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({}), Item::new("trash", "remove", "/build/targets/lane-a", Some(0), "poisoned")));
    assert!(m.contains("not exactly one component below /trash"), "{m}");
    is_guard(poisoned(&sim, json!({}), Item::new("trash", "remove", "/trash", Some(0), "the bin itself")));
    is_guard(poisoned(&sim, json!({"trash_dir": "/trash"}), Item::new("trash", "remove", "/trashy/old", Some(0), "a sibling with the same prefix")));
    assert!(sim.s().has("/build/targets/lane-a/debug/big.o") && sim.s().has("/trash/old/x"));
    assert!(!sim.calls().iter().any(|c| c.starts_with("rm -r")));
}

#[test]
fn a_running_container_is_refused_at_apply_time() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({}), Item::new("containers", "rm-container", "gate-9", Some(0), "poisoned")));
    assert!(m.contains("is running"));
    assert!(sim.s().containers_exited.contains(&"gate-9".to_string()));
}

#[test]
fn a_container_outside_the_prefix_is_refused_at_apply_time() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({}), Item::new("containers", "rm-container", "other-1", Some(0), "poisoned")));
    assert!(m.contains("does not match the prefix"));
    assert!(!sim.calls().iter().any(|c| c.starts_with("docker rm ")));
}

#[test]
fn an_image_outside_the_patterns_or_in_use_is_refused_at_apply_time() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({}), Item::new("images", "rm-image", "keepme:1", Some(0), "poisoned")));
    assert!(m.contains("matches no configured pattern"));
    let m = is_guard(poisoned(&sim, json!({}), Item::new("images", "rm-image", "gate-img:2", Some(0), "poisoned")));
    assert!(m.contains("is used by a container"));
    assert!(!sim.calls().iter().any(|c| c.starts_with("docker rmi ")));
}

#[test]
fn an_unknown_action_is_refused() {
    let sim = linux_sim();
    let m = is_guard(poisoned(&sim, json!({}), Item::new("targets", "format-disk", "/build", Some(0), "poisoned")));
    assert!(m.contains("unknown action format-disk"));
}

#[test]
fn clean_and_move_refuse_kept_paths() {
    let sim = linux_sim();
    for kind in ["clean", "move"] {
        is_guard(poisoned(&sim, json!({}), Item::new("targets", kind, "/repo/main", Some(0), "poisoned")));
        is_guard(poisoned(&sim, json!({}), Item::new("targets", kind, "/repo", Some(0), "a parent of main")));
    }
    assert!(!sim.calls().iter().any(|c| c.starts_with("cargo clean") || c.starts_with("mv ")));
}

#[test]
fn a_move_without_a_trash_dir_is_skipped_never_deleted() {
    let sim = linux_sim();
    let (cfg, h) = hostcfg(json!({}), false, 50.0);
    let mut t = build_cfg(&cfg, &h).unwrap();
    t.trash_dir = None;
    let ops = Ops::new(&h, &sim, &t);
    let (after, status, note) = do_item(&ops, &t, &Item::new("targets", "move", "/build/targets/lane-a", Some(7), "x"), None).unwrap();
    assert_eq!((after, status), (Some(7), "skipped"));
    assert!(note.contains("no trash_dir configured"));
    assert!(sim.s().has("/build/targets/lane-a/debug/big.o"));
    assert!(!sim.calls().iter().any(|c| c.starts_with("mv ")));
}

#[test]
fn a_move_goes_to_the_recycle_bin_not_to_nothing() {
    let sim = linux_sim();
    let (h, t, _inv) = gather_sim(&sim, json!({}), false, 50.0);
    let ops = Ops::new(&h, &sim, &t);
    let (_a, status, note) = do_item(&ops, &t, &Item::new("targets", "move", "/build/targets/lane-a", Some(7), "x"), None).unwrap();
    assert_eq!(status, "ok");
    assert!(note.starts_with("moved to /trash/lane-a-"), "{note}");
    assert!(!sim.s().has("/build/targets/lane-a/debug/big.o"));
    let dest = note.trim_start_matches("moved to ").to_string();
    assert!(sim.s().has(&format!("{dest}/debug/big.o")), "the content is in the bin, not gone");
}

// ---------------------------------------------------------------- the commands

#[test]
fn auto_runs_only_unattended_classes_and_summarises() {
    let sim = linux_sim();
    {
        let mut s = sim.s();
        s.free = 500.0;
        s.get_mut("/build/chain/debug/big.o").unwrap().size = MB;
    }
    let d = tempfile::tempdir().unwrap();
    let p = write_conf(d.path(), json!({"log": d.path().join("l.jsonl").to_string_lossy()}), true);
    let o = cli(&sim, &["tidy", "--auto"], &p);
    assert_eq!(o.rc, 0, "{}", o.stdout);
    assert!(o.stdout.trim().lines().count() <= 5);
    assert_eq!(sim.s().total("/build/targets/lane-a"), cargo_tag().len() as i64);
    assert_eq!(sim.s().containers_exited, ["gate-9", "other-1"]);
    assert!(!sim.s().has("/trash/old"));
    assert_eq!(sim.s().images.len(), 4); // images not unattended
    assert!(sim.s().has("/repo/wt/lane-a")); // worktrees not unattended
    assert!(sim.s().deleted_branches.is_empty());
    assert!(sim.s().cache_row.is_some()); // cache not unattended
}

#[test]
fn auto_never_runs_an_attended_only_item() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    let d = tempfile::tempdir().unwrap();
    let p = write_conf(d.path(), json!({"log": d.path().join("l.jsonl").to_string_lossy()}), true);
    cli(&sim, &["tidy", "--auto"], &p);
    assert!(sim.s().has("/build/chain/debug/big.o"), "the over-budget shared target is attended only");
    assert!(!sim.calls().iter().any(|c| c.starts_with("cargo clean") && c.contains("/build/chain")));
}

#[test]
fn dry_run_changes_nothing() {
    let sim = linux_sim();
    let state = |sim: &Shared| {
        let s = sim.s();
        (s.snapshot(), s.containers_exited.clone(), s.images.clone())
    };
    let snap = state(&sim);
    let d = tempfile::tempdir().unwrap();
    let logp = d.path().join("l");
    let p = write_conf(d.path(), json!({"log": logp.to_string_lossy()}), false);
    let o = cli(&sim, &["tidy"], &p);
    assert!(o.stdout.contains("dry run"));
    assert!(!logp.exists());
    assert_eq!(snap, state(&sim));
    assert!(!sim.calls().iter().any(|c| ["cargo", "docker rm", "unlink", "rm ", "git -C /repo/main worktree remove", "git -C /repo/main tag", "git -C /repo/main branch -D"].iter().any(|p| c.starts_with(p))));
}

#[test]
fn an_unknown_class_is_refused_before_anything_runs() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let p = write_conf(d.path(), json!({}), true);
    let o = cli(&sim, &["tidy", "--class", "targets,everything", "--apply"], &p);
    assert_eq!(o.rc, 1);
    assert!(o.stderr.contains("unknown class in --class"));
    assert!(sim.calls().is_empty());
}

#[test]
fn a_guard_violation_exits_3_and_says_nothing_further_was_touched() {
    let sim = win_sim();
    sim.s().unlink_noop = true;
    let d = tempfile::tempdir().unwrap();
    let mut x = win_extra();
    x["log"] = json!(d.path().join("l.jsonl").to_string_lossy());
    let p = write_conf(d.path(), x, true);
    // the host in write_conf is linux; make it windows like the sim
    let text = std::fs::read_to_string(&p).unwrap().replace("os = \"linux\"", "os = \"windows\"");
    std::fs::write(&p, text).unwrap();
    let o = cli(&sim, &["tidy", "--apply", "--class", "worktrees"], &p);
    assert_eq!(o.rc, 3, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("stopped, nothing further touched"));
    assert!(sim.s().has("C:/repo/main/node_modules/pkg.js"));
}

// ---------------------------------------------------------------- config

#[test]
fn unknown_key_and_bad_class() {
    let (cfg, h) = hostcfg(json!({"nope": 1}), false, 50.0);
    assert!(build_cfg(&cfg, &h).unwrap_err().to_string().contains("unknown key(s) ['nope']"));
    let (cfg, h) = hostcfg(json!({"unattended": ["everything"]}), false, 50.0);
    assert!(build_cfg(&cfg, &h).unwrap_err().to_string().contains("unknown class 'everything'"));
    let (cfg, h) = hostcfg(json!({"orphan_targets": "burn"}), false, 50.0);
    assert!(build_cfg(&cfg, &h).unwrap_err().to_string().contains("orphan_targets: must be keep or clean"));
    let (cfg, h) = hostcfg(json!({"trash_days": "seven"}), false, 50.0);
    assert!(build_cfg(&cfg, &h).unwrap_err().to_string().contains("must be a number"));
    let (cfg, h) = hostcfg(json!({"keep_paths": "x"}), false, 50.0);
    assert!(build_cfg(&cfg, &h).unwrap_err().to_string().contains("must be a list of strings"));
}

#[test]
fn defaults_are_conservative() {
    let t = tidycfg::TidyCfg::default();
    assert_eq!(t.unattended, ["targets", "containers", "trash"]);
    assert_eq!(t.cache_days, 0.0);
    assert_eq!(t.orphan_targets, "keep");
    assert_eq!(CLASSES.len(), 6);
}
