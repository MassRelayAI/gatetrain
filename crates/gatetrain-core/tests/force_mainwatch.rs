//! `gatetrain main-watch`: a main commit with no full chain gets one started (through a pushed non-main branch), its steps are ingested,
//! every red becomes a post-fix item with an owner, and the deploy command runs only for a green sha. Real git; the configured
//! commands are recorded, not run.
mod force_fix;
use force_fix::*;
use gatetrain_core::force::cfg;
use gatetrain_core::force::exec::{Exec, LocalExec, Out};
use gatetrain_core::force::mainwatch::*;
use gatetrain_core::ledger::Ledger;
use std::cell::RefCell;

struct Rec {
    shells: RefCell<Vec<String>>,
    runs: RefCell<Vec<String>>,
    sh_rc: i32,
}
impl Rec {
    fn new() -> Rec {
        Rec { shells: RefCell::new(vec![]), runs: RefCell::new(vec![]), sh_rc: 0 }
    }
}
impl Exec for Rec {
    fn run(&self, argv: &[String]) -> Out {
        self.runs.borrow_mut().push(argv.join(" "));
        LocalExec.run(argv)
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        self.shells.borrow_mut().push(cmd.to_string());
        let _ = cwd;
        Out::new(self.sh_rc, "")
    }
}

const CFG: &str = r#"
[mainwatch]
chain_cmd = "start-chain {branch} --no-stamp # {sha}"
branch_prefix = "chain/main-"
log_template = "logs/{short}.log"
[[mainwatch.owners]]
step = "lint*"
owner = "alice"
"#;

fn ledger() -> Ledger {
    let mut l = Ledger::open_in_memory().unwrap();
    l.migrate().unwrap();
    l
}

fn log(sha: &str, mode: &str, reds: &[&str], done: bool) -> String {
    let mut s = format!("branch chain/main-x at {sha} on pc\nCHAIN_MODE {mode}\n");
    for (i, st) in ["build", "lint-ts", "test-api"].iter().enumerate() {
        let rc = if reds.contains(st) { 1 } else { 0 };
        s.push_str(&format!("STEP_START {st} 2026-11-25T10:0{i}:00Z\nSTEP_TIME {st} 5s\nSTEP_EXIT {st} {rc}\n"));
        if rc != 0 {
            s.push_str(&format!("RED_NOW {st} rc=1 at 2026-11-25T10:0{i}:05Z (x)\n"));
        }
    }
    if done {
        let summary = if reds.is_empty() { "none".to_string() } else { reds.iter().map(|r| format!("{r}(rc=1)")).collect::<Vec<_>>().join(" ") };
        s.push_str(&format!("RED_SUMMARY: {summary}\nGATES_DONE\n"));
    }
    s
}

fn setup() -> (Repo, String) {
    let r = repo();
    let sha = sh(&r.origin, &["rev-parse", "main"]);
    (r, sha)
}

fn watch(r: &Repo, e: &Rec, l: &mut Ledger, deploy: Option<&str>, logtext: Option<String>) -> WatchOut {
    let c = cfg::parse(CFG).unwrap();
    let a = WatchArgs { repo: r.p(), deploy_cmd: deploy.map(String::from), who: "watcher".into() };
    watch_once(e, &c.land, &c.watch, &a, l, &|_p: &str| logtext.clone())
}

#[test]
fn a_main_commit_without_a_chain_starts_one_through_a_pushed_non_main_branch() {
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    let o = watch(&r, &e, &mut l, None, None);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(o.started, "{}", o.text);
    let branch = format!("chain/main-{}", &sha[..12]);
    assert_eq!(sh(&r.origin, &["rev-parse", &format!("refs/heads/{branch}")]), sha, "the chain branch is on the remote");
    assert_eq!(sh(&r.origin, &["rev-parse", "main"]), sha, "main is untouched");
    let shells = e.shells.borrow();
    assert_eq!(shells.len(), 1);
    assert_eq!(shells[0], format!("start-chain {branch} --no-stamp # {sha}"));
    assert!(!e.runs.borrow().iter().any(|c| c.contains("push") && c.contains("refs/heads/main")), "never a push to main");
    drop(shells);
    // the next pass finds the start recorded and the log not there yet: it does not start a second chain
    let o = watch(&r, &e, &mut l, None, None);
    assert!(!o.started && o.text.contains("still running"), "{}", o.text);
    assert_eq!(e.shells.borrow().len(), 1);
}

#[test]
fn a_green_log_is_ingested_and_the_deploy_runs_once_for_that_sha_only() {
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    watch(&r, &e, &mut l, Some("deploy {sha}"), None); // starts the chain; no deploy yet
    assert!(!e.shells.borrow().iter().any(|c| c.starts_with("deploy")), "deploy before green");
    let o = watch(&r, &e, &mut l, Some("deploy {sha}"), Some(log(&sha, "full", &[], true)));
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(o.deployed && o.post_fix.is_empty(), "{}", o.text);
    assert!(e.shells.borrow().iter().any(|c| *c == format!("deploy {sha}")), "{:?}", e.shells.borrow());
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM runs WHERE commit_sha=? AND outcome='green'", [&sha], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "the steps are in the ledger");
    let steps: i64 = l.conn.query_row("SELECT COUNT(*) FROM steps", [], |r| r.get(0)).unwrap();
    assert_eq!(steps, 3);
    let again = watch(&r, &e, &mut l, Some("deploy {sha}"), Some(log(&sha, "full", &[], true)));
    assert!(!again.deployed && !again.started, "{}", again.text);
    assert_eq!(e.shells.borrow().iter().filter(|c| c.starts_with("deploy")).count(), 1, "deployed twice");
}

#[test]
fn a_red_log_lists_post_fix_items_with_owners_and_never_deploys() {
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    watch(&r, &e, &mut l, Some("deploy {sha}"), None);
    let o = watch(&r, &e, &mut l, Some("deploy {sha}"), Some(log(&sha, "full", &["lint-ts", "test-api"], true)));
    assert_eq!(o.rc, 1, "{}", o.text);
    assert!(!o.deployed && !e.shells.borrow().iter().any(|c| c.starts_with("deploy")), "deployed a red sha");
    let want = vec![PostFix { sha: sha.clone(), step: "lint-ts".into(), rc: 1, owner: "alice".into() }, PostFix { sha: sha.clone(), step: "test-api".into(), rc: 1, owner: "unassigned".into() }];
    assert_eq!(o.post_fix, want);
    assert!(o.text.contains("post-fix") && o.text.contains("lint-ts") && o.text.contains("owner alice"), "{}", o.text);
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM decisions WHERE kind='post-fix' AND outcome='open'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
    // the next pass lists them again but records them once
    let o2 = watch(&r, &e, &mut l, Some("deploy {sha}"), Some(log(&sha, "full", &["lint-ts", "test-api"], true)));
    assert_eq!(o2.post_fix.len(), 2);
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM decisions WHERE kind='post-fix'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
}

#[test]
fn a_sha_that_already_has_a_full_chain_is_not_started_again() {
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    watch(&r, &e, &mut l, None, None);
    watch(&r, &e, &mut l, None, Some(log(&sha, "full", &[], true)));
    let o = watch(&r, &e, &mut l, None, None);
    assert!(!o.started && o.text.contains("already has a full chain"), "{}", o.text);
    assert_eq!(e.shells.borrow().len(), 1);
}

#[test]
fn a_partial_log_is_not_a_full_chain() {
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    watch(&r, &e, &mut l, Some("deploy {sha}"), None);
    let o = watch(&r, &e, &mut l, Some("deploy {sha}"), Some(log(&sha, "partial", &[], true)));
    assert!(!o.deployed && o.text.contains("not a full chain"), "{}", o.text);
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn without_a_chain_command_it_says_so_and_pushes_nothing() {
    let (r, _sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    let c = cfg::parse("[mainwatch]\n").unwrap();
    let a = WatchArgs { repo: r.p(), deploy_cmd: None, who: "w".into() };
    let o = watch_once(&e, &c.land, &c.watch, &a, &mut l, &|_| None);
    assert_eq!(o.rc, 2, "{}", o.text);
    assert!(o.text.contains("chain_cmd"), "{}", o.text);
    assert!(!e.runs.borrow().iter().any(|c| c.contains(" push ")));
}

fn ingest_elsewhere(l: &mut Ledger, rid: &str, text: &str) {
    // another path (gatetrain ingest, tick) puts a run in the ledger; main-watch did not read that log
    let p = gatetrain_core::logfmt::parse_log(text).unwrap();
    let outcome = if p.red_summary.as_deref() == Some("none") { "green" } else { "red" };
    let r = gatetrain_core::runs::ingest(l, "ledger", rid, &p, outcome, Some("pc"), false, "2026-11-25T10:10:00Z").unwrap();
    assert!(matches!(r, gatetrain_core::runs::Ingest::Done));
}

#[test]
fn a_partial_green_run_ingested_elsewhere_never_counts_as_the_full_chain() {
    // B6: any finished chain run for the sha used to count, so a green --only run could trigger the deploy
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    ingest_elsewhere(&mut l, "only-lint", &log(&sha, "partial", &[], true));
    ingest_elsewhere(&mut l, "no-mode", &log(&sha, "full", &[], true).replace("CHAIN_MODE full\n", ""));
    let o = watch(&r, &e, &mut l, Some("deploy {sha}"), None);
    assert!(!o.deployed && !e.shells.borrow().iter().any(|c| c.starts_with("deploy")), "deployed on a partial run: {}", o.text);
    assert!(o.started, "a full chain is still wanted: {}", o.text);
}

#[test]
fn a_full_run_ingested_elsewhere_does_count() {
    let (r, sha) = setup();
    let (e, mut l) = (Rec::new(), ledger());
    ingest_elsewhere(&mut l, "full-run", &log(&sha, "full", &[], true));
    let o = watch(&r, &e, &mut l, Some("deploy {sha}"), None);
    assert!(!o.started && o.deployed, "{}", o.text);
}
