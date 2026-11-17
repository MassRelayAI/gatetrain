//! `build_cfg` of tidy.py: the `[tidy]` table overlaid by the host's own `tidy` table, validated. The hosts, disks and the
//! tables themselves come from the one config module ([`crate::config`]); nothing about hosts.toml is parsed here.
use crate::config::{repr, Config, ConfigError, Host};
use crate::pycompat::{repr_list, repr_str};
use toml::Value;

fn err<T>(s: String) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(s))
}

fn is_num(v: &Value) -> bool {
    matches!(v, Value::Integer(_) | Value::Float(_))
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Integer(i) => *i as f64,
        Value::Float(f) => *f,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------- the [tidy] table

pub const CLASSES: [&str; 6] = ["targets", "worktrees", "containers", "images", "cache", "trash"];
pub const DEFAULT_UNATTENDED: [&str; 3] = ["targets", "containers", "trash"];

#[derive(Debug, Clone)]
pub struct Target {
    pub path: String,
    pub lane: Option<String>,
    pub budget_gb: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct TidyCfg {
    pub main_checkout: Option<String>,
    pub root_checkout: Option<String>,
    pub main_branch: String,
    /// tools rewrite these in main while tidy runs; they do not count as a change of main
    pub main_volatile_paths: Vec<String>,
    pub keep_paths: Vec<String>,
    /// paths or globs that tidy never removes, never removes anything containing, and never reaches through a link (default: none)
    pub protect: Vec<String>,
    pub keep_lanes: Vec<String>,
    pub keep_branches: Vec<String>,
    pub targets: Vec<Target>,
    pub target_roots: Vec<String>,
    pub worktree_prefix: String,
    pub min_idle_min: f64,
    /// keep | clean (a target whose lane has no branch)
    pub orphan_targets: String,
    pub trash_dir: Option<String>,
    pub trash_days: f64,
    pub container_prefix: Option<String>,
    /// container-name prefixes that are never planned or removed, whatever `container_prefix` says (reference stacks)
    pub protect_containers: Vec<String>,
    /// `docker save` an unused image to `<trash_dir>/docker-images/` before it is removed; the archive ages out like any trash entry
    pub image_archive: bool,
    /// globs that win over `image_patterns`: an image matching one is never planned or removed
    pub image_keep_patterns: Vec<String>,
    pub image_patterns: Vec<String>,
    /// 0 = never touch the docker build cache
    pub cache_days: f64,
    pub evidence_dirs: Vec<String>,
    pub unattended: Vec<String>,
    pub log: String,
    pub scratch_dir: Option<String>,
    pub link_canary: Option<String>,
    pub link_canary_count: Option<i64>,
    /// a size probe slower than this is unknown, never a crash
    pub size_timeout_s: f64,
    /// a removal, move or cargo clean gets this long before it is "stopped"
    pub remove_timeout_s: f64,
    /// a worktree with a file changed more recently than this many days is kept (0 = off)
    pub worktree_min_idle_days: f64,
    /// how deep the "newest file inside" walk goes
    pub idle_walk_depth: f64,
    /// the link listing walks a whole worktree: it gets this long (seconds)
    pub link_list_timeout_s: f64,
    /// every probe of ONE item (a worktree, a target, a recycle-bin entry) shares this many seconds; an item that runs out is skipped
    /// and reported, never sized by guess (0 = no budget)
    pub item_budget_s: f64,
}

impl Default for TidyCfg {
    fn default() -> Self {
        TidyCfg {
            main_checkout: None,
            root_checkout: None,
            main_branch: "main".into(),
            main_volatile_paths: vec![".obsidian/".into()],
            keep_paths: vec![],
            protect: vec![],
            keep_lanes: vec![],
            keep_branches: vec![],
            targets: vec![],
            target_roots: vec![],
            worktree_prefix: String::new(),
            min_idle_min: 60.0,
            orphan_targets: "keep".into(),
            trash_dir: None,
            trash_days: 7.0,
            container_prefix: None,
            protect_containers: vec![],
            image_archive: false,
            image_keep_patterns: vec![],
            image_patterns: vec![],
            cache_days: 0.0,
            evidence_dirs: vec![],
            unattended: DEFAULT_UNATTENDED.iter().map(|s| s.to_string()).collect(),
            log: "tidy-log.jsonl".into(),
            scratch_dir: None,
            link_canary: None,
            link_canary_count: None,
            size_timeout_s: 30.0,
            remove_timeout_s: 1800.0,
            worktree_min_idle_days: 2.0,
            idle_walk_depth: 12.0,
            link_list_timeout_s: 300.0,
            item_budget_s: 600.0,
        }
    }
}

const KNOWN: [&str; 33] = [
    "cache_days", "container_prefix", "evidence_dirs", "idle_walk_depth", "image_archive", "image_keep_patterns", "image_patterns", "item_budget_s", "keep_branches", "keep_lanes", "keep_paths", "link_canary",
    "link_canary_count", "link_list_timeout_s", "log", "main_branch", "main_checkout", "main_volatile_paths", "min_idle_min", "orphan_targets", "protect", "protect_containers", "remove_timeout_s", "root_checkout", "scratch_dir",
    "size_timeout_s", "target_roots", "targets", "trash_days", "trash_dir", "unattended", "worktree_min_idle_days", "worktree_prefix",
];

fn str_list(k: &str, v: &Value) -> Result<Vec<String>, ConfigError> {
    if let Value::Array(a) = v {
        let mut o = Vec::new();
        for x in a {
            match x {
                Value::String(s) => o.push(s.clone()),
                _ => return err(format!("tidy.{k}: must be a list of strings, got {}", repr(v))),
            }
        }
        return Ok(o);
    }
    err(format!("tidy.{k}: must be a list of strings, got {}", repr(v)))
}

fn nonempty(k: &str, v: &Value) -> Result<String, ConfigError> {
    match v {
        Value::String(s) if !s.is_empty() => Ok(s.clone()),
        _ => err(format!("tidy.{k}: must be a non-empty string, got {}", repr(v))),
    }
}

fn number(k: &str, v: &Value) -> Result<f64, ConfigError> {
    if is_num(v) {
        Ok(as_f64(v))
    } else {
        err(format!("tidy.{k}: must be a number, got {}", repr(v)))
    }
}

/// `tidy.build_cfg`: the top-level `[tidy]` table, overlaid by the host's own `tidy` table, validated.
pub fn build_cfg(cfg: &Config, host: &Host) -> Result<TidyCfg, ConfigError> {
    let mut d = cfg.tidy.clone();
    for (k, v) in &host.tidy {
        d.insert(k.clone(), v.clone());
    }
    let mut bad: Vec<String> = d.keys().filter(|k| !KNOWN.contains(&k.as_str())).cloned().collect();
    bad.sort();
    if !bad.is_empty() {
        return err(format!("tidy: unknown key(s) {}; allowed {}", repr_list(&bad), repr_list(&KNOWN)));
    }
    let mut t = TidyCfg::default();
    // Python validates and assigns key by key in the table's order, so the first bad key is the one reported.
    for (k, v) in &d {
        match k.as_str() {
            "keep_paths" => t.keep_paths = str_list(k, v)?,
            "protect" => {
                t.protect = str_list(k, v)?;
                if t.protect.iter().any(|p| p.trim().is_empty()) {
                    return err("tidy.protect: an entry is empty; an empty path would protect nothing and read as if it protected everything".into());
                }
            }
            "main_volatile_paths" => t.main_volatile_paths = str_list(k, v)?,
            "keep_lanes" => t.keep_lanes = str_list(k, v)?,
            "keep_branches" => t.keep_branches = str_list(k, v)?,
            "target_roots" => t.target_roots = str_list(k, v)?,
            "image_patterns" => t.image_patterns = str_list(k, v)?,
            "image_keep_patterns" => t.image_keep_patterns = str_list(k, v)?,
            "image_archive" => match v {
                Value::Boolean(b) => t.image_archive = *b,
                _ => return err("tidy.image_archive: expected true or false".into()),
            },
            "evidence_dirs" => t.evidence_dirs = str_list(k, v)?,
            "unattended" => t.unattended = str_list(k, v)?,
            "main_checkout" => t.main_checkout = Some(nonempty(k, v)?),
            "root_checkout" => t.root_checkout = Some(nonempty(k, v)?),
            "main_branch" => t.main_branch = nonempty(k, v)?,
            "worktree_prefix" => t.worktree_prefix = nonempty(k, v)?,
            "orphan_targets" => t.orphan_targets = nonempty(k, v)?,
            "trash_dir" => t.trash_dir = Some(nonempty(k, v)?),
            "container_prefix" => t.container_prefix = Some(nonempty(k, v)?),
            "protect_containers" => {
                t.protect_containers = str_list(k, v)?;
                if t.protect_containers.iter().any(|p| p.trim().is_empty()) {
                    return err("tidy.protect_containers: an entry is empty; an empty prefix would protect every container".into());
                }
            }
            "log" => t.log = nonempty(k, v)?,
            "scratch_dir" => t.scratch_dir = Some(nonempty(k, v)?),
            "link_canary" => t.link_canary = Some(nonempty(k, v)?),
            "min_idle_min" => t.min_idle_min = number(k, v)?,
            "trash_days" => t.trash_days = number(k, v)?,
            "cache_days" => t.cache_days = number(k, v)?,
            "size_timeout_s" => t.size_timeout_s = number(k, v)?,
            "remove_timeout_s" => t.remove_timeout_s = number(k, v)?,
            "worktree_min_idle_days" => t.worktree_min_idle_days = number(k, v)?,
            "idle_walk_depth" => t.idle_walk_depth = number(k, v)?,
            "link_list_timeout_s" => t.link_list_timeout_s = number(k, v)?,
            "item_budget_s" => t.item_budget_s = number(k, v)?,
            "link_canary_count" => match v {
                Value::Integer(i) => t.link_canary_count = Some(*i),
                other => return err(format!("tidy.link_canary_count: must be an integer, got {}", repr(other))),
            },
            "targets" => {
                let Value::Array(a) = v else { return err("tidy.targets[0]: needs a table with a path".into()) };
                t.targets.clear();
                for (i, x) in a.iter().enumerate() {
                    let bad_entry = || ConfigError::Invalid(format!("tidy.targets[{i}]: needs a table with a path"));
                    let Value::Table(m) = x else { return Err(bad_entry()) };
                    let Some(Value::String(path)) = m.get("path") else { return Err(bad_entry()) };
                    let lane = match m.get("lane") {
                        None => None,
                        Some(Value::String(s)) => Some(s.clone()),
                        Some(o) => return err(format!("tidy.targets[{i}].lane: must be a string, got {}", repr(o))),
                    };
                    let budget_gb = match m.get("budget_gb") {
                        None => None,
                        Some(o) if is_num(o) => Some(as_f64(o)),
                        Some(o) => return err(format!("tidy.targets[{i}].budget_gb: must be a number, got {}", repr(o))),
                    };
                    t.targets.push(Target { path: path.clone(), lane, budget_gb });
                }
            }
            _ => {}
        }
    }
    if t.orphan_targets != "keep" && t.orphan_targets != "clean" {
        return err("tidy.orphan_targets: must be keep or clean".into());
    }
    for c in &t.unattended {
        if !CLASSES.contains(&c.as_str()) {
            return err(format!("tidy.unattended: unknown class {}; allowed {}", repr_str(c), repr_list(&CLASSES)));
        }
    }
    if t.worktree_prefix.contains(['/', '\\']) {
        return err(format!(
            "tidy.worktree_prefix: {} holds a path separator; it is a prefix of the worktree FOLDER NAME (for example \"wt-\"), compared with the last part of each worktree path",
            repr_str(&t.worktree_prefix)
        ));
    }
    for k in t.main_checkout.iter().chain(t.root_checkout.iter()).chain(t.keep_paths.iter()).chain(t.protect.iter()) {
        if k.starts_with("\\\\") || k.replace('\\', "/").starts_with("//") {
            return err(format!("tidy: the path {} is a UNC or extended-length (\\\\?\\) spelling; write the drive path (C:/dir) so the keep list compares like the removal guards", repr_str(k)));
        }
    }
    if !super::ops::safe_branch(&t.main_branch) {
        return err(format!("tidy.main_branch: {} is not a plain branch name", repr_str(&t.main_branch)));
    }
    if !d.is_empty() && crate::config::host_os(host) == "windows" && (t.link_canary.is_none() || t.link_canary_count.is_none()) {
        return err("tidy: a Windows host needs link_canary (a folder with known links) and link_canary_count".into());
    }
    if t.trash_dir.is_some() {
        check_trash_dir(&t)?;
    }
    if t.image_archive && t.trash_dir.as_deref().is_none_or(|d| d.trim().is_empty()) {
        return err("tidy.image_archive needs trash_dir: the archive lives in <trash_dir>/docker-images and ages out with the recycle bin".into());
    }
    if t.container_prefix.is_none() {
        t.container_prefix = host.container_prefix.clone();
    }
    Ok(t)
}

/// A recycle bin that is a root, a drive, or that holds or sits inside anything we keep would let `remove` delete what it must not.
pub fn check_trash_dir(t: &TidyCfg) -> Result<(), ConfigError> {
    use super::ops::{norm, safe_path};
    let td = t.trash_dir.clone().unwrap_or_default();
    if let Err(e) = safe_path(&td, None) {
        return err(format!("tidy.trash_dir: {e}"));
    }
    let nt = norm(&td);
    let mut kept: Vec<String> = vec![];
    kept.extend(t.main_checkout.clone());
    kept.extend(t.root_checkout.clone());
    kept.extend(t.keep_paths.iter().cloned());
    kept.extend(t.target_roots.iter().cloned());
    kept.extend(t.targets.iter().map(|x| x.path.clone()));
    for k in kept.iter().filter(|k| !k.is_empty()) {
        let nk = norm(k);
        if nk == nt || nk.starts_with(&format!("{nt}/")) {
            return err(format!("tidy.trash_dir {td} is, or contains, {k}"));
        }
    }
    let mut inside: Vec<String> = vec![];
    inside.extend(t.main_checkout.clone());
    inside.extend(t.root_checkout.clone());
    inside.extend(t.keep_paths.iter().cloned());
    for k in inside.iter().filter(|k| !k.is_empty()) {
        if nt.starts_with(&format!("{}/", norm(k))) {
            return err(format!("tidy.trash_dir {td} is inside {k}"));
        }
    }
    Ok(())
}
