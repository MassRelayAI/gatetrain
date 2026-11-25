//! The forced commands' config: top-level `[land]`, `[mainwatch]`, `[limits]`, `[measure]` tables of hosts.toml.
use regex::Regex;
use std::path::Path;
use toml::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Safe,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::Fast => "FAST",
            Mode::Safe => "SAFE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub glob: String,
    pub re: Regex,
    pub mode: Mode,
    /// full gates that must pass before a SAFE change in this area lands
    pub gates: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LandCfg {
    pub main: String,
    pub remote: String,
    pub scratch_dir: Option<String>,
    /// cheap checks, run in the scratch worktree; `{files}`, `{branch}`, `{sha}` are replaced
    pub checks: Vec<String>,
    pub safe_gates: Vec<String>,
    pub rules: Vec<Rule>,
    pub retries: usize,
}

impl Default for LandCfg {
    fn default() -> Self {
        LandCfg { main: "main".into(), remote: "origin".into(), scratch_dir: None, checks: vec![], safe_gates: vec![], rules: vec![], retries: 3 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WatchCfg {
    pub repo: Option<String>,
    /// started for a main commit that has no full chain; `{branch}` and `{sha}` are replaced
    pub chain_cmd: Option<String>,
    pub branch_prefix: String,
    /// where the chain writes its log; `{sha}` and `{short}` are replaced
    pub log_template: Option<String>,
    pub owners: Vec<(Regex, String)>,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_worktrees: usize,
    pub max_idle_days: f64,
    pub max_worktree_gb: f64,
    pub max_unlanded_hours: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_worktrees: 10, max_idle_days: 2.0, max_worktree_gb: 20.0, max_unlanded_hours: 24.0 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ForceCfg {
    pub land: LandCfg,
    pub watch: WatchCfg,
    pub limits: Limits,
    pub vm_disks: Vec<String>,
}

pub fn glob_re(g: &str) -> Result<Regex, String> {
    let mut s = String::from("^");
    let c: Vec<char> = g.replace('\\', "/").chars().collect();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '*' if i + 1 < c.len() && c[i + 1] == '*' => {
                if i + 2 < c.len() && c[i + 2] == '/' {
                    s.push_str("(?:.*/)?");
                    i += 3;
                    continue;
                }
                s.push_str(".*");
                i += 2;
                continue;
            }
            '*' => s.push_str("[^/]*"),
            '?' => s.push_str("[^/]"),
            ch => s.push_str(&regex::escape(&ch.to_string())),
        }
        i += 1;
    }
    s.push('$');
    Regex::new(&s).map_err(|e| format!("glob {g}: {e}"))
}

fn strs(v: Option<&Value>, k: &str) -> Result<Vec<String>, String> {
    match v {
        None => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(String::from).ok_or(format!("{k}: a list of strings"))).collect(),
        Some(_) => Err(format!("{k}: a list of strings")),
    }
}

fn s(t: &toml::Table, k: &str) -> Option<String> {
    t.get(k).and_then(|v| v.as_str()).map(String::from)
}

fn num(t: &toml::Table, k: &str, d: f64) -> Result<f64, String> {
    match t.get(k) {
        None => Ok(d),
        Some(Value::Integer(i)) => Ok(*i as f64),
        Some(Value::Float(f)) => Ok(*f),
        Some(_) => Err(format!("{k}: must be a number")),
    }
}

pub fn parse(text: &str) -> Result<ForceCfg, String> {
    let t: toml::Table = text.parse().map_err(|e: toml::de::Error| format!("config: {e}"))?;
    let empty = toml::Table::new();
    let tbl = |k: &str| t.get(k).and_then(|v| v.as_table()).unwrap_or(&empty);
    let l = tbl("land");
    let mut rules = vec![];
    for (i, r) in l.get("rules").and_then(|v| v.as_array()).into_iter().flatten().enumerate() {
        let r = r.as_table().ok_or(format!("land.rules[{i}]: a table"))?;
        let glob = s(r, "glob").ok_or(format!("land.rules[{i}].glob: required"))?;
        let mode = match s(r, "mode").as_deref() {
            Some("fast") | Some("FAST") => Mode::Fast,
            Some("safe") | Some("SAFE") => Mode::Safe,
            _ => return Err(format!("land.rules[{i}].mode: must be \"fast\" or \"safe\"")),
        };
        rules.push(Rule { re: glob_re(&glob)?, glob, mode, gates: strs(r.get("gates"), "land.rules.gates")? });
    }
    let land = LandCfg {
        main: s(l, "main").unwrap_or("main".into()),
        remote: s(l, "remote").unwrap_or("origin".into()),
        scratch_dir: s(l, "scratch_dir"),
        checks: strs(l.get("checks"), "land.checks")?,
        safe_gates: strs(l.get("safe_gates"), "land.safe_gates")?,
        rules,
        retries: num(l, "retries", 3.0)? as usize,
    };
    let w = tbl("mainwatch");
    let mut owners = vec![];
    for (i, o) in w.get("owners").and_then(|v| v.as_array()).into_iter().flatten().enumerate() {
        let o = o.as_table().ok_or(format!("mainwatch.owners[{i}]: a table"))?;
        let g = s(o, "step").ok_or(format!("mainwatch.owners[{i}].step: required"))?;
        owners.push((glob_re(&g)?, s(o, "owner").ok_or(format!("mainwatch.owners[{i}].owner: required"))?));
    }
    let watch = WatchCfg { repo: s(w, "repo"), chain_cmd: s(w, "chain_cmd"), branch_prefix: s(w, "branch_prefix").unwrap_or("chain/main-".into()), log_template: s(w, "log_template"), owners };
    let m = tbl("limits");
    let d = Limits::default();
    let limits = Limits {
        max_worktrees: num(m, "max_worktrees", d.max_worktrees as f64)? as usize,
        max_idle_days: num(m, "max_idle_days", d.max_idle_days)?,
        max_worktree_gb: num(m, "max_worktree_gb", d.max_worktree_gb)?,
        max_unlanded_hours: num(m, "max_unlanded_hours", d.max_unlanded_hours)?,
    };
    let vm_disks = strs(tbl("measure").get("vm_disks"), "measure.vm_disks")?;
    Ok(ForceCfg { land, watch, limits, vm_disks })
}

pub fn load(path: &str) -> Result<ForceCfg, String> {
    let text = std::fs::read_to_string(Path::new(path)).map_err(|e| format!("cannot read {path}: {e}"))?;
    parse(&text)
}
