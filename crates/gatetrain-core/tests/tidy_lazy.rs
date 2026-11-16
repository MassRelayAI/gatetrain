//! A worktree that cheap checks keep (keep list, uncommitted work, not merged, the main checkout) is never walked: its links are
//! not listed, its size not measured, its idle time not walked. Those walks cover node_modules and build targets; doing them for
//! every worktree made a dry run on a workstation with fifty large worktrees print nothing for minutes.
mod common;
use common::*;
use gatetrain_core::tidy::report::format_check;
use serde_json::json;

/// A probe that walks the tree under `p` (du, a link listing, an idle walk); a one-level listing is not a walk.
fn walks(c: &str, p: &str) -> bool {
    let w: Vec<&str> = c.split_whitespace().map(|x| x.trim_matches('\'')).collect();
    let names_p = w.contains(&p);
    names_p && (c.starts_with("du -sk") || (c.starts_with("find") && !c.contains("-maxdepth 1 ")))
}

#[test]
fn a_worktree_kept_by_a_cheap_check_is_never_walked() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let calls = sim.calls();
    // lane-b not merged, lane-d uncommitted work, lane-c on the keep list, main the main checkout
    for kept in ["/repo/wt/lane-b", "/repo/wt/lane-d", "/repo/wt/lane-c", "/repo/main"] {
        let w = inv.worktrees.iter().find(|w| w.path == kept).unwrap();
        assert!(w.keep.is_some(), "{kept} should be kept");
        let walked: Vec<&String> = calls.iter().filter(|c| walks(c, kept)).collect();
        assert!(walked.is_empty(), "{kept} is kept, yet walked: {walked:?}");
    }
    // the worktree that would be removed is still fully probed: links listed (lane-a has one), size measured
    assert!(calls.iter().any(|c| c.starts_with("find") && c.contains("-type l") && walks(c, "/repo/wt/lane-a")));
    assert!(calls.iter().any(|c| c.starts_with("du -sk") && walks(c, "/repo/wt/lane-a")));
    assert_eq!(by_key(&inv.items, "/repo/wt/lane-a").len(), 1);
}

#[test]
fn a_kept_worktree_shows_its_links_as_not_listed() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    let out = format_check(&inv);
    let row = |p: &str| out.lines().find(|l| l.starts_with(&format!("{p} "))).unwrap().split_whitespace().map(String::from).collect::<Vec<_>>();
    // path branch merged dirty links plan...
    assert_eq!(row("/repo/wt/lane-b")[4], "-", "{out}");
    assert_eq!(row("/repo/wt/lane-a")[4], "1", "{out}");
}
