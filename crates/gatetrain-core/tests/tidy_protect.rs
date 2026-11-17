//! Two promises of tidy, enforced in code and proved here:
//!   1. a `protect` path is never removed, never contained in a removal, and never reached through a link;
//!   2. every removal names the rule that allowed it in the action log, and an item with no rule is never removed.
mod common;
use common::*;
use gatetrain_core::tidy::apply::apply_plan_report;
use gatetrain_core::tidy::inventory::Item;
use serde_json::json;

fn classes(c: &[&str]) -> Vec<String> {
    c.iter().map(|x| x.to_string()).collect()
}

fn with_log(extra: serde_json::Value, log: &std::path::Path) -> serde_json::Value {
    let mut e = extra;
    e.as_object_mut().unwrap().insert("log".into(), json!(log.to_string_lossy()));
    e
}

#[test]
fn a_worktree_that_contains_a_protected_folder_is_kept() {
    let sim = linux_sim();
    sim.s().dir("/repo/wt/lane-a/captures", DAY);
    sim.s().file("/repo/wt/lane-a/captures/shot.png", 1000, DAY);
    // without the protection the merged, clean worktree is planned (so the test can fail)
    let (_h, _t, inv) = gather_sim(&sim, merged(json!({})), false, 50.0);
    assert_eq!(by_key(&inv.items, "/repo/wt/lane-a").len(), 1);
    let extra = merged(json!({"protect": ["/repo/wt/lane-a/captures"]}));
    let (h, t, inv) = gather_sim(&sim, extra, false, 50.0);
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty(), "a worktree holding a protected folder is not planned");
    let w = inv.worktrees.iter().find(|w| w.path == "/repo/wt/lane-a").unwrap();
    assert_eq!(w.keep.as_deref(), Some("kept: protected"));
    // and --apply leaves it and its capture alone
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h2, t2, inv2) = gather_sim(&sim, with_log(merged(json!({"protect": ["/repo/wt/lane-a/captures"]})), &log), false, 50.0);
    let _ = (h, t);
    apply_plan_report(&h2, &sim, &t2, &inv2, &classes(&["worktrees"]), Some(NOW as f64), None, true).unwrap();
    assert!(sim.s().has("/repo/wt/lane-a/captures/shot.png"));
    assert!(sim.s().has("/repo/wt/lane-a"));
}

#[test]
fn a_trash_entry_inside_a_protected_path_is_kept() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, merged(json!({})), false, 50.0);
    assert_eq!(by_key(&inv.items, "/trash/old").len(), 1, "an old trash entry is planned when nothing protects it");
    for protect in ["/trash/old", "/trash", "/trash/*"] {
        let (h, t, inv) = gather_sim(&sim, merged(json!({"protect": [protect]})), false, 50.0);
        assert!(by_key(&inv.items, "/trash/old").is_empty(), "protect = {protect}");
        let e = inv.trash.iter().find(|x| x.path == "/trash/old").unwrap();
        assert_eq!(e.keep.as_deref(), Some("kept: protected"), "protect = {protect}");
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("l.jsonl");
        let _ = (h, t);
        let (h2, t2, inv2) = gather_sim(&sim, with_log(merged(json!({"protect": [protect]})), &log), false, 50.0);
        apply_plan_report(&h2, &sim, &t2, &inv2, &classes(&["trash"]), Some(NOW as f64), None, true).unwrap();
        assert!(sim.s().has("/trash/old/x"), "protect = {protect}");
    }
}

#[test]
fn a_link_into_a_protected_path_is_not_followed_for_deletion() {
    let sim = linux_sim();
    sim.s().dir("/captures", 20 * DAY);
    sim.s().file("/captures/shot.png", 1000, 20 * DAY);
    sim.s().link("/trash/old-link", "/captures");
    let (_h, _t, inv) = gather_sim(&sim, merged(json!({})), false, 50.0);
    let before = by_key(&inv.items, "/trash/old-link").len();
    let extra = merged(json!({"protect": ["/captures"]}));
    let (_h, _t, inv) = gather_sim(&sim, extra.clone(), false, 50.0);
    assert!(by_key(&inv.items, "/trash/old-link").is_empty(), "a link that resolves into a protected path is kept (without protect it was planned {before} time(s))");
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, with_log(extra, &log), false, 50.0);
    apply_plan_report(&h, &sim, &t, &inv, &classes(&["trash", "worktrees", "targets"]), Some(NOW as f64), None, true).unwrap();
    assert!(sim.s().has("/captures/shot.png"), "the protected capture is untouched");
    assert!(sim.s().has("/trash/old-link"), "the link into it is kept too");
}

#[test]
fn a_worktree_link_to_a_protected_folder_is_unlinked_never_walked() {
    let sim = linux_sim();
    // lane-a holds a link to main's node_modules; protect that folder
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, with_log(merged(json!({"protect": ["/repo/main/node_modules"]})), &log), false, 50.0);
    apply_plan_report(&h, &sim, &t, &inv, &classes(&["worktrees"]), Some(NOW as f64), None, true).unwrap();
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"), "the target of a link is never deleted");
}

#[test]
fn an_item_that_is_applied_without_a_protect_hit_still_goes() {
    // the protection is not a blanket refusal: an unrelated path in the config changes nothing
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, with_log(merged(json!({"protect": ["/somewhere/else"]})), &log), false, 50.0);
    apply_plan_report(&h, &sim, &t, &inv, &classes(&["trash"]), Some(NOW as f64), None, true).unwrap();
    assert!(!sim.s().has("/trash/old/x"));
}

#[test]
fn every_removal_names_its_rule_in_the_log() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, inv) = gather_sim(&sim, with_log(merged(json!({})), &log), false, 50.0);
    let cls = classes(&["targets", "worktrees", "containers", "images", "cache", "trash"]);
    apply_plan_report(&h, &sim, &t, &inv, &cls, Some(NOW as f64), None, true).unwrap();
    let recs = all_records(&log);
    assert!(!recs.is_empty());
    let mut seen = std::collections::BTreeSet::new();
    for r in &recs {
        let rule = r["rule"].as_str().unwrap_or("");
        assert!(!rule.is_empty(), "a record without a rule: {r}");
        assert!(!r["why"].as_str().unwrap_or("").is_empty(), "a record without its reason: {r}");
        assert!(r["ts"].is_string() && r["class"].is_string() && r["item"].is_string() && r.get("before_bytes").is_some(), "{r}");
        seen.insert((r["class"].as_str().unwrap().to_string(), rule.to_string()));
    }
    for want in [
        ("worktrees", "merged-clean-worktree"),
        ("targets", "lane-merged-target"),
        ("targets", "over-budget-target"),
        ("containers", "exited-container"),
        ("images", "unused-image"),
        ("cache", "old-build-cache"),
        ("trash", "old-trash-entry"),
    ] {
        assert!(seen.contains(&(want.0.to_string(), want.1.to_string())), "no log line for rule {want:?}; saw {seen:?}");
    }
}

#[test]
fn an_item_that_matches_no_rule_is_never_removed() {
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h, t, mut inv) = gather_sim(&sim, with_log(merged(json!({})), &log), false, 50.0);
    inv.items.clear();
    // a plan row that nothing explains: right kind, right place, but no rule behind it
    inv.items.push(Item::new("trash", "remove", "/trash/old", Some(50 * MB), "someone wanted it gone"));
    let (done, failed, _) = apply_plan_report(&h, &sim, &t, &inv, &classes(&["trash"]), Some(NOW as f64), None, true).unwrap();
    assert_eq!((done, failed), (0, 1));
    assert!(sim.s().has("/trash/old/x"), "nothing was removed");
    let recs = all_records(&log);
    assert!(recs.iter().all(|r| r["status"] != "intent"), "no intent line: nothing was about to run");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["status"], "refused");
    assert_eq!(recs[0]["note"], "kept: no rule");
}

#[test]
fn the_inventory_never_plans_an_item_without_a_rule() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, merged(json!({})), false, 50.0);
    assert!(!inv.items.is_empty());
    assert!(inv.items.iter().all(|i| !i.rule.is_empty()), "{:?}", inv.items.iter().filter(|i| i.rule.is_empty()).map(|i| i.key.clone()).collect::<Vec<_>>());
}

#[test]
fn an_exited_container_under_a_protected_prefix_is_never_planned_or_removed() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, merged(json!({})), false, 50.0);
    assert_eq!(by_key(&inv.items, "gate-1").len(), 1, "an exited container under the prefix is planned when nothing protects it");
    let protected = merged(json!({"protect_containers": ["gate-1"]}));
    let (_h, _t, inv) = gather_sim(&sim, protected.clone(), false, 50.0);
    assert!(by_key(&inv.items, "gate-1").is_empty(), "a protected prefix is never planned");
    assert!(!inv.containers.contains(&"gate-1".to_string()));
    // a plan item that slipped in (a stale plan) is still refused at apply time
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("l.jsonl");
    let (h2, t2, mut inv2) = gather_sim(&sim, with_log(protected, &log), false, 50.0);
    inv2.items.push(Item::new("containers", "rm-container", "gate-1", Some(0), "exited throwaway container").ruled("exited-container"));
    let r = apply_plan_report(&h2, &sim, &t2, &inv2, &classes(&["containers"]), Some(NOW as f64), None, true);
    assert!(r.is_err(), "the guard stops the apply");
    assert!(sim.s().containers_exited.contains(&"gate-1".to_string()), "a protected container is left alone");
}

#[test]
fn an_empty_protect_containers_entry_is_a_config_error() {
    let sim = linux_sim();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gather_sim(&sim, merged(json!({"protect_containers": [""]})), false, 50.0)));
    assert!(r.is_err(), "an empty prefix is refused");
}
