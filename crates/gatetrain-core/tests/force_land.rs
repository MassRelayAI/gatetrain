//! `gatetrain land`: the path classifier (FAST / SAFE, fail closed) and the landing itself, on real git in throwaway folders.
mod force_fix;
use force_fix::*;
use gatetrain_core::declog::{self, NewEntry};
use gatetrain_core::force::cfg::{self, Mode};
use gatetrain_core::force::exec::{git, Exec, LocalExec, Out};
use gatetrain_core::force::land::{classify, land, record_verdict, verdict_for, LandArgs};
use gatetrain_core::ledger::Ledger;
use std::cell::Cell;

const CFG: &str = r#"
[land]
checks = ["git status --short"]
safe_gates = ["git log -1 --oneline"]
[[land.rules]]
glob = "docs/**"
mode = "fast"
[[land.rules]]
glob = "*.md"
mode = "fast"
[[land.rules]]
glob = "**/migrations/**"
mode = "safe"
[[land.rules]]
glob = "crates/mr-kernel/**"
mode = "safe"
gates = ["git log -1 --oneline"]
"#;

fn lcfg() -> cfg::LandCfg {
    cfg::parse(CFG).unwrap().land
}

fn files(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn ledger() -> Ledger {
    let mut l = Ledger::open_in_memory().unwrap();
    l.migrate().unwrap();
    l
}

#[test]
fn a_migration_file_is_safe() {
    let (m, rows) = classify(&lcfg(), &files(&["apps/api/src/db/migrations/0042_x.sql"]));
    assert_eq!(m, Mode::Safe);
    assert_eq!(rows[0].rule, "**/migrations/**");
}

#[test]
fn a_docs_only_change_is_fast() {
    let (m, rows) = classify(&lcfg(), &files(&["docs/a/b.md", "README.md"]));
    assert_eq!(m, Mode::Fast);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].rule, "docs/**");
}

#[test]
fn an_unknown_path_is_safe_and_one_safe_file_makes_the_change_safe() {
    let (m, rows) = classify(&lcfg(), &files(&["tools/new-thing.sh"]));
    assert_eq!(m, Mode::Safe);
    assert!(rows[0].rule.contains("no rule"), "{}", rows[0].rule);
    let (m, _) = classify(&lcfg(), &files(&["docs/a.md", "crates/mr-kernel/src/gates.rs"]));
    assert_eq!(m, Mode::Safe);
    // with no rules at all, everything is safe
    let (m, _) = classify(&cfg::LandCfg::default(), &files(&["docs/a.md"]));
    assert_eq!(m, Mode::Safe);
}

#[test]
fn the_most_careful_matching_rule_wins() {
    let c = cfg::parse("[[land.rules]]\nglob=\"docs/**\"\nmode=\"fast\"\n[[land.rules]]\nglob=\"docs/migrations/**\"\nmode=\"safe\"\n").unwrap().land;
    let (m, rows) = classify(&c, &files(&["docs/migrations/x.md"]));
    assert_eq!((m, rows[0].rule.as_str()), (Mode::Safe, "docs/migrations/**"));
}

fn args(r: &Repo, b: &str, plan: bool) -> LandArgs {
    LandArgs { repo: r.p(), branch: b.into(), plan, who: "alice".into() }
}

fn origin_main(r: &Repo) -> String {
    sh(&r.origin, &["rev-parse", "main"])
}

#[test]
fn plan_prints_the_mode_and_the_rule_per_file_and_changes_nothing() {
    let r = repo();
    branch(&r, "feat", &["docs/x.md", "crates/mr-kernel/a.rs"]);
    let before = origin_main(&r);
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", true), None);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(o.text.contains("mode: SAFE"), "{}", o.text);
    assert!(o.text.contains("docs/x.md") && o.text.contains("docs/**") && o.text.contains("crates/mr-kernel/**"), "{}", o.text);
    assert_eq!(origin_main(&r), before);
}

#[test]
fn a_fast_branch_lands_records_and_leaves_no_scratch_worktree() {
    let r = repo();
    let tip = branch(&r, "feat", &["docs/x.md"]);
    let mut l = ledger();
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), Some(&mut l));
    assert_eq!(o.rc, 0, "{}", o.text);
    let merged = o.merged.clone().unwrap();
    assert_eq!(origin_main(&r), merged);
    assert!(sh(&r.origin, &["ls-tree", "-r", "--name-only", "main"]).contains("docs/x.md"));
    let rows: Vec<String> = l
        .conn
        .prepare("SELECT subject||'|'||actor||'|'||outcome||'|'||evidence FROM decisions WHERE kind='land'")
        .unwrap()
        .query_map([], |x| x.get(0))
        .unwrap()
        .map(|x| x.unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].starts_with("feat|alice|landed|"), "{}", rows[0]);
    assert!(rows[0].contains(&tip) && rows[0].contains(&merged) && rows[0].contains("git status --short"), "{}", rows[0]);
    // the scratch worktree is gone: one worktree left, and no land-* folder beside the repo
    assert_eq!(sh(&r.work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    assert!(!std::fs::read_dir(r.tmp.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("land-")));
}

#[test]
fn safe_without_a_verdict_is_refused_naming_the_missing_item() {
    let r = repo();
    let tip = branch(&r, "feat", &["crates/mr-kernel/a.rs"]);
    let before = origin_main(&r);
    let mut l = ledger();
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), Some(&mut l));
    assert_eq!(o.rc, 4, "{}", o.text);
    assert!(o.text.contains("missing: independent-read verdict") && o.text.contains(&tip), "{}", o.text);
    assert_eq!(origin_main(&r), before);
    // a verdict for the tip lets it land; a later fail verdict takes it away again
    record_verdict(&mut l, &tip, "bob", true, "read the diff").unwrap();
    assert_eq!(verdict_for(&l, &tip).as_deref(), Some("bob"));
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), Some(&mut l));
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(o.text.contains("mode: SAFE"), "{}", o.text);
    let tip2 = branch(&r, "feat2", &["tools/y.sh"]);
    record_verdict(&mut l, &tip2, "bob", true, "ok").unwrap();
    record_verdict(&mut l, &tip2, "carol", false, "found a hole").unwrap();
    assert_eq!(verdict_for(&l, &tip2), None);
}

#[test]
fn safe_with_no_gate_configured_is_refused_too() {
    let r = repo();
    let tip = branch(&r, "feat", &["tools/y.sh"]);
    let mut l = ledger();
    record_verdict(&mut l, &tip, "bob", true, "ok").unwrap();
    let c = cfg::parse("[land]\nchecks=[\"git status\"]\n").unwrap().land;
    let o = land(&LocalExec, &c, &args(&r, "feat", false), Some(&mut l));
    assert_eq!(o.rc, 4, "{}", o.text);
    assert!(o.text.contains("missing: a full gate"), "{}", o.text);
}

#[test]
fn a_red_check_stops_before_the_push() {
    let r = repo();
    branch(&r, "feat", &["docs/x.md"]);
    let before = origin_main(&r);
    let mut c = lcfg();
    c.checks = vec!["git no-such-subcommand".into()];
    let o = land(&LocalExec, &c, &args(&r, "feat", false), None);
    assert_eq!(o.rc, 6, "{}", o.text);
    assert_eq!(origin_main(&r), before);
    assert_eq!(sh(&r.work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
}

#[test]
fn a_conflict_is_refused_and_cleaned() {
    let r = repo();
    branch(&r, "feat", &["docs/x.md"]);
    write(&r.work, "docs/x.md", "main wrote this first\n");
    sh(&r.work, &["add", "docs/x.md"]);
    sh(&r.work, &["commit", "-m", "main edits x"]);
    sh(&r.work, &["push", "origin", "main"]);
    let before = origin_main(&r);
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), None);
    assert_eq!(o.rc, 5, "{}", o.text);
    assert_eq!(origin_main(&r), before);
    assert_eq!(sh(&r.work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
}

/// Another writer pushes to main between our merge and our push: the push is rejected, we fetch, re-merge and push again; never forced.
struct Race<'a> {
    other: String,
    done: Cell<bool>,
    _r: &'a Repo,
}

impl Exec for Race<'_> {
    fn run(&self, argv: &[String]) -> Out {
        if !self.done.get() && argv.iter().any(|a| a == "push") && argv.iter().any(|a| a.starts_with("HEAD:")) {
            self.done.set(true);
            let o = std::path::Path::new(&self.other);
            write(o, "other.txt", "someone else\n");
            sh(o, &["add", "other.txt"]);
            sh(o, &["commit", "-m", "other work"]);
            sh(o, &["push", "origin", "main"]);
        }
        assert!(!argv.iter().any(|x| x == "push") || !argv.iter().any(|a| a == "--force" || a == "-f" || a.starts_with("--force-")), "never force: {argv:?}");
        LocalExec.run(argv)
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        LocalExec.sh(cwd, cmd)
    }
}

#[test]
fn a_rejected_push_is_fetched_re_merged_and_retried_without_force() {
    let r = repo();
    branch(&r, "feat", &["docs/x.md"]);
    let other = r.tmp.path().join("other");
    sh(r.tmp.path(), &["clone", "-q", r.origin.to_str().unwrap(), "other"]);
    let race = Race { other: other.to_string_lossy().into_owned(), done: Cell::new(false), _r: &r };
    let o = land(&race, &lcfg(), &args(&r, "feat", false), None);
    assert_eq!(o.rc, 0, "{}", o.text);
    let tree = sh(&r.origin, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.contains("docs/x.md") && tree.contains("other.txt"), "{tree}");
    assert_eq!(origin_main(&r), o.merged.unwrap());
    let _ = git(&LocalExec, &r.p(), &["status"]);
    let _ = NewEntry::default();
    let _ = declog::GENESIS;
}
