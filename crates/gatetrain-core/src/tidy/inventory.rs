//! `gather`: the read-only inventory of leftovers (`tidy.gather`), with its plan items. Nothing here writes.
use super::config::TidyCfg;
use crate::config::Host;
use crate::pycompat::J;
use super::ops::{lane_of, norm, Ops, R};
use crate::pycompat::{fnmatchcase, fmt_g, split_ws, splitlines, strip};
use crate::runner::Runner;
use regex::Regex;

pub const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// What an item adds to its JSON form beyond the common fields.
#[derive(Debug, Clone, PartialEq)]
pub enum Extra {
    None,
    Target { cargo: bool, tag: Option<bool> },
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
                    Extra::Target { cargo, tag } => J::Obj(vec![("cargo".into(), J::Bool(*cargo)), ("tag".into(), J::opt_bool(*tag))]),
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
pub fn find_branch(lane: &str, wts: &[(String, Option<String>)], branches: &[String], cfg: &TidyCfg) -> Option<String> {
    for w in wts {
        if let Some(b) = &w.1 {
            if lane_of(&w.0, cfg) == lane.to_lowercase() {
                return Some(b.clone());
            }
        }
    }
    let all: Vec<String> = branches.iter().cloned().chain(wts.iter().filter_map(|w| w.1.clone())).collect();
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
    let kept_n: std::collections::HashSet<String> = kept.iter().map(|k| norm(k)).collect();
    // worktrees
    for (wpath, wbranch) in &wts {
        let n = norm(wpath);
        let mut info = WtInfo { path: wpath.clone(), branch: wbranch.clone(), merged: None, dirty: None, links: 0, bytes: None, keep: None };
        if kept_n.contains(&n) {
            info.keep = Some("main/root/keep list".into());
        } else if wbranch.is_none() {
            info.keep = Some("detached head".into());
        } else {
            let br = wbranch.clone().unwrap();
            info.merged = if br != cfg.main_branch { ops.merged(&br)? } else { None };
            info.dirty = ops.dirty(wpath)?;
            info.links = ops.list_links(wpath)?.len();
            info.bytes = ops.size(wpath)?;
            if br == cfg.main_branch {
                info.keep = Some("main branch".into());
            } else if cfg.keep_branches.contains(&br) || cfg.keep_lanes.contains(&lane_of(wpath, cfg)) {
                info.keep = Some("keep list".into());
            } else if info.dirty != Some(false) {
                info.keep = Some(if info.dirty == Some(true) { "uncommitted work" } else { "status unreadable" }.into());
            } else if info.merged != Some(true) {
                info.keep = Some(if info.merged == Some(false) { "not merged" } else { "merge state unknown" }.into());
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
    let mut entries: Vec<(String, Option<String>, Option<f64>)> = cfg.targets.iter().map(|x| (x.path.clone(), x.lane.clone(), x.budget_gb)).collect();
    for root in &cfg.target_roots {
        for nm in ops.listdir(root)? {
            entries.push((ops.join(root, &nm), Some(nm), None));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for (path, lane, budget) in entries {
        if seen.contains(&norm(&path)) || !ops.exists(&path)? {
            continue;
        }
        seen.insert(norm(&path));
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
            let reason: Option<String> = if kept_n.contains(&norm(&path)) || cfg.keep_lanes.contains(&lane) || br.as_ref().map(|b| cfg.keep_branches.contains(b)).unwrap_or(false) {
                Some("keep list".into())
            } else if wt_dirty.map(|d| d != Some(false)).unwrap_or(false) {
                Some("its worktree has uncommitted work".into())
            } else if t.idle_min.map(|i| i < cfg.min_idle_min).unwrap_or(true) {
                Some(format!("used in the last {} min", fmt_g(cfg.min_idle_min)))
            } else if br.is_none() && cfg.orphan_targets != "clean" {
                Some("no lane branch found (orphan_targets = keep)".into())
            } else if br.is_some() && t.merged != Some(true) {
                Some(if t.merged == Some(false) { "lane not merged" } else { "merge state unknown" }.into())
            } else {
                None
            };
            t.keep = reason.clone();
            if reason.is_none() && size.map(|s| s != 0).unwrap_or(true) {
                let mut it = Item::new("targets", if cargo { "clean" } else { "move" }, &path, size, &format!("lane {}: {}", lane, if br.is_some() { "branch content is on main" } else { "orphan" }));
                it.extra = Extra::Target { cargo, tag: t.tag };
                inv.items.push(it);
            }
        } else if t.over && !kept_n.contains(&norm(&path)) && (cargo || !names.is_empty()) {
            let mut it = Item::new("targets", if cargo { "clean" } else { "move" }, &path, size, &format!("over its {} GB budget", fmt_g(budget.unwrap())));
            it.attended_only = true;
            it.extra = Extra::Target { cargo, tag: t.tag };
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
        if ops.exists(td)? {
            for nm in ops.listdir(td)? {
                let p = ops.join(td, &nm);
                let mt = ops.mtime(&p)?;
                let age = mt.map(|m| (now - m as f64) / 86400.0);
                let sz = ops.size(&p)?;
                inv.trash.push(TrashInfo { path: p.clone(), bytes: sz, age_days: age });
                if let Some(a) = age {
                    if a > cfg.trash_days {
                        inv.items.push(Item::new("trash", "remove", &p, sz, &format!("in the recycle bin {} days (> {})", crate::pycompat::fmt_f(a, 0), fmt_g(cfg.trash_days))));
                    }
                }
            }
        }
    }
    // profiles in the wrong place (report only)
    let prof = Regex::new(r"(^|[\\/])(SingletonLock|Cookies|Local State|lockfile)$").unwrap();
    for root in &cfg.evidence_dirs {
        let cmd = if ops.win {
            format!("dir /S /B {}", ops.q(root))
        } else {
            format!("find {} -maxdepth 5 \\( -name SingletonLock -o -name Cookies -o -name 'Local State' -o -name lockfile \\)", ops.q(root))
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
    Ok(inv)
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}
