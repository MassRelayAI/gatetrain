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
    /// How long the deploy command may run before its process tree is ended and the deploy fails; `None` = no limit.
    pub timeout_min: Option<f64>,
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
    /// `[merge] allow_cached_steps` (`verified` default | `never`): `never` refuses every proof that holds a cached step (`policy-never`).
    pub allow_cached_steps: String,
    /// `[proof] accept_roles` (default `["proof"]`): host roles whose portable results may count for a proof on another host.
    pub accept_roles: Vec<String>,
    /// `[proof.host_roles]`: host name to role, so a result's host can be judged by its role.
    pub host_roles: BTreeMap<String, String>,
    /// `[proof] max_age_hours`: a result older than this is refused (`evidence-stale`); unset = no limit.
    pub max_age_hours: Option<f64>,
    /// `[proof] shadow_min` (default 3): would-hits with no defect a key must show before its results count for a proof.
    pub shadow_min: usize,
    /// `[proof] host` and `host_class`: the proof host's name and class, for selecting evidence at launch (T5 re-verifies from the log).
    pub proof_host: Option<String>,
    pub proof_host_class: Option<String>,
    /// `[chain] steps_file`: the chain's steps file; its sha-256 is the plan digest a source stamp must equal (`evidence-stamp`).
    pub steps_file: Option<String>,
    /// `[evidence] stamp_dirs`: folders of `stamp-*.json` the engine reads step results from (the stamp is the fact, design 7.2).
    pub evidence_stamp_dirs: Vec<String>,
    /// `[preflight] chain_command`: launches the pre-flight as a run under the holder on a light host (state `preflighting`, E5).
    /// `{sha}`, `{run}`, `{log}` are substituted. Without it `[preflight] command` runs and blocks as before.
    pub preflight_chain_command: Vec<String>,
    /// `[preflight] steps_file` / `stamp_dir`: the pre-flight's steps file (its digest must be the stamp's) and where its stamp is written.
    pub preflight_steps_file: Option<String>,
    pub preflight_stamp_dir: Option<String>,
    /// `[pipeline] max_ahead` (default 1): unmerged batches a new batch may be stacked on. `prove_ahead` (default false): prove before the base is proven.
    pub max_ahead: usize,
    pub prove_ahead: bool,
    /// `[close] tidy` (default false): T9 tidies the batch's own recorded items before `closed` (otherwise it closes at once).
    pub close_tidy: bool,
    /// `[preflight] steps`: the pre-flight's step names, for the light run's memory admission.
    pub preflight_steps: Vec<String>,
    /// `[close] roots`: path items must sit under one of these folders (`tidy.not-owned` otherwise). Empty = no root rule.
    pub close_roots: Vec<String>,
    /// `[close] own_workdir` (default false): the batch's checkout is part of its owned set (it is never the main checkout).
    pub close_own_workdir: bool,
    /// `[close] protected_container_prefixes`: container name prefixes close never touches (`tidy.reference-stack`).
    pub close_protected_container_prefixes: Vec<String>,
    /// `[disk] tidy_auto` (default false): the disk floor may run `tidy --auto` once before refusing (`tidy:auto` allowed by policy).
    pub tidy_auto: bool,
    /// `[preflight] legacy_stamp_command`: runs once after a green launched pre-flight, with `{sha}` and `{results}`.
    pub legacy_stamp_command: Vec<String>,
    /// `[chain] storm_reds` (default 20): when one cause explains at least this many reds of a running chain, `tick` reports it and names
    /// the stop command (engine-v2 section 8). It only reports; nothing is stopped.
    pub storm_reds: usize,
    /// `[tick] stuck_minutes` (default 60): a tick that finds the ledger lock held longer than this reports it and exits 1.
    pub stuck_minutes: f64,
    /// `[tick] max_wait_minutes` (default 120): a batch that has waited longer (a health check that never passes, a host that keeps
    /// refusing) is reported as needing a person.
    pub max_wait_minutes: f64,
    /// `[tick] max_run_minutes` (default 360): the same for a proving or pre-flight run that never finishes.
    pub max_run_minutes: f64,
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
            allow_cached_steps: "verified".into(),
            accept_roles: vec!["proof".into()],
            host_roles: BTreeMap::new(),
            max_age_hours: None,
            shadow_min: 3,
            proof_host: None,
            proof_host_class: None,
            steps_file: None,
            evidence_stamp_dirs: vec![],
            preflight_chain_command: vec![],
            preflight_steps_file: None,
            preflight_stamp_dir: None,
            max_ahead: 1,
            prove_ahead: false,
            close_tidy: false,
            preflight_steps: vec![],
            close_roots: vec![],
            close_own_workdir: false,
            close_protected_container_prefixes: vec![],
            tidy_auto: false,
            legacy_stamp_command: vec![],
            storm_reds: 20,
            stuck_minutes: 60.0,
            max_wait_minutes: 120.0,
            max_run_minutes: 360.0,
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
        if let Some(n) = v.get("tick").and_then(|t| t.get("stuck_minutes")) {
            match n.as_float().or_else(|| n.as_integer().map(|i| i as f64)) {
                Some(m) if m > 0.0 => c.stuck_minutes = m,
                _ => return Err(Error::Batch(format!("gatetrain.toml: [tick] stuck_minutes is a number of minutes above 0, not {n}"))),
            }
        }
        for (key, slot) in [("max_wait_minutes", &mut c.max_wait_minutes), ("max_run_minutes", &mut c.max_run_minutes)] {
            if let Some(n) = v.get("tick").and_then(|t| t.get(key)) {
                match n.as_float().or_else(|| n.as_integer().map(|i| i as f64)) {
                    Some(m) if m > 0.0 => *slot = m,
                    _ => return Err(Error::Batch(format!("gatetrain.toml: [tick] {key} is a number of minutes above 0, not {n}"))),
                }
            }
        }
        if let Some(n) = v.get("chain").and_then(|c| c.get("storm_reds")) {
            match n.as_integer() {
                Some(n) if n >= 1 => c.storm_reds = n as usize,
                _ => return Err(Error::Batch(format!("gatetrain.toml: [chain] storm_reds is a whole number of at least 1, not {n}"))),
            }
        }
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
        let strv = |t: Option<&toml::Value>, k: &str| t.and_then(|t| t.get(k)).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(String::from);
        if let Some(m) = v.get("merge") {
            if let Some(a) = m.get("allow_cached_steps").and_then(|a| a.as_str()) {
                if !matches!(a, "never" | "verified") {
                    return Err(Error::Batch(format!("gatetrain.toml: [merge] allow_cached_steps is `never` or `verified`, not `{a}`")));
                }
                c.allow_cached_steps = a.to_string();
            }
        }
        if let Some(p) = v.get("proof") {
            if p.get("accept_roles").is_some() {
                c.accept_roles = argv(p.get("accept_roles"));
            }
            for (h, r) in p.get("host_roles").and_then(|t| t.as_table()).into_iter().flatten() {
                if let Some(r) = r.as_str() {
                    c.host_roles.insert(h.clone(), r.to_string());
                }
            }
            c.max_age_hours = p.get("max_age_hours").and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)));
            if let Some(n) = p.get("shadow_min").and_then(|n| n.as_integer()) {
                c.shadow_min = n.max(0) as usize;
            }
            c.proof_host = strv(Some(p), "host");
            c.proof_host_class = strv(Some(p), "host_class");
        }
        c.steps_file = strv(v.get("chain"), "steps_file");
        c.evidence_stamp_dirs = argv(v.get("evidence").and_then(|e| e.get("stamp_dirs")));
        if let Some(p) = v.get("preflight") {
            c.preflight_chain_command = argv(p.get("chain_command"));
            c.preflight_steps_file = strv(Some(p), "steps_file");
            c.preflight_steps = argv(p.get("steps"));
            c.preflight_stamp_dir = strv(Some(p), "stamp_dir");
            if !c.preflight_chain_command.is_empty() && c.preflight_stamp_dir.is_none() {
                return Err(Error::Batch("gatetrain.toml: [preflight] chain_command needs a stamp_dir (where the chain writes its stamp)".into()));
            }
        }
        if let Some(cl) = v.get("close") {
            c.close_tidy = cl.get("tidy").and_then(|b| b.as_bool()).unwrap_or(false);
            c.close_roots = argv(cl.get("roots"));
            c.close_own_workdir = cl.get("own_workdir").and_then(|b| b.as_bool()).unwrap_or(false);
            c.close_protected_container_prefixes = argv(cl.get("protected_container_prefixes"));
            if c.close_tidy && c.close_roots.is_empty() {
                return Err(Error::Batch("gatetrain.toml: [close] tidy = true needs [close] roots (the folders a batch may tidy inside); without them nothing is owned".into()));
            }
            for r in &c.close_roots {
                crate::tidy::close::parse_root(r).map_err(|why| Error::Batch(format!("gatetrain.toml: [close] roots: {why}")))?;
            }
            if let Some(r) = c.close_roots.iter().find(|r| !r.starts_with("prefix:") && crate::tidy::close::is_fs_root(r)) {
                return Err(Error::Batch(format!("gatetrain.toml: [close] roots names the file-system root {r:?}; a root owns every path below it, so it must be a folder of its own")));
            }
        }
        c.tidy_auto = v.get("disk").and_then(|d| d.get("tidy_auto")).and_then(|b| b.as_bool()).unwrap_or(false);
        c.legacy_stamp_command = argv(v.get("preflight").and_then(|p| p.get("legacy_stamp_command")));
        if let Some(p) = v.get("pipeline") {
            if let Some(n) = p.get("max_ahead").and_then(|n| n.as_integer()) {
                c.max_ahead = n.max(0) as usize;
            }
            c.prove_ahead = p.get("prove_ahead").and_then(|b| b.as_bool()).unwrap_or(false);
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
                        timeout_min: match t.get("timeout_min") {
                            None => None,
                            Some(x) => match x.as_float().or_else(|| x.as_integer().map(|i| i as f64)) {
                                Some(m) if m > 0.0 => Some(m),
                                _ => return Err(Error::Batch(format!("gatetrain.toml: [deploy.{name}] timeout_min is a number of minutes above 0, not {x}"))),
                            },
                        },
                    },
                );
            }
        }
        Ok(c)
    }
}
