//! `gatetrain tidy --force [--keep <regex>] [--apply]`: every worktree but the main checkout and the kept or live ones is ARCHIVED first
//! (uncommitted work committed by named paths; the tip pushed to the remote as `archive/<worktree>` unless a remote ref already holds it;
//! STOP when that cannot be proven) and then removed junction-safely with the main-checkout guard; finished cargo targets are cleaned
//! (with the rustc wrapper unset) and exited throwaway containers removed. The keep, protect and `keep_paths` lists always win.
//! A dry run by default: the plan table first (founder 2026-11-25: everything can be forced, measured or cleaned).
//!
//! The orchestration here talks to a [`ForceHost`]; [`OpsHost`] is the real one, over the tidy machinery that already owns the link-safe
//! removal, the main guard and the protect list. Tests hand it a stub that records.
use crate::config::Host;
use crate::runner::Runner;
use crate::tidy::apply::do_item;
use crate::tidy::config::TidyCfg;
use crate::tidy::inventory::{gather, Item};
use crate::tidy::ops::{lane_of, norm, Fatal, Ops};
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub struct WtState {
    pub path: String,
    pub name: String,
    pub branch: Option<String>,
    pub head: String,
    pub locked: bool,
    /// the uncommitted paths (stackdump files excluded), as git names them
    pub dirty: Vec<String>,
    /// ignored files that are not build output (`.env.local`, scratch notes): removal would delete them unarchived
    pub ignored: Vec<String>,
    pub is_main: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SweepItem {
    /// targets | containers
    pub class: String,
    pub key: String,
    pub bytes: Option<i64>,
    pub note: String,
}

pub trait ForceHost {
    /// Refresh the remote-tracking refs, so "already on the remote" is proved on current data.
    fn fetch(&self) -> Result<(), String>;
    fn list(&self) -> Result<Vec<WtState>, String>;
    /// Why the keep, protect or lane lists say this worktree stays, if they do.
    fn kept_by_config(&self, path: &str) -> Option<String>;
    /// Is `head` on some remote branch? An error is "cannot be proven".
    fn remote_contains(&self, head: &str) -> Result<bool, String>;
    /// Commit exactly the uncommitted paths of the worktree, by name; returns the new tip. Err when anything stays uncommitted.
    fn commit_paths(&self, wt: &WtState, msg: &str) -> Result<String, String>;
    /// Push `head` as `refname` on the remote and read it back; Err unless the remote now holds `head`.
    fn push_archive(&self, wt: &WtState, head: &str, refname: &str) -> Result<(), String>;
    /// Remove the worktree junction-safely and check the main checkout is unchanged. Err = STOP.
    fn remove(&self, wt: &WtState) -> Result<String, String>;
    fn sweep_plan(&self) -> Result<Vec<SweepItem>, String>;
    fn sweep_apply(&self, it: &SweepItem) -> Result<String, String>;
}

pub struct ForceOut {
    /// 0 ok (or a dry run); 1 a sweep item failed; 6 STOP (nothing after the named worktree was touched)
    pub rc: i32,
    pub text: String,
}

enum Action {
    Main,
    Keep(String),
    Remove,
}

fn decide(h: &dyn ForceHost, w: &WtState, keep: Option<&Regex>) -> Action {
    if w.is_main {
        return Action::Main;
    }
    if let Some(r) = keep {
        if r.is_match(&w.name) || r.is_match(&w.path) {
            return Action::Keep(format!("kept (--keep {})", r.as_str()));
        }
    }
    if let Some(why) = h.kept_by_config(&w.path) {
        return Action::Keep(format!("kept ({why})"));
    }
    if w.locked {
        return Action::Keep("kept (locked)".into());
    }
    Action::Remove
}

fn mb(b: Option<i64>) -> String {
    b.map(|x| format!("{:.1} GB", x as f64 / 1_073_741_824.0)).unwrap_or("size unknown".into())
}

/// Plan, and with `apply` execute. `date` (YYYY-MM-DD) goes into the wip commit message.
pub fn run(h: &dyn ForceHost, keep: Option<&Regex>, apply: bool, date: &str) -> ForceOut {
    run_with(h, keep, apply, date, false)
}

/// [`run`], with `include_ignored` accepting that ignored files (an `.env.local`, scratch notes) are deleted with the worktree: without it they are listed and the run STOPs at that worktree.
pub fn run_with(h: &dyn ForceHost, keep: Option<&Regex>, apply: bool, date: &str, include_ignored: bool) -> ForceOut {
    let mut t = String::new();
    macro_rules! say {
        ($($a:tt)*) => {{ t.push_str(&format!($($a)*)); t.push('\n'); }};
    }
    if let Err(e) = h.fetch() {
        say!("STOP: cannot refresh the remote refs, so nothing can be proven: {e}");
        return ForceOut { rc: 6, text: t };
    }
    let wts = match h.list() {
        Ok(w) => w,
        Err(e) => {
            say!("STOP: cannot list the worktrees: {e}");
            return ForceOut { rc: 6, text: t };
        }
    };
    say!("tidy --force plan ({}):", if apply { "APPLY" } else { "dry run" });
    say!("  {:<34} {:<9} {}", "worktree", "action", "detail");
    let mut todo: Vec<&WtState> = vec![];
    for w in &wts {
        match decide(h, w, keep) {
            Action::Main => say!("  {:<34} {:<9} main checkout", w.name, "keep"),
            Action::Keep(why) => say!("  {:<34} {:<9} {why}", w.name, "keep"),
            Action::Remove => {
                let on = h.remote_contains(&w.head);
                let steps = match (&on, w.dirty.is_empty()) {
                    (Err(e), _) => format!("cannot prove the tip is on the remote ({e}): will STOP"),
                    (Ok(true), true) => "tip already on the remote; remove".to_string(),
                    (Ok(_), true) => format!("push tip to archive/{}, remove", w.name),
                    (Ok(_), false) => format!("{} uncommitted path(s) committed by name, push to archive/{}, remove", w.dirty.len(), w.name),
                };
                let n = if w.dirty.is_empty() { String::new() } else { format!("{} uncommitted; ", w.dirty.len()) };
                let blocked = if w.ignored.is_empty() || include_ignored { String::new() } else { format!("BLOCKED: {} ignored file(s) would be deleted unarchived ({}); ", w.ignored.len(), w.ignored.iter().take(3).cloned().collect::<Vec<_>>().join(", ")) };
                say!("  {:<34} {:<9} {blocked}{n}{steps}", w.name, "remove");
                todo.push(w);
            }
        }
    }
    let sweep = match h.sweep_plan() {
        Ok(s) => s,
        Err(e) => {
            say!("  (cargo targets and containers could not be planned: {e})");
            vec![]
        }
    };
    for s in &sweep {
        say!("  {:<34} {:<9} {} ({}; {})", s.key, if s.class == "containers" { "rm" } else { "clean" }, s.class, mb(s.bytes), s.note);
    }
    if !apply {
        say!("dry run: nothing was changed. Add --apply to execute. The main checkout, --keep, keep_paths, protect and locked worktrees are never touched.");
        return ForceOut { rc: 0, text: t };
    }
    for w in todo {
        if !w.ignored.is_empty() && !include_ignored {
            say!(
                "STOP {}: {} ignored file(s) would be deleted with the worktree and are not archived ({}); copy them out, or rerun with --include-ignored to accept the loss. Nothing of it was removed and nothing after it was touched",
                w.name,
                w.ignored.len(),
                w.ignored.join(", ")
            );
            return ForceOut { rc: 6, text: t };
        }
        let mut head = w.head.clone();
        let mut known_on_remote = None;
        if !w.dirty.is_empty() {
            let msg = format!("wip(archive): uncommitted work of {} saved before the worktree was removed ({date})", w.name);
            match h.commit_paths(w, &msg) {
                Ok(n) => {
                    say!("  {}: committed {} uncommitted path(s) as {n}", w.name, w.dirty.len());
                    head = n;
                    known_on_remote = Some(false);
                }
                Err(e) => {
                    say!("STOP {}: could not commit its uncommitted work ({e}); nothing of it was removed and nothing after it was touched", w.name);
                    return ForceOut { rc: 6, text: t };
                }
            }
        }
        let on = match known_on_remote {
            Some(b) => b,
            None => match h.remote_contains(&head) {
                Ok(b) => b,
                Err(e) => {
                    say!("STOP {}: cannot prove the tip is on the remote ({e}); nothing was removed", w.name);
                    return ForceOut { rc: 6, text: t };
                }
            },
        };
        if !on {
            let r = format!("archive/{}", w.name);
            if let Err(e) = h.push_archive(w, &head, &r) {
                say!("STOP {}: the tip could not be archived to the remote ({e}); the worktree is intact and nothing after it was touched", w.name);
                return ForceOut { rc: 6, text: t };
            }
            say!("  {}: archived {head} to the remote as {r}", w.name);
        }
        match h.remove(w) {
            Ok(n) => say!("  {}: removed ({n})", w.name),
            Err(e) => {
                say!("STOP {}: removal stopped ({e}); nothing after it was touched", w.name);
                return ForceOut { rc: 6, text: t };
            }
        }
    }
    let mut rc = 0;
    // after the worktrees: their cargo targets are finished now; plan again so the targets of the removed lanes are seen
    let sweep = h.sweep_plan().unwrap_or(sweep);
    for s in &sweep {
        match h.sweep_apply(s) {
            Ok(n) => say!("  {} {}: {n}", s.class, s.key),
            Err(e) => {
                say!("  {} {}: FAILED ({e})", s.class, s.key);
                rc = 1;
            }
        }
    }
    say!("tidy --force done{}", if rc == 1 { " with failed items" } else { "" });
    ForceOut { rc, text: t }
}

// ---------------------------------------------------------------- the real host

fn fs(e: Fatal) -> String {
    e.to_string()
}

fn unquote(s: &str) -> String {
    let t = s.trim_end();
    t.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(t).to_string()
}

/// Folders that are rebuilt, not work: an ignored path inside one of them is not listed.
const BUILD_DIRS: [&str; 16] = ["node_modules", "target", "dist", "build", "out", ".next", ".turbo", ".cache", "__pycache__", ".venv", "venv", ".pytest_cache", ".gradle", "coverage", ".parcel-cache", ".nuxt"];

/// The `!! path` entries of `git status --ignored` that are not build output and not stackdump files.
pub fn parse_ignored(out: &str) -> Vec<String> {
    let mut v = vec![];
    let pieces: Vec<&str> = if out.contains('\0') { out.split('\0').collect() } else { out.lines().collect() };
    for e in pieces {
        let Some(p) = e.strip_prefix("!! ") else { continue };
        let p = unquote(p);
        let p = p.trim_end_matches('/').to_string();
        if p.ends_with(".stackdump") || p.split('/').any(|c| BUILD_DIRS.contains(&c)) {
            continue;
        }
        v.push(p);
    }
    v
}

fn secret_name(path: &str) -> bool {
    let b = path.rsplit('/').next().unwrap_or("").to_lowercase();
    let example = [".example", ".sample", ".template", ".dist"].iter().any(|x| b.ends_with(x));
    (b == ".env" || b.starts_with(".env.")) && !example
        || [".pem", ".key", ".pfx", ".p12", ".secret", ".secrets", ".kdbx"].iter().any(|x| b.ends_with(x))
        || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa", "credentials"].iter().any(|x| b.starts_with(x))
        || b.starts_with("secrets.")
        || b == "secrets"
}

fn secret_content(text: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static RE: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let re = RE.get_or_init(|| {
        [
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "a private key block"),
            (r"AKIA[0-9A-Z]{16}", "an AWS access key id"),
            (r"gh[pousr]_[A-Za-z0-9]{30,}", "a GitHub token"),
            (r"sk-ant-[A-Za-z0-9_-]{20,}", "an Anthropic key"),
            (r"xox[baprs]-[A-Za-z0-9-]{10,}", "a Slack token"),
        ]
        .iter()
        .map(|(p, n)| (Regex::new(p).unwrap(), *n))
        .collect()
    });
    re.iter().find(|(r, _)| r.is_match(text)).map(|(_, n)| *n)
}

/// `git status --porcelain` paths; stackdump files are not work.
pub fn parse_status(out: &str) -> Vec<String> {
    let mut v = vec![];
    if out.contains('\0') {
        let mut it = out.split('\0');
        while let Some(e) = it.next() {
            if e.len() < 4 {
                continue;
            }
            let (xy, p) = e.split_at(3);
            if xy.starts_with('R') || xy.starts_with('C') || xy[1..].starts_with('R') {
                it.next(); // the old name of a rename follows
            }
            if !p.ends_with(".stackdump") {
                v.push(p.to_string());
            }
        }
    } else {
        for l in out.lines().filter(|l| l.len() > 3) {
            let p = l[3..].rsplit(" -> ").next().unwrap_or("");
            let p = unquote(p);
            if !p.ends_with(".stackdump") {
                v.push(p);
            }
        }
    }
    v
}

/// A link-safe folder remover for [`crate::force::land::land`]: tidy's own removal (links unlinked first and never followed, link targets re-checked,
/// the main checkout's tracked count and status asserted before and after). `cfg.main_checkout` must be the repository the folder belongs to.
pub fn ops_remover<'a>(host: &'a Host, runner: &'a dyn Runner, cfg: &'a TidyCfg) -> impl Fn(&str) -> Result<(), String> + 'a {
    move |p: &str| {
        let ops = Ops::new(host, runner, cfg);
        let (status, note) = ops.remove_dir_with(p, None).map_err(fs)?;
        if status == "ok" {
            Ok(())
        } else {
            Err(format!("{status}: {note}"))
        }
    }
}

pub struct OpsHost<'a> {
    pub host: &'a Host,
    pub runner: &'a dyn Runner,
    /// the host's tidy config with the force settings applied
    pub cfg: TidyCfg,
    pub remote: String,
    prunable: RefCell<HashSet<String>>,
    items: RefCell<Vec<Item>>,
}

impl<'a> OpsHost<'a> {
    pub fn new(host: &'a Host, runner: &'a dyn Runner, base: &TidyCfg, remote: &str) -> OpsHost<'a> {
        let mut cfg = base.clone();
        cfg.cargo_unset_wrapper = true;
        cfg.orphan_targets = "clean".into();
        OpsHost { host, runner, cfg, remote: remote.into(), prunable: RefCell::new(HashSet::new()), items: RefCell::new(vec![]) }
    }
    fn ops(&self) -> Ops<'_> {
        Ops::new(self.host, self.runner, &self.cfg)
    }
    fn main(&self) -> Result<String, String> {
        self.cfg.main_checkout.clone().ok_or("main_checkout is not configured for this host".to_string())
    }
}

impl ForceHost for OpsHost<'_> {
    fn fetch(&self) -> Result<(), String> {
        let m = self.main()?;
        let c = self.ops().git(&m, &["fetch", "-q", "--prune", &self.remote]).map_err(fs)?;
        if c.ok() {
            Ok(())
        } else {
            Err(format!("git fetch {} failed: {}", self.remote, c.out.trim()))
        }
    }

    fn list(&self) -> Result<Vec<WtState>, String> {
        let ops = self.ops();
        let m = self.main()?;
        let mut v = vec![];
        for w in ops.worktrees().map_err(fs)? {
            let is_main = norm(&w.path) == norm(&m);
            if w.prunable {
                self.prunable.borrow_mut().insert(norm(&w.path));
            }
            let dirty = if w.prunable || is_main {
                vec![]
            } else {
                let c = ops.git(&w.path, &["status", "--porcelain", "-z", "--untracked-files=normal"]).map_err(fs)?;
                if !c.ok() {
                    return Err(format!("cannot read the status of {}", w.path));
                }
                parse_status(&c.out)
            };
            let ignored = if w.prunable || is_main {
                vec![]
            } else {
                let c = ops.git(&w.path, &["status", "--porcelain", "-z", "--ignored=matching", "--untracked-files=normal"]).map_err(fs)?;
                if !c.ok() {
                    return Err(format!("cannot read the ignored files of {}", w.path));
                }
                parse_ignored(&c.out)
            };
            let name = norm(&w.path).rsplit('/').next().unwrap_or("").to_string();
            v.push(WtState { path: w.path.clone(), name, branch: w.branch.clone(), head: w.head.clone().unwrap_or_default(), locked: w.locked, dirty, ignored, is_main });
        }
        Ok(v)
    }

    fn kept_by_config(&self, path: &str) -> Option<String> {
        let ops = self.ops();
        if self.prunable.borrow().contains(&norm(path)) {
            return Some("its folder is missing; `git worktree prune` is a person's call".into());
        }
        if let Err(e) = ops.check_path(path) {
            return Some(fs(e));
        }
        match ops.protect_hit(path) {
            Ok(Some(p)) => return Some(format!("protected by {p}")),
            Ok(None) => {}
            Err(e) => return Some(format!("protect list unreadable: {e}")),
        }
        let lane = lane_of(path, &self.cfg);
        if self.cfg.keep_lanes.contains(&lane) {
            return Some(format!("keep_lanes: {lane}"));
        }
        None
    }

    fn remote_contains(&self, head: &str) -> Result<bool, String> {
        // the live remote is the proof, not the remote-tracking refs (a branch deleted there survives a plain fetch): every branch tip the remote
        // lists now is compared with `head`; a tip whose object is not here cannot be an ancestor, which only means a push, never a loss
        let m = self.main()?;
        let ops = self.ops();
        let l = ops.git(&m, &["ls-remote", "--heads", &self.remote]).map_err(fs)?;
        if !l.ok() {
            return Err(format!("git ls-remote {} failed: {}", self.remote, l.out.trim()));
        }
        for line in l.out.lines() {
            let sha = line.split_whitespace().next().unwrap_or("");
            if sha.len() < 40 {
                continue;
            }
            if sha == head || ops.git(&m, &["merge-base", "--is-ancestor", head, sha]).map_err(fs)?.ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn commit_paths(&self, wt: &WtState, msg: &str) -> Result<String, String> {
        let ops = self.ops();
        let c = ops.git(&wt.path, &["status", "--porcelain", "-z", "--untracked-files=normal"]).map_err(fs)?;
        let paths = parse_status(&c.out);
        if !c.ok() {
            return Err("cannot read the status".into());
        }
        if let Some(p) = paths.iter().find(|p| p.contains('\n') || p.starts_with('-')) {
            return Err(format!("the path {p:?} cannot be named safely"));
        }
        // `git add` would commit what a junction or symlink leads to (a node_modules link into a shared cache): a link is never archived by name
        for p in &paths {
            let full = ops.join(&wt.path, p.trim_end_matches('/'));
            if ops.is_link(&full).map_err(fs)? != Some(false) {
                return Err(format!("{p} is a link (or cannot be probed); it cannot be archived by name. Ignore or remove the link, then run again"));
            }
        }
        // the archive goes to a remote: an untracked file that looks like a secret is never committed (the secret scan of the repo is an editor hook, not a git hook)
        let un = ops.git(&wt.path, &["ls-files", "--others", "--exclude-standard", "-z"]).map_err(fs)?;
        for f in un.out.split('\0').filter(|f| !f.is_empty() && !f.ends_with(".stackdump")) {
            if secret_name(f) {
                return Err(format!("{f} looks like a secret (by its name); it is not committed or pushed. Move it out of the worktree, or delete it, then run again"));
            }
            if self.runner.is_local() {
                let full = ops.join(&wt.path, f);
                if let Ok(m) = std::fs::symlink_metadata(&full) {
                    if m.is_file() && m.len() <= 2_000_000 {
                        if let Ok(b) = std::fs::read(&full) {
                            if let Some(what) = secret_content(&String::from_utf8_lossy(&b)) {
                                return Err(format!("{f} looks like a secret (it holds {what}); it is not committed or pushed. Move it out of the worktree, or delete it, then run again"));
                            }
                        }
                    }
                }
            }
        }
        for chunk in paths.chunks(25) {
            let mut a: Vec<&str> = vec!["add", "--"];
            a.extend(chunk.iter().map(|s| s.as_str()));
            let r = ops.git(&wt.path, &a).map_err(fs)?;
            if !r.ok() {
                return Err(format!("git add failed: {}", r.out.trim()));
            }
        }
        let r = ops.git(&wt.path, &["commit", "-q", "-m", msg]).map_err(fs)?;
        if !r.ok() {
            return Err(format!("git commit failed: {}", r.out.trim()));
        }
        let left = ops.git(&wt.path, &["status", "--porcelain", "-z", "--untracked-files=normal"]).map_err(fs)?;
        let n = parse_status(&left.out).len();
        if n > 0 {
            return Err(format!("{n} path(s) are still uncommitted after the archive commit"));
        }
        let h = ops.git(&wt.path, &["rev-parse", "HEAD"]).map_err(fs)?;
        Ok(h.out.trim().to_string())
    }

    fn push_archive(&self, wt: &WtState, head: &str, refname: &str) -> Result<(), String> {
        let ops = self.ops();
        let m = self.main()?;
        let mut names = vec![refname.to_string(), format!("{refname}-{}", &head[..head.len().min(8)])];
        let mut last = String::new();
        while let Some(n) = names.first().cloned() {
            names.remove(0);
            let p = ops.git(&wt.path, &["push", "-q", &self.remote, &format!("{head}:refs/heads/{n}")]).map_err(fs)?;
            if p.ok() {
                let l = ops.git(&m, &["ls-remote", &self.remote, &format!("refs/heads/{n}")]).map_err(fs)?;
                if l.ok() && l.out.trim().starts_with(head) {
                    return Ok(());
                }
                last = format!("the remote does not report {head} at {n} after the push");
            } else {
                last = p.out.trim().to_string();
            }
        }
        Err(last)
    }

    fn remove(&self, wt: &WtState) -> Result<String, String> {
        let ops = self.ops();
        let (p, m) = (wt.path.as_str(), self.main()?);
        ops.check_path(p).map_err(fs)?;
        if ops.is_link(p).map_err(fs)? != Some(false) {
            return Err(format!("{p} is a link or cannot be probed; not removing through it"));
        }
        if !ops.toplevel_ok(p).map_err(fs)? {
            return Err(format!("{p} is not the top level of its own worktree"));
        }
        // the same reading as the plan (`parse_status`): a *.stackdump is junk, not work; anything else that is uncommitted stops the removal
        let st = ops.git(p, &["status", "--porcelain", "-z", "--untracked-files=all", "--ignore-submodules=none"]).map_err(fs)?;
        if !st.ok() || !parse_status(&st.out).is_empty() {
            return Err(format!("{p} still has uncommitted work (or its status cannot be read)"));
        }
        match ops.worktrees().map_err(fs)?.into_iter().find(|w| norm(&w.path) == norm(p)) {
            None => return Err(format!("{p} is no longer a registered worktree")),
            Some(w) if w.locked => return Err(format!("{p} is locked now")),
            _ => {}
        }
        if ops.git_busy(p).map_err(fs)? != Some(false) {
            return Err(format!("{p} has a rebase, merge or bisect in progress (or that could not be told)"));
        }
        if let Some(run) = ops.held_by_hold(p).map_err(fs)? {
            return Err(format!("{p} is named by the host lock of run {run}"));
        }
        if !ops.win && ops.held_by_process(p).map_err(fs)? != Some(false) {
            return Err(format!("{p} is held by a running process (or that could not be told)"));
        }
        let before = ops.snapshot_main().map_err(fs)?;
        if wt.branch.is_none() {
            // no branch holds a detached commit: once the folder goes, only a tag keeps it from gc
            let h = ops.git(p, &["rev-parse", "HEAD"]).map_err(fs)?.out.trim().to_string();
            if h.len() < 40 {
                return Err(format!("cannot read HEAD of {p}"));
            }
            let tag = format!("archive/detached-{}", &h[..12]);
            if !ops.git(&m, &["tag", &tag, &h]).map_err(fs)?.ok() {
                let have = ops.git(&m, &["rev-parse", &format!("refs/tags/{tag}^{{commit}}")]).map_err(fs)?;
                if have.out.trim() != h {
                    return Err(format!("could not create the archive tag {tag} for {h}; the worktree is kept"));
                }
            }
        }
        let (status, note) = ops.remove_dir_with(p, None).map_err(fs)?;
        if status != "ok" {
            return Err(format!("{status}: {note}"));
        }
        let r = ops.git_to(&m, &["worktree", "remove", "--force", p], Some(self.cfg.remove_timeout_s)).map_err(fs)?;
        if !r.ok() {
            return Err(format!("{p} is removed but git could not unregister it (STALE REGISTRATION)"));
        }
        ops.assert_main(before).map_err(fs)?;
        Ok(format!("links unlinked, removed, main checkout unchanged; branch {} kept", wt.branch.as_deref().unwrap_or("(detached)")))
    }

    fn sweep_plan(&self) -> Result<Vec<SweepItem>, String> {
        let inv = gather(self.host, self.runner, &self.cfg, None).map_err(fs)?;
        let mut v = vec![];
        let mut keep = vec![];
        for it in inv.items {
            let ok = (it.cls == "targets" && it.kind == "clean") || (it.cls == "containers" && it.kind == "rm-container");
            if ok {
                v.push(SweepItem { class: it.cls.clone(), key: it.key.clone(), bytes: it.bytes, note: it.why.clone() });
                keep.push(it);
            }
        }
        *self.items.borrow_mut() = keep;
        Ok(v)
    }

    fn sweep_apply(&self, s: &SweepItem) -> Result<String, String> {
        let items = self.items.borrow();
        let it = items.iter().find(|i| i.cls == s.class && i.key == s.key).ok_or("the item is not in the last plan")?;
        let ops = self.ops();
        if it.rule.is_empty() {
            return Err("kept: no rule".into());
        }
        if ops.protect_hit(&it.key).map_err(fs)?.is_some() {
            return Err("kept: protected".into());
        }
        let (_, status, note) = do_item(&ops, &self.cfg, it, None).map_err(fs)?;
        if status == "ok" {
            Ok(note)
        } else {
            Err(format!("{status}: {note}"))
        }
    }
}
