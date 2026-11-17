//! T9 close: tidy only what a batch itself recorded (engine-and-governance T9, the pipeline audit).
//!
//! The batch's owned set is its chain checkout, its batch and lane worktrees, the target folders its runs recorded, the exited containers
//! its runs named (by exact name, never a pattern), `line/<lane>` and `line/batch-<b>` once proven on main with a LOCAL `archive/<branch>`
//! tag, and the runs' own temp step folders. Stamps, out folders and `.full` files are never in it.
//!
//! [`run_close`] judges every item against the refusal codes below and acts only on the ones that pass; the actions go through
//! [`CloseEnv`], whose production form ([`OpsEnv`]) moves paths to `trash_dir` with the existing tidy executor (`apply::do_item`).
//! A refusal is a row, never an error: the batch stays `live` and is tried again later.
use super::apply::do_item;
use super::config::TidyCfg;
use super::inventory::Item;
use super::ops::{norm, Fatal, Ops};
use crate::{Error, Result};

/// Refusal codes (decision-log rules).
pub const CODES: [&str; 10] = [
    "tidy.protected",
    "tidy.reference-stack",
    "tidy.uncommitted",
    "tidy.live-lane",
    "tidy.main-checkout",
    "tidy.not-owned",
    "tidy.in-use",
    "tidy.no-archive-tag",
    "tidy.unreadable",
    "tidy.check-not-clean",
];

#[derive(Debug, Clone, PartialEq)]
pub struct OwnedItem {
    /// `worktree` | `target` | `container` | `branch` | `tmp`
    pub kind: String,
    pub key: String,
}

impl OwnedItem {
    pub fn new(kind: &str, key: &str) -> OwnedItem {
        OwnedItem { kind: kind.into(), key: key.into() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct OwnedSet {
    pub items: Vec<OwnedItem>,
    /// Branches a worktree of the set may be on: the batch's own stack branches and its lane refs.
    pub branches: Vec<String>,
    /// The batch's chain checkout (the one worktree that may be detached).
    pub chain_checkout: Option<String>,
    /// When non-empty, a path item must sit under one of these folders (`[close] roots`).
    pub roots: Vec<String>,
}

/// What close needs to know and do. Every probe may fail or be unknown; unknown is never read as safe.
pub trait CloseEnv {
    fn exists(&self, kind: &str, key: &str) -> Result<Option<bool>>;
    /// The protect entry that covers the path (is it, contains it, sits inside it or links into it).
    fn protected(&self, key: &str) -> Result<Option<String>>;
    fn reference_stack(&self, name: &str) -> Result<bool>;
    fn is_main_checkout(&self, path: &str) -> Result<bool>;
    fn worktree_branch(&self, path: &str) -> Result<Option<String>>;
    fn dirty(&self, path: &str) -> Result<Option<bool>>;
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>>;
    /// Why the item is in use: the lock is held, it was written within 60 minutes, the container is running.
    fn in_use(&self, kind: &str, key: &str) -> Result<Option<String>>;
    fn archive_tag(&self, branch: &str) -> Result<bool>;
    /// Worktrees (other than the main checkout) that sit on one of these branches: the lane worktrees of the batch.
    fn worktrees_on(&self, _branches: &[String]) -> Result<Vec<String>> {
        Ok(vec![])
    }
    /// Move a path to `trash_dir`, remove a container by exact name, delete a branch. `Err(why)` = it did not go.
    fn dispose(&self, kind: &str, key: &str) -> Result<std::result::Result<(), String>>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub code: String,
    pub key: String,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct CloseReport {
    pub refusals: Vec<Row>,
    pub done: Vec<String>,
    /// Already gone when looked at.
    pub gone: Vec<String>,
    /// What `check` still shows of the owned set after the run.
    pub residue: Vec<String>,
}

impl CloseReport {
    /// Closed means the owned set is gone: nothing refused, nothing left over.
    pub fn clean(&self) -> bool {
        self.refusals.is_empty() && self.residue.is_empty()
    }
}

fn under(path: &str, root: &str) -> bool {
    let (p, r) = (norm(path), norm(root));
    p == r || p.starts_with(&format!("{r}/"))
}

fn row(code: &str, key: &str, why: impl Into<String>) -> Row {
    Row { code: code.into(), key: key.into(), why: why.into() }
}

/// The first refusal of one item, `None` when it may go.
fn judge(set: &OwnedSet, it: &OwnedItem, env: &dyn CloseEnv) -> Result<Option<Row>> {
    let (kind, key) = (it.kind.as_str(), it.key.as_str());
    let is_path = matches!(kind, "worktree" | "target" | "tmp");
    if is_path && env.protected(key)?.is_some() {
        let hit = env.protected(key)?.unwrap_or_default();
        return Ok(Some(row("tidy.protected", key, format!("{key} is, contains, sits inside or links into the protect path {hit}"))));
    }
    if kind == "container" && env.reference_stack(key)? {
        return Ok(Some(row("tidy.reference-stack", key, format!("container {key} carries a protected reference-stack prefix"))));
    }
    if kind == "worktree" && env.is_main_checkout(key)? {
        return Ok(Some(row("tidy.main-checkout", key, format!("{key} is the main checkout"))));
    }
    if is_path && !set.roots.is_empty() && !set.roots.iter().any(|r| under(key, r)) {
        return Ok(Some(row("tidy.not-owned", key, format!("{key} is not under any [close] root"))));
    }
    if kind == "worktree" {
        match env.worktree_branch(key)? {
            Some(b) if set.branches.contains(&b) => {}
            None if set.chain_checkout.as_deref().is_some_and(|c| norm(c) == norm(key)) => {}
            other => return Ok(Some(row("tidy.not-owned", key, format!("worktree {key} is on {} which is not a branch of this batch", other.unwrap_or_else(|| "no branch".into()))))),
        }
        match env.dirty(key)? {
            Some(true) => return Ok(Some(row("tidy.uncommitted", key, format!("{key} has uncommitted work")))),
            None => return Ok(Some(row("tidy.unreadable", key, format!("the state of {key} could not be read")))),
            Some(false) => {}
        }
        if let Some(b) = env.worktree_branch(key)? {
            match env.merged_on_main(&b)? {
                Some(true) => {}
                Some(false) => return Ok(Some(row("tidy.live-lane", key, format!("branch {b} is not proven on main; the lane is live")))),
                None => return Ok(Some(row("tidy.unreadable", key, format!("whether {b} is on main could not be read")))),
            }
        }
    }
    if kind == "branch" {
        match env.merged_on_main(key)? {
            Some(true) => {}
            Some(false) => return Ok(Some(row("tidy.live-lane", key, format!("branch {key} is not proven on main")))),
            None => return Ok(Some(row("tidy.unreadable", key, format!("whether {key} is on main could not be read")))),
        }
        if !env.archive_tag(key)? {
            return Ok(Some(row("tidy.no-archive-tag", key, format!("no local tag archive/{key}; a branch is deleted only after it is archived (never pushed)"))));
        }
    }
    if let Some(why) = env.in_use(kind, key)? {
        return Ok(Some(row("tidy.in-use", key, why)));
    }
    Ok(None)
}

/// Judge and act on every owned item, then look again. Probes that fail are `tidy.unreadable`, never a crash and never a removal.
pub fn run_close(set: &OwnedSet, env: &dyn CloseEnv) -> CloseReport {
    let mut rep = CloseReport::default();
    let mut approved: Vec<&OwnedItem> = vec![];
    for it in &set.items {
        match env.exists(&it.kind, &it.key) {
            Ok(Some(false)) => {
                rep.gone.push(it.key.clone());
                continue;
            }
            Ok(Some(true)) => {}
            Ok(None) | Err(_) => {
                rep.refusals.push(row("tidy.unreadable", &it.key, format!("whether {} exists could not be read", it.key)));
                continue;
            }
        }
        match judge(set, it, env) {
            Ok(None) => approved.push(it),
            Ok(Some(r)) => rep.refusals.push(r),
            Err(e) => rep.refusals.push(row("tidy.unreadable", &it.key, format!("probe failed: {e}"))),
        }
    }
    for it in approved {
        match env.dispose(&it.kind, &it.key) {
            Ok(Ok(())) => rep.done.push(it.key.clone()),
            Ok(Err(why)) => rep.residue.push(format!("{}: {why}", it.key)),
            Err(e) => rep.residue.push(format!("{}: {e}", it.key)),
        }
    }
    // `check`: what is still there that was approved and acted on, and what a refusal left in place
    for it in &set.items {
        if rep.done.contains(&it.key) {
            if let Ok(Some(true)) = env.exists(&it.kind, &it.key) {
                rep.residue.push(format!("{}: still present after the move", it.key));
            }
        }
    }
    rep.residue.sort();
    rep.residue.dedup();
    rep
}

// ---------------------------------------------------------------- production: the existing tidy executor

/// [`CloseEnv`] over `Ops` (a runner and the host's tidy config). Paths are moved with `apply::do_item` kind `move`; a worktree is then
/// unregistered with `git worktree remove --force` on the folder that is already gone.
pub struct OpsEnv<'a> {
    pub ops: &'a Ops<'a>,
    pub cfg: &'a TidyCfg,
    /// Container name prefixes that are never touched (`[close] protected_container_prefixes`).
    pub reference_prefixes: Vec<String>,
}

fn e(f: Fatal) -> Error {
    Error::Batch(f.to_string())
}

impl OpsEnv<'_> {
    fn main(&self) -> Result<&str> {
        self.cfg.main_checkout.as_deref().ok_or_else(|| Error::Batch("close needs main_checkout in the tidy config".into()))
    }

    fn docker_names(&self, all: bool) -> Result<Vec<String>> {
        let cmd = if all { r#"docker ps -a --format "{{.Names}}""# } else { r#"docker ps --format "{{.Names}}""# };
        let o = self.ops.run0(cmd).map_err(e)?;
        if !o.ok() {
            return Err(Error::Batch("docker ps failed".into()));
        }
        Ok(o.out.split_whitespace().map(String::from).collect())
    }
}

impl CloseEnv for OpsEnv<'_> {
    fn exists(&self, kind: &str, key: &str) -> Result<Option<bool>> {
        match kind {
            "container" => Ok(Some(self.docker_names(true)?.iter().any(|n| n == key))),
            "branch" => {
                let c = self.ops.git(self.main()?, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{key}")]).map_err(e)?;
                Ok(Some(c.ok()))
            }
            _ => self.ops.exists(key).map_err(e),
        }
    }
    fn protected(&self, key: &str) -> Result<Option<String>> {
        self.ops.protect_hit(key).map_err(e)
    }
    fn reference_stack(&self, name: &str) -> Result<bool> {
        Ok(self.reference_prefixes.iter().any(|p| !p.is_empty() && name.starts_with(p.as_str())))
    }
    fn is_main_checkout(&self, path: &str) -> Result<bool> {
        Ok([self.cfg.main_checkout.as_deref(), self.cfg.root_checkout.as_deref()].into_iter().flatten().any(|m| norm(m) == norm(path)))
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>> {
        Ok(self.ops.worktrees().map_err(e)?.into_iter().find(|w| norm(&w.path) == norm(path)).and_then(|w| w.branch))
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>> {
        self.ops.dirty(path).map_err(e)
    }
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>> {
        self.ops.merged(branch).map_err(e)
    }
    fn in_use(&self, kind: &str, key: &str) -> Result<Option<String>> {
        if kind == "container" {
            return Ok(self.docker_names(false)?.iter().any(|n| n == key).then(|| format!("container {key} is running")));
        }
        if kind == "branch" {
            return Ok(None);
        }
        if let Some((_, run)) = self.ops.hold_named().map_err(e)?.into_iter().find(|(p, _)| under(key, p) || under(p, key)) {
            return Ok(Some(format!("the host lock is held by run {run}, which names {key}")));
        }
        match self.ops.recent(key, 60.0).map_err(e)? {
            Some(true) => Ok(Some(format!("{key} was written within the last 60 minutes"))),
            Some(false) => Ok(None),
            None => Ok(Some(format!("whether {key} was written recently could not be told"))),
        }
    }
    fn worktrees_on(&self, branches: &[String]) -> Result<Vec<String>> {
        Ok(self.ops.worktrees().map_err(e)?.into_iter().filter(|w| w.branch.as_ref().is_some_and(|b| branches.contains(b)) && !self.cfg.main_checkout.as_deref().is_some_and(|m| norm(m) == norm(&w.path))).map(|w| w.path).collect())
    }
    fn archive_tag(&self, branch: &str) -> Result<bool> {
        let c = self.ops.git(self.main()?, &["rev-parse", "--verify", "--quiet", &format!("refs/tags/archive/{branch}")]).map_err(e)?;
        Ok(c.ok())
    }
    fn dispose(&self, kind: &str, key: &str) -> Result<std::result::Result<(), String>> {
        let verdict = |r: super::ops::R<(Option<i64>, &'static str, String)>| -> Result<std::result::Result<(), String>> {
            match r {
                Ok((_, "ok", _)) => Ok(Ok(())),
                Ok((_, status, note)) => Ok(Err(format!("{status}: {note}"))),
                Err(Fatal::Guard(m)) | Err(Fatal::Item(m)) => Ok(Err(m)),
                Err(f) => Err(e(f)),
            }
        };
        match kind {
            "container" => verdict(do_item(self.ops, self.cfg, &Item::new("containers", "rm-container", key, None, "owned by the closed batch"), None)),
            "branch" => {
                let m = self.main()?;
                let sha = self.ops.git(m, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{key}^{{commit}}")]).map_err(e)?;
                let sha = sha.out.trim().to_string();
                if sha.is_empty() {
                    return Ok(Err(format!("cannot resolve {key}")));
                }
                let d = self.ops.git(m, &["update-ref", "-d", &format!("refs/heads/{key}"), &sha]).map_err(e)?;
                Ok(if d.ok() { Ok(()) } else { Err(format!("branch {key} could not be deleted")) })
            }
            _ => {
                let cls = if kind == "worktree" { "worktrees" } else { "targets" };
                let r = verdict(do_item(self.ops, self.cfg, &Item::new(cls, "move", key, None, "owned by the closed batch"), None))?;
                if r.is_ok() && kind == "worktree" {
                    let m = self.main()?;
                    let g = self.ops.git(m, &["worktree", "remove", "--force", key]).map_err(e)?;
                    if !g.ok() {
                        return Ok(Err(format!("{key} was moved but git could not unregister the worktree")));
                    }
                }
                Ok(r)
            }
        }
    }
}

// ---------------------------------------------------------------- a fake for tests

/// A scripted environment: no docker, no git, no removal. Public for the batch tests (feature `test-fakes`).
#[cfg(any(test, feature = "test-fakes"))]
#[derive(Default)]
pub struct FakeCloseEnv {
    pub present: std::cell::RefCell<std::collections::HashSet<(String, String)>>,
    pub unreadable: std::collections::HashSet<String>,
    pub protect: Vec<String>,
    pub reference_prefixes: Vec<String>,
    pub main_checkout: Option<String>,
    pub wt_branch: std::collections::HashMap<String, String>,
    pub dirty: std::collections::HashSet<String>,
    pub unmerged: std::collections::HashSet<String>,
    pub in_use: std::collections::HashMap<String, String>,
    pub tags: std::collections::HashSet<String>,
    pub fail_dispose: std::collections::HashSet<String>,
    /// Lane worktrees `worktrees_on` reports.
    pub lane_worktrees: Vec<String>,
    pub disposed: std::cell::RefCell<Vec<(String, String)>>,
}

#[cfg(any(test, feature = "test-fakes"))]
impl FakeCloseEnv {
    pub fn with(items: &[(&str, &str)]) -> FakeCloseEnv {
        let f = FakeCloseEnv::default();
        for (k, v) in items {
            f.present.borrow_mut().insert((k.to_string(), v.to_string()));
        }
        f
    }
}

#[cfg(any(test, feature = "test-fakes"))]
impl CloseEnv for FakeCloseEnv {
    fn exists(&self, kind: &str, key: &str) -> Result<Option<bool>> {
        if self.unreadable.contains(key) {
            return Ok(None);
        }
        Ok(Some(self.present.borrow().contains(&(kind.to_string(), key.to_string()))))
    }
    fn protected(&self, key: &str) -> Result<Option<String>> {
        Ok(self.protect.iter().find(|p| under(key, p) || under(p, key)).cloned())
    }
    fn reference_stack(&self, name: &str) -> Result<bool> {
        Ok(self.reference_prefixes.iter().any(|p| name.starts_with(p.as_str())))
    }
    fn is_main_checkout(&self, path: &str) -> Result<bool> {
        Ok(self.main_checkout.as_deref().is_some_and(|m| norm(m) == norm(path)))
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>> {
        Ok(self.wt_branch.get(path).cloned())
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>> {
        Ok(Some(self.dirty.contains(path)))
    }
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>> {
        Ok(Some(!self.unmerged.contains(branch)))
    }
    fn in_use(&self, _kind: &str, key: &str) -> Result<Option<String>> {
        Ok(self.in_use.get(key).cloned())
    }
    fn archive_tag(&self, branch: &str) -> Result<bool> {
        Ok(self.tags.contains(branch))
    }
    fn worktrees_on(&self, _branches: &[String]) -> Result<Vec<String>> {
        Ok(self.lane_worktrees.clone())
    }
    fn dispose(&self, kind: &str, key: &str) -> Result<std::result::Result<(), String>> {
        self.disposed.borrow_mut().push((kind.to_string(), key.to_string()));
        if self.fail_dispose.contains(key) {
            return Ok(Err("the move failed".into()));
        }
        self.present.borrow_mut().remove(&(kind.to_string(), key.to_string()));
        Ok(Ok(()))
    }
}

/// An [`OpsEnv`] that owns what it borrows (the host, its runner and its tidy config), so a seam can hold it.
pub struct OwnedEnv {
    pub host: crate::config::Host,
    pub runner: Box<dyn super::Runner>,
    pub cfg: TidyCfg,
    pub reference_prefixes: Vec<String>,
}

impl OwnedEnv {
    fn with<T>(&self, f: impl FnOnce(&OpsEnv<'_>) -> T) -> T {
        let ops = Ops::new(&self.host, self.runner.as_ref(), &self.cfg);
        f(&OpsEnv { ops: &ops, cfg: &self.cfg, reference_prefixes: self.reference_prefixes.clone() })
    }
}

impl CloseEnv for OwnedEnv {
    fn exists(&self, kind: &str, key: &str) -> Result<Option<bool>> {
        self.with(|e| e.exists(kind, key))
    }
    fn protected(&self, key: &str) -> Result<Option<String>> {
        self.with(|e| e.protected(key))
    }
    fn reference_stack(&self, name: &str) -> Result<bool> {
        self.with(|e| e.reference_stack(name))
    }
    fn is_main_checkout(&self, path: &str) -> Result<bool> {
        self.with(|e| e.is_main_checkout(path))
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>> {
        self.with(|e| e.worktree_branch(path))
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>> {
        self.with(|e| e.dirty(path))
    }
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>> {
        self.with(|e| e.merged_on_main(branch))
    }
    fn in_use(&self, kind: &str, key: &str) -> Result<Option<String>> {
        self.with(|e| e.in_use(kind, key))
    }
    fn archive_tag(&self, branch: &str) -> Result<bool> {
        self.with(|e| e.archive_tag(branch))
    }
    fn worktrees_on(&self, branches: &[String]) -> Result<Vec<String>> {
        self.with(|e| e.worktrees_on(branches))
    }
    fn dispose(&self, kind: &str, key: &str) -> Result<std::result::Result<(), String>> {
        self.with(|e| e.dispose(kind, key))
    }
}
