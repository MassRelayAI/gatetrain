//! `gather`: the inventory of leftovers (`tidy.gather`), with its plan items. It changes nothing on the host EXCEPT one small file: it
//! records when each unstamped recycle-bin entry was first seen (`.gatetrain-seen.json` inside the bin), so a `check` writes that file.
use super::config::TidyCfg;
use crate::config::Host;
use crate::pycompat::J;
use super::ops::{guard, lane_of, norm, safe_branch, Fatal, Ops, Wt, R};
use crate::pycompat::{fnmatchcase, fmt_g, split_ws, splitlines, strip};
use std::collections::HashSet;
use crate::runner::Runner;
use regex::Regex;

pub const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A sink for one progress line.
pub type ProgressFn = Box<dyn Fn(&str)>;

thread_local! {
    static PROGRESS: std::cell::RefCell<Option<ProgressFn>> = const { std::cell::RefCell::new(None) };
}

/// Where `gather` reports each item as it finishes (the CLI streams it to stderr). Off unless set: a library call stays quiet.
pub fn set_progress(f: Option<ProgressFn>) {
    PROGRESS.with(|p| *p.borrow_mut() = f);
}

fn progress(line: String) {
    PROGRESS.with(|p| {
        if let Some(f) = p.borrow().as_ref() {
            f(&line)
        }
    });
}

/// ` [12s]` when an item took 5 s or more, else nothing (fast items print the same text on every run).
fn slow(t: std::time::Instant) -> String {
    let s = t.elapsed().as_secs_f64();
    if s >= 5.0 {
        format!(" [{}s]", crate::pycompat::fmt_f(s, 0))
    } else {
        String::new()
    }
}

/// The note for an item whose probes ran out of `item_budget_s`.
pub fn over_budget(cfg: &TidyCfg) -> String {
    format!("skipped: its probes ran past item_budget_s ({}s); left alone, size unknown", fmt_g(cfg.item_budget_s))
}

/// What an item adds to its JSON form beyond the common fields.
#[derive(Debug, Clone, PartialEq)]
pub enum Extra {
    None,
    Target { cargo: bool, tag: Option<bool>, branch: Option<String> },
    Worktree { branch: String, links: usize },
    /// a detached-HEAD checkout: the commit it is on and whether that commit is on main
    Detached { sha: String, merged: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub cls: String,
    /// clean | move | remove | rm-container | rm-image | prune-cache | remove-worktree | remove-detached
    pub kind: String,
    /// exact path or name
    pub key: String,
    /// None = unknown
    pub bytes: Option<i64>,
    pub why: String,
    /// the named rule that allows the removal (empty: none, and the item is never removed)
    pub rule: String,
    /// why it is left alone
    pub keep: Option<String>,
    pub attended_only: bool,
    pub extra: Extra,
}

impl Item {
    pub fn new(cls: &str, kind: &str, key: &str, bytes: Option<i64>, why: &str) -> Item {
        Item { cls: cls.into(), kind: kind.into(), key: key.into(), bytes, why: why.into(), rule: String::new(), keep: None, attended_only: false, extra: Extra::None }
    }

    /// Name the rule that allows this item to be removed.
    pub fn ruled(mut self, rule: &str) -> Item {
        self.rule = rule.into();
        self
    }

    /// `asdict(item)`, key order as the dataclass declares it.
    pub fn to_json(&self) -> J {
        J::Obj(vec![
            ("cls".into(), J::s(&self.cls)),
            ("kind".into(), J::s(&self.kind)),
            ("key".into(), J::s(&self.key)),
            ("bytes".into(), J::opt_int(self.bytes)),
            ("why".into(), J::s(&self.why)),
            ("rule".into(), J::s(&self.rule)),
            ("keep".into(), J::opt_str(&self.keep)),
            ("attended_only".into(), J::Bool(self.attended_only)),
            (
                "extra".into(),
                match &self.extra {
                    Extra::None => J::Obj(vec![]),
                    Extra::Target { cargo, tag, branch } => J::Obj(vec![("cargo".into(), J::Bool(*cargo)), ("tag".into(), J::opt_bool(*tag)), ("branch".into(), J::opt_str(branch))]),
                    Extra::Worktree { branch, links } => J::Obj(vec![("branch".into(), J::s(branch)), ("links".into(), J::Int(*links as i64))]),
                    Extra::Detached { sha, merged } => J::Obj(vec![("sha".into(), J::s(sha)), ("merged".into(), J::Bool(*merged))]),
                },
            ),
        ])
    }
}

#[derive(Debug, Clone)]
pub struct WtInfo {
    pub path: String,
    pub branch: Option<String>,
    pub merged: Option<bool>,
    pub dirty: Option<bool>,
    /// None = not listed (the worktree was kept before its links mattered)
    pub links: Option<usize>,
    pub bytes: Option<i64>,
    pub keep: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TargetInfo {
    pub path: String,
    pub lane: Option<String>,
    pub bytes: Option<i64>,
    pub idle_min: Option<f64>,
    pub cargo: bool,
    pub tag: Option<bool>,
    pub merged: Option<bool>,
    pub branch: Option<String>,
    pub budget_gb: Option<f64>,
    pub over: bool,
    pub keep: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub name: String,
    pub bytes: i64,
    pub in_use: bool,
}

#[derive(Debug, Clone)]
pub struct TrashInfo {
    pub path: String,
    pub bytes: Option<i64>,
    pub age_days: Option<f64>,
    pub keep: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub host: String,
    /// (path, free_gb, min_free_gb)
    pub disks: Vec<(String, Option<f64>, f64)>,
    pub targets: Vec<TargetInfo>,
    pub worktrees: Vec<WtInfo>,
    pub containers: Vec<String>,
    pub images: Vec<ImageInfo>,
    /// (size, reclaimable)
    pub cache: Option<(i64, i64)>,
    pub trash: Vec<TrashInfo>,
    pub profiles: Vec<String>,
    pub items: Vec<Item>,
    pub breaches: Vec<String>,
}

pub fn human(n: Option<i64>) -> String {
    match n {
        None => "?".into(),
        Some(n) => human_f(n as f64),
    }
}

pub fn human_f(n: f64) -> String {
    let mut n = n;
    for u in ["B", "KB", "MB", "GB", "TB"] {
        if n.abs() < 1024.0 || u == "TB" {
            return if u == "B" { format!("{} {u}", crate::pycompat::fmt_f(n, 0)) } else { format!("{} {u}", crate::pycompat::fmt_f(n, 1)) };
        }
        n /= 1024.0;
    }
    unreachable!()
}

/// A full commit id as git prints it (SHA-1 or SHA-256): hex only, never a name or a flag.
fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn parse_size(s: &str) -> i64 {
    let re = Regex::new(r"(?i)^\s*([0-9.]+)\s*([KMGT]?)i?B?\s*$").unwrap();
    let Some(m) = re.captures(strip(s)) else { return 0 };
    let Ok(v) = m[1].parse::<f64>() else { return 0 };
    let p = ["", "K", "M", "G", "T"].iter().position(|x| *x == m[2].to_uppercase()).unwrap();
    (v * 1024f64.powi(p as i32)) as i64
}

pub fn is_cargo(names: &[String]) -> bool {
    let has = |x: &str| names.iter().any(|n| n == x);
    (has("debug") || has("release")) && (has("tmp") || has(".rustc_info.json"))
}

/// `find_branch`: the branch of a lane, by worktree folder name, then by branch name.
pub fn find_branch(lane: &str, wts: &[Wt], branches: &[String], cfg: &TidyCfg) -> Option<String> {
    for w in wts {
        if let Some(b) = &w.branch {
            if lane_of(&w.path, cfg).to_lowercase() == lane.to_lowercase() {
                return Some(b.clone());
            }
        }
    }
    let all: Vec<String> = branches.iter().cloned().chain(wts.iter().filter_map(|w| w.branch.clone())).collect();
    all.into_iter().find(|b| b == lane || b.rsplit('/').next().unwrap() == lane)
}

fn truthy_s(x: &Option<String>) -> Option<&str> {
    x.as_deref().filter(|s| !s.is_empty())
}

pub fn gather(host: &Host, runner: &dyn Runner, cfg: &TidyCfg, now: Option<f64>) -> R<Inventory> {
    let now = now.unwrap_or_else(now_secs);
    let ops = Ops::new(host, runner, cfg);
    let mut inv = Inventory { host: host.name.clone(), ..Default::default() };
    // disks
    for d in &host.disks {
        let f = ops.free_gb(&d.path)?;
        inv.disks.push((d.path.clone(), f, d.min_free_gb.f()));
        if let Some(f) = f {
            if f < d.min_free_gb.f() {
                inv.breaches.push(format!("disk {}: {} GB free < {}", d.path, crate::pycompat::fmt_f(f, 0), fmt_g(d.min_free_gb.f())));
            }
        }
    }
    let (mut wts, mut branches) = (vec![], vec![]);
    if cfg.main_checkout.is_some() {
        wts = ops.worktrees()?;
        branches = ops.branches()?;
    }
    let kept: Vec<String> = [cfg.main_checkout.clone(), cfg.root_checkout.clone()].into_iter().chain(cfg.keep_paths.iter().cloned().map(Some)).flatten().filter(|k| !k.is_empty()).collect();
    let kept_n: HashSet<String> = kept.iter().map(|k| norm(k)).collect();
    let keep_lanes_l: HashSet<String> = cfg.keep_lanes.iter().map(|x| x.to_lowercase()).collect();
    if let Some(td) = truthy_s(&cfg.trash_dir) {
        let nt = norm(td);
        for w in &wts {
            let nw = norm(&w.path);
            if nw == nt || nw.starts_with(&format!("{nt}/")) {
                return guard(format!("the recycle bin {td} holds the registered worktree {}; fix trash_dir", w.path));
            }
        }
    }
    let mut kept_r: HashSet<String> = HashSet::new();
    if cfg.main_checkout.is_some() {
        for k in &kept {
            if let Some(r) = ops.rp(k)? {
                kept_r.insert(r);
            }
        }
    }
    let first_n = wts.first().map(|w| norm(&w.path));
    let first_r = match wts.first() {
        Some(w) => ops.rp(&w.path)?,
        None => None,
    };
    // worktrees: the cheap checks (keep lists, merge proof, git status) decide first; only a worktree that would be removed has its links
    // listed, its size measured and its idle time walked - those walk the whole tree (node_modules, build targets) and are slow
    for (idx, w) in wts.iter().enumerate() {
        let (wpath, wbranch) = (&w.path, &w.branch);
        let t0 = std::time::Instant::now();
        ops.start_item();
        let n = norm(wpath);
        let wr = ops.rp(wpath)?;
        let mut detached_item: Option<Item> = None;
        let mut info = WtInfo { path: wpath.clone(), branch: wbranch.clone(), merged: None, dirty: None, links: None, bytes: None, keep: None };
        if kept_n.contains(&n) || wr.as_ref().map(|r| kept_r.contains(r)).unwrap_or(false) {
            info.keep = Some("main/root/keep list".into());
        } else if idx == 0 || Some(&n) == first_n.as_ref() || (wr.is_some() && wr == first_r) {
            info.keep = Some("main worktree".into()); // the first entry of `git worktree list` is the main checkout, whatever its spelling
        } else if w.locked {
            info.keep = Some("locked".into());
        } else if wbranch.is_none() && !(w.detached && w.head.as_deref().is_some_and(is_sha)) {
            info.keep = Some("detached head".into()); // no branch and no commit id git printed: nothing to prove
        } else if wbranch.is_none() {
            // a chain checkout: detached, so there is no branch to keep or delete; its commit is what is judged
            let sha = w.head.clone().unwrap();
            if ops.win && ops.win_unsafe(wpath) {
                info.keep = Some("unsafe name".into());
            } else if keep_lanes_l.contains(&lane_of(wpath, cfg).to_lowercase()) {
                info.keep = Some("keep list".into());
            } else if ops.link_state(wpath)? != super::ops::LinkState::Real {
                info.keep = Some("is a link, missing or unreadable".into());
            } else if !ops.toplevel_ok(wpath)? {
                info.keep = Some("unverifiable (its own git root cannot be confirmed)".into());
            } else {
                info.dirty = ops.dirty(wpath)?;
                let merged = ops.merged_sha(&sha)?;
                info.merged = merged;
                let idle_days = if merged == Some(true) { cfg.worktree_min_idle_days } else { cfg.chain_checkout_max_age_days };
                if info.dirty != Some(false) {
                    info.keep = Some(if info.dirty == Some(true) { "uncommitted work" } else { "status unreadable" }.into());
                } else if merged != Some(true) && cfg.chain_checkout_max_age_days <= 0.0 {
                    info.keep = Some(if merged == Some(false) { "detached head, not merged" } else { "detached head, merge state unknown" }.into());
                } else if idle_days > 0.0 && ops.recent(wpath, idle_days * 1440.0)? != Some(false) {
                    info.keep = Some(format!("used in the last {} days", fmt_g(idle_days)));
                } else if ops.held_by_process(wpath)? != Some(false) {
                    info.keep = Some("held by a running process (or that cannot be told)".into());
                } else {
                    match ops.list_links(wpath) {
                        Ok(l) => info.links = Some(l.len()),
                        Err(Fatal::Item(e)) => info.keep = Some(format!("cannot list its links ({e})")),
                        Err(e) => return Err(e),
                    }
                    info.bytes = ops.size(wpath)?;
                }
                if info.keep.is_none() {
                    let (rule, why) = if merged == Some(true) { ("merged-detached-checkout", format!("detached at {}, which is on {}", &sha[..sha.len().min(12)], cfg.main_branch)) } else { ("aged-detached-checkout", format!("detached at {}, not on {}, untouched for {} days", &sha[..sha.len().min(12)], cfg.main_branch, fmt_g(cfg.chain_checkout_max_age_days))) };
                    let mut it = Item::new("worktrees", "remove-detached", wpath, info.bytes, &why).ruled(rule);
                    it.extra = Extra::Detached { sha, merged: merged == Some(true) };
                    detached_item = Some(it);
                }
            }
        } else if !safe_branch(wbranch.as_deref().unwrap()) || (ops.win && ops.win_unsafe(wpath)) {
            info.keep = Some("unsafe name".into());
        } else if ops.link_state(wpath)? != super::ops::LinkState::Real {
            info.keep = Some(
                match ops.link_state(wpath)? {
                    super::ops::LinkState::Link => "is a link",
                    super::ops::LinkState::Missing => "folder is missing",
                    _ => "link state unknown",
                }
                .into(),
            );
        } else if !ops.toplevel_ok(wpath)? {
            info.keep = Some("unverifiable (its own git root cannot be confirmed)".into());
        } else {
            let br = wbranch.clone().unwrap();
            info.merged = if br != cfg.main_branch { ops.merged(&br)? } else { None };
            info.dirty = ops.dirty(wpath)?;
            if br == cfg.main_branch {
                info.keep = Some("main branch".into());
            } else if cfg.keep_branches.contains(&br) || keep_lanes_l.contains(&lane_of(wpath, cfg).to_lowercase()) {
                info.keep = Some("keep list".into());
            } else if info.dirty != Some(false) {
                info.keep = Some(if info.dirty == Some(true) { "uncommitted work" } else { "status unreadable" }.into());
            } else if info.merged != Some(true) {
                info.keep = Some(if info.merged == Some(false) { "not merged" } else { "merge state unknown" }.into());
            } else {
                match ops.list_links(wpath) {
                    Ok(l) => info.links = Some(l.len()),
                    // a listing that timed out, or a name it cannot be trusted with: this one item is kept
                    Err(Fatal::Item(e)) => info.keep = Some(format!("cannot list its links ({e})")),
                    Err(e) => return Err(e),
                }
                info.bytes = ops.size(wpath)?;
                if info.keep.is_none() && cfg.worktree_min_idle_days > 0.0 && ops.recent(wpath, cfg.worktree_min_idle_days * 1440.0)? != Some(false) {
                    info.keep = Some(format!("used in the last {} days", fmt_g(cfg.worktree_min_idle_days)));
                }
            }
        }
        if ops.end_item() {
            info.keep = Some(over_budget(cfg));
            info.bytes = None;
        }
        progress(format!(
            "tidy: worktree {}/{} {}: {}{}{}",
            idx + 1,
            wts.len(),
            wpath,
            info.keep.clone().unwrap_or_else(|| "tidy".into()),
            info.bytes.map(|b| format!(" ({})", human(Some(b)))).unwrap_or_default(),
            slow(t0)
        ));
        if let Some(it) = detached_item.filter(|_| info.keep.is_none()) {
            inv.items.push(it);
        } else if info.keep.is_none() {
            let br = info.branch.clone().unwrap();
            let mut it = Item::new("worktrees", "remove-worktree", wpath, info.bytes, &format!("branch {} is on {}", br, cfg.main_branch)).ruled("merged-clean-worktree");
            it.extra = Extra::Worktree { branch: br, links: info.links.unwrap_or(0) };
            inv.items.push(it);
        }
        inv.worktrees.push(info);
    }
    // targets
    let held = ops.hold_named()?;
    let held_by = |path: &str, lane: Option<&str>| -> Option<String> {
        let np = norm(path);
        for (h, run) in &held {
            let base = h.rsplit('/').next().unwrap_or("").to_lowercase();
            if np == *h || np.starts_with(&format!("{h}/")) || h.starts_with(&format!("{np}/")) || lane.map(|l| l.to_lowercase() == base).unwrap_or(false) {
                return Some(run.clone());
            }
        }
        None
    };
    let mut entries: Vec<(String, Option<String>, Option<f64>)> = cfg.targets.iter().map(|x| (x.path.clone(), x.lane.clone(), x.budget_gb)).collect();
    for root in &cfg.target_roots {
        for nm in ops.listdir(root)? {
            if ops.win && ops.win_unsafe(&nm) {
                inv.targets.push(TargetInfo {
                    path: ops.join(root, &nm),
                    lane: Some(nm.clone()),
                    bytes: None,
                    idle_min: None,
                    cargo: false,
                    tag: None,
                    merged: None,
                    branch: None,
                    budget_gb: None,
                    over: false,
                    keep: Some("unsafe name".into()),
                });
                continue;
            }
            entries.push((ops.join(root, &nm), Some(nm), None));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let n_entries = entries.len();
    for (ti, (path, lane, budget)) in entries.into_iter().enumerate() {
        if seen.contains(&norm(&path)) {
            continue;
        }
        let t0 = std::time::Instant::now();
        ops.start_item();
        match ops.exists(&path)? {
            None => {
                inv.breaches.push(format!("cannot tell whether {path} exists; left alone"));
                continue;
            }
            Some(false) => continue,
            Some(true) => {}
        }
        seen.insert(norm(&path));
        let lk = ops.is_link(&path)?;
        if lk != Some(false) {
            inv.targets.push(TargetInfo {
                path: path.clone(),
                lane: lane.clone(),
                bytes: None,
                idle_min: None,
                cargo: false,
                tag: None,
                merged: None,
                branch: None,
                budget_gb: budget,
                over: false,
                keep: Some(if lk == Some(true) { "is a link" } else { "link state unknown" }.into()),
            });
            continue;
        }
        let names = ops.listdir(&path)?;
        let size = ops.size(&path)?;
        let mt = ops.mtime(&path)?;
        let cargo = is_cargo(&names);
        let budget_truthy = budget.filter(|b| *b != 0.0);
        let mut t = TargetInfo {
            path: path.clone(),
            lane: lane.clone(),
            bytes: size,
            idle_min: mt.map(|m| (now - m as f64) / 60.0),
            cargo,
            tag: if cargo { Some(names.iter().any(|n| n == "CACHEDIR.TAG")) } else { None },
            merged: None,
            branch: None,
            budget_gb: budget,
            over: matches!((budget_truthy, size), (Some(b), Some(s)) if s as f64 > b * GB),
            keep: None,
        };
        if t.over {
            inv.breaches.push(format!("target {}: {} over budget {} GB", path, human(size), fmt_g(budget.unwrap())));
        }
        if let Some(lane) = truthy_s(&lane).map(String::from) {
            let br = find_branch(&lane, &wts, &branches, cfg);
            t.branch = br.clone();
            let wt = br.as_ref().and_then(|b| inv.worktrees.iter().find(|w| w.branch.as_ref() == Some(b)));
            let wt_dirty = wt.map(|w| w.dirty);
            t.merged = match &br {
                Some(b) => ops.merged(b)?,
                None => None,
            };
            let reason: Option<String> = if kept_n.contains(&norm(&path)) || keep_lanes_l.contains(&lane.to_lowercase()) || br.as_ref().map(|b| cfg.keep_branches.contains(b)).unwrap_or(false) {
                Some("keep list".into())
            } else if let Some(run) = held_by(&path, Some(lane.as_str())) {
                Some(format!("named by the running hold {run}"))
            } else if wt_dirty.map(|d| d != Some(false)).unwrap_or(false) {
                Some("its worktree has uncommitted work".into())
            } else if t.idle_min.map(|i| i < cfg.min_idle_min).unwrap_or(true) {
                Some(format!("used in the last {} min", fmt_g(cfg.min_idle_min)))
            } else if br.is_none() && cfg.orphan_targets != "clean" {
                Some("no lane branch found (orphan_targets = keep)".into())
            } else if br.is_some() && t.merged != Some(true) {
                Some(if t.merged == Some(false) { "lane not merged" } else { "merge state unknown" }.into())
            } else if ops.recent(&path, cfg.min_idle_min)? != Some(false) {
                Some(format!("a file inside changed in the last {} min (or its activity cannot be read)", fmt_g(cfg.min_idle_min)))
            } else {
                None
            };
            t.keep = reason.clone();
            if ops.end_item() {
                t.keep = Some(over_budget(cfg));
                t.bytes = None;
            } else if reason.is_none() && size.map(|s| s != 0).unwrap_or(true) {
                let mut it = Item::new("targets", if cargo { "clean" } else { "move" }, &path, size, &format!("lane {}: {}", lane, if br.is_some() { "branch content is on main" } else { "orphan" })).ruled(if br.is_some() { "lane-merged-target" } else { "orphan-target" });
                it.extra = Extra::Target { cargo, tag: t.tag, branch: br.clone() };
                inv.items.push(it);
            }
        } else if ops.end_item() {
            t.keep = Some(over_budget(cfg));
            t.bytes = None;
        } else if t.over && !kept_n.contains(&norm(&path)) && held_by(&path, None).is_none() && (cargo || !names.is_empty()) {
            let mut it = Item::new("targets", if cargo { "clean" } else { "move" }, &path, size, &format!("over its {} GB budget", fmt_g(budget.unwrap()))).ruled("over-budget-target");
            it.attended_only = true;
            it.extra = Extra::Target { cargo, tag: t.tag, branch: None };
            inv.items.push(it);
        }
        progress(format!("tidy: target {}/{} {}: {}{}{}", ti + 1, n_entries, t.path, t.keep.clone().unwrap_or_else(|| "tidy".into()), t.bytes.map(|b| format!(" ({})", human(Some(b)))).unwrap_or_default(), slow(t0)));
        inv.targets.push(t);
        // Python appends the target before judging it; the order inside `inv.targets` is the order of `entries`.
    }
    ops.end_item(); // a target that was skipped early leaves no budget running into the host-wide probes below
    // containers
    if let Some(prefix) = truthy_s(&cfg.container_prefix) {
        let ex = ops.run0(r#"docker ps -a --filter status=exited --format "{{.Names}}""#)?;
        let rn = ops.run0(r#"docker ps --format "{{.Names}}""#)?;
        let running: std::collections::HashSet<String> = splitlines(&rn.out).iter().map(|l| strip(l).to_string()).collect();
        if ex.ok() {
            for n in splitlines(&ex.out).iter().map(|l| strip(l).to_string()).filter(|l| l.starts_with(prefix)) {
                if cfg.protect_containers.iter().any(|p| n.starts_with(p.as_str())) {
                    continue; // a protected prefix (a reference stack): never inventoried, never planned
                }
                inv.containers.push(n.clone());
                if !running.contains(&n) {
                    inv.items.push(Item::new("containers", "rm-container", &n, Some(0), "exited throwaway container").ruled("exited-container"));
                }
            }
        }
    }
    // images
    if !cfg.image_patterns.is_empty() {
        let im = ops.run0(r#"docker images --format "{{.Repository}}:{{.Tag}}|{{.ID}}|{{.Size}}""#)?;
        let us = ops.run0(r#"docker ps -a --format "{{.Image}}""#)?;
        let used: std::collections::HashSet<String> = splitlines(&us.out).iter().map(|l| strip(l).to_string()).filter(|l| !l.is_empty()).collect();
        if im.ok() {
            for line in splitlines(&im.out) {
                let parts: Vec<&str> = strip(&line).split('|').collect();
                if parts.len() != 3 {
                    continue;
                }
                let (name, iid, sz) = (parts[0], parts[1], parts[2]);
                if name.ends_with(":<none>") || !cfg.image_patterns.iter().any(|p| fnmatchcase(name, p)) {
                    continue;
                }
                // image_keep_patterns win over image_patterns: such an image is listed but never planned
                let kept = cfg.image_keep_patterns.iter().any(|p| fnmatchcase(name, p));
                let in_use = used.contains(name) || used.iter().any(|u| iid.starts_with(u.as_str()) || u.starts_with(iid));
                inv.images.push(ImageInfo { name: name.into(), bytes: parse_size(sz), in_use });
                if !in_use && !kept {
                    inv.items.push(Item::new("images", "rm-image", name, Some(parse_size(sz)), "no container uses it; matches a configured pattern").ruled("unused-image"));
                }
            }
        }
    }
    // build cache
    if cfg.cache_days > 0.0 {
        let o = ops.run0(r#"docker system df --format "{{.Type}}|{{.Size}}|{{.Reclaimable}}""#)?;
        for line in splitlines(&o.out) {
            let p: Vec<&str> = strip(&line).split('|').collect();
            if p.len() == 3 && p[0].to_lowercase().starts_with("build cache") {
                let w = split_ws(p[2]);
                let rec = if w.is_empty() { 0 } else { parse_size(w[0]) };
                inv.cache = Some((parse_size(p[1]), rec));
                inv.items.push(Item::new("cache", "prune-cache", "build cache", Some(rec), &format!("older than {} days (upper bound: all reclaimable)", fmt_g(cfg.cache_days))).ruled("old-build-cache"));
            }
        }
    }
    // recycle bin
    if let Some(td) = truthy_s(&cfg.trash_dir) {
        let te = ops.exists(td)?;
        if te.is_none() {
            inv.breaches.push(format!("cannot tell whether the recycle bin {td} exists; left alone"));
        }
        if te == Some(true) {
            let seen = ops.read_seen(td)?;
            let mut new_seen: std::collections::BTreeMap<String, super::ops::SeenRec> = std::collections::BTreeMap::new();
            // the image archive (`<trash_dir>/docker-images/<name>@<id>.tar`): each tar ages and is purged on its own, so the folder is
            // never an entry itself
            let mut names: Vec<String> = Vec::new();
            for n in ops.listdir(td)?.into_iter().filter(|n| n != super::ops::SEEN) {
                let sub = ops.join(td, &n);
                if n == super::ops::IMAGE_ARCHIVE && ops.is_link(&sub)? == Some(false) && ops.exists(&sub)? == Some(true) {
                    for c in ops.listdir(&sub)? {
                        names.push(format!("{n}/{c}"));
                    }
                } else {
                    names.push(n);
                }
            }
            for (bi, nm) in names.iter().enumerate() {
                let nm = nm.clone();
                let p = ops.join(td, &nm);
                if ops.win && ops.win_unsafe(&nm) {
                    inv.trash.push(TrashInfo { path: p, bytes: None, age_days: None, keep: Some("unsafe name".into()) });
                    continue;
                }
                let t0 = std::time::Instant::now();
                ops.start_item();
                let mt = ops.mtime(&p)?;
                let rec = match seen.get(&nm) {
                    Some(r) if !(r.m.is_some() && mt.is_some() && r.m != mt) => r.clone(),
                    _ => super::ops::SeenRec { t: now, m: mt }, // new, or a NEW entry under a reused name (its own folder time differs): first seen now
                };
                new_seen.insert(nm.clone(), rec.clone());
                // it counts from when tidy FIRST SAW it, never from a folder time that may be old or reset; a name stamp counts only when
                // it is the later of the two
                let mut a = ((now - rec.t) / 86400.0).max(0.0);
                if let Some(secs) = stamp_secs(&nm) {
                    a = a.min(((now - secs) / 86400.0).max(0.0));
                }
                let age = Some(a);
                // only an entry old enough to go is sized: a young one is listed with its age, its size unknown ("?")
                let sz = if a > cfg.trash_days { ops.size(&p)? } else { None };
                let cut = ops.end_item();
                let keep = cut.then(|| over_budget(cfg));
                progress(format!("tidy: recycle bin {}/{} {}: {}d{}{}{}", bi + 1, names.len(), p, crate::pycompat::fmt_f(a, 0), if cut { String::new() } else { sz.map(|b| format!(" ({})", human(Some(b)))).unwrap_or_default() }, keep.as_ref().map(|k| format!(", {k}")).unwrap_or_default(), slow(t0)));
                inv.trash.push(TrashInfo { path: p.clone(), bytes: if cut { None } else { sz }, age_days: age, keep: keep.clone() });
                if let (Some(a), None) = (age, keep) {
                    if a > cfg.trash_days {
                        inv.items.push(Item::new("trash", "remove", &p, sz, &format!("in the recycle bin {} days (> {})", crate::pycompat::fmt_f(a, 0), fmt_g(cfg.trash_days))).ruled("old-trash-entry"));
                    }
                }
            }
            if new_seen != seen {
                // also drops the records of entries that are gone
                let all: std::collections::BTreeMap<String, serde_json::Value> = new_seen.iter().map(|(k, v)| (k.clone(), serde_json::json!({"t": v.t, "m": v.m}))).collect();
                let body = serde_json::to_string(&all).unwrap_or_default();
                if let Err(e) = runner.put_file(&ops.join(td, super::ops::SEEN), &body) {
                    inv.breaches.push(format!("cannot record when the recycle bin entries were first seen ({e}); their age stays 0"));
                }
            }
        }
    }
    // profiles in the wrong place (report only)
    let prof = Regex::new(r"(^|[\\/])(SingletonLock|Cookies|Local State|lockfile)$").unwrap();
    for root in &cfg.evidence_dirs {
        let cmd = if ops.win {
            format!("dir /S /B {}", ops.q(root)?)
        } else {
            format!("find {} -maxdepth 5 \\( -name SingletonLock -o -name Cookies -o -name 'Local State' -o -name lockfile \\)", ops.q(root)?)
        };
        let o = ops.run0(&cmd)?;
        for l in splitlines(&o.out) {
            if prof.is_match(strip(&l)) {
                inv.profiles.push(strip(&l).to_string());
            }
        }
        if !inv.profiles.is_empty() {
            inv.breaches.push(format!("{} browser-profile file(s) inside {}", inv.profiles.len(), root));
        }
    }
    let mut names: HashSet<String> = inv.worktrees.iter().map(|w| lane_of(&w.path, cfg).to_lowercase()).collect();
    names.extend(inv.targets.iter().filter_map(|t| t.lane.as_ref()).map(|l| l.to_lowercase()));
    for kl in &cfg.keep_lanes {
        if !names.contains(&kl.to_lowercase()) {
            inv.breaches.push(format!("keep_lanes entry {} matches no worktree or target (worktree_prefix {}); it protects nothing", crate::pycompat::repr_str(kl), crate::pycompat::repr_str(&cfg.worktree_prefix)));
        }
    }
    refuse_unruled_and_protected(&ops, &mut inv)?;
    Ok(inv)
}

/// The last word of the inventory: an item with no named rule, or on a protected path, is dropped from the plan and its row says why.
fn refuse_unruled_and_protected(ops: &Ops, inv: &mut Inventory) -> R<()> {
    let items = std::mem::take(&mut inv.items);
    for it in items {
        let pathy = matches!(it.cls.as_str(), "worktrees" | "targets" | "trash");
        let why_not = if it.rule.is_empty() {
            Some("kept: no rule")
        } else if pathy && ops.protect_hit(&it.key)?.is_some() {
            Some("kept: protected")
        } else {
            None
        };
        let Some(why_not) = why_not else {
            inv.items.push(it);
            continue;
        };
        progress(format!("tidy: {} {}: {why_not}", it.cls, it.key));
        let same = |p: &str| norm(p) == norm(&it.key);
        for w in inv.worktrees.iter_mut().filter(|w| same(&w.path)) {
            w.keep = Some(why_not.into());
        }
        for t in inv.targets.iter_mut().filter(|t| same(&t.path)) {
            t.keep = Some(why_not.into());
        }
        for t in inv.trash.iter_mut().filter(|t| same(&t.path)) {
            t.keep = Some(why_not.into());
        }
        if !pathy {
            inv.breaches.push(format!("{} {}: {why_not}", it.cls, it.key));
        }
    }
    Ok(())
}

/// The local time in a recycle-bin name `<name>-YYYYMMDDHHMMSS` (what `move` puts there), as epoch seconds.
pub fn stamp_secs(name: &str) -> Option<f64> {
    use chrono::TimeZone;
    let m = Regex::new(r"-(\d{14})$").unwrap().captures(name)?;
    let d = chrono::NaiveDateTime::parse_from_str(&m[1], "%Y%m%d%H%M%S").ok()?;
    chrono::Local.from_local_datetime(&d).earliest().map(|t| t.timestamp() as f64)
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
