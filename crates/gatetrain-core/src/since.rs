//! Incremental re-run (`gatetrain chain --since <sha>`): which steps of a new tip must run again, and which may carry the previous
//! run's green result.
//!
//! Every chain run writes `result-<sha>-<tiers>.json` in its `--out` folder, green or red. A later run with `--since <sha>` reads that
//! file and reuses a step only when ALL of these hold; any doubt re-runs the step:
//! - run level: the previous run had the same tiers and mode, the same toolchain and environment fingerprints, was whole (not
//!   `--only`/`--skip`), was on a clean tree, its commit is known, and no lockfile or toolchain pin changed between the two commits;
//! - step level: the step's definition is unchanged, it declares `inputs` (no inputs = always runs), it was green there (red, lost,
//!   skipped and not-run steps run), its result is no older than `max_age` runs, no declared input differs between the two commits
//!   or is dirty, and none of the steps it `needs` re-runs.
//!
//! A reused step is never silent: `STEP_REUSED <step> <run-id> <sha>` in the log, `reused-from <run-id> <sha>` in the ledger row, and
//! `freshness: "reused"` in the stamp, so a consumer can require fresh steps. `--full` runs everything.
use crate::chain::{git_out, Plan, Step};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Basenames whose change between the two commits re-runs everything (a dependency or toolchain change).
const LOCKS: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    "bun.lock",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "go.sum",
    "Gemfile.lock",
    "composer.lock",
    ".node-version",
    ".nvmrc",
    ".tool-versions",
    ".python-version",
];

#[derive(Debug, Clone)]
pub struct SinceOpts {
    /// a commit (looked up as `result-<sha>-<tiers>.json` in `--out`) or the path of a result file
    pub prev: String,
    /// a result carried through more than this many runs is re-run
    pub max_age: u32,
    /// `--full`: run everything
    pub full: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepResult {
    pub name: String,
    pub tier: String,
    /// the step's exit code; None = skipped
    pub rc: Option<i64>,
    /// hash of the step's definition
    pub def: String,
    /// `fresh` (ran in that run) or `reused`
    pub freshness: String,
    /// the run and commit where the step last ran for real
    pub origin_run: String,
    pub origin_sha: String,
    /// how many runs the result has been carried through (0 = fresh)
    pub age: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub v: u32,
    pub run_id: String,
    pub sha: String,
    pub mode: String,
    pub tiers: String,
    pub toolchain: String,
    pub env: String,
    pub dirty: bool,
    pub partial: bool,
    pub steps: Vec<StepResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Run(String),
    Reuse { run: String, sha: String, age: u32 },
}

/// What the run being planned is.
pub struct Cur<'a> {
    pub mode: &'a str,
    pub tiers: &'a str,
    pub toolchain: &'a str,
    pub env: &'a str,
}

type ChangedFn<'a> = dyn Fn(&str, &[String]) -> Option<Vec<String>> + 'a;

/// The git questions the rules ask; a closure per question keeps the rules testable without a repository.
pub struct Git<'a> {
    /// paths that differ between the previous commit and HEAD, limited to the pathspecs (empty list = the whole tree); None = git could not say
    pub changed: &'a ChangedFn<'a>,
    /// whether any of these pathspecs has uncommitted or untracked changes; None = git could not say
    pub dirty: &'a dyn Fn(&[String]) -> Option<bool>,
}

pub fn tiers_of(plan: &Plan, selected: &[usize]) -> String {
    let has = |t| selected.iter().any(|&i| plan.steps[i].tier == t);
    match (has(crate::chain::Tier::Fast), has(crate::chain::Tier::Slow)) {
        (true, true) => "fast+slow",
        (true, false) => "fast",
        (false, true) => "slow",
        _ => "none",
    }
    .to_string()
}

pub fn result_path(out: &Path, sha: &str, tiers: &str) -> PathBuf {
    out.join(format!("result-{sha}-{tiers}.json"))
}

/// The hash of everything about a step that decides what it does (not its timeout, cores or scheduling hints).
pub fn step_def(step: &Step, prefix: &str) -> String {
    let v = serde_json::json!([
        step.run,
        step.argv,
        step.needs,
        step.inputs,
        step.cache_salt,
        step.shards,
        step.shard_check,
        step.shard_env,
        step.runner,
        step.requires,
        prefix,
        crate::chain::tier_name(step.tier)
    ]);
    Sha256::digest(v.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Read the previous result: `prev` is a path to a result file, or a commit looked up in `out`.
pub fn load_prev(out: &Path, prev: &str, sha_of: &dyn Fn(&str) -> Option<String>, tiers: &str) -> Result<RunResult, String> {
    let p = Path::new(prev);
    let path = if p.is_file() {
        p.to_path_buf()
    } else {
        let full = sha_of(prev).ok_or_else(|| format!("--since {prev:?} is neither a result file nor a commit of this repository"))?;
        result_path(out, &full, tiers)
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("no previous result {}: {e}", path.display()))?;
    let r: RunResult = serde_json::from_str(&text).map_err(|e| format!("{} is not a result file: {e}", path.display()))?;
    if r.v != 1 {
        return Err(format!("{}: result version {} is not understood", path.display(), r.v));
    }
    Ok(r)
}

/// Why nothing at all may be reused from `prev` (None = the run-level rules pass).
pub fn run_level_refusal(prev: &RunResult, cur: &Cur, git: &Git) -> Option<String> {
    if prev.tiers != cur.tiers {
        return Some(format!("the previous run covered {} and this one {}", prev.tiers, cur.tiers));
    }
    if prev.mode != cur.mode {
        return Some(format!("the previous run's mode was {} and this one's is {}", prev.mode, cur.mode));
    }
    if prev.partial {
        return Some("the previous run was partial (--only or --skip)".into());
    }
    if prev.dirty {
        return Some("the previous run was on a dirty tree".into());
    }
    if prev.toolchain != cur.toolchain {
        return Some("the toolchain fingerprint differs".into());
    }
    if prev.env != cur.env {
        return Some("the environment fingerprint differs".into());
    }
    match (git.changed)(&prev.sha, &[]) {
        None => Some(format!("git cannot compare {} with this commit", short(&prev.sha))),
        Some(files) => files
            .iter()
            .find(|f| {
                let base = f.rsplit('/').next().unwrap_or(f);
                LOCKS.contains(&base) || base.starts_with("rust-toolchain")
            })
            .map(|f| format!("a lockfile or toolchain pin changed: {f}")),
    }
}

/// One decision per step of the plan (a step that is not selected gets `Run("not selected")`, which nothing reads).
pub fn decide(plan: &Plan, selected: &[usize], prefix: &str, prev: &RunResult, cur: &Cur, o: &SinceOpts, git: &Git) -> Vec<Decision> {
    let mut d: Vec<Decision> = plan.steps.iter().map(|_| Decision::Run("not selected".into())).collect();
    let all = |why: String, d: &mut Vec<Decision>| {
        for &i in selected {
            d[i] = Decision::Run(why.clone());
        }
    };
    if o.full {
        all("--full".into(), &mut d);
        return d;
    }
    if let Some(why) = run_level_refusal(prev, cur, git) {
        all(why, &mut d);
        return d;
    }
    for &i in selected {
        d[i] = step_decision(&plan.steps[i], prefix, prev, o, git);
    }
    // a step whose need re-runs re-runs too: its own inputs may not show what the need changed
    loop {
        let mut changed = false;
        for &i in selected {
            if !matches!(d[i], Decision::Reuse { .. }) {
                continue;
            }
            let bad = plan.steps[i]
                .needs
                .iter()
                .find(|n| plan.steps.iter().position(|s| &s.name == *n).is_some_and(|j| selected.contains(&j) && matches!(d[j], Decision::Run(_))));
            if let Some(n) = bad {
                d[i] = Decision::Run(format!("needs {n}, which runs"));
                changed = true;
            }
        }
        if !changed {
            return d;
        }
    }
}

fn step_decision(s: &Step, prefix: &str, prev: &RunResult, o: &SinceOpts, git: &Git) -> Decision {
    let run = |w: &str| Decision::Run(w.to_string());
    if s.inputs.is_empty() {
        return run("no declared inputs (always runs)");
    }
    let Some(p) = prev.steps.iter().find(|p| p.name == s.name) else { return run("not in the previous run") };
    if p.rc != Some(0) {
        return run(&match p.rc {
            Some(rc) => format!("it was red in the previous run (rc={rc})"),
            None => "it did not run in the previous run".to_string(),
        });
    }
    if p.def != step_def(s, prefix) {
        return run("its definition changed");
    }
    if p.age + 1 > o.max_age {
        return run(&format!("its result is older than {} runs", o.max_age));
    }
    match (git.changed)(&prev.sha, &s.inputs) {
        None => return run("git cannot compare its inputs"),
        Some(f) if !f.is_empty() => return run(&format!("its inputs changed ({})", f[0])),
        Some(_) => {}
    }
    match (git.dirty)(&s.inputs) {
        None => return run("git cannot say whether its inputs are clean"),
        Some(true) => return run("its inputs are dirty"),
        Some(false) => {}
    }
    Decision::Reuse { run: p.origin_run.clone(), sha: p.origin_sha.clone(), age: p.age + 1 }
}

fn short(s: &str) -> &str {
    &s[..s.len().min(7)]
}

/// The real git behind [`Git`], run in the plan's directory.
pub fn git_changed(cwd: &Path, prev_sha: &str, specs: &[String]) -> Option<Vec<String>> {
    let mut a: Vec<&str> = vec!["diff", "--name-only", prev_sha, "HEAD", "--"];
    if specs.is_empty() {
        a.push(":/");
    }
    a.extend(specs.iter().map(String::as_str));
    git_out(cwd, &a).map(|t| t.lines().map(str::to_string).collect())
}

pub fn git_dirty(cwd: &Path, specs: &[String]) -> Option<bool> {
    let mut a: Vec<&str> = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    a.extend(specs.iter().map(String::as_str));
    git_out(cwd, &a).map(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        crate::chain::load(
            "[[step]]\nname='lint'\ntier='fast'\nrun='true'\ninputs=['src/**']\n[[step]]\nname='unit'\ntier='fast'\nrun='true'\ninputs=['lib/**']\n[[step]]\nname='e2e'\ntier='slow'\nrun='true'\nneeds=['unit']\ninputs=['e2e/**']\n[[step]]\nname='any'\ntier='slow'\nrun='true'\n",
        )
        .unwrap()
    }

    fn prev_for(p: &Plan) -> RunResult {
        RunResult {
            v: 1,
            run_id: "r1".into(),
            sha: "aaaaaaaa".into(),
            mode: "preflight".into(),
            tiers: "fast+slow".into(),
            toolchain: "tc".into(),
            env: "env".into(),
            dirty: false,
            partial: false,
            steps: p
                .steps
                .iter()
                .map(|s| StepResult {
                    name: s.name.clone(),
                    tier: crate::chain::tier_name(s.tier).into(),
                    rc: Some(0),
                    def: step_def(s, ""),
                    freshness: "fresh".into(),
                    origin_run: "r1".into(),
                    origin_sha: "aaaaaaaa".into(),
                    age: 0,
                })
                .collect(),
        }
    }

    fn cur() -> Cur<'static> {
        Cur { mode: "preflight", tiers: "fast+slow", toolchain: "tc", env: "env" }
    }

    fn opts() -> SinceOpts {
        SinceOpts { prev: "aaaaaaaa".into(), max_age: 5, full: false }
    }

    /// `touched`: the paths git says differ between the commits.
    fn go(p: &Plan, prev: &RunResult, c: &Cur, o: &SinceOpts, touched: &[&str]) -> Vec<Decision> {
        let t: Vec<String> = touched.iter().map(|s| s.to_string()).collect();
        let changed = move |_: &str, specs: &[String]| Some(t.iter().filter(|f| specs.is_empty() || specs.iter().any(|s| f.starts_with(s.trim_end_matches("**")))).cloned().collect::<Vec<_>>());
        let dirty = |_: &[String]| Some(false);
        let g = Git { changed: &changed, dirty: &dirty };
        let sel: Vec<usize> = (0..p.steps.len()).collect();
        decide(p, &sel, "", prev, c, o, &g)
    }

    fn reused(d: &Decision) -> bool {
        matches!(d, Decision::Reuse { .. })
    }

    #[test]
    fn an_untouched_declared_step_is_reused_and_labelled_with_its_origin() {
        let p = plan();
        let d = go(&p, &prev_for(&p), &cur(), &opts(), &["docs/readme.md"]);
        assert_eq!(d[0], Decision::Reuse { run: "r1".into(), sha: "aaaaaaaa".into(), age: 1 });
        assert!(reused(&d[1]) && reused(&d[2]));
        assert!(matches!(&d[3], Decision::Run(w) if w.contains("no declared inputs")), "always-class runs: {:?}", d[3]);
    }

    #[test]
    fn a_reused_step_whose_input_changed_runs() {
        let p = plan();
        let d = go(&p, &prev_for(&p), &cur(), &opts(), &["src/a.rs"]);
        assert!(matches!(&d[0], Decision::Run(w) if w.contains("inputs changed")), "{:?}", d[0]);
        assert!(reused(&d[1]), "an unrelated step stays reused");
    }

    #[test]
    fn a_red_or_lost_or_not_run_step_runs() {
        let p = plan();
        for rc in [Some(1), Some(125), None] {
            let mut prev = prev_for(&p);
            prev.steps[1].rc = rc;
            let d = go(&p, &prev, &cur(), &opts(), &[]);
            assert!(matches!(&d[1], Decision::Run(_)), "rc {rc:?}: {:?}", d[1]);
            assert!(matches!(&d[2], Decision::Run(w) if w.contains("needs unit")), "the dependent runs too: {:?}", d[2]);
            assert!(reused(&d[0]));
        }
        let mut prev = prev_for(&p);
        prev.steps.retain(|s| s.name != "lint");
        assert!(matches!(&go(&p, &prev, &cur(), &opts(), &[])[0], Decision::Run(_)), "absent from the previous run");
    }

    #[test]
    fn a_toolchain_or_environment_change_or_a_lockfile_reruns_everything() {
        let p = plan();
        let prev = prev_for(&p);
        let mut c = cur();
        c.toolchain = "other";
        assert!(go(&p, &prev, &c, &opts(), &[]).iter().all(|d| !reused(d)), "toolchain");
        let mut c = cur();
        c.env = "other";
        assert!(go(&p, &prev, &c, &opts(), &[]).iter().all(|d| !reused(d)), "environment");
        for lock in ["Cargo.lock", "web/package-lock.json", "rust-toolchain.toml"] {
            assert!(go(&p, &prev, &cur(), &opts(), &[lock]).iter().all(|d| !reused(d)), "{lock}");
        }
    }

    #[test]
    fn another_tier_mode_a_partial_or_dirty_previous_run_is_refused() {
        let p = plan();
        let mut prev = prev_for(&p);
        prev.tiers = "fast".into();
        assert!(go(&p, &prev, &cur(), &opts(), &[]).iter().all(|d| !reused(d)));
        let mut prev = prev_for(&p);
        prev.mode = "full".into();
        assert!(go(&p, &prev, &cur(), &opts(), &[]).iter().all(|d| !reused(d)));
        let mut prev = prev_for(&p);
        prev.partial = true;
        assert!(go(&p, &prev, &cur(), &opts(), &[]).iter().all(|d| !reused(d)));
        let mut prev = prev_for(&p);
        prev.dirty = true;
        assert!(go(&p, &prev, &cur(), &opts(), &[]).iter().all(|d| !reused(d)));
    }

    #[test]
    fn a_changed_definition_or_an_old_result_runs() {
        let p = plan();
        let mut prev = prev_for(&p);
        prev.steps[0].def = "something else".into();
        assert!(matches!(&go(&p, &prev, &cur(), &opts(), &[])[0], Decision::Run(w) if w.contains("definition")));
        let mut prev = prev_for(&p);
        prev.steps[0].age = 5;
        assert!(matches!(&go(&p, &prev, &cur(), &opts(), &[])[0], Decision::Run(w) if w.contains("older than 5")));
        prev.steps[0].age = 4;
        assert!(reused(&go(&p, &prev, &cur(), &opts(), &[])[0]), "age 4 carried once more is 5, still allowed");
    }

    #[test]
    fn full_runs_everything() {
        let p = plan();
        let mut o = opts();
        o.full = true;
        assert!(go(&p, &prev_for(&p), &cur(), &o, &[]).iter().all(|d| matches!(d, Decision::Run(w) if w == "--full")));
    }

    #[test]
    fn git_failure_or_dirty_inputs_run_never_reuse() {
        let p = plan();
        let prev = prev_for(&p);
        let none = |_: &str, _: &[String]| None;
        let clean = |_: &[String]| Some(false);
        let sel: Vec<usize> = (0..4).collect();
        let d = decide(&p, &sel, "", &prev, &cur(), &opts(), &Git { changed: &none, dirty: &clean });
        assert!(d.iter().all(|d| !reused(d)));
        let same = |_: &str, _: &[String]| Some(vec![]);
        let dirty = |_: &[String]| Some(true);
        let d = decide(&p, &sel, "", &prev, &cur(), &opts(), &Git { changed: &same, dirty: &dirty });
        assert!(d.iter().all(|d| !reused(d)));
    }
}
