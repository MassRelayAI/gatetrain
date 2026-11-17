//! `OpsEnv` and `OwnedEnv` (T9 close in production) against the `Sim` runner: protect_hit and link resolution, `docker ps` parsing,
//! the order of a worktree's removal, the branch delete by ref and sha, and `worktrees_on` leaving the main checkout alone.
mod common;
use common::*;
use gatetrain_core::tidy::close::{run_close, CloseEnv, OpsEnv, OwnedEnv, OwnedItem, OwnedSet};
use gatetrain_core::tidy::Ops;
use serde_json::json;

fn setup(extra: serde_json::Value) -> (Shared, gatetrain_core::config::Host, gatetrain_core::tidy::config::TidyCfg) {
    let sim = linux_sim();
    let (h, t, _inv) = gather_sim(&sim, extra, false, 50.0);
    (sim, h, t)
}

fn set(items: &[(&str, &str)], branches: &[&str]) -> OwnedSet {
    OwnedSet { items: items.iter().map(|(k, v)| OwnedItem::new(k, v)).collect(), branches: branches.iter().map(|b| b.to_string()).collect(), chain_checkout: None, roots: vec!["/repo/wt".into(), "/build".into(), "/trash".into()], shared_targets: vec![] }
}

#[test]
fn protect_hit_covers_a_path_inside_one_and_a_link_into_one() {
    let (sim, h, t) = setup(json!({"protect": ["/build/targets/lane-c"]}));
    sim.s().link("/build/lnk", "/build/targets/lane-c");
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    assert!(env.protected("/build/targets/lane-c/deep/x").unwrap().is_some(), "inside");
    assert!(env.protected("/build/targets").unwrap().is_some(), "a folder that contains a protected path");
    assert!(env.protected("/build/lnk").unwrap().is_some(), "a link that leads into a protected folder");
    assert!(env.protected("/build/targets/lane-a").unwrap().is_none());
}

#[test]
fn docker_names_are_parsed_for_existence_and_for_running() {
    let (sim, h, t) = setup(json!({}));
    sim.s().image_users = vec!["gate-1".into(), "gate-9".into()]; // `docker ps -a --format` of the sim
    sim.s().containers_running = vec!["gate-9".into()];
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    assert_eq!(env.exists("container", "gate-1").unwrap(), Some(true));
    assert_eq!(env.exists("container", "gate-x").unwrap(), Some(false));
    assert!(env.in_use("container", "gate-9").unwrap().is_some_and(|w| w.contains("running")));
    assert!(env.in_use("container", "gate-1").unwrap().is_none());
}

#[test]
fn worktrees_on_never_returns_the_main_checkout() {
    let (sim, h, t) = setup(json!({}));
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    let got = env.worktrees_on(&["main".to_string(), "line/lane-a".to_string()]).unwrap();
    assert_eq!(got, vec!["/repo/wt/lane-a".to_string()]);
}

#[test]
fn a_worktree_goes_in_the_order_tag_then_remove_then_unregister_then_branch_by_sha_with_no_move() {
    let (sim, h, t) = setup(json!({}));
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    let rep = run_close(&set(&[("worktree", "/repo/wt/lane-a")], &["line/lane-a"]), &env);
    assert!(rep.clean(), "{rep:?}");
    let calls = sim.calls();
    let pos = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap_or_else(|| panic!("no call with {needle}: {calls:?}"));
    assert!(pos("tag archive/line/lane-a") < pos("worktree remove"));
    assert!(pos("worktree remove") < pos("update-ref -d refs/heads/line/lane-a"));
    assert!(!calls.iter().any(|c| c.starts_with("mv -- ")), "a worktree is not moved: {calls:?}");
    assert!(sim.s().deleted_branches.contains(&"line/lane-a".to_string()));
    // main is untouched by the removal
    assert!(sim.s().has("/repo/main/node_modules/pkg.js"));
}

#[test]
fn a_locked_worktree_is_refused_and_nothing_is_removed() {
    let (sim, h, t) = setup(json!({}));
    sim.s().locked.insert("/repo/wt/lane-a".into());
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    let rep = run_close(&set(&[("worktree", "/repo/wt/lane-a")], &["line/lane-a"]), &env);
    assert!(rep.refusals.iter().any(|r| r.code == "tidy.in-use"), "{rep:?}");
    assert!(sim.s().has("/repo/wt/lane-a"));
    assert!(sim.s().deleted_branches.is_empty());
}

#[test]
fn b1_a_branch_whose_archive_tag_points_elsewhere_is_archived_at_its_own_sha_before_it_is_deleted() {
    let (sim, h, t) = setup(json!({}));
    sim.s().branches.push("line/solo".into());
    sim.s().ancestors.insert("line/solo".into());
    sim.s().tag_refs.insert("archive/line/solo".into(), "sha-OLD-ONE".into()); // the name is taken by another commit
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    let rep = run_close(&set(&[("branch", "line/solo")], &["line/solo"]), &env);
    assert!(rep.clean(), "{rep:?}");
    let tags = sim.s().tag_refs.clone();
    assert!(tags.values().any(|s| s == "sha-line/solo"), "an archive tag at the sha that is deleted: {tags:?}");
    assert_eq!(tags["archive/line/solo"], "sha-OLD-ONE", "the other tag is never overwritten");
    assert!(sim.s().deleted_branches.contains(&"line/solo".to_string()));
}

#[test]
fn b1_a_branch_that_moves_after_it_was_judged_is_kept() {
    let (sim, h, t) = setup(json!({}));
    sim.s().branches.push("line/solo".into());
    sim.s().ancestors.insert("line/solo".into());
    sim.s().move_branch_on_tag = Some(("line/solo".into(), "sha-MOVED".into())); // a commit lands between the archive and the delete
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    let rep = run_close(&set(&[("branch", "line/solo")], &["line/solo"]), &env);
    assert!(!rep.residue.is_empty(), "the delete by sha failed and is left over: {rep:?}");
    assert!(sim.s().branches.contains(&"line/solo".to_string()), "the moved branch stays");
    assert!(sim.s().deleted_branches.is_empty());
}

#[test]
fn a_branch_with_a_registered_worktree_on_it_is_not_deleted() {
    let (sim, h, t) = setup(json!({}));
    let ops = Ops::new(&h, &sim, &t);
    let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
    // lane-d is dirty: its worktree is refused, so its branch must stay with it
    let rep = run_close(&set(&[("worktree", "/repo/wt/lane-d"), ("branch", "line/lane-d")], &["line/lane-d"]), &env);
    assert!(rep.refusals.iter().any(|r| r.code == "tidy.uncommitted"), "{rep:?}");
    assert!(sim.s().branches.contains(&"line/lane-d".to_string()));
    assert!(sim.s().deleted_branches.is_empty(), "{:?}", sim.s().deleted_branches);
}

#[test]
fn the_owned_env_answers_like_the_ops_env() {
    let (sim, h, t) = setup(json!({}));
    let owned = OwnedEnv { host: h.clone(), runner: Box::new(sim.clone()), cfg: t.clone(), reference_prefixes: vec![] };
    assert_eq!(owned.worktrees_on(&["line/lane-a".to_string()]).unwrap(), vec!["/repo/wt/lane-a".to_string()]);
    assert_eq!(owned.exists("worktree", "/repo/wt/lane-a").unwrap(), Some(true));
    assert!(owned.is_main_checkout("/repo/main").unwrap());
    assert_eq!(owned.dirty("/repo/wt/lane-d").unwrap(), Some(true));
}

// ---- review round 2

fn env_for<'a>(ops: &'a Ops<'a>, t: &'a gatetrain_core::tidy::config::TidyCfg) -> OpsEnv<'a> {
    OpsEnv { ops, cfg: t, reference_prefixes: vec![] }
}

#[test]
fn bl2_a_worktree_of_a_kept_lane_is_protected_and_its_branch_stays() {
    // keep_lanes compares without case, as tidy's own planner does
    let (sim, h, t) = setup(json!({"keep_lanes": ["LANE-A"]}));
    let ops = Ops::new(&h, &sim, &t);
    let rep = run_close(&set(&[("worktree", "/repo/wt/lane-a"), ("branch", "line/lane-a")], &["line/lane-a"]), &env_for(&ops, &t));
    assert!(rep.done.is_empty() && rep.refusals.iter().any(|r| r.code == "tidy.protected" && r.key == "/repo/wt/lane-a"), "{rep:?}");
    assert!(sim.s().has("/repo/wt/lane-a"));
    assert!(sim.s().deleted_branches.is_empty(), "{:?}", sim.s().deleted_branches);
}

#[test]
fn bl2_a_worktree_on_a_kept_branch_is_protected() {
    let (sim, h, t) = setup(json!({"keep_branches": ["line/lane-a"]}));
    let ops = Ops::new(&h, &sim, &t);
    let rep = run_close(&set(&[("worktree", "/repo/wt/lane-a")], &["line/lane-a"]), &env_for(&ops, &t));
    assert!(rep.done.is_empty() && rep.refusals.iter().any(|r| r.code == "tidy.protected"), "{rep:?}");
    assert!(sim.s().has("/repo/wt/lane-a"));
    assert!(sim.s().deleted_branches.is_empty(), "{:?}", sim.s().deleted_branches);
}

#[test]
fn bl2_a_kept_branch_is_never_deleted() {
    let (sim, h, t) = setup(json!({"keep_branches": ["line/solo"]}));
    sim.s().branches.push("line/solo".into());
    sim.s().ancestors.insert("line/solo".into());
    let ops = Ops::new(&h, &sim, &t);
    let rep = run_close(&set(&[("branch", "line/solo")], &["line/solo"]), &env_for(&ops, &t));
    assert!(rep.refusals.iter().any(|r| r.code == "tidy.protected" && r.key == "line/solo"), "{rep:?}");
    assert!(sim.s().deleted_branches.is_empty(), "{:?}", sim.s().deleted_branches);
    assert!(sim.s().tags.is_empty(), "not even archived: {:?}", sim.s().tags);
}

#[test]
fn bl1_a_path_that_cannot_be_resolved_is_unreadable_and_stays() {
    let (sim, h, t) = setup(json!({}));
    sim.s().realpath_fails.push("/build/targets/lane-a".into());
    let ops = Ops::new(&h, &sim, &t);
    let rep = run_close(&set(&[("target", "/build/targets/lane-a")], &[]), &env_for(&ops, &t));
    assert!(rep.done.is_empty() && rep.refusals.iter().any(|r| r.code == "tidy.unreadable"), "{rep:?}");
    assert!(sim.s().has("/build/targets/lane-a/debug/big.o"));
}

#[test]
fn minor3_an_archive_tag_that_lands_on_another_commit_keeps_the_branch() {
    // `git tag` reports success but the tag reads back at another sha: the branch is not deleted
    let (sim, h, t) = setup(json!({}));
    sim.s().branches.push("line/solo".into());
    sim.s().ancestors.insert("line/solo".into());
    sim.s().wrong_tag_sha = Some("sha-SOMETHING-ELSE".into());
    let ops = Ops::new(&h, &sim, &t);
    let rep = run_close(&set(&[("branch", "line/solo")], &["line/solo"]), &env_for(&ops, &t));
    assert!(rep.refusals.iter().any(|r| r.code == "tidy.no-archive-tag"), "{rep:?}");
    assert!(sim.s().branches.contains(&"line/solo".to_string()));
    assert!(sim.s().deleted_branches.is_empty(), "{:?}", sim.s().deleted_branches);
}

#[test]
fn minor6_a_root_that_holds_the_main_checkout_owns_nothing_in_production() {
    // /repo holds /repo/main: a worktree below it is not owned through that root
    let (sim, h, t) = setup(json!({}));
    let ops = Ops::new(&h, &sim, &t);
    let mut s = set(&[("worktree", "/repo/wt/lane-a")], &["line/lane-a"]);
    s.roots = vec!["/repo".into()];
    let rep = run_close(&s, &env_for(&ops, &t));
    assert!(rep.done.is_empty() && rep.refusals.iter().any(|r| r.code == "tidy.not-owned" && r.why.contains("holds the main checkout")), "{rep:?}");
    assert!(sim.s().has("/repo/wt/lane-a"));
}

#[test]
fn ma2_a_link_to_a_shared_chain_target_is_not_owned_in_production() {
    // the recorded folder is a link whose target is the shared [chain] target_dirs folder
    let (sim, h, t) = setup(json!({}));
    sim.s().link("/build/lnk", "/build/chain");
    let ops = Ops::new(&h, &sim, &t);
    let mut s = set(&[("target", "/build/lnk")], &[]);
    s.shared_targets = vec!["/build/chain".into()];
    let rep = run_close(&s, &env_for(&ops, &t));
    assert!(rep.done.is_empty() && rep.refusals.iter().any(|r| r.code == "tidy.not-owned" && r.why.contains("shared")), "{rep:?}");
    assert!(sim.s().has("/build/lnk") && sim.s().has("/build/chain/debug/big.o"));
}
