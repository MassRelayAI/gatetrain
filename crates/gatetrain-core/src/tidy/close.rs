//! T9 close: tidy only what a batch itself recorded (engine-and-governance T9, the pipeline audit).
//!
//! The batch's owned set is its chain checkout (only with `[close] own_workdir`), its lane worktrees, the target folders its runs recorded
//! (`<state>/targets/<run>.txt`, never the shared `[chain] target_dirs`), the exited containers its runs named (exact names, never a
//! pattern), the branches it created or was made of (`batch/<id>/s<n>`, `line/<lane>`) once proven on main and archived at the very sha
//! that is deleted, and its runs' own temp step folders. Stamps, out folders and `.full` files are never in it.
//!
//! [`run_close`] judges every item against the refusal codes below, judges it AGAIN at the moment of action, and only then acts. Path items
//! (targets, temp folders) are moved to `trash_dir` by the existing tidy executor; a worktree goes through the existing
//! `apply::remove_worktree` (archive tag, removal that never follows a link, unregister, branch delete by sha); a standalone branch is
//! resolved to one sha once, proven on main on that sha, archived at that sha, and deleted by compare-and-swap on that sha. A branch is never
//! touched while a registered worktree is on it. A refusal is a row, never an error: the batch stays `live` and is tried again later.
use super::apply::do_item;
use super::config::TidyCfg;
use super::inventory::{Extra, Item};
use super::ops::{lane_of, norm, Fatal, Ops};
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

/// Container name prefixes that are never touched, whatever the configuration says (the reference stack).
pub const BUILTIN_REFERENCE_PREFIXES: [&str; 1] = ["mr-ref-"];

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
    /// A path item must sit under one of these folders (`[close] roots`); none configured means nothing is owned.
    pub roots: Vec<String>,
    /// The shared `[chain] target_dirs`: a recorded target or temp folder that is one, holds one or sits inside one is never the batch's.
    pub shared_targets: Vec<String>,
}

/// What close needs to know and do. Every probe may fail or be unknown; unknown is never read as safe.
pub trait CloseEnv {
    fn exists(&self, kind: &str, key: &str) -> Result<Option<bool>>;
    /// The protect entry that covers the path (is it, contains it, sits inside it or links into it).
    fn protected(&self, key: &str) -> Result<Option<String>>;
    fn reference_stack(&self, name: &str) -> Result<bool>;
    /// The path is the main checkout.
    fn is_main_checkout(&self, path: &str) -> Result<bool>;
    /// The path is the main checkout or sits inside it.
    fn in_main_checkout(&self, path: &str) -> Result<bool> {
        self.is_main_checkout(path)
    }
    /// A registered worktree other than `path` itself sits inside `path`.
    fn contains_worktree(&self, _path: &str) -> Result<bool> {
        Ok(false)
    }
    /// The path sits strictly below one of `roots`, by every spelling this environment can see (as written, and where it leads).
    fn owned_under(&self, path: &str, roots: &[String]) -> Result<bool> {
        Ok(roots.iter().any(|r| inside(path, r)))
    }
    /// The main checkout (or the root checkout) is `path` or sits below it: such a folder can never be a `[close]` root.
    fn holds_main_checkout(&self, _path: &str) -> Result<bool> {
        Ok(false)
    }
    /// `a` is `b`, sits inside it or holds it, by every spelling this environment can see.
    fn overlaps(&self, a: &str, b: &str) -> Result<bool> {
        Ok(at_or_under(a, b) || at_or_under(b, a))
    }
    /// Why the tidy config keeps it: the worktree's lane is in `keep_lanes` (any case) or the branch is in `keep_branches`.
    fn kept(&self, _worktree: Option<&str>, _branch: Option<&str>) -> Option<String> {
        None
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>>;
    fn worktree_locked(&self, _path: &str) -> Result<bool> {
        Ok(false)
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>>;
    /// Whether the branch (by name) is proven on main: the proof of a worktree's lane.
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>>;
    /// The one commit a branch is at now.
    fn resolve_branch(&self, branch: &str) -> Result<Option<String>>;
    /// Whether that very commit is proven on main.
    fn merged_sha(&self, sha: &str) -> Result<Option<bool>>;
    /// Registered worktrees that are on the branch.
    fn branch_worktrees(&self, branch: &str) -> Result<Vec<String>>;
    /// A local tag `archive/<branch>` (or a suffixed one) that points at exactly `sha`; created when absent, never pushed, never overwritten.
    fn ensure_archive(&self, branch: &str, sha: &str) -> Result<std::result::Result<String, String>>;
    /// Delete the branch only if it still points at `sha` (compare-and-swap).
    fn delete_branch(&self, branch: &str, sha: &str) -> Result<std::result::Result<(), String>>;
    /// Why the item is in use: the lock is held, it was written within 60 minutes, the container is running.
    fn in_use(&self, kind: &str, key: &str) -> Result<Option<String>>;
    /// Worktrees (other than the main checkout) that sit on one of these branches: the lane worktrees of the batch.
    fn worktrees_on(&self, _branches: &[String]) -> Result<Vec<String>> {
        Ok(vec![])
    }
    /// Move a target or temp folder to `trash_dir`, remove a worktree, remove a container by exact name. `Err(why)` = it did not go.
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

/// `path` is `root` or sits below it (normalised spellings, nothing resolved).
fn at_or_under(path: &str, root: &str) -> bool {
    let (p, r) = (norm(path), norm(root));
    p == r || p.starts_with(&format!("{}/", r.trim_end_matches('/')))
}

/// `path` sits strictly below `root`: a record that names a root itself never owns the whole root.
fn inside(path: &str, root: &str) -> bool {
    at_or_under(path, root) && norm(path) != norm(root)
}

/// A file-system root (`/`, `C:/`, `//host/share`): never a `[close]` root, since it would own every path.
pub fn is_fs_root(p: &str) -> bool {
    let n = norm(p.trim());
    let b = n.as_bytes();
    n.is_empty() || (b.len() == 2 && b[1] == b':') || (n.starts_with("//") && n[2..].split('/').filter(|x| !x.is_empty()).count() <= 2)
}

fn row(code: &str, key: &str, why: impl Into<String>) -> Row {
    Row { code: code.into(), key: key.into(), why: why.into() }
}

/// What a passing judgement carries to the action: a branch's sha, resolved once.
#[derive(Debug, Clone, Default)]
struct Cleared {
    sha: Option<String>,
}

/// The first refusal of one item, or what it is cleared with. Nothing here changes anything.
fn judge(set: &OwnedSet, it: &OwnedItem, env: &dyn CloseEnv) -> Result<std::result::Result<Cleared, Row>> {
    let (kind, key) = (it.kind.as_str(), it.key.as_str());
    let refuse = |code: &str, why: String| Ok(Err(row(code, key, why)));
    let is_path = matches!(kind, "worktree" | "target" | "tmp");
    if is_path {
        if let Some(hit) = env.protected(key)? {
            return refuse("tidy.protected", format!("{key} is, contains, sits inside or links into the protect path {hit}"));
        }
        if kind == "worktree" && env.is_main_checkout(key)? {
            return refuse("tidy.main-checkout", format!("{key} is the main checkout"));
        }
        if env.in_main_checkout(key)? {
            return refuse("tidy.main-checkout", format!("{key} is, or sits inside, the main checkout"));
        }
        if set.roots.is_empty() {
            return refuse("tidy.not-owned", format!("{key}: no [close] roots are configured, so no path is owned"));
        }
        let owning: Vec<&String> = set.roots.iter().filter(|r| inside(key, r)).collect();
        if owning.is_empty() {
            return refuse("tidy.not-owned", format!("{key} is not strictly below any [close] root"));
        }
        let (mut good, mut bad) = (vec![], vec![]);
        for r in owning {
            if is_fs_root(r) {
                bad.push(format!("{r} is a file-system root"));
            } else if env.holds_main_checkout(r)? {
                bad.push(format!("{r} holds the main checkout"));
            } else {
                good.push(r.clone());
            }
        }
        if good.is_empty() {
            return refuse("tidy.not-owned", format!("{key} is only below [close] roots that can own nothing: {}", bad.join("; ")));
        }
        if !env.owned_under(key, &good)? {
            return refuse("tidy.not-owned", format!("{key} leads outside every [close] root once its links are resolved"));
        }
        if env.contains_worktree(key)? {
            return refuse("tidy.not-owned", format!("{key} contains a registered worktree; it is not moved with its contents"));
        }
        if matches!(kind, "target" | "tmp") {
            for s in &set.shared_targets {
                if env.overlaps(key, s)? {
                    return refuse("tidy.not-owned", format!("{key} is, holds or sits inside the shared [chain] target_dirs folder {s}; it is never one batch's"));
                }
            }
        }
    }
    if kind == "container" && (BUILTIN_REFERENCE_PREFIXES.iter().any(|p| key.starts_with(p)) || env.reference_stack(key)?) {
        return refuse("tidy.reference-stack", format!("container {key} carries a protected reference-stack prefix"));
    }
    let mut cleared = Cleared::default();
    if kind == "worktree" {
        let b = env.worktree_branch(key)?;
        if let Some(why) = env.kept(Some(key), b.as_deref()) {
            return refuse("tidy.protected", why);
        }
        match &b {
            Some(b) if set.branches.contains(b) => {}
            None if set.chain_checkout.as_deref().is_some_and(|c| norm(c) == norm(key)) => {}
            other => return refuse("tidy.not-owned", format!("worktree {key} is on {} which is not a branch of this batch", other.clone().unwrap_or_else(|| "no branch".into()))),
        }
        if env.worktree_locked(key)? {
            return refuse("tidy.in-use", format!("worktree {key} is locked"));
        }
        match env.dirty(key)? {
            Some(true) => return refuse("tidy.uncommitted", format!("{key} has uncommitted work")),
            None => return refuse("tidy.unreadable", format!("the state of {key} could not be read")),
            Some(false) => {}
        }
        if let Some(b) = &b {
            match env.merged_on_main(b)? {
                Some(true) => {}
                Some(false) => return refuse("tidy.live-lane", format!("branch {b} is not proven on main; the lane is live")),
                None => return refuse("tidy.unreadable", format!("whether {b} is on main could not be read")),
            }
        }
    }
    if kind == "branch" {
        if let Some(why) = env.kept(None, Some(key)) {
            return refuse("tidy.protected", why);
        }
        let Some(sha) = env.resolve_branch(key)? else { return refuse("tidy.unreadable", format!("branch {key} cannot be resolved to one commit")) };
        match env.merged_sha(&sha)? {
            Some(true) => {}
            Some(false) => return refuse("tidy.live-lane", format!("branch {key} ({sha}) is not proven on main")),
            None => return refuse("tidy.unreadable", format!("whether {key} ({sha}) is on main could not be read")),
        }
        let on = env.branch_worktrees(key)?;
        if !on.is_empty() {
            return refuse("tidy.in-use", format!("branch {key} has a registered worktree on it ({}); it goes with its worktree or not at all", on.join(", ")));
        }
        cleared.sha = Some(sha);
    }
    if let Some(why) = env.in_use(kind, key)? {
        return refuse("tidy.in-use", why);
    }
    Ok(Ok(cleared))
}

/// Judge, judge again, act. Probes that fail are `tidy.unreadable`, never a crash and never a removal. Everything but branches goes first,
/// so a branch is only ever judged after the worktree that was on it has gone (or refused, which keeps the branch).
pub fn run_close(set: &OwnedSet, env: &dyn CloseEnv) -> CloseReport {
    let mut rep = CloseReport::default();
    for phase in [false, true] {
        for it in set.items.iter().filter(|i| (i.kind == "branch") == phase) {
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
            let plan = match judge(set, it, env) {
                Ok(Ok(c)) => c,
                Ok(Err(r)) => {
                    rep.refusals.push(r);
                    continue;
                }
                Err(e) => {
                    rep.refusals.push(row("tidy.unreadable", &it.key, format!("probe failed: {e}")));
                    continue;
                }
            };
            // the plan may be minutes old: protect, dirty, in use and merged are read again at the moment of action
            let cleared = match judge(set, it, env) {
                Ok(Ok(c)) if c.sha == plan.sha => c,
                Ok(Ok(_)) => {
                    rep.refusals.push(row("tidy.unreadable", &it.key, format!("{} changed between the plan and the action", it.key)));
                    continue;
                }
                Ok(Err(r)) => {
                    rep.refusals.push(r);
                    continue;
                }
                Err(e) => {
                    rep.refusals.push(row("tidy.unreadable", &it.key, format!("probe failed: {e}")));
                    continue;
                }
            };
            let went = if it.kind == "branch" {
                let sha = cleared.sha.clone().unwrap_or_default();
                match env.ensure_archive(&it.key, &sha) {
                    Ok(Ok(_)) => env.delete_branch(&it.key, &sha),
                    Ok(Err(why)) => {
                        rep.refusals.push(row("tidy.no-archive-tag", &it.key, why));
                        continue;
                    }
                    Err(e) => Err(e),
                }
            } else {
                env.dispose(&it.kind, &it.key)
            };
            match went {
                Ok(Ok(())) => rep.done.push(it.key.clone()),
                Ok(Err(why)) => rep.residue.push(format!("{}: {why}", it.key)),
                Err(e) => rep.residue.push(format!("{}: {e}", it.key)),
            }
        }
    }
    // `check`: what is still there that was acted on
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

/// [`CloseEnv`] over `Ops` (a runner and the host's tidy config).
pub struct OpsEnv<'a> {
    pub ops: &'a Ops<'a>,
    pub cfg: &'a TidyCfg,
    /// Container name prefixes that are never touched, besides the built-in ones (`[close] protected_container_prefixes`).
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

    fn resolved(&self, p: &str, follow_last: bool) -> Result<String> {
        let r = if follow_last { self.ops.rp(p) } else { self.ops.rpl(p) }.map_err(e)?;
        r.ok_or_else(|| Error::Batch(format!("cannot resolve {p}; what cannot be located is never judged safe")))
    }

    /// An item's spellings: as written, where it sits (its parent resolved), and where it leads (a link followed).
    fn item_forms(&self, p: &str) -> Result<Vec<String>> {
        let mut v = vec![norm(p), self.resolved(p, false)?, self.resolved(p, true)?];
        v.dedup();
        Ok(v)
    }

    /// A place's spellings: as written and resolved.
    fn place_forms(&self, p: &str) -> Result<Vec<String>> {
        let mut v = vec![norm(p), self.resolved(p, true)?];
        v.dedup();
        Ok(v)
    }

    /// Every spelling of the main checkout and of the root checkout.
    fn mains(&self) -> Result<Vec<String>> {
        let mut v = vec![];
        for m in [self.cfg.main_checkout.as_deref(), self.cfg.root_checkout.as_deref()].into_iter().flatten().filter(|m| !m.is_empty()) {
            v.extend(self.place_forms(m)?);
        }
        Ok(v)
    }

    fn rev(&self, r: &str) -> Result<Option<String>> {
        let c = self.ops.git(self.main()?, &["rev-parse", "--verify", "--quiet", r]).map_err(e)?;
        let s = c.out.trim().to_string();
        Ok((c.ok() && !s.is_empty() && !s.contains(char::is_whitespace)).then_some(s))
    }
}

impl CloseEnv for OpsEnv<'_> {
    fn exists(&self, kind: &str, key: &str) -> Result<Option<bool>> {
        match kind {
            "container" => Ok(Some(self.docker_names(true)?.iter().any(|n| n == key))),
            "branch" => Ok(Some(self.rev(&format!("refs/heads/{key}^{{commit}}"))?.is_some())),
            _ => self.ops.exists(key).map_err(e),
        }
    }
    fn protected(&self, key: &str) -> Result<Option<String>> {
        self.ops.protect_hit(key).map_err(e)
    }
    fn reference_stack(&self, name: &str) -> Result<bool> {
        Ok(self.reference_prefixes.iter().any(|p| !p.is_empty() && name.starts_with(p.as_str())))
    }
    // the main-checkout, worktree and root rules compare every spelling: as written and resolved, on both sides (a junction, a
    // symlinked folder or a short name never makes a path look like somewhere else); a spelling that cannot be resolved is unreadable
    fn is_main_checkout(&self, path: &str) -> Result<bool> {
        let (forms, mains) = (self.item_forms(path)?, self.mains()?);
        Ok(forms.iter().any(|f| mains.contains(f)))
    }
    fn in_main_checkout(&self, path: &str) -> Result<bool> {
        let (forms, mains) = (self.item_forms(path)?, self.mains()?);
        Ok(forms.iter().any(|f| mains.iter().any(|m| at_or_under(f, m))))
    }
    fn contains_worktree(&self, path: &str) -> Result<bool> {
        let forms = self.item_forms(path)?;
        let own = [norm(path), self.resolved(path, false)?];
        for w in self.ops.worktrees().map_err(e)? {
            let wf = self.place_forms(&w.path)?;
            if wf.iter().any(|x| own.contains(x)) {
                continue; // the worktree is the item itself
            }
            if wf.iter().any(|x| forms.iter().any(|f| at_or_under(x, f))) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn owned_under(&self, path: &str, roots: &[String]) -> Result<bool> {
        // below a root as written, and where it sits below where that root leads
        let at = self.resolved(path, false)?;
        for r in roots {
            if inside(path, r) && inside(&at, &self.resolved(r, true)?) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn overlaps(&self, a: &str, b: &str) -> Result<bool> {
        let (fa, fb) = (self.item_forms(a)?, self.place_forms(b)?);
        Ok(fa.iter().any(|x| fb.iter().any(|y| at_or_under(x, y) || at_or_under(y, x))))
    }
    fn holds_main_checkout(&self, path: &str) -> Result<bool> {
        let (forms, mains) = (self.place_forms(path)?, self.mains()?);
        Ok(mains.iter().any(|m| forms.iter().any(|f| at_or_under(m, f))))
    }
    fn kept(&self, worktree: Option<&str>, branch: Option<&str>) -> Option<String> {
        if let Some(b) = branch.filter(|b| self.cfg.keep_branches.iter().any(|k| k == b)) {
            return Some(format!("branch {b} is on the tidy config's keep_branches"));
        }
        let lane = lane_of(worktree?, self.cfg);
        self.cfg.keep_lanes.iter().any(|k| k.to_lowercase() == lane.to_lowercase()).then(|| format!("lane {lane} is on the tidy config's keep_lanes"))
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>> {
        Ok(self.ops.worktrees().map_err(e)?.into_iter().find(|w| norm(&w.path) == norm(path)).and_then(|w| w.branch))
    }
    fn worktree_locked(&self, path: &str) -> Result<bool> {
        Ok(self.ops.worktrees().map_err(e)?.into_iter().any(|w| norm(&w.path) == norm(path) && w.locked))
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>> {
        self.ops.dirty(path).map_err(e)
    }
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>> {
        self.ops.merged(branch).map_err(e)
    }
    fn resolve_branch(&self, branch: &str) -> Result<Option<String>> {
        if !super::ops::safe_branch(branch) {
            return Ok(None);
        }
        self.rev(&format!("refs/heads/{branch}^{{commit}}"))
    }
    fn merged_sha(&self, sha: &str) -> Result<Option<bool>> {
        self.ops.merged_sha(sha).map_err(e)
    }
    fn branch_worktrees(&self, branch: &str) -> Result<Vec<String>> {
        Ok(self.ops.worktrees().map_err(e)?.into_iter().filter(|w| w.branch.as_deref() == Some(branch)).map(|w| w.path).collect())
    }
    fn ensure_archive(&self, branch: &str, sha: &str) -> Result<std::result::Result<String, String>> {
        let m = self.main()?;
        let mut tag = format!("archive/{branch}");
        let mut at = self.rev(&format!("refs/tags/{tag}^{{commit}}"))?;
        if at.as_deref().is_some_and(|s| s != sha) {
            tag = format!("{tag}-{}", &sha[..sha.len().min(8)]); // that name belongs to another commit: never overwrite or reuse it
            at = self.rev(&format!("refs/tags/{tag}^{{commit}}"))?;
        }
        if at.as_deref() != Some(sha) {
            let t = self.ops.git(m, &["tag", &tag, sha]).map_err(e)?;
            if !t.ok() {
                return Ok(Err(format!("could not create the archive tag {tag} for {branch}; the branch is kept")));
            }
        }
        match self.rev(&format!("refs/tags/{tag}^{{commit}}"))? {
            Some(s) if s == sha => Ok(Ok(tag)),
            _ => Ok(Err(format!("archive tag {tag} does not point at {sha}; the branch is kept"))),
        }
    }
    fn delete_branch(&self, branch: &str, sha: &str) -> Result<std::result::Result<(), String>> {
        let d = self.ops.git(self.main()?, &["update-ref", "-d", &format!("refs/heads/{branch}"), sha]).map_err(e)?;
        Ok(if d.ok() { Ok(()) } else { Err(format!("branch {branch} no longer points at {sha} (or could not be deleted); it is kept")) })
    }
    fn in_use(&self, kind: &str, key: &str) -> Result<Option<String>> {
        if kind == "container" {
            return Ok(self.docker_names(false)?.iter().any(|n| n == key).then(|| format!("container {key} is running")));
        }
        if kind == "branch" {
            return Ok(None);
        }
        if let Some((_, run)) = self.ops.hold_named().map_err(e)?.into_iter().find(|(p, _)| at_or_under(key, p) || at_or_under(p, key)) {
            return Ok(Some(format!("the host lock is held by run {run}, which names {key}")));
        }
        match self.ops.recent(key, 60.0).map_err(e)? {
            Some(true) => Ok(Some(format!("{key} was written within the last 60 minutes"))),
            Some(false) => Ok(None),
            None => Ok(Some(format!("whether {key} was written recently could not be told"))),
        }
    }
    fn worktrees_on(&self, branches: &[String]) -> Result<Vec<String>> {
        Ok(self.ops.worktrees().map_err(e)?.into_iter().filter(|w| w.branch.as_ref().is_some_and(|b| branches.contains(b)) && !self.in_main_checkout(&w.path).unwrap_or(true)).map(|w| w.path).collect())
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
            "worktree" => {
                // the existing remover: archive tag at the sha, a removal that never follows a link, unregister, branch by sha
                let Some(branch) = self.worktree_branch(key)? else { return Ok(Err(format!("{key} is not on a branch; a detached worktree is not removed by close"))) };
                let mut it = Item::new("worktrees", "remove-worktree", key, None, "owned by the closed batch");
                it.extra = Extra::Worktree { branch, links: 0 };
                verdict(do_item(self.ops, self.cfg, &it, None))
            }
            _ => verdict(do_item(self.ops, self.cfg, &Item::new("targets", "move", key, None, "owned by the closed batch"), None)),
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
    /// Branches that already have an archive tag at their own sha.
    pub tags: std::collections::HashSet<String>,
    /// When true a missing archive tag is created (the real env does); by default it cannot be.
    pub can_tag: bool,
    pub fail_dispose: std::collections::HashSet<String>,
    pub disposed: std::cell::RefCell<Vec<(String, String)>>,
    /// The sha a branch is at (default `sha-<branch>`) and the sha `archive/<branch>` points at (a branch absent here has no tag sha).
    pub branch_sha: std::collections::HashMap<String, String>,
    pub tag_sha: std::collections::HashMap<String, String>,
    /// `dirty(path)` turns true from this call number on (1 = the first probe): a worktree written to between plan and action.
    pub dirty_on_call: std::collections::HashMap<String, usize>,
    pub dirty_calls: std::cell::RefCell<std::collections::HashMap<String, usize>>,
    pub dirty_unknown: std::collections::HashSet<String>,
    pub merge_unknown: std::collections::HashSet<String>,
    /// `dispose` says ok and removes nothing.
    pub ghost: std::collections::HashSet<String>,
    pub locked: std::collections::HashSet<String>,
    /// `resolve_branch(branch)` answers this sha from the second call on: a commit lands between the plan and the action.
    pub sha_moves: std::collections::HashMap<String, String>,
    pub resolve_calls: std::cell::RefCell<std::collections::HashMap<String, usize>>,
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

    fn is_main(&self, p: &str) -> bool {
        self.main_checkout.as_deref().is_some_and(|m| norm(m) == norm(p))
    }

    fn registered(&self, p: &str) -> bool {
        self.present.borrow().contains(&("worktree".to_string(), p.to_string()))
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
        Ok(self.protect.iter().find(|p| at_or_under(key, p) || at_or_under(p, key)).cloned())
    }
    fn reference_stack(&self, name: &str) -> Result<bool> {
        Ok(self.reference_prefixes.iter().any(|p| name.starts_with(p.as_str())))
    }
    fn is_main_checkout(&self, path: &str) -> Result<bool> {
        Ok(self.is_main(path))
    }
    fn in_main_checkout(&self, path: &str) -> Result<bool> {
        Ok(self.main_checkout.as_deref().is_some_and(|m| at_or_under(path, m)))
    }
    fn contains_worktree(&self, path: &str) -> Result<bool> {
        Ok(self.wt_branch.keys().any(|w| self.registered(w) && norm(w) != norm(path) && at_or_under(w, path)))
    }
    fn holds_main_checkout(&self, path: &str) -> Result<bool> {
        Ok(self.main_checkout.as_deref().is_some_and(|m| at_or_under(m, path)))
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>> {
        Ok(self.wt_branch.get(path).cloned())
    }
    fn worktree_locked(&self, path: &str) -> Result<bool> {
        Ok(self.locked.contains(path))
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>> {
        if self.dirty_unknown.contains(path) {
            return Ok(None);
        }
        let n = {
            let mut c = self.dirty_calls.borrow_mut();
            let n = c.entry(path.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        Ok(Some(self.dirty.contains(path) || self.dirty_on_call.get(path).is_some_and(|from| n >= *from)))
    }
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>> {
        if self.merge_unknown.contains(branch) {
            return Ok(None);
        }
        Ok(Some(!self.unmerged.contains(branch)))
    }
    fn resolve_branch(&self, branch: &str) -> Result<Option<String>> {
        let n = {
            let mut c = self.resolve_calls.borrow_mut();
            let n = c.entry(branch.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        if let (true, Some(s)) = (n >= 2, self.sha_moves.get(branch)) {
            return Ok(Some(s.clone()));
        }
        Ok(Some(self.branch_sha.get(branch).cloned().unwrap_or_else(|| format!("sha-{branch}"))))
    }
    fn merged_sha(&self, sha: &str) -> Result<Option<bool>> {
        let b = sha.strip_prefix("sha-").unwrap_or(sha);
        self.merged_on_main(b)
    }
    fn branch_worktrees(&self, branch: &str) -> Result<Vec<String>> {
        let mut v: Vec<String> = self.wt_branch.iter().filter(|(p, b)| b.as_str() == branch && self.registered(p)).map(|(p, _)| p.clone()).collect();
        v.sort();
        Ok(v)
    }
    fn ensure_archive(&self, branch: &str, sha: &str) -> Result<std::result::Result<String, String>> {
        if self.tags.contains(branch) || self.tag_sha.get(branch).is_some_and(|s| s == sha) || self.can_tag {
            return Ok(Ok(format!("archive/{branch}")));
        }
        Ok(Err(format!("no local tag archive/{branch} at {sha}; a branch is deleted only after it is archived (never pushed)")))
    }
    fn delete_branch(&self, branch: &str, _sha: &str) -> Result<std::result::Result<(), String>> {
        self.disposed.borrow_mut().push(("branch".to_string(), branch.to_string()));
        if self.fail_dispose.contains(branch) {
            return Ok(Err("the delete failed".into()));
        }
        if !self.ghost.contains(branch) {
            self.present.borrow_mut().remove(&("branch".to_string(), branch.to_string()));
        }
        Ok(Ok(()))
    }
    fn in_use(&self, _kind: &str, key: &str) -> Result<Option<String>> {
        Ok(self.in_use.get(key).cloned())
    }
    fn worktrees_on(&self, branches: &[String]) -> Result<Vec<String>> {
        let mut v: Vec<String> = self.wt_branch.iter().filter(|(p, b)| branches.contains(b) && !self.is_main(p) && self.registered(p)).map(|(p, _)| p.clone()).collect();
        v.sort();
        Ok(v)
    }
    fn dispose(&self, kind: &str, key: &str) -> Result<std::result::Result<(), String>> {
        self.disposed.borrow_mut().push((kind.to_string(), key.to_string()));
        if self.fail_dispose.contains(key) {
            return Ok(Err("the move failed".into()));
        }
        if self.ghost.contains(key) {
            return Ok(Ok(()));
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
    fn in_main_checkout(&self, path: &str) -> Result<bool> {
        self.with(|e| e.in_main_checkout(path))
    }
    fn contains_worktree(&self, path: &str) -> Result<bool> {
        self.with(|e| e.contains_worktree(path))
    }
    fn owned_under(&self, path: &str, roots: &[String]) -> Result<bool> {
        self.with(|e| e.owned_under(path, roots))
    }
    fn holds_main_checkout(&self, path: &str) -> Result<bool> {
        self.with(|e| e.holds_main_checkout(path))
    }
    fn overlaps(&self, a: &str, b: &str) -> Result<bool> {
        self.with(|e| e.overlaps(a, b))
    }
    fn kept(&self, worktree: Option<&str>, branch: Option<&str>) -> Option<String> {
        self.with(|e| e.kept(worktree, branch))
    }
    fn worktree_branch(&self, path: &str) -> Result<Option<String>> {
        self.with(|e| e.worktree_branch(path))
    }
    fn worktree_locked(&self, path: &str) -> Result<bool> {
        self.with(|e| e.worktree_locked(path))
    }
    fn dirty(&self, path: &str) -> Result<Option<bool>> {
        self.with(|e| e.dirty(path))
    }
    fn merged_on_main(&self, branch: &str) -> Result<Option<bool>> {
        self.with(|e| e.merged_on_main(branch))
    }
    fn resolve_branch(&self, branch: &str) -> Result<Option<String>> {
        self.with(|e| e.resolve_branch(branch))
    }
    fn merged_sha(&self, sha: &str) -> Result<Option<bool>> {
        self.with(|e| e.merged_sha(sha))
    }
    fn branch_worktrees(&self, branch: &str) -> Result<Vec<String>> {
        self.with(|e| e.branch_worktrees(branch))
    }
    fn ensure_archive(&self, branch: &str, sha: &str) -> Result<std::result::Result<String, String>> {
        self.with(|e| e.ensure_archive(branch, sha))
    }
    fn delete_branch(&self, branch: &str, sha: &str) -> Result<std::result::Result<(), String>> {
        self.with(|e| e.delete_branch(branch, sha))
    }
    fn in_use(&self, kind: &str, key: &str) -> Result<Option<String>> {
        self.with(|e| e.in_use(kind, key))
    }
    fn worktrees_on(&self, branches: &[String]) -> Result<Vec<String>> {
        self.with(|e| e.worktrees_on(branches))
    }
    fn dispose(&self, kind: &str, key: &str) -> Result<std::result::Result<(), String>> {
        self.with(|e| e.dispose(kind, key))
    }
}
