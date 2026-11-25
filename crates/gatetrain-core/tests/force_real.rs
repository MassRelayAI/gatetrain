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

