//! `gatetrain branches`: every branch and worktree with its state, and the limits that turn a pile-up into a red line. Real git in
//! throwaway folders; the scan is read-only, and a recording runner proves it.
mod force_fix;
use force_fix::*;
use gatetrain_core::force::branches::*;
use gatetrain_core::force::cfg::Limits;
use gatetrain_core::force::exec::{Exec, LocalExec, Out};
use std::cell::RefCell;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Rec(RefCell<Vec<String>>);
impl Exec for Rec {
    fn run(&self, argv: &[String]) -> Out {
        self.0.borrow_mut().push(argv.join(" "));
        LocalExec.run(argv)
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        self.0.borrow_mut().push(format!("sh {cwd} {cmd}"));
        LocalExec.sh(cwd, cmd)
    }
}

/// main + worktrees wt-live (1h old, 2 dirty files), wt-old (5 days old), a landed branch (same content on main by another commit),
/// a lone branch, and an archived one (its tip also at origin archive/x).
fn fixture() -> Repo {
    let r = repo();
    let t = now();
    for (name, age) in [("live", 3600), ("old", 5 * 86400)] {
        sh(&r.work, &["checkout", "-q", "-b", name, "main"]);
        write(&r.work, &format!("{name}.txt"), "x\n");
        sh(&r.work, &["add", "."]);
        sh_dated(&r.work, &["commit", "-m", name], t - age);
        sh(&r.work, &["checkout", "-q", "main"]);
        let wt = r.tmp.path().join(format!("wt-{name}"));
        sh(&r.work, &["worktree", "add", wt.to_str().unwrap(), name]);
    }
    write(&r.tmp.path().join("wt-live"), "a.txt", "1\n");
    write(&r.tmp.path().join("wt-live"), "b.txt", "2\n");
    // landed: the branch's content reaches main by a different commit
    sh(&r.work, &["checkout", "-q", "-b", "landed", "main"]);
    write(&r.work, "l.txt", "same\n");
    sh(&r.work, &["add", "l.txt"]);
    sh_dated(&r.work, &["commit", "-m", "landed work"], t - 3 * 86400);
    sh(&r.work, &["checkout", "-q", "main"]);
    write(&r.work, "l.txt", "same\n");
    sh(&r.work, &["add", "l.txt"]);
    sh(&r.work, &["commit", "-m", "squash of landed"]);
    sh(&r.work, &["push", "origin", "main"]);
    // lone
    sh(&r.work, &["checkout", "-q", "-b", "lone", "main"]);
    write(&r.work, "lone.txt", "x\n");
    sh(&r.work, &["add", "."]);
    sh_dated(&r.work, &["commit", "-m", "lone"], t - 3600);
    sh(&r.work, &["checkout", "-q", "main"]);
    // archived
    sh(&r.work, &["checkout", "-q", "-b", "gone", "main"]);
    write(&r.work, "gone.txt", "x\n");
    sh(&r.work, &["add", "."]);
    sh_dated(&r.work, &["commit", "-m", "gone"], t - 4 * 86400);
    sh(&r.work, &["push", "origin", "gone:refs/heads/archive/x"]);
    sh(&r.work, &["checkout", "-q", "main"]);
    r
}

fn hs<'a>(_r: &'a Repo, e: &'a dyn Exec, repo: &'a str) -> HostScan<'a> {
    HostScan { host: "pc", e, repo, main: "main", remote: "origin", prefix: "wt-", win: false, stale_days: 2.0, sizes: false }
}

fn row<'a>(rows: &'a [BranchRow], name: &str) -> &'a BranchRow {
    rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row {name}: {:?}", rows.iter().map(|r| &r.name).collect::<Vec<_>>()))
}

#[test]
fn every_branch_and_worktree_gets_a_state_owner_and_counts() {
    let r = fixture();
    let p = r.p();
    let owner = |b: &str| (b == "live").then(|| "alice".to_string());
    let rows = scan(&hs(&r, &LocalExec, &p), now(), &owner).unwrap();
    let live = row(&rows, "wt-live");
    assert_eq!((live.kind.as_str(), live.state.as_str(), live.owner.as_str(), live.dirty, live.ahead, live.behind), ("worktree", "live", "alice", 2, 1, 1));
    let old = row(&rows, "wt-old");
    assert_eq!((old.state.as_str(), old.owner.as_str(), old.dirty), ("stale", "old", 0));
    assert!(old.unlanded_hours.unwrap() > 100.0 && !old.on_main);
    let landed = row(&rows, "landed");
    assert_eq!((landed.kind.as_str(), landed.state.as_str(), landed.on_main), ("branch", "landed", true));
    assert_eq!(row(&rows, "lone").state, "live");
    assert_eq!(row(&rows, "gone").state, "archived");
    // the main checkout is not a row, and a branch held by a worktree is one row, not two
    assert!(!rows.iter().any(|x| x.name == "main" || x.name == "live" || x.name == "old"));
}

#[test]
fn remote_only_branches_are_rows_of_the_remote_host() {
    let r = fixture();
    sh(&r.work, &["push", "origin", "lone:refs/heads/lone"]);
    sh(&r.work, &["push", "origin", "lone:refs/heads/remote-only"]);
    sh(&r.work, &["fetch", "-q", "origin"]);
    let p = r.p();
    let rows = scan_remote(&hs(&r, &LocalExec, &p), now(), &["lone".to_string()], &|_| None).unwrap();
    assert!(rows.iter().all(|x| x.host == "remote" && x.kind == "remote"));
    assert!(rows.iter().any(|x| x.name == "origin/remote-only"));
    assert!(!rows.iter().any(|x| x.name == "origin/lone"), "a branch that is also local is not listed twice");
    assert_eq!(row(&rows, "origin/archive/x").state, "archived");
}

#[test]
fn the_scan_is_read_only() {
    let r = fixture();
    let p = r.p();
    let before = (sh(&r.work, &["for-each-ref"]), sh(&r.work, &["worktree", "list"]), sh(&r.work, &["status", "--porcelain"]));
    let rec = Rec(RefCell::new(vec![]));
    let _ = scan(&hs(&r, &rec, &p), now(), &|_| None).unwrap();
    let after = (sh(&r.work, &["for-each-ref"]), sh(&r.work, &["worktree", "list"]), sh(&r.work, &["status", "--porcelain"]));
    assert_eq!(before, after);
    let allowed = ["worktree list", "for-each-ref", "rev-list", "rev-parse", "merge-tree", "status", "log", "merge-base", "diff", "show-ref", "config"];
    let calls = rec.0.borrow();
    assert!(calls.len() > 10, "the scan ran no commands: {calls:?}");
    for c in calls.iter() {
        assert!(c.starts_with("git -C "), "only git: {c}");
        let sub = c.splitn(4, ' ').nth(3).unwrap_or("");
        assert!(allowed.iter().any(|a| sub.starts_with(a)), "a command that could change something: {c}");
    }
}

#[test]
fn limits_become_breaches_each_with_its_fix() {
    let r = fixture();
    let p = r.p();
    let rows = scan(&hs(&r, &LocalExec, &p), now(), &|_| None).unwrap();
    let lim = Limits { max_worktrees: 1, max_idle_days: 2.0, max_worktree_gb: 20.0, max_unlanded_hours: 24.0 };
    let b = breaches(&rows, &lim);
    let get = |rule: &str, item: &str| b.iter().find(|x| x.rule == rule && x.item == item).cloned();
    let w = get("max_worktrees", "pc").expect("worktree count breach");
    assert!(w.detail.contains("2 worktrees") && w.fix.contains("gatetrain tidy --force"), "{w:?}");
    let idle = get("max_idle_days", "wt-old").expect("idle breach");
    assert!(idle.fix.contains("gatetrain land old"), "{idle:?}");
    assert!(get("max_unlanded_hours", "wt-old").is_some());
    assert!(get("max_idle_days", "landed").is_none() && get("max_idle_days", "gone").is_none(), "landed and archived work is not idle work");
    assert!(get("max_idle_days", "wt-live").is_none());
    // defaults: ten worktrees, two days, twenty GB, one day
    assert!(breaches(&rows, &Limits::default()).iter().all(|x| x.rule != "max_worktrees"));
    // a worktree over its size limit
    let mut big = row(&rows, "wt-live").clone();
    big.size_bytes = Some(21 * 1024 * 1024 * 1024);
    let b = breaches(&[big], &Limits::default());
    assert!(b.iter().any(|x| x.rule == "max_worktree_gb" && x.fix.contains("gatetrain tidy --force")), "{b:?}");
}

#[test]
fn the_table_and_json_carry_every_column() {
    let r = fixture();
    let p = r.p();
    let rows = scan(&hs(&r, &LocalExec, &p), now(), &|_| None).unwrap();
    let b = breaches(&rows, &Limits { max_worktrees: 1, ..Limits::default() });
    let t = table(&rows, &b);
    for w in ["host", "state", "owner", "ahead", "behind", "dirty", "wt-old", "stale", "landed", "BREACH", "max_worktrees"] {
        assert!(t.contains(w), "{w} missing from\n{t}");
    }
    let j = to_json(&rows, &b);
    assert_eq!(j["rows"].as_array().unwrap().len(), rows.len());
    assert!(j["rows"][0]["host"] == "pc" && j["rows"][0].get("state").is_some() && j["breaches"].as_array().unwrap().len() == b.len());
}

#[test]
fn the_ledger_names_the_owner_of_a_landed_branch() {
    use gatetrain_core::declog::{self, NewEntry};
    let mut l = gatetrain_core::ledger::Ledger::open_in_memory().unwrap();
    l.migrate().unwrap();
    assert_eq!(ledger_owner(&l, "feat"), None);
    declog::append(&mut l, &NewEntry { at: "2026-11-25T00:00:00Z".into(), actor: "dana".into(), kind: "land".into(), subject: Some("feat".into()), outcome: "landed".into(), reason: "x".into(), ..Default::default() }).unwrap();
    assert_eq!(ledger_owner(&l, "feat").as_deref(), Some("dana"));
}

#[test]
fn robocopy_byte_totals_are_read_from_its_summary() {
    let out = "\n               Total    Copied   Skipped  Mismatch    FAILED    Extras\n    Dirs :        12         0        12         0         0         0\n   Files :       340         0       340         0         0         0\n   Bytes :  52428800         0  52428800         0         0         0\n";
    assert_eq!(parse_robocopy_bytes(out), Some(52_428_800));
    assert_eq!(parse_robocopy_bytes("nothing here"), None);
}
