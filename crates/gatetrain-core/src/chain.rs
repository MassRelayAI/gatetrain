//! `gatetrain chain`: the step runner (docs/log-format.md is what it prints).
//!
//! A steps file lists `[[step]]`s, each `fast` or `slow`. A tier runs its steps in parallel up to `par`, in file order, a step
//! starting only when every step it `needs` is green. A step whose need went red (or was skipped) is not run: it prints a
//! `SKIPPED <step> (...)` line, which is not a log-format marker and so is ignored by the parser (`STEP_EXIT <step> skipped` would
//! not parse: the exit marker takes an integer). Every step of a started tier runs to the end; `slow` starts only when `fast` was
//! green, or with `keep_going`. Commands go through [`crate::runner`] (its timeout kills the process tree).
use crate::logfmt::Ts;
use crate::runner::{run_cmd_full, shell_command, LocalRunner, Runner};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Fast,
    Slow,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub tier: Tier,
    pub run: Option<String>,
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub needs: Vec<String>,
    pub timeout_s: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub cwd: Option<String>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

pub struct Opts {
    pub par: usize,
    pub out: PathBuf,
    pub keep_going: bool,
    pub only: Vec<String>,
    pub skip: Vec<String>,
}

pub struct Outcome {
    /// every red step, in the order they failed: (name, rc)
    pub reds: Vec<(String, i64)>,
    /// the stamp path, or why none was written
    pub stamp: Result<PathBuf, String>,
}

/// Parse and validate a steps file. Err = config error (exit 2).
pub fn load(text: &str) -> Result<Plan, String> {
    let plan: Plan = toml::from_str(text).map_err(|e| format!("steps file: {e}"))?;
    if plan.steps.is_empty() {
        return Err("steps file: no [[step]]".into());
    }
    let mut seen: Vec<&Step> = Vec::new();
    for s in &plan.steps {
        if s.name.is_empty() || !s.name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
            return Err(format!("step name {:?}: use letters, digits, . _ - only", s.name));
        }
        if seen.iter().any(|p| p.name == s.name) {
            return Err(format!("duplicate step name {:?}", s.name));
        }
        match (&s.run, &s.argv) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => return Err(format!("step {:?}: give exactly one of run and argv", s.name)),
        }
        if s.argv.as_ref().is_some_and(|a| a.is_empty()) {
            return Err(format!("step {:?}: argv is empty", s.name));
        }
        if s.timeout_s.is_some_and(|t| t <= 0.0 || !t.is_finite()) {
            return Err(format!("step {:?}: timeout_s must be positive", s.name));
        }
        seen.push(s);
    }
    for s in &plan.steps {
        for n in &s.needs {
            let Some(d) = plan.steps.iter().find(|x| &x.name == n) else {
                return Err(format!("step {:?} needs {n:?}, which is no step", s.name));
            };
            if s.tier == Tier::Fast && d.tier == Tier::Slow {
                return Err(format!("fast step {:?} needs slow step {n:?}", s.name));
            }
        }
    }
    // cycle: repeatedly settle steps whose needs are all settled
    let mut done: HashSet<&str> = HashSet::new();
    while done.len() < plan.steps.len() {
        let before = done.len();
        for s in &plan.steps {
            if !done.contains(s.name.as_str()) && s.needs.iter().all(|n| done.contains(n.as_str())) {
                done.insert(&s.name);
            }
        }
        if done.len() == before {
            let left: Vec<&str> = plan.steps.iter().map(|s| s.name.as_str()).filter(|n| !done.contains(n)).collect();
            return Err(format!("needs cycle among: {}", left.join(", ")));
        }
    }
    Ok(plan)
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Pending,
    Running,
    Green,
    Red,
    Skipped,
}

struct Done {
    idx: usize,
    rc: i64,
    secs: f64,
}

fn exec(step: &Step, cwd: &Path, out: &Path) -> (i64, f64) {
    let t0 = Instant::now();
    let mut c = match (&step.argv, &step.run) {
        (Some(a), _) => {
            let mut c = Command::new(&a[0]);
            c.args(&a[1..]);
            c
        }
        // one stream: the group's stderr is folded into stdout
        (None, Some(r)) => shell_command(&format!("( {r} ) 2>&1")),
        _ => unreachable!("load() refuses a step with neither"),
    };
    c.current_dir(cwd);
    let limit = step.timeout_s.unwrap_or(365.0 * 86400.0); // the runner's own default is 60 s
    let (rc, mut text) = match run_cmd_full(c, Some(limit)) {
        Ok(Some(o)) => (i64::from(o.code), if o.stderr.is_empty() { o.stdout } else { format!("{}{}", o.stdout, o.stderr) }),
        Ok(None) => (124, format!("chain: timed out after {limit} s; the process tree was killed (its output so far is lost)\n")),
        Err(e) => (127, format!("chain: cannot start: {e}\n")),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if let Err(e) = std::fs::write(out.join(format!("{}.full", step.name)), text) {
        eprintln!("chain: cannot write {}.full: {e}", step.name);
    }
    (rc, t0.elapsed().as_secs_f64())
}

fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let o = LocalRunner.run_argv(cwd, &argv).ok()?;
    (o.code == 0).then(|| o.stdout.trim().to_string())
}

fn host() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty() && !v.contains(char::is_whitespace)))
        .unwrap_or_else(|| "unknown".into())
}

/// Run the plan, calling `emit` once per log-format line. Err = config error (a name in only/skip that is no step, an unusable out folder).
pub fn run(plan: &Plan, digest: &str, o: &Opts, emit: &mut dyn FnMut(&str)) -> Result<Outcome, String> {
    for n in o.only.iter().chain(&o.skip) {
        if !plan.steps.iter().any(|s| &s.name == n) {
            return Err(format!("--only/--skip names {n:?}, which is no step"));
        }
    }
    std::fs::create_dir_all(&o.out).map_err(|e| format!("cannot create {}: {e}", o.out.display()))?;
    let cwd = PathBuf::from(plan.cwd.clone().unwrap_or_else(|| ".".into()));
    let selected: Vec<usize> = (0..plan.steps.len())
        .filter(|&i| {
            let n = &plan.steps[i].name;
            (o.only.is_empty() || o.only.contains(n)) && !o.skip.contains(n)
        })
        .collect();
    let par = o.par.max(1);

    // the tree must be clean before AND after; the out folder is not part of the tree it proves
    let out_abs = o.out.canonicalize().unwrap_or_else(|_| o.out.clone());
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
    let mut porcelain: Vec<String> = vec!["status".into(), "--porcelain".into(), "--".into(), ".".into()];
    if let Ok(rel) = out_abs.strip_prefix(&cwd_abs) {
        if !rel.as_os_str().is_empty() {
            porcelain.push(format!(":(exclude){}", rel.to_string_lossy().replace('\\', "/")));
        }
    }
    let porcelain: Vec<&str> = porcelain.iter().map(String::as_str).collect();
    let sha = git_out(&cwd, &["rev-parse", "HEAD"]);
    let dirty_before = sha.as_ref().map(|_| git_out(&cwd, &porcelain).unwrap_or_else(|| "git status failed".into()));
    if let Some(sha) = &sha {
        let branch = git_out(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into());
        emit(&format!("branch {} at {} on {}", branch.replace(char::is_whitespace, "_"), sha, host()));
    }
    emit("CHAIN_MODE preflight");
    emit(&format!("CHAIN_PARALLEL {par}"));

    let mut state = vec![St::Pending; plan.steps.len()];
    let mut times: Vec<(usize, i64, f64)> = Vec::new();
    let mut reds: Vec<(String, i64)> = Vec::new();
    let t_run = Instant::now();
    let mut fast_red = false;
    for tier in [Tier::Fast, Tier::Slow] {
        let mine: Vec<usize> = selected.iter().copied().filter(|&i| plan.steps[i].tier == tier).collect();
        if mine.is_empty() {
            continue;
        }
        if tier == Tier::Slow && fast_red && !o.keep_going {
            for &i in &mine {
                state[i] = St::Skipped;
                emit(&format!("SKIPPED {} (a fast step is red; --keep-going runs slow anyway)", plan.steps[i].name));
            }
            continue;
        }
        let (tx, rx) = mpsc::channel::<Done>();
        let mut running = 0usize;
        loop {
            // settle skips first: a pending step whose need is red or skipped never runs
            let mut changed = true;
            while changed {
                changed = false;
                for &i in &mine {
                    if state[i] != St::Pending {
                        continue;
                    }
                    let bad = plan.steps[i]
                        .needs
                        .iter()
                        .find(|n| plan.steps.iter().position(|s| &s.name == *n).is_some_and(|j| matches!(state[j], St::Red | St::Skipped)));
                    if let Some(n) = bad {
                        state[i] = St::Skipped;
                        emit(&format!("SKIPPED {} (needs {n}, which is not green)", plan.steps[i].name));
                        changed = true;
                    }
                }
            }
            // start what is ready, in file order
            for &i in &mine {
                if running >= par {
                    break;
                }
                if state[i] != St::Pending {
                    continue;
                }
                let ready = plan.steps[i].needs.iter().all(|n| match plan.steps.iter().position(|s| &s.name == n) {
                    // a need outside --only/--skip is not waited for
                    Some(j) => state[j] == St::Green || !selected.contains(&j),
                    None => true,
                });
                if !ready {
                    continue;
                }
                state[i] = St::Running;
                running += 1;
                emit(&format!("STEP_START {} {}", plan.steps[i].name, Ts::now_utc().iso()));
                let (step, cwd, out, tx) = (plan.steps[i].clone(), cwd.clone(), o.out.clone(), tx.clone());
                std::thread::spawn(move || {
                    let (rc, secs) = exec(&step, &cwd, &out);
                    let _ = tx.send(Done { idx: i, rc, secs });
                });
            }
            if running == 0 {
                break;
            }
            let d = rx.recv_timeout(Duration::from_secs(3600 * 24 * 366)).map_err(|e| format!("chain: lost a worker: {e}"))?;
            running -= 1;
            let name = &plan.steps[d.idx].name;
            emit(&format!("STEP_TIME {name} {:.1}s", d.secs));
            emit(&format!("STEP_EXIT {name} {}", d.rc));
            times.push((d.idx, d.rc, d.secs));
            if d.rc == 0 {
                state[d.idx] = St::Green;
            } else {
                state[d.idx] = St::Red;
                reds.push((name.clone(), d.rc));
                if tier == Tier::Fast {
                    fast_red = true;
                }
                emit(&format!("RED_NOW {name} rc={} at {} (full output: {name}.full)", d.rc, Ts::now_utc().iso()));
            }
        }
    }
    emit(&format!(
        "RED_SUMMARY: {}",
        if reds.is_empty() { "none".to_string() } else { summary_in_file_order(plan, &reds) }
    ));
    emit("GATES_DONE");

    let stamp = if !reds.is_empty() {
        Err("the run is red".to_string())
    } else if let (Some(sha), Some(before)) = (&sha, &dirty_before) {
        let after = git_out(&cwd, &porcelain).unwrap_or_else(|| "git status failed".into());
        if !before.is_empty() || !after.is_empty() {
            Err("the tree is dirty (git status --porcelain is not empty): no stamp written".to_string())
        } else {
            let steps: Vec<serde_json::Value> = times
                .iter()
                .map(|&(i, rc, secs)| {
                    let tier = if plan.steps[i].tier == Tier::Fast { "fast" } else { "slow" };
                    serde_json::json!({"name": plan.steps[i].name, "tier": tier, "rc": rc, "seconds": (secs * 10.0).round() / 10.0})
                })
                .collect();
            let body = serde_json::json!({
                "sha": sha, "host": host(), "at": Ts::now_utc().iso(), "mode": "preflight", "par": par,
                "total_seconds": (t_run.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                "digest": digest, "steps": steps,
            });
            let p = o.out.join(format!("stamp-{sha}.json"));
            std::fs::write(&p, serde_json::to_string_pretty(&body).unwrap_or_default() + "\n").map_err(|e| format!("cannot write {}: {e}", p.display()))?;
            Ok(p)
        }
    } else {
        Err("not a git repository: no stamp written".to_string())
    };
    Ok(Outcome { reds, stamp })
}

/// The digest a stamp carries: sha-256 of the steps file, so a stamp says which checks produced it.
pub fn digest_of(steps_text: &str) -> String {
    Sha256::digest(steps_text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// `RED_SUMMARY` lists reds in steps-file order, so two runs of the same failure print the same line (RED_NOW keeps completion order).
fn summary_in_file_order(plan: &Plan, reds: &[(String, i64)]) -> String {
    let mut v: Vec<(usize, &(String, i64))> = reds.iter().map(|r| (plan.steps.iter().position(|s| s.name == r.0).unwrap_or(usize::MAX), r)).collect();
    v.sort_by_key(|(i, _)| *i);
    v.iter().map(|(_, (n, rc))| format!("{n}(rc={rc})")).collect::<Vec<_>>().join(" ")
}
