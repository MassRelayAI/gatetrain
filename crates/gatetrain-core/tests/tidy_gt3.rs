//! GT-3: honest tidy. (b) detached chain checkouts are a tidy class, (c) the `keep_lanes` warning is once per config change, (d) a worktree
//! removal never follows a link out of the worktree. (a), the docker prune's own measurement, is in tidy_measured.rs.
//! Every command runs on the fake host (`common::Sim`): no docker, git or removal here is real.
mod common;
use common::*;
use gatetrain_core::tidy::apply::apply_plan;
use gatetrain_core::tidy::inventory::Inventory;
use gatetrain_core::tidy::Fatal;
use serde_json::json;

const CHAIN: &str = "/repo/wt/chain-1";

/// lane-a..lane-new as in `linux_sim`, plus a detached chain checkout at CHAIN, ten days old and clean.
fn chain_sim(merged: bool) -> Shared {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.worktrees.push((CHAIN.into(), String::new()));
        s.dir(CHAIN, 10 * DAY);
        s.file(&format!("{CHAIN}/src.rs"), 5, 10 * DAY);
        if merged {
            let sha = s.detached_sha(CHAIN);
            s.ancestors.insert(sha);
        }
    }
    sim
}

fn gather_chain(sim: &Shared, extra: serde_json::Value) -> Inventory {
    gather_sim(sim, extra, false, 50.0).2
}

fn chain_item(inv: &Inventory) -> Option<&gatetrain_core::tidy::inventory::Item> {
    inv.items.iter().find(|i| i.key == CHAIN)
}

fn keep_of(inv: &Inventory) -> String {
    inv.worktrees.iter().find(|w| w.path == CHAIN).unwrap().keep.clone().unwrap_or_default()
}

fn apply_worktrees(sim: &Shared, extra: serde_json::Value, d: &tempfile::TempDir) -> Result<(usize, usize), Fatal> {
    let log = d.path().join("l.jsonl");
    let mut extra = extra;
    extra.as_object_mut().unwrap().insert("log".into(), json!(log.to_string_lossy()));
    let (h, t, inv) = gather_sim(sim, extra, false, 50.0);
    apply_plan(&h, sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false)
}

// ---------------------------------------------------------------- (b) detached chain checkouts

#[test]
fn a_merged_detached_checkout_is_planned_in_the_dry_run() {
    let sim = chain_sim(true);
    let inv = gather_chain(&sim, json!({}));
    let it = chain_item(&inv).expect("planned");
    assert_eq!((it.cls.as_str(), it.kind.as_str(), it.rule.as_str()), ("worktrees", "remove-detached", "merged-detached-checkout"));
    assert!(sim.s().has(CHAIN), "a plan removes nothing");
    assert!(!sim.calls().iter().any(|c| c.contains("worktree remove")), "{:?}", sim.calls());
}

#[test]
fn applying_removes_it_with_git_worktree_remove_and_creates_no_tag_for_a_merged_commit() {
    let sim = chain_sim(true);
    let d = tempfile::tempdir().unwrap();
    let (done, failed) = apply_worktrees(&sim, json!({}), &d).unwrap();
    assert!(done >= 1 && failed == 0, "{done} {failed}");
    assert!(!sim.s().has(CHAIN));
    assert!(!sim.s().worktrees.iter().any(|w| w.0 == CHAIN), "unregistered");
    assert!(sim.calls().iter().any(|c| c.contains("worktree remove") && c.contains(CHAIN)));
    assert!(!sim.calls().iter().any(|c| c.contains(" tag archive/detached")), "a merged commit needs no archive tag");
    assert!(!sim.s().deleted_branches.iter().any(|b| b.contains("chain")), "no branch is deleted for it: there is none");
    assert!(sim.s().has("/repo/main/a.rs"), "the main checkout is untouched");
}

#[test]
fn a_dirty_detached_checkout_is_never_planned() {
    let sim = chain_sim(true);
    sim.s().dirty.insert(CHAIN.into());
    let inv = gather_chain(&sim, json!({}));
    assert!(chain_item(&inv).is_none());
    assert_eq!(keep_of(&inv), "uncommitted work");
}

#[test]
fn a_detached_checkout_a_process_works_in_is_never_planned_nor_removed() {
    let sim = chain_sim(true);
    sim.s().holders = vec![format!("{CHAIN}/target/debug")]; // a process whose working directory is inside it
    let inv = gather_chain(&sim, json!({}));
    assert!(chain_item(&inv).is_none());
    assert!(keep_of(&inv).starts_with("held by a running process"), "{}", keep_of(&inv));
    // planned while idle, held by the time it is applied: the item is refused at the moment of action
    let sim = chain_sim(true);
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, json!({"log": log.to_string_lossy()}), false, 50.0);
    assert!(chain_item(&inv).is_some());
    sim.s().holders = vec![CHAIN.into()];
    let (_done, failed) = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    assert!(failed >= 1);
    assert!(sim.s().has(CHAIN), "held: kept");
}

#[test]
fn when_the_process_list_cannot_be_read_the_checkout_is_kept() {
    let sim = chain_sim(true);
    sim.s().holders_fail = true;
    let inv = gather_chain(&sim, json!({}));
    assert!(chain_item(&inv).is_none(), "cannot tell is read as held");
}

#[test]
fn an_unmerged_detached_checkout_stays_unless_an_age_is_configured_and_it_has_been_idle_that_long() {
    let sim = chain_sim(false);
    let inv = gather_chain(&sim, json!({}));
    assert!(chain_item(&inv).is_none());
    assert_eq!(keep_of(&inv), "detached head, not merged");
    // 10 days idle against a 30-day age: still kept
    let inv = gather_chain(&sim, json!({"chain_checkout_max_age_days": 30}));
    assert!(chain_item(&inv).is_none());
    assert!(keep_of(&inv).starts_with("used in the last 30"), "{}", keep_of(&inv));
    // against 7 days: planned under its own rule
    let inv = gather_chain(&sim, json!({"chain_checkout_max_age_days": 7}));
    assert_eq!(chain_item(&inv).unwrap().rule, "aged-detached-checkout");
}

#[test]
fn an_aged_unmerged_checkout_is_archived_under_a_tag_before_it_goes() {
    let sim = chain_sim(false);
    let d = tempfile::tempdir().unwrap();
    let (done, failed) = apply_worktrees(&sim, json!({"chain_checkout_max_age_days": 7}), &d).unwrap();
    assert!(done >= 1 && failed == 0);
    assert!(!sim.s().has(CHAIN));
    let sha = sim.s().detached_sha(CHAIN);
    let tag = format!("tag archive/detached-{}", &sha[..12]);
    let calls = sim.calls();
    let ti = calls.iter().position(|c| c.contains(&tag)).unwrap_or_else(|| panic!("no tag call in {calls:?}"));
    let ri = calls.iter().position(|c| c.contains("worktree remove") && c.contains(CHAIN)).unwrap();
    assert!(ti < ri, "the tag comes first");
}

#[test]
fn a_detached_checkout_that_moved_since_the_plan_is_not_removed() {
    let sim = chain_sim(true);
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, json!({"log": log.to_string_lossy()}), false, 50.0);
    // now it is on a branch: its registration no longer says detached
    sim.s().worktrees.iter_mut().find(|w| w.0 == CHAIN).unwrap().1 = "line/other".into();
    let (_done, failed) = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    assert!(failed >= 1);
    assert!(sim.s().has(CHAIN));
}

// ---------------------------------------------------------------- (d) links that point out of a worktree

#[test]
fn a_link_out_of_a_detached_checkout_is_unlinked_not_followed_and_a_link_inside_goes_with_it() {
    let sim = chain_sim(true);
    {
        let s = &mut *sim.s();
        s.dir(&format!("{CHAIN}/inner"), 10 * DAY);
        s.file(&format!("{CHAIN}/inner/f"), 3, 10 * DAY);
        s.link(&format!("{CHAIN}/ws-inside"), &format!("{CHAIN}/inner")); // target INSIDE: fine
        s.link(&format!("{CHAIN}/node_modules"), "/repo/main/node_modules"); // target in ANOTHER checkout: unlinked first
    }
    let d = tempfile::tempdir().unwrap();
    let (_done, failed) = apply_worktrees(&sim, json!({}), &d).unwrap();
    assert_eq!(failed, 0);
    assert!(!sim.s().has(CHAIN));
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"), "the other checkout's files are still there");
    assert!(!sim.calls().iter().any(|c| c.starts_with("rm -r") && c.contains("/repo/main")), "never a recursive remove into the target");
}

#[test]
fn if_the_unlink_does_nothing_the_removal_stops_before_it_can_follow_the_link() {
    let sim = chain_sim(true);
    {
        let s = &mut *sim.s();
        s.link(&format!("{CHAIN}/node_modules"), "/repo/main/node_modules");
        s.unlink_noop = true; // the unlink "succeeds" and removes nothing: the old failure
    }
    let d = tempfile::tempdir().unwrap();
    let r = apply_worktrees(&sim, json!({}), &d);
    assert!(matches!(r, Err(Fatal::Guard(_))) || r.as_ref().is_ok_and(|x| x.1 >= 1), "a failed unlink must be a guard stop or a failed item: {r:?}");
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"), "the link target must survive a failed unlink");
}

#[test]
fn a_branch_worktree_with_a_link_to_another_checkout_keeps_that_checkout_whole() {
    let sim = linux_sim(); // lane-a has node_modules -> /repo/main/node_modules
    let d = tempfile::tempdir().unwrap();
    let (done, failed) = apply_worktrees(&sim, json!({}), &d).unwrap();
    assert!(done >= 1 && failed == 0);
    assert!(!sim.s().has("/repo/wt/lane-a"));
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"));
}

// ---------------------------------------------------------------- (c) the keep_lanes warning, once per config change

fn tidy_run(sim: &Shared, conf: &str, args: &[&str]) -> gatetrain_core::tidy::CliOut {
    let mut a = vec!["tidy"];
    a.extend(args);
    cli(sim, &a, conf)
}

#[test]
fn a_keep_lanes_entry_that_matches_nothing_warns_once_then_stays_quiet_until_the_config_changes() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let sim = linux_sim();
    sim.s().free = 500.0;
    let conf = write_conf(d.path(), json!({"log": log.to_string_lossy(), "keep_lanes": ["lane-c", "no-such-lane"]}), true);
    // the fixture's shared target is over its budget (a threshold of its own that never goes away); the keep_lanes notice is the other one
    let first = tidy_run(&sim, &conf, &["--auto"]);
    assert!(first.stdout.contains("2 threshold(s) crossed"), "{}", first.stdout);
    let second = tidy_run(&sim, &conf, &["--auto"]);
    assert!(second.stdout.contains("1 threshold(s) crossed"), "the config notice is not repeated: {}", second.stdout);
    // the config changes: another entry matches nothing, so it is said again
    let conf2 = write_conf(d.path(), json!({"log": log.to_string_lossy(), "keep_lanes": ["lane-c", "no-such-lane", "also-missing"]}), true);
    let third = tidy_run(&sim, &conf2, &["--auto"]);
    assert!(third.stdout.contains("3 threshold(s) crossed"), "{}", third.stdout);
    let fourth = tidy_run(&sim, &conf2, &["--auto"]);
    assert!(fourth.stdout.contains("1 threshold(s) crossed"), "{}", fourth.stdout);
}

#[test]
fn a_dry_run_never_records_that_it_warned_and_check_always_shows_it() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let sim = linux_sim();
    let conf = write_conf(d.path(), json!({"log": log.to_string_lossy(), "keep_lanes": ["no-such-lane"]}), true);
    tidy_run(&sim, &conf, &[]);
    assert!(!d.path().join("l.jsonl.notified").exists(), "a dry run writes no state");
    tidy_run(&sim, &conf, &["--auto"]);
    assert!(d.path().join("l.jsonl.notified").exists());
    let o = cli(&sim, &["check"], &conf);
    assert!(o.stdout.contains("keep_lanes entry"), "check is the read-only truth and always says it: {}", o.stdout);
}

#[test]
fn other_thresholds_are_never_hidden() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let sim = linux_sim(); // 5 GB free against a 50 GB floor
    let conf = write_conf(d.path(), json!({"log": log.to_string_lossy(), "keep_lanes": ["no-such-lane"]}), true);
    tidy_run(&sim, &conf, &["--auto"]);
    let o = tidy_run(&sim, &conf, &["--auto"]);
    assert!(o.stdout.contains("THRESHOLD: disk /build"), "{}", o.stdout);
}

// ---------------------------------------------------------------- review fixes (0.2.1)

#[test]
fn an_unreadable_process_makes_the_answer_unknown_and_the_checkout_is_kept() {
    let sim = chain_sim(true);
    sim.s().holders = vec!["/somewhere/else".into(), gatetrain_core::tidy::ops::UNREAD.into()];
    let inv = gather_chain(&sim, json!({}));
    assert!(chain_item(&inv).is_none());
    assert!(keep_of(&inv).contains("held by a running process"), "{}", keep_of(&inv));
}

#[test]
fn a_container_mounting_the_checkout_or_a_parent_or_a_child_holds_it() {
    for src in [CHAIN, "/repo/wt", "/repo/wt/chain-1/sub"] {
        let sim = chain_sim(true);
        sim.s().holders = vec![format!("MOUNT {src}")];
        assert!(chain_item(&gather_chain(&sim, json!({}))).is_none(), "mount {src}");
    }
    let sim = chain_sim(true);
    sim.s().holders = vec!["MOUNT /other".into()];
    assert!(chain_item(&gather_chain(&sim, json!({}))).is_some());
}

#[test]
fn an_open_file_or_the_executable_inside_the_checkout_holds_it() {
    let sim = chain_sim(true);
    sim.s().holders = vec!["/".into(), format!("{CHAIN}/target/debug/tool (deleted)")];
    assert!(chain_item(&gather_chain(&sim, json!({}))).is_none());
}

#[test]
fn on_windows_a_detached_checkout_is_always_kept_because_no_handle_check_exists() {
    let sim = win_sim();
    {
        let s = &mut *sim.s();
        s.worktrees.push(("C:/repo/wt/chain-1".into(), String::new()));
        s.dir("C:/repo/wt/chain-1", 10 * DAY);
        s.file("C:/repo/wt/chain-1/a", 5, 10 * DAY);
        let sha = s.detached_sha("C:/repo/wt/chain-1");
        s.ancestors.insert(sha);
    }
    let inv = gather_sim(&sim, win_extra(), true, 50.0).2;
    assert!(inv.items.iter().all(|i| i.key != "C:/repo/wt/chain-1"));
    assert!(inv.worktrees.iter().find(|w| w.path == "C:/repo/wt/chain-1").unwrap().keep.clone().unwrap().contains("held by a running process"));
}

#[test]
fn a_file_written_after_the_plan_stops_a_merged_detached_removal_at_apply_time() {
    let sim = chain_sim(true);
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, json!({"log": log.to_string_lossy(), "worktree_min_idle_days": 2}), false, 50.0);
    assert!(chain_item(&inv).is_some());
    sim.s().file(&format!("{CHAIN}/fresh.rs"), 1, 60); // a minute old
    let (_done, failed) = apply_plan(&h, &sim, &t, &inv, &["worktrees".to_string()], Some(NOW as f64), None, false).unwrap();
    assert!(failed >= 1);
    assert!(sim.s().has(CHAIN));
}

#[test]
fn a_content_equal_but_not_ancestor_commit_is_archived_under_a_tag() {
    let sim = chain_sim(false);
    {
        let s = &mut *sim.s();
        let sha = s.detached_sha(CHAIN);
        let t = s.tree_main.clone();
        s.mtree.insert(sha, t); // merging it into main changes nothing: "merged" by content, not an ancestor
    }
    let d = tempfile::tempdir().unwrap();
    let (done, _f) = apply_worktrees(&sim, json!({}), &d).unwrap();
    assert!(done >= 1);
    assert!(!sim.s().has(CHAIN));
    assert!(sim.calls().iter().any(|c| c.contains(" tag archive/detached-")), "{:?}", sim.calls());
}

#[test]
fn a_half_done_rebase_merge_cherry_pick_or_bisect_keeps_the_checkout() {
    for f in ["rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD", "BISECT_LOG"] {
        let sim = chain_sim(true);
        sim.s().dir(&format!("{CHAIN}/.git/{f}"), 10 * DAY);
        let inv = gather_chain(&sim, json!({}));
        assert!(chain_item(&inv).is_none(), "{f}");
        assert!(keep_of(&inv).contains("in progress"), "{f}: {}", keep_of(&inv));
    }
}

#[test]
fn a_checkout_named_by_the_host_lock_is_kept() {
    let sim = chain_sim(true);
    let (cfg, mut h) = hostcfg(json!({}), false, 50.0);
    h.lock_file = Some("/run/h.lock".into());
    let lock = json!({"state": "held", "run_id": "r1", "targets": [CHAIN]});
    sim.s().scripts.insert("/run/h.lock.info".into(), lock.to_string());
    let t = gatetrain_core::tidy::config::build_cfg(&cfg, &h).unwrap();
    let inv = gatetrain_core::tidy::inventory::gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
    assert!(chain_item(&inv).is_none());
    assert!(keep_of(&inv).contains("host lock of run r1"), "{}", keep_of(&inv));
}
