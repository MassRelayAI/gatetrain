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

/// The scratch worktrees of these tests hold no links, so a plain removal of the folder is enough here; the link-safe one is tested in force_real.rs.
fn rm(p: &str) -> Result<(), String> {
    std::fs::remove_dir_all(p).map_err(|e| e.to_string())
}

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
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", true), None, &rm);
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
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), Some(&mut l), &rm);
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
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), Some(&mut l), &rm);
    assert_eq!(o.rc, 4, "{}", o.text);
    assert!(o.text.contains("missing: independent-read verdict") && o.text.contains(&tip), "{}", o.text);
    assert_eq!(origin_main(&r), before);
    // a verdict for the tip lets it land; a later fail verdict takes it away again
    record_verdict(&mut l, &tip, "bob", true, "read the diff").unwrap();
    assert_eq!(verdict_for(&l, &tip).as_deref(), Some("bob"));
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), Some(&mut l), &rm);
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
    let o = land(&LocalExec, &c, &args(&r, "feat", false), Some(&mut l), &rm);
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
    let o = land(&LocalExec, &c, &args(&r, "feat", false), None, &rm);
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
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), None, &rm);
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
    let o = land(&race, &lcfg(), &args(&r, "feat", false), None, &rm);
    assert_eq!(o.rc, 0, "{}", o.text);
    let tree = sh(&r.origin, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.contains("docs/x.md") && tree.contains("other.txt"), "{tree}");
    assert_eq!(origin_main(&r), o.merged.unwrap());
    let _ = git(&LocalExec, &r.p(), &["status"]);
    let _ = NewEntry::default();
    let _ = declog::GENESIS;
}

#[test]
fn a_migration_renamed_into_docs_is_still_safe() {
    // B3: `git diff --name-only` prints only the new name of a rename, and docs/** is FAST
    let r = repo();
    write(&r.work, "apps/db/migrations/0001.sql", "create table t(x int);\n");
    sh(&r.work, &["add", "."]);
    sh(&r.work, &["commit", "-m", "add a migration"]);
    sh(&r.work, &["push", "origin", "main"]);
    sh(&r.work, &["checkout", "-q", "-b", "mv", "main"]);
    sh(&r.work, &["mv", "apps/db/migrations/0001.sql", "docs-0001.sql"]);
    std::fs::create_dir_all(r.work.join("docs")).unwrap();
    sh(&r.work, &["mv", "docs-0001.sql", "docs/0001.sql"]);
    sh(&r.work, &["commit", "-am", "move it out of the migrations folder"]);
    sh(&r.work, &["checkout", "-q", "main"]);
    let o = land(&LocalExec, &lcfg(), &args(&r, "mv", true), None, &rm);
    assert!(o.text.contains("mode: SAFE"), "{}", o.text);
    assert!(o.text.contains("apps/db/migrations/0001.sql") && o.text.contains("docs/0001.sql"), "both sides of the rename are classified:\n{}", o.text);
}

/// Records every shell line instead of running it.
struct NoShell(std::cell::RefCell<Vec<String>>);
impl Exec for NoShell {
    fn run(&self, argv: &[String]) -> Out {
        LocalExec.run(argv)
    }
    fn sh(&self, _cwd: &str, cmd: &str) -> Out {
        self.0.borrow_mut().push(cmd.to_string());
        Out::new(0, "")
    }
}

#[test]
fn a_file_name_with_shell_syntax_never_reaches_a_shell_line() {
    // B4: `docs/x$(cmd).md` was put inside plain double quotes of the check line
    let r = repo();
    branch(&r, "feat", &["docs/x$(cmd).md"]);
    let before = origin_main(&r);
    let mut c = lcfg();
    c.checks = vec!["lint {files}".into()];
    let e = NoShell(Default::default());
    let o = land(&e, &c, &args(&r, "feat", false), None, &rm);
    assert_eq!(o.rc, 4, "{}", o.text);
    assert!(o.text.contains("unsafe") && o.text.contains("docs/x$(cmd).md"), "{}", o.text);
    assert!(e.0.borrow().is_empty(), "a check line was built from the name: {:?}", e.0.borrow());
    assert_eq!(origin_main(&r), before);
    assert_eq!(sh(&r.work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
}

#[test]
fn a_branch_name_with_shell_syntax_is_refused_when_a_check_uses_it() {
    let r = repo();
    branch(&r, "feat$(x)", &["docs/a.md"]);
    let mut c = lcfg();
    c.checks = vec!["lint {branch}".into()];
    let e = NoShell(Default::default());
    let o = land(&e, &c, &args(&r, "feat$(x)", false), None, &rm);
    assert_eq!(o.rc, 4, "{}", o.text);
    assert!(e.0.borrow().is_empty(), "{:?}", e.0.borrow());
    // and a check that uses neither placeholder is not affected by an odd name
    let mut c = lcfg();
    c.checks = vec!["true".into()];
    let o = land(&e, &c, &args(&r, "feat$(x)", false), None, &rm);
    assert_eq!(o.rc, 0, "{}", o.text);
}

/// Remembers whether the remover has run; the git call that unregisters the scratch worktree must come after it.
struct Order<'a> {
    removed: &'a Cell<bool>,
    seen: std::cell::RefCell<Vec<String>>,
}
impl Exec for Order<'_> {
    fn run(&self, argv: &[String]) -> Out {
        if argv.iter().any(|a| a == "worktree") && argv.iter().any(|a| a == "remove") {
            self.seen.borrow_mut().push(format!("git worktree remove (folder removed first: {})", self.removed.get()));
            assert!(self.removed.get(), "git worktree remove ran before the link-safe remover: {argv:?}");
        }
        LocalExec.run(argv)
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        LocalExec.sh(cwd, cmd)
    }
}

#[test]
fn the_scratch_folder_goes_by_the_link_safe_remover_before_git_unregisters_it() {
    // B5: `git worktree remove` can follow a junction the checks created; the folder is removed by tidy's removal first
    let r = repo();
    branch(&r, "feat", &["docs/x.md"]);
    let removed = Cell::new(false);
    let e = Order { removed: &removed, seen: Default::default() };
    let paths = std::cell::RefCell::new(vec![]);
    let remover = |p: &str| -> Result<(), String> {
        paths.borrow_mut().push(p.to_string());
        let r = rm(p);
        removed.set(true);
        r
    };
    let o = land(&e, &lcfg(), &args(&r, "feat", false), None, &remover);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert_eq!(paths.borrow().len(), 1);
    assert!(paths.borrow()[0].replace('\\', "/").rsplit('/').next().unwrap().starts_with("land-"), "{:?}", paths.borrow());
    assert_eq!(e.seen.borrow().len(), 1, "git was never told the worktree is gone");
    assert_eq!(sh(&r.work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
}

#[test]
fn a_remover_that_refuses_leaves_the_scratch_folder_and_still_reports_the_landing() {
    let r = repo();
    branch(&r, "feat", &["docs/x.md"]);
    let o = land(&LocalExec, &lcfg(), &args(&r, "feat", false), None, &|_p: &str| Err("a link target is gone".to_string()));
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(o.text.contains("left in place") && o.text.contains("a link target is gone"), "{}", o.text);
    assert_eq!(sh(&r.origin, &["rev-parse", "main"]), o.merged.unwrap());
    assert!(std::fs::read_dir(r.tmp.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("land-")), "the folder must not be force-deleted by anything else");
}

#[test]
fn files_reach_a_check_after_a_double_dash() {
    // a name that starts with a dash must not be read as an option by the check
    let r = repo();
    branch(&r, "feat", &["docs/-rf.md"]);
    let mut c = lcfg();
    c.checks = vec!["lint {files}".into()];
    let e = NoShell(Default::default());
    let o = land(&e, &c, &args(&r, "feat", false), None, &rm);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert_eq!(e.0.borrow()[0], "lint -- \"docs/-rf.md\"");
}
