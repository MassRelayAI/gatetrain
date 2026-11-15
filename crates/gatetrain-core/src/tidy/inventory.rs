//! `gather`: the read-only inventory of leftovers (`tidy.gather`), with its plan items. Nothing here writes.
use super::config::TidyCfg;
use crate::config::Host;
use crate::pycompat::J;
use super::ops::{guard, lane_of, norm, safe_branch, Ops, Wt, R};
use crate::pycompat::{fnmatchcase, fmt_g, split_ws, splitlines, strip};
use std::collections::HashSet;
use crate::runner::Runner;
use regex::Regex;

pub const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// What an item adds to its JSON form beyond the common fields.
#[derive(Debug, Clone, PartialEq)]
pub enum Extra {
    None,
    Target { cargo: bool, tag: Option<bool>, branch: Option<String> },
    Worktree { branch: String, links: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub cls: String,
    /// clean | move | remove | rm-container | rm-image | prune-cache | remove-worktree
    pub kind: String,
    /// exact path or name
    pub key: String,
    /// None = unknown
    pub bytes: Option<i64>,
    pub why: String,
    /// why it is left alone
    pub keep: Option<String>,
    pub attended_only: bool,
    pub extra: Extra,
}

impl Item {
    pub fn new(cls: &str, kind: &str, key: &str, bytes: Option<i64>, why: &str) -> Item {
        Item { cls: cls.into(), kind: kind.into(), key: key.into(), bytes, why: why.into(), keep: None, attended_only: false, extra: Extra::None }
    }

    /// `asdict(item)`, key order as the dataclass declares it.
    pub fn to_json(&self) -> J {
        J::Obj(vec![
            ("cls".into(), J::s(&self.cls)),
            ("kind".into(), J::s(&self.kind)),
            ("key".into(), J::s(&self.key)),
            ("bytes".into(), J::opt_int(self.bytes)),
            ("why".into(), J::s(&self.why)),
            ("keep".into(), J::opt_str(&self.keep)),
            ("attended_only".into(), J::Bool(self.attended_only)),
            (
                "extra".into(),
                match &self.extra {
                    Extra::None => J::Obj(vec![]),
                    Extra::Target { cargo, tag, branch } => J::Obj(vec![("cargo".into(), J::Bool(*cargo)), ("tag".into(), J::opt_bool(*tag)), ("branch".into(), J::opt_str(branch))]),
                    Extra::Worktree { branch, links } => J::Obj(vec![("branch".into(), J::s(branch)), ("links".into(), J::Int(*links as i64))]),
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
    pub links: usize,
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
    // worktrees
    for (idx, w) in wts.iter().enumerate() {
        let (wpath, wbranch) = (&w.path, &w.branch);
        let n = norm(wpath);
        let wr = ops.rp(wpath)?;
        let mut info = WtInfo { path: wpath.clone(), branch: wbranch.clone(), merged: None, dirty: None, links: 0, bytes: None, keep: None };
        if kept_n.contains(&n) || wr.as_ref().map(|r| kept_r.contains(r)).unwrap_or(false) {
            info.keep = Some("main/root/keep list".into());
        } else if idx == 0 || Some(&n) == first_n.as_ref() || (wr.is_some() && wr == first_r) {
            info.keep = Some("main worktree".into()); // the first entry of `git worktree list` is the main checkout, whatever its spelling
        } else if w.locked {
            info.keep = Some("locked".into());
        } else if wbranch.is_none() {
            info.keep = Some("detached head".into());
        } else if !safe_branch(wbranch.as_deref().unwrap()) || (ops.win && ops.win_unsafe(wpath)) {
            info.keep = Some("unsafe name".into());
        } else if ops.is_link(wpath)? != Some(false) {
            info.keep = Some(if ops.is_link(wpath)? == Some(true) { "is a link" } else { "link state unknown" }.into());
        } else {
            let br = wbranch.clone().unwrap();
            info.merged = if br != cfg.main_branch { ops.merged(&br)? } else { None };
            info.dirty = ops.dirty(wpath)?;
            info.links = ops.list_links(wpath)?.len();
            info.bytes = ops.size(wpath)?;
            if br == cfg.main_branch {
                info.keep = Some("main branch".into());
            } else if cfg.keep_branches.contains(&br) || keep_lanes_l.contains(&lane_of(wpath, cfg).to_lowercase()) {
                info.keep = Some("keep list".into());
            } else if info.dirty != Some(false) {
                info.keep = Some(if info.dirty == Some(true) { "uncommitted work" } else { "status unreadable" }.into());
            } else if info.merged != Some(true) {
                info.keep = Some(if info.merged == Some(false) { "not merged" } else { "merge state unknown" }.into());
            } else if cfg.worktree_min_idle_days > 0.0 && ops.recent(wpath, cfg.worktree_min_idle_days * 1440.0)? != Some(false) {
                info.keep = Some(format!("used in the last {} days", fmt_g(cfg.worktree_min_idle_days)));
            }
        }
        if info.keep.is_none() {
            let br = info.branch.clone().unwrap();
            let mut it = Item::new("worktrees", "remove-worktree", wpath, info.bytes, &format!("branch {} is on {}", br, cfg.main_branch));
            it.extra = Extra::Worktree { branch: br, links: info.links };
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
    for (path, lane, budget) in entries {
        if seen.contains(&norm(&path)) {
            continue;
        }
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
            if reason.is_none() && size.map(|s| s != 0).unwrap_or(true) {
                let mut it = Item::new("targets", if cargo { "clean" } else { "move" }, &path, size, &format!("lane {}: {}", lane, if br.is_some() { "branch content is on main" } else { "orphan" }));
                it.extra = Extra::Target { cargo, tag: t.tag, branch: br.clone() };
                inv.items.push(it);
            }
        } else if t.over && !kept_n.contains(&norm(&path)) && held_by(&path, None).is_none() && (cargo || !names.is_empty()) {
            let mut it = Item::new("targets", if cargo { "clean" } else { "move" }, &path, size, &format!("over its {} GB budget", fmt_g(budget.unwrap())));
            it.attended_only = true;
            it.extra = Extra::Target { cargo, tag: t.tag, branch: None };
            inv.items.push(it);
        }
        inv.targets.push(t);
        // Python appends the target before judging it; the order inside `inv.targets` is the order of `entries`.
    }
    // containers
    if let Some(prefix) = truthy_s(&cfg.container_prefix) {
        let ex = ops.run0(r#"docker ps -a --filter status=exited --format "{{.Names}}""#)?;
        let rn = ops.run0(r#"docker ps --format "{{.Names}}""#)?;
        let running: std::collections::HashSet<String> = splitlines(&rn.out).iter().map(|l| strip(l).to_string()).collect();
        if ex.ok() {
            for n in splitlines(&ex.out).iter().map(|l| strip(l).to_string()).filter(|l| l.starts_with(prefix)) {
                inv.containers.push(n.clone());
                if !running.contains(&n) {
                    inv.items.push(Item::new("containers", "rm-container", &n, Some(0), "exited throwaway container"));
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
                let in_use = used.contains(name) || used.iter().any(|u| iid.starts_with(u.as_str()) || u.starts_with(iid));
                inv.images.push(ImageInfo { name: name.into(), bytes: parse_size(sz), in_use });
                if !in_use {
                    inv.items.push(Item::new("images", "rm-image", name, Some(parse_size(sz)), "no container uses it; matches a configured pattern"));
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
                inv.items.push(Item::new("cache", "prune-cache", "build cache", Some(rec), &format!("older than {} days (upper bound: all reclaimable)", fmt_g(cfg.cache_days))));
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
            let mut fresh: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
            for nm in ops.listdir(td)? {
                if nm == super::ops::SEEN {
                    continue;
                }
                let p = ops.join(td, &nm);
                if ops.win && ops.win_unsafe(&nm) {
                    inv.trash.push(TrashInfo { path: p, bytes: None, age_days: None, keep: Some("unsafe name".into()) });
                    continue;
                }
                let age: Option<f64>;
                if let Some(secs) = stamp_secs(&nm) {
                    // the name says when it was moved here; the folder's own mtime is when it was last used
                    age = Some((now - secs).max(0.0) / 86400.0);
                } else {
                    // no stamp: it counts from when tidy FIRST SAW it, never from a folder time that may be old or reset
                    let first = match seen.get(&nm) {
                        Some(f) => *f,
                        None => {
                            fresh.insert(nm.clone(), now);
                            now
                        }
                    };
                    age = Some(((now - first) / 86400.0).max(0.0));
                }
                let sz = ops.size(&p)?;
                inv.trash.push(TrashInfo { path: p.clone(), bytes: sz, age_days: age, keep: None });
                if let Some(a) = age {
                    if a > cfg.trash_days {
                        inv.items.push(Item::new("trash", "remove", &p, sz, &format!("in the recycle bin {} days (> {})", crate::pycompat::fmt_f(a, 0), fmt_g(cfg.trash_days))));
                    }
                }
            }
            if !fresh.is_empty() {
                let present: HashSet<String> = ops.listdir(td)?.into_iter().collect();
                let mut all: std::collections::BTreeMap<String, f64> = seen.iter().filter(|(k, _)| present.contains(*k)).map(|(k, v)| (k.clone(), *v)).collect();
                all.extend(fresh);
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
    Ok(inv)
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
