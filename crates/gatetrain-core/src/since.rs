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
    /// `--db`: the ledger the previous run was recorded in; its row must agree with the result file
    pub db: Option<PathBuf>,
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
    /// the declared inputs held ignored, untracked or modified files when the run ended: git cannot vouch for them, so the step is never reused from this result
    #[serde(default)]
    pub unclean: bool,
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
    /// when the run ended (ISO 8601, UTC): the newest result of a commit wins
    #[serde(default)]
    pub at: String,
    /// sha-256 of the run's log (the ledger keeps the same value)
    #[serde(default)]
    pub log_sha256: String,
    /// sha-256 over the fields above: a result edited afterwards no longer verifies
    #[serde(default)]
    pub seal: String,
    pub steps: Vec<StepResult>,
}

impl RunResult {
    pub fn compute_seal(&self) -> String {
        let v = serde_json::json!([self.v, self.run_id, self.sha, self.mode, self.tiers, self.toolchain, self.env, self.dirty, self.partial, self.at, self.log_sha256, self.steps]);
        Sha256::digest(v.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Run(String),
    Reuse { run: String, sha: String, age: u32 },
}

/// What the run being planned is.
pub struct Cur<'a> {
    pub mode: &'a str,
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
    /// the tracked files under the pathspecs: how many, and the tracked symlinks among them as (path, target); None = git could not say
    pub tracked: &'a dyn Fn(&[String]) -> Option<Tracked>,
}

#[derive(Debug, Clone, Default)]
pub struct Tracked {
    pub files: usize,
    pub links: Vec<(String, String)>,
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

/// Read and verify one result file: it must parse, be version 1, name `want` (when given), carry a verifying seal and (with `db`) agree with the ledger. The second value is the
/// ledger's own row number of the run (0 without a ledger): the order of runs is the ledger's, never the file's.
fn read_result(path: &Path, want: Option<&str>, db: Option<&Path>) -> Result<(RunResult, i64), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("no previous result {}: {e}", path.display()))?;
    let r: RunResult = serde_json::from_str(&text).map_err(|e| format!("{} is not a result file: {e}", path.display()))?;
    if r.v != 1 {
        return Err(format!("{}: result version {} is not understood", path.display(), r.v));
    }
    if let Some(w) = want.filter(|w| *w != r.sha) {
        return Err(format!("{} names commit {} but --since resolved to {}", path.display(), short(&r.sha), short(w)));
    }
    if r.seal != r.compute_seal() {
        return Err(format!("{}: its seal does not verify (the file was edited or is from another version)", path.display()));
    }
    let order = match db {
        Some(db) => check_ledger(db, &r)?,
        None => 0,
    };
    Ok((r, order))
}

/// Read the previous result: `prev` is a path to a result file, or a commit: then every `result-<sha>-*.json` of `out` that verifies is read (a tier run and a
/// whole run of one commit each leave one) and the results of one environment (mode, fingerprints, clean, whole) are merged step by step. The newest run in the
/// LEDGER's order is the base; with a ledger, a step whose newest ledger row (any run of the commit) is red is red here. Refused (Err) when no file verifies.
pub fn load_prev(out: &Path, prev: &str, sha_of: &dyn Fn(&str) -> Option<String>, tiers: &str, db: Option<&Path>) -> Result<RunResult, String> {
    let p = Path::new(prev);
    if p.is_file() {
        let (mut r, _) = read_result(p, None, db)?;
        if let Some(db) = db {
            apply_ledger(db, &mut r)?;
        }
        return Ok(r);
    }
    let full = sha_of(prev).ok_or_else(|| format!("--since {prev:?} is neither a result file nor a commit of this repository"))?;
    let own = result_path(out, &full, tiers);
    let mut files: Vec<PathBuf> = std::fs::read_dir(out)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&format!("result-{full}-")) && n.to_string_lossy().ends_with(".json")))
        .collect();
    files.sort();
    let mut first_err = format!("no previous result {}", own.display());
    let mut ok: Vec<(i64, RunResult)> = vec![];
    for f in &files {
        match read_result(f, Some(&full), db) {
            Ok(x) => ok.push((x.1, x.0)),
            Err(e) => first_err = e,
        }
    }
    // the newest run (by the LEDGER's row order; the file's own `at` is only a tie-break without a ledger, anyone can re-seal it) is the base; an older run of the same
    // environment only fills in steps the newest never ran. A step the newest run has keeps the newest result, green or red: an older green never overrides it
    ok.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.at.cmp(&a.1.at)));
    let mut it = ok.into_iter().map(|x| x.1);
    let Some(mut base) = it.next() else { return Err(first_err) };
    for o in it {
        if (o.mode.as_str(), o.toolchain.as_str(), o.env.as_str(), o.dirty, o.partial) != (base.mode.as_str(), base.toolchain.as_str(), base.env.as_str(), base.dirty, base.partial) {
            continue;
        }
        for s in o.steps {
            if !base.steps.iter().any(|b| b.name == s.name) {
                base.steps.push(s);
            }
        }
    }
    if let Some(db) = db {
        apply_ledger(db, &mut base)?;
    }
    base.tiers = tiers.to_string();
    Ok(base)
}

/// With `--db`: the ledger must hold this run, for this commit, with this log digest and the same exit code for every step the result lists. Returns the run's ledger row number.
fn check_ledger(db: &Path, r: &RunResult) -> Result<i64, String> {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("cannot read ledger {}: {e}", db.display()))?;
    let row: Option<(i64, Option<String>, Option<String>)> = c
        .query_row("SELECT rowid, commit_sha, log_sha256 FROM runs WHERE id = ?", [&r.run_id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))
        .ok();
    let Some((rowid, sha, log)) = row else { return Err(format!("run {} is not in the ledger {}", r.run_id, db.display())) };
    if sha.as_deref() != Some(r.sha.as_str()) || log.as_deref() != Some(r.log_sha256.as_str()) {
        return Err(format!("the ledger's row for run {} disagrees with the result file (commit or log digest)", r.run_id));
    }
    for s in &r.steps {
        let rc: Option<i64> = c.query_row("SELECT exit_code FROM steps WHERE run_id = ? AND name = ?", [&r.run_id, &s.name], |x| x.get(0)).ok();
        if rc != s.rc {
            return Err(format!("the ledger has step {} of run {} at rc {rc:?}, the result file says {:?}", s.name, r.run_id, s.rc));
        }
    }
    Ok(rowid)
}

/// The ledger decides which result of a step is the newest: the last row, in the ledger's own order, of any run of this commit that ran the step. When that row is not green the
/// step is red here, whatever an older result file (replayed and re-sealed with a newer `at`) says.
fn apply_ledger(db: &Path, r: &mut RunResult) -> Result<(), String> {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| format!("cannot read ledger {}: {e}", db.display()))?;
    for s in &mut r.steps {
        let rc: Option<i64> = c
            .query_row("SELECT s.exit_code FROM steps s JOIN runs u ON u.id = s.run_id WHERE u.commit_sha = ? AND s.name = ? ORDER BY u.rowid DESC LIMIT 1", [&r.sha, &s.name], |x| x.get(0))
            .ok();
        if let Some(rc) = rc.filter(|rc| *rc != 0) {
            s.rc = Some(rc);
        }
    }
    Ok(())
}

/// Why nothing at all may be reused from `prev` (None = the run-level rules pass).
pub fn run_level_refusal(prev: &RunResult, cur: &Cur, git: &Git) -> Option<String> {
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
        // and a step that runs pulls in everything it needs: its output may be read from disk, so a need is never taken from an earlier run while a step reading it runs
        for &i in selected {
            if !matches!(d[i], Decision::Run(_)) {
                continue;
            }
            for n in &plan.steps[i].needs {
                if let Some(j) = plan.steps.iter().position(|s| &s.name == n).filter(|j| selected.contains(j) && matches!(d[*j], Decision::Reuse { .. })) {
                    d[j] = Decision::Run(format!("{} runs and needs it", plan.steps[i].name));
                    changed = true;
                }
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
    if p.unclean {
        return run("its inputs held ignored, untracked or modified files when it last ran");
    }
    match (git.tracked)(&s.inputs) {
        None => return run("git cannot list its inputs"),
        Some(t) if t.files == 0 => return run("its inputs match no tracked file"),
        Some(t) => {
            if let Some((l, tg)) = t.links.iter().find(|(l, tg)| link_escapes(l, tg, &s.inputs)) {
                return run(&format!("tracked symlink {l} points outside its inputs ({tg})"));
            }
        }
    }
    match (git.dirty)(&s.inputs) {
        None => return run("git cannot say whether its inputs are clean"),
        Some(true) => return run("its inputs are dirty (modified, untracked or ignored files)"),
        Some(false) => {}
    }
    Decision::Reuse { run: p.origin_run.clone(), sha: p.origin_sha.clone(), age: p.age + 1 }
}

fn short(s: &str) -> &str {
    &s[..s.len().min(7)]
}

/// The real git behind [`Git`], run in the plan's directory.
pub fn git_changed(cwd: &Path, prev_sha: &str, to: &str, specs: &[String]) -> Option<Vec<String>> {
    let mut a: Vec<&str> = vec!["diff", "--name-only", prev_sha, to, "--"];
    if specs.is_empty() {
        a.push(":/");
    }
    a.extend(specs.iter().map(String::as_str));
    git_out(cwd, &a).map(|t| t.lines().map(str::to_string).collect())
}

/// The directory part of each pathspec before its first glob character: what the step declares it reads (empty = the whole tree).
fn input_roots(specs: &[String]) -> Vec<String> {
    specs
        .iter()
        .map(|s| {
            let s = s.trim_start_matches("./");
            let cut = s.find(['*', '?', '[']).unwrap_or(s.len());
            let head = &s[..cut];
            // a literal file or the part of a glob before its last `/`
            if cut == s.len() { s.to_string() } else { head[..head.rfind('/').map_or(0, |i| i + 1)].to_string() }
        })
        .collect()
}

/// Whether a tracked symlink at `link` (relative to the plan's directory) pointing at `target` reaches outside every input root.
pub fn link_escapes(link: &str, target: &str, specs: &[String]) -> bool {
    if target.starts_with('/') {
        return true;
    }
    let mut parts: Vec<&str> = link.split('/').collect();
    parts.pop();
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return true;
                }
            }
            c => parts.push(c),
        }
    }
    let resolved = parts.join("/");
    !input_roots(specs).iter().any(|r| r.is_empty() || resolved == r.trim_end_matches('/') || resolved.starts_with(&format!("{}/", r.trim_end_matches('/'))))
}

/// Tracked files under the pathspecs and the tracked symlinks among them with their targets.
pub fn git_tracked(cwd: &Path, specs: &[String]) -> Option<Tracked> {
    let mut a: Vec<&str> = vec!["ls-files", "-s", "--"];
    a.extend(specs.iter().map(String::as_str));
    let ls = git_out(cwd, &a)?;
    let mut t = Tracked::default();
    for l in ls.lines() {
        let Some((meta, path)) = l.split_once('\t') else { continue };
        t.files += 1;
        let mut m = meta.split_whitespace();
        if let (Some("120000"), Some(blob)) = (m.next(), m.next()) {
            let target = git_out(cwd, &["cat-file", "blob", blob])?;
            t.links.push((path.to_string(), target));
        }
    }
    Some(t)
}

pub fn git_dirty(cwd: &Path, specs: &[String]) -> Option<bool> {
    let mut a: Vec<&str> = vec!["status", "--porcelain", "--untracked-files=all", "--ignored=matching", "--"];
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
            at: String::new(),
            log_sha256: String::new(),
            seal: String::new(),
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
                    unclean: false,
                })
                .collect(),
        }
    }

    fn cur() -> Cur<'static> {
        Cur { mode: "preflight", toolchain: "tc", env: "env" }
    }

    fn opts() -> SinceOpts {
        SinceOpts { prev: "aaaaaaaa".into(), max_age: 5, full: false, db: None }
    }

    /// `touched`: the paths git says differ between the commits.
    fn go(p: &Plan, prev: &RunResult, c: &Cur, o: &SinceOpts, touched: &[&str]) -> Vec<Decision> {
        let t: Vec<String> = touched.iter().map(|s| s.to_string()).collect();
        let changed = move |_: &str, specs: &[String]| Some(t.iter().filter(|f| specs.is_empty() || specs.iter().any(|s| f.starts_with(s.trim_end_matches("**")))).cloned().collect::<Vec<_>>());
        let dirty = |_: &[String]| Some(false);
        let tracked = |_: &[String]| Some(Tracked { files: 1, links: vec![] });
        let g = Git { changed: &changed, dirty: &dirty, tracked: &tracked };
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
    fn another_mode_a_partial_or_dirty_previous_run_is_refused() {
        let p = plan();
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
    fn a_need_is_never_reused_while_a_step_that_reads_it_runs() {
        let p = plan();
        let mut prev = prev_for(&p);
        prev.steps[2].def = "changed".into(); // e2e runs; it needs unit, whose own inputs did not change
        let d = go(&p, &prev, &cur(), &opts(), &[]);
        assert!(matches!(&d[2], Decision::Run(_)));
        assert!(matches!(&d[1], Decision::Run(w) if w.contains("e2e runs and needs it")), "{:?}", d[1]);
        assert!(reused(&d[0]), "an unrelated step stays reused");
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
        let trk = |_: &[String]| Some(Tracked { files: 1, links: vec![] });
        let none = |_: &str, _: &[String]| None;
        let clean = |_: &[String]| Some(false);
        let sel: Vec<usize> = (0..4).collect();
        let d = decide(&p, &sel, "", &prev, &cur(), &opts(), &Git { changed: &none, dirty: &clean, tracked: &trk });
        assert!(d.iter().all(|d| !reused(d)));
        let same = |_: &str, _: &[String]| Some(vec![]);
        let dirty = |_: &[String]| Some(true);
        let d = decide(&p, &sel, "", &prev, &cur(), &opts(), &Git { changed: &same, dirty: &dirty, tracked: &trk });
        assert!(d.iter().all(|d| !reused(d)));
    }

    #[test]
    fn inputs_matching_no_tracked_file_or_holding_unclean_files_or_a_symlink_out_run() {
        let p = plan();
        let sel: Vec<usize> = (0..4).collect();
        let same = |_: &str, _: &[String]| Some(vec![]);
        let clean = |_: &[String]| Some(false);
        let none = |_: &[String]| Some(Tracked { files: 0, links: vec![] });
        let d = decide(&p, &sel, "", &prev_for(&p), &cur(), &opts(), &Git { changed: &same, dirty: &clean, tracked: &none });
        assert!(matches!(&d[0], Decision::Run(w) if w.contains("match no tracked file")), "{:?}", d[0]);
        let link = |_: &[String]| Some(Tracked { files: 2, links: vec![("src/l".into(), "../shared".into())] });
        let d = decide(&p, &sel, "", &prev_for(&p), &cur(), &opts(), &Git { changed: &same, dirty: &clean, tracked: &link });
        assert!(matches!(&d[0], Decision::Run(w) if w.contains("symlink")), "{:?}", d[0]);
        let mut prev = prev_for(&p);
        prev.steps[0].unclean = true;
        let ok = |_: &[String]| Some(Tracked { files: 1, links: vec![] });
        let d = decide(&p, &sel, "", &prev, &cur(), &opts(), &Git { changed: &same, dirty: &clean, tracked: &ok });
        assert!(matches!(&d[0], Decision::Run(w) if w.contains("ignored")), "{:?}", d[0]);
    }

    #[test]
    fn a_symlink_inside_the_inputs_is_fine_one_pointing_out_is_not() {
        let specs = vec!["a/**".to_string()];
        assert!(!link_escapes("a/l", "x/y", &specs));
        assert!(!link_escapes("a/b/l", "../c", &specs));
        assert!(link_escapes("a/l", "../shared", &specs));
        assert!(link_escapes("a/l", "/etc/hosts", &specs));
        assert!(!link_escapes("a/l", "../anything", &["**".to_string()]));
    }

    #[test]
    fn a_result_file_that_was_edited_or_is_for_another_commit_is_refused() {
        let p = plan();
        let d = std::env::temp_dir().join(format!("gt-since-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut r = prev_for(&p);
        r.seal = r.compute_seal();
        let f = result_path(&d, &r.sha, "fast+slow");
        std::fs::write(&f, serde_json::to_string(&r).unwrap()).unwrap();
        let sha_of = |s: &str| Some(s.to_string());
        assert!(load_prev(&d, "aaaaaaaa", &sha_of, "fast+slow", None).is_ok());
        r.steps[0].rc = Some(0);
        r.steps[1].def = "tampered".into();
        std::fs::write(&f, serde_json::to_string(&r).unwrap()).unwrap();
        assert!(load_prev(&d, "aaaaaaaa", &sha_of, "fast+slow", None).unwrap_err().contains("seal"));
        r.seal = r.compute_seal();
        std::fs::write(&f, serde_json::to_string(&r).unwrap()).unwrap();
        std::fs::copy(&f, result_path(&d, "bbbbbbbb", "fast+slow")).unwrap();
        let other2 = |_: &str| Some("bbbbbbbb".to_string());
        assert!(load_prev(&d, "x", &other2, "fast+slow", None).unwrap_err().contains("resolved to"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
