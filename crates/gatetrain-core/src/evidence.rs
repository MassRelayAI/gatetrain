//! The one evidence rule (engine-v2 section 7): when may a proof reuse a step result it did not run itself.
//!
//! A *step result* is read from a stamp (`stamp-*.json`, the fact; engine-v2 5.4) that `gatetrain chain` wrote for a green run. The
//! engine verifies a `STEP_CACHED <step> <key> from <run>` line of a proof log against those stamps (T5), and offers the candidates to
//! the chain in an evidence file at launch (T4). The proof host is never trusted to vouch for itself: [`verify`] is the authority.
//!
//! Every refusal is the one existing code, `proof.cached-unverified`, with a `why` from [`WHY`] naming the failing condition of
//! section 7.2. A key must also have been seen in shadow (`STEP_CACHE_SHADOW`) to agree with real runs before it counts (7.4); the
//! shadow lines are kept in `<state>/shadow/shadow.jsonl`, once per (run, step).
use crate::batchcfg::BatchConfig;
use crate::Result;
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The `why` of a refusal, one per condition of 7.2 (index = condition - 1; `policy-never` is the policy value `never`).
pub const WHY: [&str; 9] = ["evidence-sha", "evidence-key", "evidence-class", "evidence-stamp", "evidence-partial", "evidence-stale", "evidence-undeclared", "evidence-unproven", "evidence-missing"];

/// One green step of a stamped run.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step: String,
    pub key: String,
    pub sha: String,
    pub host: String,
    pub host_class: String,
    pub run_id: String,
    /// sha-256 of the stamp file's bytes: the file is written once and never changed (5.4).
    pub stamp_digest: String,
    /// The stamp's steps-file digest.
    pub plan_digest: String,
    pub at: String,
    pub portable: bool,
    pub needs: Vec<String>,
    pub partial: bool,
    /// Every step of the source run and its exit code (for condition 5).
    pub rows: Vec<(String, i64)>,
}

fn hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// sha-256 (hex) of a file's bytes; `None` when it cannot be read.
pub fn file_digest(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| hex(&b))
}

/// Every green, keyed step of every `stamp*.json` in `dirs`. An unreadable or malformed stamp contributes nothing.
pub fn load_results(dirs: &[PathBuf]) -> Vec<StepResult> {
    let mut out = vec![];
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("stamp")))
            .collect();
        files.sort();
        for f in files {
            let Ok(bytes) = std::fs::read(&f) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
            let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
            let rows: Vec<(String, i64)> = v["steps"].as_array().into_iter().flatten().map(|r| (r["name"].as_str().unwrap_or_default().to_string(), r["rc"].as_i64().unwrap_or(-1))).collect();
            let run_id = v["run_id"].as_str().map(String::from).unwrap_or_else(|| f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
            for r in v["steps"].as_array().into_iter().flatten() {
                let key = r["key"].as_str().unwrap_or_default();
                if r["rc"].as_i64() != Some(0) || key.is_empty() {
                    continue;
                }
                out.push(StepResult {
                    step: r["name"].as_str().unwrap_or_default().to_string(),
                    key: key.to_string(),
                    sha: s("sha"),
                    host: s("host"),
                    host_class: s("host_class"),
                    run_id: run_id.clone(),
                    stamp_digest: hex(&bytes),
                    plan_digest: s("digest"),
                    at: s("at"),
                    portable: r["portable"].as_bool().unwrap_or(false),
                    needs: r["needs"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect(),
                    partial: v["partial"].as_bool().unwrap_or(false),
                    rows: rows.clone(),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- shadow (7.4)

fn shadow_file(state_dir: &Path) -> PathBuf {
    state_dir.join("shadow").join("shadow.jsonl")
}

fn shadow_rows(state_dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(shadow_file(state_dir)).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Record the `STEP_CACHE_SHADOW <step> would-hit|miss <key> rc=<n>` lines of a run's log; a (run, step) already recorded is skipped.
/// Returns how many lines were recorded now.
pub fn record_shadow(state_dir: &Path, run: &str, log: &str) -> Result<usize> {
    let re = Regex::new(r"^STEP_CACHE_SHADOW (\S+) (would-hit|miss) (\S+) rc=(-?\d+)").unwrap();
    let have: Vec<(String, String)> = shadow_rows(state_dir).iter().map(|r| (r["run"].as_str().unwrap_or_default().to_string(), r["step"].as_str().unwrap_or_default().to_string())).collect();
    let mut add: Vec<(String, bool, String, i64)> = vec![];
    for l in log.lines() {
        if let Some(m) = re.captures(l) {
            let step = m[1].to_string();
            if have.iter().any(|(r, s)| r == run && *s == step) || add.iter().any(|a| a.0 == step) {
                continue;
            }
            add.push((step, &m[2] == "would-hit", m[3].to_string(), m[4].parse().unwrap_or(-1)));
        }
    }
    if add.is_empty() {
        return Ok(0);
    }
    let f = shadow_file(state_dir);
    std::fs::create_dir_all(f.parent().unwrap())?;
    let mut text = String::new();
    for (step, hit, key, rc) in &add {
        text.push_str(&json!({"run": run, "step": step, "would_hit": hit, "key": key, "rc": rc}).to_string());
        text.push('\n');
    }
    use std::io::Write;
    std::fs::OpenOptions::new().create(true).append(true).open(&f)?.write_all(text.as_bytes())?;
    Ok(add.len())
}

/// `(clean would-hits, defects)` of a key of a step. A defect is a would-hit whose real run was red: the key says "same inputs" while
/// the verdict differs. A miss counts for nothing.
pub fn shadow_status(state_dir: &Path, step: &str, key: &str) -> (usize, usize) {
    let (mut ok, mut bad) = (0, 0);
    for r in shadow_rows(state_dir) {
        if r["step"] == step && r["key"] == key && r["would_hit"] == true {
            if r["rc"].as_i64() == Some(0) {
                ok += 1;
            } else {
                bad += 1;
            }
        }
    }
    (ok, bad)
}

// ---------------------------------------------------------------- the log line

/// `(step, key, source run)` of every `STEP_CACHED` line; the key and the run are `None` for the older bare form `STEP_CACHED <step>`.
pub fn cached_lines(log: &str) -> Vec<(String, Option<String>, Option<String>)> {
    let re = Regex::new(r"^STEP_CACHED (\S+)(?: (\S+) from (\S+))?").unwrap();
    log.lines().filter_map(|l| re.captures(l)).map(|m| (m[1].to_string(), m.get(2).map(|k| k.as_str().to_string()), m.get(3).map(|k| k.as_str().to_string()))).collect()
}

// ---------------------------------------------------------------- the rule

/// What the proof of a sha on a host knows when it judges a result.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub sha: &'a str,
    /// The proof host's name; empty = unknown (a launch-time selection then skips the host rule; T5 never has it empty).
    pub host: &'a str,
    pub host_class: Option<&'a str>,
    /// sha-256 of the steps file of the proof; `None` = unknown, which refuses (`evidence-stamp`).
    pub plan_digest: Option<&'a str>,
    /// "now" in seconds since the epoch, `None` when unreadable.
    pub now: Option<i64>,
    pub cfg: &'a BatchConfig,
    pub state_dir: &'a Path,
}

#[derive(Debug, Clone)]
pub struct Accepted {
    pub source_run: String,
    pub input_key: String,
    pub source_host: String,
    pub stamp_digest: String,
    pub verified_by: &'static str,
}

#[derive(Debug, Clone)]
pub enum Verdict {
    Accepted(Accepted),
    Refused { why: &'static str, detail: String },
}

fn short(s: &str) -> &str {
    &s[..s.len().min(9)]
}

/// The first failing condition of 7.2 for one candidate, as its index into [`WHY`] and the words; `None` when it passes 1 to 8.
/// Condition 9 holds by construction: the candidate was found in the engine's own stamps. Condition 7 (a keyed line has `inputs`) is
/// the caller's: a line with no key never reaches here.
fn first_failure(r: &StepResult, key: &str, c: &Ctx<'_>, parse: &dyn Fn(&str) -> Option<i64>) -> Option<(usize, String)> {
    if r.sha != c.sha {
        return Some((0, format!("the result is for commit {}, the proof is for {}", short(&r.sha), short(c.sha))));
    }
    if r.key != key {
        return Some((1, format!("the result's key is {}, the proof host computed {key}", r.key)));
    }
    if !c.host.is_empty() && r.host != c.host {
        let role = c.cfg.host_roles.get(&r.host);
        let ok = r.portable && !r.host_class.is_empty() && c.host_class == Some(r.host_class.as_str()) && role.is_some_and(|ro| c.cfg.accept_roles.iter().any(|a| a == ro));
        if !ok {
            let why = if !r.portable {
                "the step is not marked portable".to_string()
            } else if c.host_class != Some(r.host_class.as_str()) {
                format!("host class {:?} is not the proof host's {:?}", r.host_class, c.host_class.unwrap_or("unknown"))
            } else {
                format!("the role of {} ({}) is not in [proof] accept_roles", r.host, role.map(String::as_str).unwrap_or("unknown"))
            };
            return Some((2, format!("the result ran on {}, the proof is on {}: {why}", r.host, c.host)));
        }
    }
    match c.plan_digest {
        None => return Some((3, "the proof's steps file is unknown ([chain] steps_file), so the source run cannot be matched to it".into())),
        Some(d) if d != r.plan_digest => return Some((3, format!("the source run used another steps file ({} against {})", short(&r.plan_digest), short(d)))),
        _ => {}
    }
    if r.partial {
        return Some((4, "the source run was partial".into()));
    }
    for n in &r.needs {
        if !r.rows.iter().any(|(name, rc)| name == n && *rc == 0) {
            return Some((4, format!("the step needs {n}, which was not green in the source run")));
        }
    }
    if let Some(max) = c.cfg.max_age_hours {
        let age = c.now.zip(parse(&r.at)).map(|(n, t)| n - t);
        if age.is_none_or(|a| (a as f64) > max * 3600.0) {
            return Some((5, format!("the result is older than max_age_hours = {max} ({})", r.at)));
        }
    }
    let (ok, bad) = shadow_status(c.state_dir, &r.step, key);
    if bad > 0 {
        return Some((7, format!("key {key} has a key-model defect: {bad} would-hit(s) whose real run was red (cache.key-model-defect)")));
    }
    if ok < c.cfg.shadow_min {
        return Some((7, format!("key {key} has {ok} of {} shadow would-hits", c.cfg.shadow_min)));
    }
    None
}

/// Judge the `STEP_CACHED <step> <key>` line of a proof. `parse` reads an ISO time as seconds.
pub fn verify(step: &str, key: &str, results: &[StepResult], c: &Ctx<'_>, parse: &dyn Fn(&str) -> Option<i64>) -> Verdict {
    let mut best: Option<(usize, String)> = None;
    for r in results.iter().filter(|r| r.step == step) {
        match first_failure(r, key, c, parse) {
            None => {
                return Verdict::Accepted(Accepted { source_run: r.run_id.clone(), input_key: key.to_string(), source_host: r.host.clone(), stamp_digest: r.stamp_digest.clone(), verified_by: "stamp" });
            }
            Some((i, why)) if best.as_ref().is_none_or(|(b, _)| i > *b) => best = Some((i, why)),
            Some(_) => {}
        }
    }
    match best {
        Some((i, detail)) => Verdict::Refused { why: WHY[i], detail },
        None => Verdict::Refused { why: "evidence-missing", detail: format!("no stamped result of step {step} was found in the engine's stamp folders ([evidence] stamp_dirs)") },
    }
}

/// The results offered to the chain at launch (T4): green results for `sha` that pass every condition except the key (the chain
/// computes the key on the proof host and compares) and, when the proof host is configured, the host rule. T5 re-verifies each hit.
pub fn select_for_launch(results: &[StepResult], c: &Ctx<'_>, parse: &dyn Fn(&str) -> Option<i64>) -> Vec<StepResult> {
    results.iter().filter(|r| first_failure(r, &r.key, c, parse).is_none()).cloned().collect()
}

/// The evidence file of a launch (`--evidence FILE`, `GATETRAIN_EVIDENCE`): the candidates the engine is willing to accept for `sha`.
pub fn evidence_file(sha: &str, rows: &[StepResult]) -> Value {
    json!({"v": 1, "sha": sha, "results": rows.iter().map(|r| json!({
        "step": r.step, "key": r.key, "sha": r.sha, "host": r.host, "host_class": r.host_class, "run_id": r.run_id,
        "stamp_digest": r.stamp_digest, "at": r.at, "portable": r.portable})).collect::<Vec<_>>()})
}
