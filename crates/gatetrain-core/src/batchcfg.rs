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
    /// The target named its `tier` (without one it is `staging`); with `[tick] enabled` every target must.
    pub tier_named: bool,
    /// How long `deploying` may wait for a healthy answer before it refuses (`deploy.health-timeout`); 0 = `[tick] max_wait_minutes`.
    /// It is the only health timer: the tick's wait age does not judge `deploying`.
    pub health_timeout_min: f64,
    /// How long the deploy command may run before its process tree is ended and the deploy fails; `None` = no limit.
    pub timeout_min: Option<f64>,
    /// `idempotent = true`: the deploy command is safe to run twice, so a resume that cannot tell whether a crash left this target
    /// half deployed may deploy it again; otherwise the interrupted attempt waits for a person.
    pub idempotent: bool,
    /// `probe = [...]` (`{sha}` substituted): exit 0 = this target already serves `sha`. Asked before the target is deployed when no
    /// checkpoint says it was, so a target a crash left in doubt is settled instead of deployed twice. An automatic target needs this
    /// or `idempotent = true` (refused at load otherwise).
    pub probe: Vec<String>,
    /// `survives_parent = true`: the deploy starts a long-lived service, so it is not killed when gatetrain dies while it runs.
    pub survives_parent: bool,
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
    /// `[preflight] accept_reused` (default false): a chain stamp from an incremental run (`chain --since`, `"reused"` non-empty) proves the pre-flight. Off: such a stamp is partial and refused.
    pub preflight_accept_reused: bool,
    /// `[lanes] accept_reused` (default false): the same for a lane stamp.
    pub lane_accept_reused: bool,
    /// `[chain] storm_reds` (default 20): when one cause explains at least this many reds of a running chain, `tick` reports it and names
    /// the stop command (engine-v2 section 8). It only reports; nothing is stopped.
    pub storm_reds: usize,
    /// `[tick] enabled` (default false): `gatetrain tick` refuses to run (exit 2) until a person turns it on, after the deploy targets
    /// carry `timeout_min` and their commands are safe to run twice.
    pub tick_enabled: bool,
    /// `[tick] stuck_minutes` (default 60): a tick that finds the ledger lock held longer than this reports it and exits 1.
    pub stuck_minutes: f64,
    /// `[tick] max_wait_minutes` (default 120): a batch that has waited longer (a health check that never passes, a host that keeps
    /// refusing) is reported as needing a person.
    pub max_wait_minutes: f64,
    /// `[tick] max_run_minutes` (default 360): the same for a proving or pre-flight run that never finishes.
    pub max_run_minutes: f64,
    /// `[retry] budget = { T4 = 1 }`: failed attempts of a transition the engine may start again without a person, per transition;
    /// a transition not named has 0 (T7 and T8a included), so its failure waits for `batch advance --retry --reason`.
    pub retry_budgets: BTreeMap<String, usize>,
    /// `[chain] launch_ack_secs` (default 60): how long a launch waits for the holder to name its run (the host lock, or `HOLDS <run>`
    /// in the log) before the launch fails clean and the holder is stopped.
    pub launch_ack_secs: f64,
    /// `[timeouts] plugin_min` (default 30): the limit of every plugin without one of its own (stack, hub rebuild, regen, health,
    /// cache verify, legacy stamp, a deploy probe); git calls have `git_min`. `[preflight] timeout_min` (default 60) and `[postmerge] timeout_min` (default 30) for those two;
    /// a deploy target has its own `timeout_min` (default 60). Every limit is a finite number of minutes above 0, at most 1440.
    pub plugin_timeout_min: f64,
    /// `[timeouts] git_min` (default 30): the limit of every git call the engine makes (git runs hooks, which can hang). Its own key,
    /// so a short health-check limit (`plugin_min`) never cuts a slow but healthy fetch or commit hook short, and the reverse.
    pub git_timeout_min: f64,
    pub preflight_timeout_min: f64,
    pub postmerge_timeout_min: f64,
    /// `[tick] max_hold_minutes` (default 720): a holder of the ledger lock older than this needs a person even while it beats.
    pub max_hold_minutes: f64,
    /// `[retry] max_interrupts` (default 3): consecutive crashes of one transition, or consecutive stopped proof runs, after which
    /// the batch needs a person (`batch.interrupt-cap`, `batch.stopped-cap`).
    pub max_interrupts: usize,
    /// `[postmerge] idempotent = true`: the post-merge command is safe to run twice (required when `[tick] enabled`).
    pub postmerge_idempotent: bool,
    pub protected: String,
    pub targets: BTreeMap<String, Target>,
}

/// The transitions that have an effect and so a failed attempt: the keys `[retry] budget` accepts.
pub const RETRYABLE: &[&str] = &["T1", "T2", "T3", "T4", "T6a", "T7", "T8a"];

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
            preflight_accept_reused: false,
            lane_accept_reused: false,
            storm_reds: 20,
            tick_enabled: false,
            stuck_minutes: 60.0,
            max_wait_minutes: 120.0,
            max_run_minutes: 360.0,
            retry_budgets: BTreeMap::new(),
            launch_ack_secs: 60.0,
            plugin_timeout_min: 30.0,
            git_timeout_min: 30.0,
            preflight_timeout_min: 60.0,
            postmerge_timeout_min: 30.0,
            max_hold_minutes: 720.0,
            max_interrupts: 3,
            postmerge_idempotent: false,
            protected: "main".into(),
            targets: BTreeMap::new(),
        }
    }
}

/// The longest plugin limit accepted: a day.
pub const MAX_LIMIT_MIN: f64 = 1440.0;

/// A plugin limit: a finite number of minutes above 0 and at most [`MAX_LIMIT_MIN`].
fn limit_min(x: &toml::Value) -> Option<f64> {
    x.as_float().or_else(|| x.as_integer().map(|i| i as f64)).filter(|m| m.is_finite() && *m > 0.0 && *m <= MAX_LIMIT_MIN)
}

fn argv(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

impl BatchConfig {
    /// How many failed attempts of `tr` the engine starts again by itself (`[retry] budget`, default 0: a person's override row).
    pub fn retry_budget(&self, tr: &str) -> usize {
        self.retry_budgets.get(tr).copied().unwrap_or(0)
    }
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
        if let Some(x) = v.get("tick").and_then(|t| t.get("enabled")) {
            c.tick_enabled = x.as_bool().ok_or_else(|| Error::Batch(format!("gatetrain.toml: [tick] enabled is true or false, not {x}")))?;
        }
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
            c.lane_accept_reused = l.get("accept_reused").and_then(|b| b.as_bool()).unwrap_or(false);
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
            c.preflight_accept_reused = p.get("accept_reused").and_then(|b| b.as_bool()).unwrap_or(false);
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
        for (table, key, slot) in [
            ("timeouts", "plugin_min", &mut c.plugin_timeout_min),
            ("timeouts", "git_min", &mut c.git_timeout_min),
            ("preflight", "timeout_min", &mut c.preflight_timeout_min),
            ("postmerge", "timeout_min", &mut c.postmerge_timeout_min),
        ] {
            if let Some(x) = v.get(table).and_then(|t| t.get(key)) {
                *slot = limit_min(x).ok_or_else(|| Error::Batch(format!("gatetrain.toml: [{table}] {key} is a number of minutes above 0 and at most {MAX_LIMIT_MIN}, not {x}")))?;
            }
        }
        if let Some(x) = v.get("tick").and_then(|t| t.get("max_hold_minutes")) {
            c.max_hold_minutes = x.as_float().or_else(|| x.as_integer().map(|i| i as f64)).filter(|m| m.is_finite() && *m > 0.0).ok_or_else(|| Error::Batch(format!("gatetrain.toml: [tick] max_hold_minutes is a number of minutes above 0, not {x}")))?;
        }
        if let Some(n) = v.get("chain").and_then(|c| c.get("launch_ack_secs")) {
            match n.as_float().or_else(|| n.as_integer().map(|i| i as f64)) {
                Some(x) if x > 0.0 && x <= 3600.0 => c.launch_ack_secs = x,
                _ => return Err(Error::Batch(format!("gatetrain.toml: [chain] launch_ack_secs is a number of seconds above 0 and at most 3600, not {n}"))),
            }
        }
        if let Some(b) = v.get("retry").and_then(|r| r.get("budget")) {
            let t = b.as_table().ok_or_else(|| Error::Batch(format!("gatetrain.toml: [retry] budget is a table of transition = count, not {b}")))?;
            for (tr, n) in t {
                if !RETRYABLE.contains(&tr.as_str()) {
                    return Err(Error::Batch(format!("gatetrain.toml: [retry] budget names {tr}; a budget is for one of {}", RETRYABLE.join(", "))));
                }
                match n.as_integer() {
                    Some(n) if n >= 0 => {
                        c.retry_budgets.insert(tr.clone(), n as usize);
                    }
                    _ => return Err(Error::Batch(format!("gatetrain.toml: [retry] budget {tr} is a whole number of at least 0, not {n}"))),
                }
            }
        }
        if let Some(n) = v.get("retry").and_then(|r| r.get("max_interrupts")) {
            match n.as_integer() {
                Some(n) if n >= 1 => c.max_interrupts = n as usize,
                _ => return Err(Error::Batch(format!("gatetrain.toml: [retry] max_interrupts is a whole number of at least 1, not {n}"))),
            }
        }
        if let Some(x) = v.get("postmerge").and_then(|p| p.get("idempotent")) {
            c.postmerge_idempotent = x.as_bool().ok_or_else(|| Error::Batch(format!("gatetrain.toml: [postmerge] idempotent is true or false, not {x}")))?;
        }
        if let Some(p) = v.get("postmerge") {
            c.postmerge_command = argv(p.get("command"));
            c.postmerge_dir = p.get("dir").and_then(|d| d.as_str()).map(String::from);
        }
        if let Some(p) = v.get("batch").and_then(|b| b.get("protected")).and_then(|p| p.as_str()) {
            c.protected = p.to_string();
        }
        if let Some(d) = v.get("deploy").and_then(|d| d.as_table()) {
            // the tiers a target may name: `staging` and `production` (never automatic), plus `[deploy] auto_tiers`; spelled exactly,
            // so `Production` or `prod` is refused instead of being deployed as an automatic tier
            let mut tiers: Vec<String> = vec!["staging".into(), "production".into()];
            if let Some(a) = d.get("auto_tiers") {
                let extra = argv(Some(a));
                if extra.is_empty() && a.as_array().is_none_or(|x| !x.is_empty()) {
                    return Err(Error::Batch(format!("gatetrain.toml: [deploy] auto_tiers is a list of tier names, not {a}")));
                }
                // any case of `production` or `prod`: a tier that reads as production is never made automatic by a spelling
                if let Some(t) = extra.iter().find(|t| matches!(t.trim().to_lowercase().as_str(), "production" | "prod")) {
                    return Err(Error::Batch(format!("gatetrain.toml: [deploy] auto_tiers names {t:?}; production (in any spelling, prod included) is never deployed automatically")));
                }
                tiers.extend(extra);
            }
            for (name, t) in d.iter().filter(|(k, _)| k.as_str() != "auto_tiers") {
                if let Some(tier) = t.get("tier") {
                    match tier.as_str() {
                        Some(x) if tiers.iter().any(|k| k == x) => {}
                        _ => return Err(Error::Batch(format!("gatetrain.toml: [deploy.{name}] tier is {tier}, which is not a known tier ({}; more automatic tiers go in [deploy] auto_tiers, spelled exactly)", tiers.join(", ")))),
                    }
                }
                if t.get("probe").is_some() && argv(t.get("probe")).is_empty() {
                    return Err(Error::Batch(format!("gatetrain.toml: [deploy.{name}] probe is a command (an argument list), not {}", t.get("probe").map(|x| x.to_string()).unwrap_or_default())));
                }
                c.targets.insert(
                    name.clone(),
                    Target {
                        command: argv(t.get("command")),
                        health: argv(t.get("health")),
                        probe: argv(t.get("probe")),
                        survives_parent: match t.get("survives_parent") {
                            None => false,
                            Some(x) => x.as_bool().ok_or_else(|| Error::Batch(format!("gatetrain.toml: [deploy.{name}] survives_parent is true or false, not {x}")))?,
                        },
                        idempotent: match t.get("idempotent") {
                            None => false,
                            Some(x) => x.as_bool().ok_or_else(|| Error::Batch(format!("gatetrain.toml: [deploy.{name}] idempotent is true or false, not {x}")))?,
                        },
                        tier: t.get("tier").and_then(|x| x.as_str()).unwrap_or("staging").to_string(),
                        tier_named: t.get("tier").is_some(),
                        health_timeout_min: t.get("health_timeout_min").and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64))).unwrap_or(0.0),
                        timeout_min: match t.get("timeout_min") {
                            None => None,
                            Some(x) => Some(limit_min(x).ok_or_else(|| Error::Batch(format!("gatetrain.toml: [deploy.{name}] timeout_min is a number of minutes above 0 and at most {MAX_LIMIT_MIN}, not {x}")))?),
                        },
                    },
                );
            }
        }
        c.check_targets()?;
        Ok(c)
    }

    /// The deploy floor at load (review of the tick, S8): an automatic target (any tier but `production`) is either declared safe to
    /// run twice (`idempotent = true`) or has a `probe` that says whether it already serves the sha, so a crash mid-deploy is never
    /// settled by guessing; with `[tick] enabled` it also has a `timeout_min`, so an unattended deploy cannot hang for its default.
    fn check_targets(&self) -> Result<()> {
        if self.tick_enabled && !self.postmerge_command.is_empty() && !self.postmerge_idempotent {
            // a resume runs the post-merge command again when a crash left it in doubt
            return Err(Error::Batch("gatetrain.toml: [postmerge] idempotent = true is required while [tick] enabled = true (a crash during the post-merge command means it runs again)".into()));
        }
        if self.tick_enabled {
            // an unattended deploy never rests on the default tier: a target that forgot `tier = "production"` would be deployed
            if let Some(name) = self.targets.iter().find(|(_, t)| !t.tier_named).map(|(n, _)| n) {
                return Err(Error::Batch(format!("gatetrain.toml: [deploy.{name}] needs an explicit tier while [tick] enabled = true (staging, production or one of [deploy] auto_tiers); the default staging is not assumed for an unattended deploy")));
            }
        }
        for (name, t) in self.targets.iter().filter(|(_, t)| t.tier != "production") {
            if t.probe.is_empty() && !t.idempotent {
                return Err(Error::Batch(format!(
                    "gatetrain.toml: [deploy.{name}] needs idempotent = true (its command is safe to run twice) or a probe = [...] that exits 0 when the target already serves {{sha}}; without either a crash mid-deploy cannot be settled"
                )));
            }
            if self.tick_enabled && t.timeout_min.is_none() {
                return Err(Error::Batch(format!("gatetrain.toml: [deploy.{name}] needs timeout_min while [tick] enabled = true (an unattended deploy must say how long it may take)")));
            }
        }
        Ok(())
    }
}
