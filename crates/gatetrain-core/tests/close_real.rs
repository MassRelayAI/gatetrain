//! T9 close in production (`OpsEnv` over the real local shell) against a real file system: temp directories, temp git repositories,
//! real junctions (Windows, `mklink /J`) and symlinks (Unix). A path recorded by another spelling than the tidy config's (a link into
//! the main checkout, into a folder that holds a worktree, out of every root, or a main checkout written through a link) is judged by
//! where it really leads, and nothing is moved (review round 2, BL1 and MA5).
mod common;
use common::to_table;
use gatetrain_core::config::{Config, Host};
use gatetrain_core::tidy::close::{run_close, CloseReport, OpsEnv, OwnedItem, OwnedSet};
use gatetrain_core::tidy::config::build_cfg;
use gatetrain_core::tidy::{LocalRunner, Ops, Runner};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn fwd(p: &Path) -> String {
    let s = p.canonicalize().unwrap().to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").map(String::from).unwrap_or(s)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write(p: impl AsRef<Path>, text: &str) {
    if let Some(d) = p.as_ref().parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, text).unwrap();
}

/// A junction on Windows (no privilege needed), a symlink on Unix.
fn dir_link(link: &Path, target: &Path) {
    if cfg!(windows) {
        let o = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link.to_string_lossy().replace('/', "\\")).arg(target.to_string_lossy().replace('/', "\\")).output().unwrap();
        assert!(o.status.success(), "mklink /J failed: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    } else {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

/// Every file below `p` (outside `.git` folders) written three days ago: nothing is "in use" by the 60-minute rule, so only the guard
/// under test can keep it.
fn age(p: &Path) {
    for e in std::fs::read_dir(p).unwrap() {
        let e = e.unwrap();
        let t = e.file_type().unwrap();
        if t.is_dir() && e.file_name() != ".git" {
            age(&e.path());
        } else if t.is_file() {
            let f = std::fs::File::options().write(true).open(e.path()).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86400)).unwrap();
        }
    }
}

fn main_repo(root: &Path) -> std::path::PathBuf {
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    write(main.join("shared/keep.txt"), "tracked\n");
    write(main.join("f.txt"), "one\n");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "base"]);
    age(&main);
    main
}

struct Fx {
    _d: tempfile::TempDir,
    root: std::path::PathBuf,
    main: std::path::PathBuf,
    build: std::path::PathBuf,
}

fn fx() -> Fx {
    let d = tempfile::Builder::new().prefix("gt-close-real-").tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let main = main_repo(&root);
    let build = root.join("build");
    std::fs::create_dir_all(&build).unwrap();
    std::fs::create_dir_all(root.join("trash")).unwrap();
    Fx { _d: d, root, main, build }
}

impl Fx {
    /// The tidy config of a real local host; `main_checkout` as given (a spelling, not necessarily the resolved one).
    fn tidy(&self, main_checkout: &str) -> Value {
        let mut tidy = json!({"main_checkout": main_checkout, "trash_dir": fwd(&self.root) + "/trash", "log": fwd(&self.root) + "/tidy.jsonl"});
        if cfg!(windows) {
            // a Windows host proves its link listing works: a folder holding one known junction
            let c = self.root.join("canary");
            std::fs::create_dir_all(self.root.join("ctarget")).unwrap();
            std::fs::create_dir_all(&c).unwrap();
            if !c.join("l").exists() {
                dir_link(&c.join("l"), &self.root.join("ctarget"));
            }
            let o = tidy.as_object_mut().unwrap();
            o.insert("link_canary".into(), json!(fwd(&c)));
            o.insert("link_canary_count".into(), json!(1));
        }
        tidy
    }

    fn close(&self, main_checkout: &str, kind: &str, key: &str, roots: &[String]) -> CloseReport {
        self.close_with(main_checkout, kind, key, roots, json!({}), &LocalRunner)
    }

    /// `extra` keys are added to the tidy config; `runner` stands between close and the real local shell.
    fn close_with(&self, main_checkout: &str, kind: &str, key: &str, roots: &[String], extra: Value, runner: &dyn Runner) -> CloseReport {
        let mut tidy = self.tidy(main_checkout);
        for (k, v) in extra.as_object().unwrap() {
            tidy.as_object_mut().unwrap().insert(k.clone(), v.clone());
        }
        let h = Host { name: "real".into(), tidy: to_table(&tidy), ..Default::default() };
        let cfg = Config { hosts: vec![h.clone()], ..Default::default() };
        let t = build_cfg(&cfg, &h).unwrap();
        let ops = Ops::new(&h, runner, &t);
        let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
        let set = OwnedSet { items: vec![OwnedItem::new(kind, key)], branches: vec![], chain_checkout: None, roots: roots.to_vec(), shared_targets: vec![] };
        run_close(&set, &env)
    }

    fn trash(&self) -> Vec<String> {
        std::fs::read_dir(self.root.join("trash")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }
}

fn refused_with(rep: &CloseReport, code: &str) -> bool {
    rep.done.is_empty() && rep.refusals.iter().any(|r| r.code == code)
}

#[test]
fn a_link_into_the_main_checkout_never_moves_a_folder_of_it() {
    let f = fx();
    dir_link(&f.build.join("lnk"), &f.main);
    let key = fwd(&f.build) + "/lnk/shared";
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(f.main.join("shared/keep.txt").exists(), "a tracked folder of the main checkout was moved through a link: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.main-checkout"), "{rep:?}");
    assert!(f.trash().is_empty(), "{:?}", f.trash());
}

#[test]
fn a_main_checkout_written_through_a_link_still_covers_its_folders() {
    let f = fx();
    dir_link(&f.root.join("alias"), &f.main);
    let spelled = fwd(&f.root) + "/alias"; // the tidy config names main by a link; the recorded key is the real path
    let key = fwd(&f.main) + "/shared";
    let rep = f.close(&spelled, "target", &key, &[fwd(&f.main)]);
    assert!(f.main.join("shared/keep.txt").exists(), "moved: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.main-checkout"), "{rep:?}");
}

#[test]
fn a_link_into_a_folder_that_holds_a_worktree_never_moves_it() {
    let f = fx();
    let wts = f.build.join("wts");
    std::fs::create_dir_all(wts.join("holder")).unwrap();
    let wt = wts.join("holder/wt1");
    git(&f.main, &["worktree", "add", "-q", "-b", "line/x", &(fwd(&wts.join("holder")) + "/wt1")]);
    age(&wts);
    dir_link(&f.build.join("lnk"), &wts);
    let key = fwd(&f.build) + "/lnk/holder"; // under the root as written and as resolved; the worktree is inside it only as resolved
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(wt.join("f.txt").exists(), "a registered worktree was moved with the folder that holds it: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned") && rep.refusals.iter().any(|r| r.why.contains("worktree")), "{rep:?}");
}

#[test]
fn a_link_out_of_every_root_is_not_owned() {
    let f = fx();
    let away = f.root.join("elsewhere");
    write(away.join("stuff/f.txt"), "not the batch's\n");
    age(&away);
    dir_link(&f.build.join("lnk"), &away);
    let key = fwd(&f.build) + "/lnk/stuff"; // under the root as written, outside it as resolved
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(away.join("stuff/f.txt").exists(), "moved: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

#[test]
fn a_plain_folder_inside_a_root_is_still_moved() {
    // the guards above must not refuse everything: the batch's own recorded folder, written as it is, goes
    let f = fx();
    write(f.build.join("tgt-b1/debug/x.o"), "obj\n");
    age(&f.build);
    let key = fwd(&f.build) + "/tgt-b1";
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(rep.clean() && rep.done == vec![key.clone()], "{rep:?}");
    assert!(!f.build.join("tgt-b1").exists());
    assert_eq!(f.trash().len(), 1, "{:?}", f.trash());
}

// ---- review round 3

/// A lane worktree `<build>/wt-live` on `line/live`: one commit that is not on main, one uncommitted change in the tracked `src/`, and
/// an ignored `target/` that holds only build output.
fn live_worktree(f: &Fx) -> std::path::PathBuf {
    let wt = f.build.join("wt-live");
    git(&f.main, &["worktree", "add", "-q", "-b", "line/live", &(fwd(&f.build) + "/wt-live")]);
    write(wt.join(".gitignore"), "target/
");
    write(wt.join("src/a.rs"), "fn a() {}
");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", "live work"]);
    write(wt.join("src/a.rs"), "fn a() { /* not committed */ }
");
    write(wt.join("target/debug/x.o"), "obj
");
    age(&wt);
    wt
}

#[test]
fn bl3_a_recorded_target_that_is_a_dirty_unmerged_worktree_is_not_moved() {
    let f = fx();
    let wt = live_worktree(&f);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/wt-live"), &[fwd(&f.build)]);
    assert!(wt.join("src/a.rs").exists(), "a live worktree was moved as a target: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

#[test]
fn bl3_a_recorded_tmp_that_is_a_locked_kept_worktree_is_not_moved() {
    let f = fx();
    let wt = live_worktree(&f);
    git(&f.main, &["worktree", "lock", &(fwd(&f.build) + "/wt-live")]);
    let extra = json!({"keep_lanes": ["live"], "worktree_prefix": "wt-", "keep_branches": ["line/live"]});
    let rep = f.close_with(&fwd(&f.main), "tmp", &(fwd(&f.build) + "/wt-live"), &[fwd(&f.build)], extra, &LocalRunner);
    assert!(wt.join("src/a.rs").exists(), "a locked, kept worktree was moved as a temp folder: {rep:?}, trash {:?}", f.trash());
    assert!(rep.done.is_empty() && !rep.refusals.is_empty(), "{rep:?}");
}

#[test]
fn bl3_a_tracked_folder_of_a_worktree_with_an_uncommitted_file_is_not_moved() {
    let f = fx();
    let wt = live_worktree(&f);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/wt-live/src"), &[fwd(&f.build)]);
    assert!(wt.join("src/a.rs").exists(), "a tracked folder of a worktree was moved: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

#[test]
fn bl3_an_ignored_build_folder_inside_a_worktree_is_still_moved() {
    // the common case: a lane's own target/ (ignored, nothing tracked) goes
    let f = fx();
    let wt = live_worktree(&f);
    let key = fwd(&f.build) + "/wt-live/target";
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(rep.clean() && rep.done == vec![key.clone()], "{rep:?}");
    assert!(!wt.join("target").exists() && wt.join("src/a.rs").exists());
}

#[test]
fn minor5_a_worktree_registered_through_a_link_is_seen_inside_the_folder_that_holds_it() {
    // added through a link; git (2.4x on Windows) records it resolved, so this guards the item side. The worktree side, a worktree
    // registered under a linked spelling, is the Sim test `minor5_a_worktree_registered_under_a_linked_spelling_is_seen_inside_its_folder`
    let f = fx();
    let wts = f.build.join("wts");
    std::fs::create_dir_all(wts.join("holder")).unwrap();
    dir_link(&f.build.join("lnkwts"), &wts);
    git(&f.main, &["worktree", "add", "-q", "-b", "line/y", &(fwd(&f.build) + "/lnkwts/holder/wt1")]);
    age(&wts);
    let listed = git(&f.main, &["worktree", "list", "--porcelain"]);
    let key = fwd(&wts) + "/holder";
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(wts.join("holder/wt1/f.txt").exists(), "moved: {rep:?}; git lists {listed}");
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}; git lists {listed}");
}

/// The real local shell, with one twist: right after the second idle probe (the last look of the second `judge`), the link `link` is
/// pointed at `to`. That is a writer that retargets a link between the judgement and the move, made deterministic.
struct Retarget {
    link: std::path::PathBuf,
    to: std::path::PathBuf,
    probes: std::cell::Cell<usize>,
}

impl Runner for Retarget {
    fn is_local(&self) -> bool {
        LocalRunner.is_local()
    }
    fn run_for(&self, cmd: &str, timeout: Option<f64>) -> Result<gatetrain_core::tidy::RunOut, String> {
        let out = LocalRunner.run_for(cmd, timeout);
        if cmd.contains("AddMinutes(-") || cmd.contains(" -mmin -") {
            self.probes.set(self.probes.get() + 1);
            if self.probes.get() == 2 {
                std::fs::remove_dir(&self.link).or_else(|_| std::fs::remove_file(&self.link)).unwrap();
                dir_link(&self.link, &self.to);
            }
        }
        out
    }
    fn run_argv(&self, cwd: &Path, argv: &[String]) -> Result<gatetrain_core::runner::Output, String> {
        LocalRunner.run_argv(cwd, argv)
    }
    fn run_argv_to(&self, argv: &[String], timeout: Option<f64>) -> Result<gatetrain_core::tidy::RunOut, String> {
        LocalRunner.run_argv_to(argv, timeout)
    }
    fn shell_free(&self) -> bool {
        LocalRunner.shell_free()
    }
    fn put_file(&self, path: &str, text: &str) -> Result<(), String> {
        LocalRunner.put_file(path, text)
    }
    fn scratch_dir(&self) -> Option<String> {
        LocalRunner.scratch_dir()
    }
}

#[test]
fn minor1_the_move_acts_on_the_folder_that_was_judged_not_where_a_link_leads_now() {
    let f = fx();
    let real = f.build.join("real");
    write(real.join("shared/x.o"), "the batch's own output\n");
    age(&real);
    let lnk = f.build.join("lnk");
    dir_link(&lnk, &real);
    let rt = Retarget { link: lnk.clone(), to: f.main.clone(), probes: std::cell::Cell::new(0) };
    let key = fwd(&f.build) + "/lnk/shared";
    let rep = f.close_with(&fwd(&f.main), "target", &key, &[fwd(&f.build)], json!({}), &rt);
    assert!(rt.probes.get() >= 2, "the second judgement made its idle probe ({} probes)", rt.probes.get());
    assert!(f.main.join("shared/keep.txt").exists(), "the link was retargeted into main after the judgement and main's folder moved: {rep:?}, trash {:?}", f.trash());
    assert!(!real.join("shared").exists(), "the judged folder is the one that went: {rep:?}");
    assert!(rep.residue.is_empty(), "the check looks where the move acted, not where the link leads now: {rep:?}");
}

#[test]
fn bl3_a_clean_tracked_folder_of_a_worktree_is_not_moved() {
    // nothing uncommitted: `ls-files` alone shows it is the worktree's
    let f = fx();
    let wt = live_worktree(&f);
    write(wt.join("docs/a.md"), "doc
");
    git(&wt, &["add", "docs"]);
    git(&wt, &["commit", "-q", "-m", "docs"]);
    age(&wt);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/wt-live/docs"), &[fwd(&f.build)]);
    assert!(wt.join("docs/a.md").exists(), "{rep:?}");
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

#[test]
fn bl3_a_folder_of_a_worktree_with_only_an_untracked_file_is_not_moved() {
    // nothing tracked: `status --porcelain` alone shows the work
    let f = fx();
    let wt = live_worktree(&f);
    write(wt.join("notes/todo.txt"), "not yet added
");
    age(&wt);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/wt-live/notes"), &[fwd(&f.build)]);
    assert!(wt.join("notes/todo.txt").exists(), "{rep:?}");
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

// ---- review round 4

impl Fx {
    /// Several items at once, with extra tidy config keys, over the real local shell.
    fn close_set(&self, items: &[(&str, String)], branches: &[&str], roots: &[String], extra: Value) -> CloseReport {
        let mut tidy = self.tidy(&fwd(&self.main));
        for (k, v) in extra.as_object().unwrap() {
            tidy.as_object_mut().unwrap().insert(k.clone(), v.clone());
        }
        let h = Host { name: "real".into(), tidy: to_table(&tidy), ..Default::default() };
        let cfg = Config { hosts: vec![h.clone()], ..Default::default() };
        let t = build_cfg(&cfg, &h).unwrap();
        let ops = Ops::new(&h, &LocalRunner, &t);
        let env = OpsEnv { ops: &ops, cfg: &t, reference_prefixes: vec![] };
        let set = OwnedSet { items: items.iter().map(|(k, v)| OwnedItem::new(k, v)).collect(), branches: branches.iter().map(|b| b.to_string()).collect(), chain_checkout: None, roots: roots.to_vec(), shared_targets: vec![] };
        run_close(&set, &env)
    }

    /// A lane worktree beside main: `<root>/main-wt-<lane>` on `line/<lane>`, at main's commit (merged).
    fn sib(&self, lane: &str) -> std::path::PathBuf {
        git(&self.main, &["worktree", "add", "-q", "-b", &format!("line/{lane}"), &(fwd(&self.root) + "/main-wt-" + lane)]);
        let wt = self.root.join(format!("main-wt-{lane}"));
        age(&wt);
        wt
    }

    fn prefix(&self) -> Vec<String> {
        vec![format!("prefix:{}/main-wt-", fwd(&self.root))]
    }

    /// `git config status.showUntrackedFiles no` on the TEMP main repository only (shared by its worktrees).
    fn hide_untracked(&self) {
        git(&self.main, &["config", "status.showUntrackedFiles", "no"]);
        assert_eq!(git(&self.main, &["config", "--local", "--get", "status.showUntrackedFiles"]).trim(), "no");
    }
}

#[test]
fn bl4_a_merged_worktree_with_an_untracked_file_is_not_removed_when_git_hides_untracked_files() {
    let f = fx();
    f.hide_untracked();
    let wt = f.sib("u");
    write(wt.join("notes.txt"), "never added\n");
    age(&wt);
    let rep = f.close_set(&[("worktree", fwd(&f.root) + "/main-wt-u")], &["line/u"], &f.prefix(), json!({}));
    assert!(wt.join("notes.txt").exists(), "a worktree with an untracked file was removed: {rep:?}");
    assert!(refused_with(&rep, "tidy.uncommitted"), "{rep:?}");
}

#[test]
fn bl4_a_worktree_folder_holding_only_an_untracked_file_is_not_moved_when_git_hides_untracked_files() {
    let f = fx();
    f.hide_untracked();
    git(&f.main, &["worktree", "add", "-q", "-b", "line/n", &(fwd(&f.build) + "/wt-n")]);
    let wt = f.build.join("wt-n");
    write(wt.join("notes/todo.txt"), "never added\n");
    age(&wt);
    let rep = f.close_set(&[("target", fwd(&f.build) + "/wt-n/notes")], &[], &[fwd(&f.build)], json!({}));
    assert!(wt.join("notes/todo.txt").exists(), "untracked work inside a worktree was moved: {rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

#[test]
fn ma8_a_folder_inside_a_kept_lanes_worktree_is_protected() {
    let f = fx();
    git(&f.main, &["worktree", "add", "-q", "-b", "line/k", &(fwd(&f.build) + "/wt-k")]);
    let wt = f.build.join("wt-k");
    write(wt.join(".gitignore"), "target/\n");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", "ignore target"]);
    write(wt.join("target/x.o"), "obj\n");
    age(&wt);
    let extra = json!({"keep_lanes": ["k"], "worktree_prefix": "wt-"});
    let rep = f.close_set(&[("target", fwd(&f.build) + "/wt-k/target")], &[], &[fwd(&f.build)], extra);
    assert!(wt.join("target/x.o").exists(), "{rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.protected"), "{rep:?}");
}

#[test]
fn ma8_a_folder_inside_a_keep_paths_entry_is_protected_and_so_is_the_entry() {
    let f = fx();
    write(f.build.join("keep/sub/x.o"), "obj\n");
    age(&f.build);
    let extra = json!({"keep_paths": [fwd(&f.build) + "/keep"]});
    for k in ["/keep/sub", "/keep"] {
        let rep = f.close_set(&[("target", fwd(&f.build) + k)], &[], &[fwd(&f.build)], extra.clone());
        assert!(f.build.join("keep/sub/x.o").exists(), "{k}: {rep:?}, trash {:?}", f.trash());
        assert!(refused_with(&rep, "tidy.protected"), "{k}: a tidy.protected row, not executor residue: {rep:?}");
    }
}

#[test]
fn r4minor1_a_recorded_folder_that_is_its_own_repository_is_not_owned() {
    let f = fx();
    let repo = f.root.join("main-wt-zz");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    write(repo.join("a.txt"), "one\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "own history"]);
    write(repo.join("a.txt"), "two, not committed\n");
    age_all(&repo);
    let rep = f.close_set(&[("tmp", fwd(&f.root) + "/main-wt-zz")], &[], &f.prefix(), json!({}));
    assert!(repo.join("a.txt").exists(), "{rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

#[test]
fn r4minor3_bl3j_a_folder_reached_through_a_link_into_a_worktree_is_judged_as_inside_it() {
    let f = fx();
    let wt = live_worktree(&f);
    dir_link(&f.build.join("lnk"), &wt);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/lnk/src"), &[fwd(&f.build)]);
    assert!(wt.join("src/a.rs").exists(), "{rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned") && rep.refusals.iter().any(|r| r.why.contains("inside the worktree")), "{rep:?}");
}

#[test]
fn r4minor3_ma7f_a_prefix_root_written_through_a_link_is_compared_with_main_resolved() {
    // prefix:<root>/lnkdir/ma, where lnkdir leads back to <root>: resolved, it owns main itself, so it owns nothing
    let f = fx();
    dir_link(&f.root.join("lnkdir"), &f.root);
    write(f.root.join("main-wt-x/out.o"), "obj\n");
    age(&f.root.join("main-wt-x"));
    let root = format!("prefix:{}/lnkdir/ma", fwd(&f.root));
    let rep = f.close_set(&[("target", fwd(&f.root) + "/lnkdir/main-wt-x")], &[], &[root], json!({}));
    assert!(f.root.join("main-wt-x/out.o").exists(), "{rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned") && rep.refusals.iter().any(|r| r.why.contains("holds the main checkout")), "{rep:?}");
}

#[test]
fn r4minor3_m1d_a_recorded_link_is_moved_as_the_link_not_what_it_leads_to() {
    let f = fx();
    let real = f.build.join("real2");
    write(real.join("x.o"), "not the batch's\n");
    age(&real);
    dir_link(&f.build.join("lnk2"), &real);
    let key = fwd(&f.build) + "/lnk2";
    let rep = f.close(&fwd(&f.main), "target", &key, &[fwd(&f.build)]);
    assert!(real.join("x.o").exists(), "the folder a recorded link leads to was moved: {rep:?}, trash {:?}", f.trash());
}

#[cfg(unix)]
#[test]
fn r4minor4_a_tracked_symlink_inside_a_worktree_is_tracked_work() {
    // a tracked link pointing at the ignored target/: probed from where it leads it looks empty, from its parent it is tracked
    let f = fx();
    let wt = live_worktree(&f);
    std::os::unix::fs::symlink("target", wt.join("out")).unwrap();
    git(&wt, &["add", "out"]);
    git(&wt, &["commit", "-q", "-m", "a tracked link"]);
    age(&wt);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/wt-live/out"), &[fwd(&f.build)]);
    assert!(std::fs::symlink_metadata(wt.join("out")).is_ok(), "a tracked link was moved out of the worktree: {rep:?}");
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}

/// Like `age`, `.git` included (its objects are read-only: made writable first), so a repository of its own is not "in use".
fn age_all(p: &Path) {
    for e in std::fs::read_dir(p).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            age_all(&e.path());
        } else if e.file_type().unwrap().is_file() {
            let mut perm = std::fs::metadata(e.path()).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            std::fs::set_permissions(e.path(), perm).unwrap();
            let f = std::fs::File::options().write(true).open(e.path()).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86400)).unwrap();
        }
    }
}

#[cfg(windows)]
#[test]
fn r4minor4_a_folder_of_a_worktree_written_in_another_case_is_still_seen_as_tracked() {
    // the probe asks from the parent with the name as written: on Windows that name is matched without case
    let f = fx();
    let wt = live_worktree(&f);
    let rep = f.close(&fwd(&f.main), "target", &(fwd(&f.build) + "/wt-live/SRC"), &[fwd(&f.build)]);
    assert!(wt.join("src/a.rs").exists(), "{rep:?}, trash {:?}", f.trash());
    assert!(refused_with(&rep, "tidy.not-owned"), "{rep:?}");
}
