//! `tidy --force` on the REAL host code (`OpsHost`: commit by named paths, push and read back, link-safe removal, the main guard), in a
//! throwaway fixture repo with real git and real links. Everything removed here is a folder this test made inside its own temp folder;
//! docker and cargo are never run (the fixture has no targets and no containers, and the sweep only plans what the inventory finds).
mod force_fix;
use force_fix::*;
use gatetrain_core::config::{self, Config};
use gatetrain_core::force::tidyforce::{self, ForceHost, OpsHost};
use gatetrain_core::runner::LocalRunner;
use gatetrain_core::tidy::config::{build_cfg, TidyCfg};
use std::path::{Path, PathBuf};
use std::process::Command;

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

pub fn make_link(link: &Path, target: &Path) {
    if cfg!(windows) {
        let o = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
        assert!(o.status.success(), "mklink: {}", String::from_utf8_lossy(&o.stdout));
    } else {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

struct Fx {
    r: Repo,
    cfg: Config,
    tidy: TidyCfg,
    canary: PathBuf,
}

impl Fx {
    fn tmp(&self) -> &Path {
        self.r.tmp.path()
    }
    fn wt(&self, name: &str) -> PathBuf {
        self.tmp().join(name)
    }
    /// a worktree `wt-<name>` on a new branch off main
    fn add_wt(&self, name: &str) -> PathBuf {
        let p = self.wt(&format!("wt-{name}"));
        sh(&self.r.work, &["worktree", "add", "-q", p.to_str().unwrap(), "-b", name, "main"]);
        p
    }
    fn host(&self) -> OpsHost<'_> {
        static RUNNER: LocalRunner = LocalRunner;
        OpsHost::new(&self.cfg.hosts[0], &RUNNER, &self.tidy, "origin")
    }
    fn main_state(&self) -> (String, String) {
        (sh(&self.r.work, &["ls-files"]), sh(&self.r.work, &["status", "--porcelain", "--untracked-files=all"]))
    }
    fn origin_has(&self, r: &str) -> bool {
        Command::new("git").arg("-C").arg(&self.r.origin).args(["rev-parse", "--verify", "--quiet", r]).output().unwrap().status.success()
    }
}

fn fx() -> Fx {
    let r = repo();
    let t = fwd(r.tmp.path());
    let canary = r.tmp.path().join("canary");
    std::fs::create_dir_all(&canary).unwrap();
    write(&canary, "keep.txt", "precious\n");
    let mut extra = String::new();
    if cfg!(windows) {
        let lc = r.tmp.path().join("lc");
        std::fs::create_dir_all(&lc).unwrap();
        make_link(&lc.join("j1"), &canary);
        extra = format!("link_canary = \"{t}/lc\"\nlink_canary_count = 1\n");
    }
    let text = format!(
        "[[hosts]]\nname = \"t\"\nkind = \"local\"\nos = \"{}\"\nroles = [\"lane-gates\"]\nreach = \"local\"\n  [[hosts.disks]]\n  path = \"{t}\"\n  min_free_gb = 1\n[tidy]\nmain_checkout = \"{}\"\nworktree_prefix = \"wt-\"\ntrash_dir = \"{t}/.trash\"\nlog = \"{t}/tidy.jsonl\"\nworktree_min_idle_days = 0\n{extra}",
        if cfg!(windows) { "windows" } else { "linux" },
        fwd(&r.work)
    );
    std::fs::create_dir_all(r.tmp.path().join(".trash")).unwrap();
    let cp = r.tmp.path().join("hosts.toml");
    std::fs::write(&cp, text).unwrap();
    let cfg = config::load(&cp).unwrap();
    let tidy = build_cfg(&cfg, &cfg.hosts[0]).unwrap();
    // node_modules is ignored everywhere in this repo, like in a real one
    std::fs::OpenOptions::new().append(true).open(r.work.join(".git/info/exclude")).and_then(|mut f| std::io::Write::write_all(&mut f, b"node_modules\n")).unwrap();
    Fx { r, cfg, tidy, canary }
}

const DATE: &str = "2026-11-25";

fn apply(f: &Fx) -> tidyforce::ForceOut {
    tidyforce::run(&f.host(), None, true, DATE)
}

#[test]
fn a_dirty_worktree_is_committed_by_name_pushed_read_back_and_removed_link_safely() {
    let f = fx();
    let w = f.add_wt("a");
    write(&w, "notes.txt", "work in progress\n");
    write(&w, "dir/inner.txt", "more\n");
    write(&w, "bash.exe.stackdump", "junk\n");
    make_link(&w.join("node_modules"), &f.canary);
    let before = f.main_state();
    let o = apply(&f);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(!w.exists(), "the worktree folder is gone:\n{}", o.text);
    assert!(f.canary.join("keep.txt").exists(), "the junction's target was followed and emptied");
    let tree = sh(&f.r.origin, &["ls-tree", "-r", "--name-only", "refs/heads/archive/wt-a"]);
    assert!(tree.contains("notes.txt") && tree.contains("dir/inner.txt"), "{tree}");
    assert!(!tree.contains("stackdump") && !tree.contains("node_modules"), "{tree}");
    assert!(!sh(&f.r.work, &["worktree", "list"]).contains("wt-a"));
    assert_eq!(f.main_state(), before, "the main checkout changed");
    assert!(sh(&f.r.work, &["log", "-1", "--format=%s", "a"]).starts_with("wip(archive):"));
}

#[test]
fn an_untracked_link_among_the_uncommitted_paths_stops_the_run_with_everything_intact() {
    let f = fx();
    let w = f.add_wt("a");
    write(&w, "notes.txt", "x\n");
    make_link(&w.join("shared"), &f.canary); // not ignored
    let o = apply(&f);
    assert_eq!(o.rc, 6, "{}", o.text);
    assert!(o.text.contains("STOP wt-a") && o.text.contains("link"), "{}", o.text);
    assert!(w.join("notes.txt").exists() && f.canary.join("keep.txt").exists());
    assert!(!f.origin_has("refs/heads/archive/wt-a"), "nothing may be pushed when the commit step stopped");
}

#[test]
fn a_worktree_with_only_a_stackdump_is_clean_and_is_removed() {
    let f = fx();
    let w = f.add_wt("s");
    write(&w, "bash.exe.stackdump", "junk\n");
    let o = apply(&f);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(!w.exists(), "{}", o.text);
    assert!(!o.text.contains("committed"), "a stackdump is not work: {}", o.text);
}

#[test]
fn a_rejected_archive_push_uses_the_fallback_name_and_never_overwrites() {
    let f = fx();
    let w = f.add_wt("a");
    write(&w, "n.txt", "x\n");
    // somebody else already owns archive/wt-a with an unrelated history
    sh(&f.r.work, &["checkout", "-q", "--orphan", "other"]);
    sh(&f.r.work, &["rm", "-rf", "-q", "."]);
    write(&f.r.work, "o.txt", "o\n");
    sh(&f.r.work, &["add", "o.txt"]);
    sh(&f.r.work, &["commit", "-m", "unrelated"]);
    let other = sh(&f.r.work, &["rev-parse", "HEAD"]);
    sh(&f.r.work, &["push", "origin", "other:refs/heads/archive/wt-a"]);
    sh(&f.r.work, &["checkout", "-q", "-f", "main"]);
    let o = apply(&f);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert_eq!(sh(&f.r.origin, &["rev-parse", "refs/heads/archive/wt-a"]), other, "the other archive was overwritten");
    let refs = sh(&f.r.origin, &["for-each-ref", "--format=%(refname)", "refs/heads/archive/"]);
    assert!(refs.lines().any(|l| l.starts_with("refs/heads/archive/wt-a-")), "{refs}");
    assert!(!w.exists());
}

#[test]
fn a_stale_remote_tracking_ref_is_not_proof_that_the_remote_holds_the_tip() {
    // B1: the branch was deleted on the remote; a plain fetch keeps origin/tmpb, and `branch -r --contains` still lists it
    let f = fx();
    sh(&f.r.work, &["checkout", "-q", "-b", "tmpb", "main"]);
    write(&f.r.work, "t.txt", "t\n");
    sh(&f.r.work, &["add", "t.txt"]);
    sh(&f.r.work, &["commit", "-m", "only here"]);
    let tip = sh(&f.r.work, &["rev-parse", "HEAD"]);
    sh(&f.r.work, &["push", "origin", "tmpb"]);
    sh(&f.r.work, &["checkout", "-q", "main"]);
    let d = f.wt("wt-d");
    sh(&f.r.work, &["worktree", "add", "-q", "--detach", d.to_str().unwrap(), &tip]);
    sh(&f.r.work, &["branch", "-D", "tmpb"]);
    sh(&f.r.origin, &["update-ref", "-d", "refs/heads/tmpb"]); // deleted on the remote
    let h = f.host();
    h.fetch().unwrap();
    assert!(!h.remote_contains(&tip).unwrap(), "a deleted remote branch was taken as proof");
    let o = apply(&f);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert_eq!(sh(&f.r.origin, &["rev-parse", "refs/heads/archive/wt-d"]), tip, "the tip was not archived:\n{}", o.text);
    assert!(!d.exists());
    // a detached head also gets a local archive tag, so the commit survives gc
    assert_eq!(sh(&f.r.work, &["rev-parse", &format!("refs/tags/archive/detached-{}^{{commit}}", &tip[..12])]), tip);
}

#[test]
fn a_junction_a_check_made_in_the_scratch_worktree_is_unlinked_never_followed() {
    // B5: the checks (an install, a regenerator) may leave a node_modules link in the scratch worktree; its removal must not empty the target
    use gatetrain_core::force::cfg;
    use gatetrain_core::force::exec::LocalExec;
    use gatetrain_core::force::land::{land, LandArgs};
    let f = fx();
    branch(&f.r, "feat", &["docs/x.md"]);
    let mk = if cfg!(windows) { format!("mklink /J node_modules {}", f.canary.display()) } else { format!("ln -s {} node_modules", f.canary.display()) };
    let mut c = cfg::parse("[[land.rules]]\nglob=\"docs/**\"\nmode=\"fast\"\n").unwrap().land;
    c.checks = vec![mk];
    let runner = LocalRunner;
    let rm = tidyforce::ops_remover(&f.cfg.hosts[0], &runner, &f.tidy);
    let a = LandArgs { repo: f.r.p(), branch: "feat".into(), plan: false, who: "t".into() };
    let before = f.main_state();
    let o = land(&LocalExec, &c, &a, None, &rm);
    assert_eq!(o.rc, 0, "{}", o.text);
    assert!(f.canary.join("keep.txt").exists(), "the junction's target was followed:\n{}", o.text);
    assert!(!std::fs::read_dir(f.tmp()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("land-")), "scratch folder left:\n{}", o.text);
    assert_eq!(sh(&f.r.work, &["worktree", "list", "--porcelain"]).matches("worktree ").count(), 1);
    assert_eq!(f.main_state(), before);
    assert_eq!(sh(&f.r.origin, &["rev-parse", "main"]), o.merged.unwrap());
}
