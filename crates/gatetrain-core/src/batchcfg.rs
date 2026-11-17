//! The plugin points the batch state machine reads from `gatetrain.toml` (design 7): regen, preflight, chain, deploy
//! targets and the protected branch. Read raw with `toml`; the full typed config loader belongs to the config lane.
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct Target {
    pub command: Vec<String>,
    pub health: Vec<String>,
    /// `production` is never deployed automatically (floor.production-human).
    pub tier: String,
    /// How long `deploying` may wait for a healthy answer before it refuses (`deploy.health-timeout`); 0 = wait for ever.
    pub health_timeout_min: f64,
}

/// A shared file ("hub") that is rebuilt, not merged, when two lanes change it: `[stack] hubs = [{ glob, rebuild }]`.
/// The rebuild command gets `{base}`, `{ours}`, `{theirs}` (files holding the three versions) and `{path}` (the work-tree file to write).
#[derive(Debug, Clone, Default)]
pub struct Hub {
    pub glob: String,
    pub rebuild: Vec<String>,
}

/// What to take when a path matching `glob` conflicts and no hub rebuilds it: `lane` (theirs), `stack` (ours) or `manual` (refuse).
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    pub glob: String,
    pub take: String,
}

#[derive(Debug, Clone)]
pub struct BatchConfig {
    pub regen_command: Vec<String>,
    pub max_passes: usize,
    pub generated_dirs: Vec<String>,
    pub preflight_command: Vec<String>,
    pub chain_command: Vec<String>,
    /// Step names of the chain, for the memory admission (`[chain] steps`).
    pub chain_steps: Vec<String>,
    /// `[stack] command`: builds the stack itself (the repository's own script with its hub rebuild and guard).
    pub stack_command: Vec<String>,
    pub hubs: Vec<Hub>,
    pub resolvers: Vec<Resolver>,
    /// `[chain] cache_verify_command`: proves a cached step belongs to this host and sha (env GATETRAIN_STEP, GATETRAIN_SHA).
    pub cache_verify_command: Vec<String>,
    /// `[postmerge] command`: after the merge, in `dir` (default: the batch checkout), before the deploy targets.
    pub postmerge_command: Vec<String>,
    pub postmerge_dir: Option<String>,
    /// `[chain] target_dirs`: the build folders the chain writes to (the disk budget is checked against them before a run).
    pub chain_target_dirs: Vec<String>,
    /// `[chain] repo_root`: the repository top level the chain must run in (refused, exit 78, when it is not one).
    pub chain_repo_root: Option<String>,
    /// `[batch] doctor` (default true): the host must pass `doctor` before a preflight or a chain starts when `--hosts` is given.
    pub doctor: bool,
    /// `[lanes] require_stamp` (default false): a lane is stacked only when `<stamp_dir>/stamp-<lane tip sha>.json` exists (`stack.lane-stamp`).
    pub require_lane_stamp: bool,
    /// `[lanes] stamp_dir`: where `gatetrain chain` wrote the lane stamps; relative to the batch checkout when not absolute.
    pub lane_stamp_dir: Option<String>,
    /// `[chain] max_full_runs_per_batch` (default: unlimited): the most full proof runs one batch may start (`budget.full-runs`).
    pub max_full_runs_per_batch: Option<usize>,
    pub protected: String,
    pub targets: BTreeMap<String, Target>,
}

impl Default for BatchConfig {
    fn default() -> Self {
        BatchConfig {
            regen_command: vec![],
            max_passes: 3,
            generated_dirs: vec![],
            preflight_command: vec![],
            chain_command: vec![],
            chain_steps: vec![],
            stack_command: vec![],
            hubs: vec![],
            resolvers: vec![],
            cache_verify_command: vec![],
            postmerge_command: vec![],
            postmerge_dir: None,
            chain_target_dirs: vec![],
            chain_repo_root: None,
            doctor: true,
            require_lane_stamp: false,
            lane_stamp_dir: None,
            max_full_runs_per_batch: None,
            protected: "main".into(),
            targets: BTreeMap::new(),
        }
    }
}

fn argv(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

impl BatchConfig {
    pub fn load(path: &Path) -> Result<BatchConfig> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Batch(format!("reading {}: {e}", path.display())))?;
        BatchConfig::parse(&text)
    }

    pub fn parse(text: &str) -> Result<BatchConfig> {
        let v: toml::Value = text.parse().map_err(|e| Error::Batch(format!("gatetrain.toml: {e}")))?;
        let mut c = BatchConfig::default();
        if let Some(r) = v.get("regen") {
            c.regen_command = argv(r.get("command"));
            if let Some(n) = r.get("max_passes").and_then(|n| n.as_integer()) {
                c.max_passes = n.max(1) as usize;
            }
            c.generated_dirs = argv(r.get("generated_dirs"));
        }
        c.preflight_command = argv(v.get("preflight").and_then(|p| p.get("command")));
        c.chain_command = argv(v.get("chain").and_then(|p| p.get("command")));
        c.chain_steps = argv(v.get("chain").and_then(|p| p.get("steps")));
        if let Some(s) = v.get("stack") {
            c.stack_command = argv(s.get("command"));
            for h in s.get("hubs").and_then(|h| h.as_array()).into_iter().flatten() {
                c.hubs.push(Hub { glob: h.get("glob").and_then(|g| g.as_str()).unwrap_or_default().to_string(), rebuild: argv(h.get("rebuild")) });
            }
            for r in s.get("resolvers").and_then(|h| h.as_array()).into_iter().flatten() {
                c.resolvers.push(Resolver { glob: r.get("glob").and_then(|g| g.as_str()).unwrap_or_default().to_string(), take: r.get("take").and_then(|g| g.as_str()).unwrap_or("manual").to_string() });
            }
        }
        c.cache_verify_command = argv(v.get("chain").and_then(|p| p.get("cache_verify_command")));
        c.chain_target_dirs = argv(v.get("chain").and_then(|p| p.get("target_dirs")));
        c.chain_repo_root = v.get("chain").and_then(|p| p.get("repo_root")).and_then(|r| r.as_str()).map(String::from);
        if let Some(d) = v.get("batch").and_then(|b| b.get("doctor")).and_then(|d| d.as_bool()) {
            c.doctor = d;
        }
        if let Some(n) = v.get("chain").and_then(|c| c.get("max_full_runs_per_batch")).and_then(|n| n.as_integer()) {
            c.max_full_runs_per_batch = Some(n.max(0) as usize);
        }
        if let Some(l) = v.get("lanes") {
            c.require_lane_stamp = l.get("require_stamp").and_then(|b| b.as_bool()).unwrap_or(false);
            c.lane_stamp_dir = l.get("stamp_dir").and_then(|d| d.as_str()).filter(|d| !d.is_empty()).map(String::from);
            if c.require_lane_stamp && c.lane_stamp_dir.is_none() {
                return Err(Error::Batch("gatetrain.toml: [lanes] require_stamp = true needs a stamp_dir".into()));
            }
        }
        if let Some(p) = v.get("postmerge") {
            c.postmerge_command = argv(p.get("command"));
            c.postmerge_dir = p.get("dir").and_then(|d| d.as_str()).map(String::from);
        }
        if let Some(p) = v.get("batch").and_then(|b| b.get("protected")).and_then(|p| p.as_str()) {
            c.protected = p.to_string();
        }
        if let Some(d) = v.get("deploy").and_then(|d| d.as_table()) {
            for (name, t) in d {
                c.targets.insert(
                    name.clone(),
                    Target {
                        command: argv(t.get("command")),
                        health: argv(t.get("health")),
                        tier: t.get("tier").and_then(|x| x.as_str()).unwrap_or("staging").to_string(),
                        health_timeout_min: t.get("health_timeout_min").and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64))).unwrap_or(0.0),
                    },
                );
            }
        }
        Ok(c)
    }
}
