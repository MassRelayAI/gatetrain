//! A dry run ends in bounded time and is never silent: every item's probes share `item_budget_s`; an item that runs out is skipped
//! and reported (never sized by guess, never planned), and each item is reported as it is judged.
mod common;
use common::*;
use gatetrain_core::tidy::inventory::{over_budget, set_progress};
use gatetrain_core::tidy::RunOut;
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// `du` of `p` takes `secs`, unless the runner was given less time: then it is cut at that limit, as the real runner does.
fn slow_du(sim: &Shared, p: &'static str, secs: f64) {
    sim.pre(Box::new(move |st, cmd| {
        if cmd.starts_with("du -sk") && cmd.contains(p) {
            let limit = st.call_timeouts.last().and_then(|x| x.1).unwrap_or(60.0);
            std::thread::sleep(Duration::from_secs_f64(limit.min(secs)));
            if limit < secs {
                return Some(RunOut::TimedOut);
            }
        }
        None
    }));
}

#[test]
fn an_item_over_its_budget_is_skipped_reported_and_not_planned() {
    let sim = linux_sim();
    slow_du(&sim, "/repo/wt/lane-a", 4.0);
    let t0 = Instant::now();
    let (_h, t, inv) = gather_sim(&sim, json!({"item_budget_s": 1, "size_timeout_s": 30}), false, 50.0);
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(3000), "the 1 s budget took {took:?}");
    let a = inv.worktrees.iter().find(|w| w.path == "/repo/wt/lane-a").unwrap();
    assert_eq!(a.keep.as_deref(), Some(over_budget(&t).as_str()));
    assert_eq!(a.bytes, None);
    assert!(by_key(&inv.items, "/repo/wt/lane-a").is_empty(), "an item cut short is never planned");
    // the next worktree gets a budget of its own
    assert_eq!(by_key(&inv.items, "/repo/wt/lane-new").len(), 1);
    // the limit the slow probe got was what was left of the budget, not its own 30 s
    let du = sim.s().call_timeouts.iter().find(|(c, _)| c.starts_with("du -sk") && c.contains("/repo/wt/lane-a")).cloned().unwrap();
    assert!(du.1.unwrap() <= 1.0, "{du:?}");
}

#[test]
fn without_a_budget_the_probe_keeps_its_own_limit() {
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({"item_budget_s": 0, "size_timeout_s": 30}), false, 50.0);
    assert_eq!(by_key(&inv.items, "/repo/wt/lane-a").len(), 1);
    let du: Vec<Option<f64>> = sim.s().call_timeouts.iter().filter(|(c, _)| c.starts_with("du -sk")).map(|x| x.1).collect();
    assert!(!du.is_empty() && du.iter().all(|t| *t == Some(30.0)), "{du:?}");
}

#[test]
fn the_default_budget_leaves_the_probe_limits_as_configured() {
    let sim = linux_sim();
    gather_sim(&sim, json!({"size_timeout_s": 30}), false, 50.0);
    let du: Vec<Option<f64>> = sim.s().call_timeouts.iter().filter(|(c, _)| c.starts_with("du -sk")).map(|x| x.1).collect();
    assert!(!du.is_empty() && du.iter().all(|t| *t == Some(30.0)), "{du:?}");
}

#[test]
fn every_item_is_reported_as_it_is_judged() {
    let lines: Rc<RefCell<Vec<String>>> = Rc::default();
    let l2 = lines.clone();
    set_progress(Some(Box::new(move |l: &str| l2.borrow_mut().push(l.to_string()))));
    let sim = linux_sim();
    let (_h, _t, inv) = gather_sim(&sim, json!({}), false, 50.0);
    set_progress(None);
    let got = lines.borrow().clone();
    let wt: Vec<&String> = got.iter().filter(|l| l.starts_with("tidy: worktree ")).collect();
    assert_eq!(wt.len(), inv.worktrees.len(), "{got:#?}");
    assert_eq!(wt[0], "tidy: worktree 1/6 /repo/main: main/root/keep list");
    assert_eq!(wt[2], "tidy: worktree 3/6 /repo/wt/lane-b: not merged");
    assert!(wt[1].starts_with("tidy: worktree 2/6 /repo/wt/lane-a: tidy ("), "{}", wt[1]);
    assert_eq!(got.iter().filter(|l| l.starts_with("tidy: target ")).count(), inv.targets.len());
    assert_eq!(got.iter().filter(|l| l.starts_with("tidy: recycle bin ")).count(), inv.trash.len());
    // quiet unless asked: a library call prints nothing
    let sim = linux_sim();
    gather_sim(&sim, json!({}), false, 50.0);
    assert_eq!(lines.borrow().len(), got.len());
}
