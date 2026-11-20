//! `gatetrain chain`: the step runner (docs/log-format.md is what it prints).
//!
//! A steps file lists `[[step]]`s, each `fast` or `slow`. A tier runs its steps in parallel up to `par`, in file order, a step
//! starting only when every step it `needs` is green. A step whose need went red (or was skipped) is not run: it prints
//! `STEP_SKIPPED <step> <reason>` (parsed into `ParsedLog::skipped`, never into the steps that ran). Every step of a started tier
//! runs to the end; `slow` starts only when `fast` was green, or with `keep_going`. A step's stdout and stderr go straight to
//! `<out>/<step>.full` as they are written ([`crate::runner::run_to_file`]: nothing held in memory, the file can be followed, a
//! timeout keeps the partial output and kills the whole process tree).
//!
//! Engine v2 (docs/design/engine-v2.md): `shards = N` runs a step's command N times at once and reports ONE logical step plus
//! `STEP_SHARD` lines; the finished run is returned as a [`RunRec`] so `chain --db` can write it into the ledger.
use crate::logfmt::Ts;
use crate::runner::{run_to_file, shell_command, LocalRunner, Runner};
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
    /// git pathspecs (relative to the plan's cwd) this step reads; with `--cache` an unchanged set means the step is not re-run. No inputs = never cached.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// mixed into the cache key: change it to invalidate the step's entries (a tool version, an env the inputs do not show)
    pub cache_salt: Option<String>,
    /// 1 to 64: run the command this many times at once, `{shard}` (1-based) and `{shards}` replaced in `run`/`argv`; reported as ONE logical step
    pub shards: Option<u32>,
    /// argv run once after every shard is green; `GATETRAIN_SHARD_OUT` names the shard output files (an OS path list). Non-zero makes the step red.
    pub shard_check: Option<Vec<String>>,
    /// the command reads `GATETRAIN_SHARD` / `GATETRAIN_SHARDS` itself, so it needs no `{shard}` placeholder
    #[serde(default)]
    pub shard_env: bool,
    /// CPU cores this step needs (1 or more; no `cores` counts as 1). With a core budget the scheduler never starts it unless that many are free; on Linux it runs under `taskset`
    pub cores: Option<u32>,
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
    /// the step cache folder (the caller's choice, one per host); None = no caching
    pub cache: Option<PathBuf>,
    /// steps that run even when the cache has them
    pub no_cache_for: Vec<String>,
    /// what the run claims to be: `preflight` (the default), `lane` or `full`. The caller refuses `full` together with only/skip.
    pub mode: String,
    /// the host's name in the log, the stamp and the ledger; None = the environment, then the OS ([`host_name`])
    pub host: Option<String>,
    /// sample the host every this many seconds into `<out>/samples.jsonl` (engine v2 E7); 0 = off
    pub samples: u64,
    /// expected seconds per step name (the ledger's medians): Some = start the longest ready step first, None = file order
    pub expected: Option<std::collections::HashMap<String, f64>>,
    /// the core budget (at least 1). It gates steps only when some selected step declares `cores`
    pub cores: usize,
    /// D446: re-run a red step ONCE on the same sha when `diagnose` classes it `flake-*`/`infra-*` or a live quarantine row names its test.
    /// Off by default: with it off the run is byte for byte what it was before.
    pub retry_flakes: bool,
    /// the tracked quarantine file (`test,step,finding,added,expires`); read only with `retry_flakes`
    pub quarantine: Option<PathBuf>,
}

/// At most this many steps of one run are re-run; a further flaky step stays red.
pub const MAX_RETRIED_STEPS: usize = 2;

/// A step that was red, re-run once, and what the second attempt gave (the stamp's `retried` list).
#[derive(Debug, Clone)]
pub struct RetryRec {
    pub name: String,
    pub first_rc: i64,
    pub final_rc: i64,
    pub class: String,
    pub via: String,
    pub test: String,
}

/// The first `n` contiguous free CPUs (`busy[i]` = CPU i is reserved), or None when no run of `n` is free.
pub fn alloc_cpus(busy: &[bool], n: usize) -> Option<Vec<usize>> {
    if n == 0 || n > busy.len() {
        return None;
    }
    (0..=busy.len() - n).find(|&s| busy[s..s + n].iter().all(|b| !b)).map(|s| (s..s + n).collect())
}

/// `0-2,5`: the list form `taskset -c` takes.
pub fn cpu_list(cpus: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < cpus.len() {
        let mut j = i;
        while j + 1 < cpus.len() && cpus[j + 1] == cpus[j] + 1 {
            j += 1;
        }
        out.push(if i == j { cpus[i].to_string() } else { format!("{}-{}", cpus[i], cpus[j]) });
        i = j + 1;
    }
    out.join(",")
}

/// The CPUs no pinned step holds, as a `taskset -c` list; None when nothing is reserved (no need to confine) or nothing is left.
pub fn free_cpu_list(busy: &[bool]) -> Option<String> {
    let free: Vec<usize> = (0..busy.len()).filter(|&i| !busy[i]).collect();
    (free.len() < busy.len() && !free.is_empty()).then(|| cpu_list(&free))
}

/// Pinning is Linux only and needs `taskset`; elsewhere the budget alone applies.
fn can_pin() -> bool {
    cfg!(target_os = "linux") && Command::new("taskset").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// The sampler of a run (E7): one JSON line per interval, appended to `<out>/samples.jsonl`; no thread, no daemon. It is driven by the
/// loop that already waits for a step to end, and ends with the run.
struct SampleSink {
    s: crate::hw::Sampler,
    last: Instant,
    every: Duration,
    file: PathBuf,
    host: String,
    wrote_header: bool,
}

impl SampleSink {
    fn new(out: &Path, host: &str, secs: u64) -> SampleSink {
        let mut s = crate::hw::Sampler::host();
        s.sample(0.0); // the baseline the first delta is taken against
        SampleSink { s, last: Instant::now(), every: Duration::from_secs(secs), file: out.join("samples.jsonl"), host: host.to_string(), wrote_header: false }
    }

    fn tick(&mut self, running_steps: usize) {
        use std::io::Write;
        let elapsed = self.last.elapsed().as_secs_f64();
        self.last = Instant::now();
        let r = self.s.sample(elapsed);
        let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) else { return };
        if !self.wrote_header {
            let _ = writeln!(f, "{}", r.unknown_json_line());
            self.wrote_header = true;
        }
        let _ = writeln!(f, "{}", r.to_json_line(&self.host, "", Some(running_steps), &Ts::now_utc().iso()));
    }

    fn maybe(&mut self, running_steps: usize) {
        if self.last.elapsed() >= self.every {
            self.tick(running_steps);
        }
    }
}

/// One logical step of a finished run, as the ledger keeps it.
#[derive(Debug, Clone)]
pub struct StepRec {
    pub name: String,
    pub tier: &'static str,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub rc: i64,
    pub secs: f64,
    pub cached: bool,
    /// for a cached step: the commit its cache entry was written at
    pub cached_from: Option<String>,
    pub key: Option<String>,
    pub shards: i64,
    pub log_ref: String,
}

/// One shard of a sharded step.
#[derive(Debug, Clone)]
pub struct ShardRec {
    pub step: String,
    pub shard: i64,
    pub of: i64,
    pub started_at: String,
    pub secs: f64,
    pub rc: i64,
    pub log_ref: String,
}

/// Everything `chain --db` writes into the ledger (engine v2, E1).
#[derive(Debug, Clone)]
pub struct RunRec {
    pub sha: Option<String>,
    pub branch: Option<String>,
    pub host: String,
    pub mode: String,
    pub par: usize,
    pub started_at: String,
    pub ended_at: String,
    pub outcome: &'static str,
    pub plan_digest: String,
    /// sha-256 of every line `run` emitted, each followed by a newline: the digest of the log a caller captured
    pub log_sha256: String,
    pub steps: Vec<StepRec>,
    pub shards: Vec<ShardRec>,
    pub skips: Vec<(String, String)>,
    /// (digest = sha-256 of the stamp file's bytes, the file's text); None when no stamp was written
    pub stamp: Option<(String, String)>,
    /// steps that were re-run once (`--retry-flakes`), in the order they finished; empty otherwise
    pub retries: Vec<RetryRec>,
}

pub struct Outcome {
    /// every red step, in the order they failed: (name, rc)
    pub reds: Vec<(String, i64)>,
    /// the stamp path, or why none was written
    pub stamp: Result<PathBuf, String>,
    pub record: RunRec,
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
        check_shards(s)?;
        if s.cores == Some(0) {
            return Err(format!("step {:?}: cores must be at least 1", s.name));
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

fn check_shards(s: &Step) -> Result<(), String> {
    let has_ph = s.run.iter().chain(s.argv.iter().flatten()).any(|t| t.contains("{shard}") || t.contains("{shards}"));
    match s.shards {
        None => {
            if has_ph {
                return Err(format!("step {:?}: {{shard}} and {{shards}} need `shards = N`", s.name));
            }
            if s.shard_check.is_some() || s.shard_env {
                return Err(format!("step {:?}: shard_check and shard_env need `shards = N`", s.name));
            }
        }
        Some(n) => {
            if !(1..=64).contains(&n) {
                return Err(format!("step {:?}: shards must be 1 to 64", s.name));
            }
            if n > 1 && !has_ph && !s.shard_env {
                return Err(format!("step {:?}: shards = {n} but the command has no {{shard}}; use {{shard}}/{{shards}} or `shard_env = true`", s.name));
            }
        }
    }
    if s.shard_check.as_ref().is_some_and(|a| a.is_empty()) {
        return Err(format!("step {:?}: shard_check is empty", s.name));
    }
    Ok(())
}

fn subst(t: &str, i: u32, n: u32) -> String {
    t.replace("{shards}", &n.to_string()).replace("{shard}", &i.to_string())
}

/// How many shards a step really has: `shards = 1` is one ordinary run (its placeholders still read 1 of 1).
fn nshards(step: &Step) -> u32 {
    step.shards.unwrap_or(1)
}

fn is_sharded(step: &Step) -> bool {
    nshards(step) > 1
}

/// The file one part of a step writes to.
fn part_file(step: &Step, part: Part) -> String {
    match part {
        Part::Whole => format!("{}.full", step.name),
        Part::Shard(i) => format!("{}.{i}of{}.full", step.name, nshards(step)),
        Part::Check => format!("{}.check.full", step.name),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    Pending,
    Running,
    Green,
    Red,
    Skipped,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Part {
    /// the step is not sharded
    Whole,
    /// shard i, 1-based
    Shard(u32),
    /// the `shard_check` command
    Check,
}

struct Done {
    idx: usize,
    part: Part,
    rc: i64,
    secs: f64,
}

/// A sharded step while it runs.
struct Sh {
    /// the next shard to start (1-based); shards queue for `--par` slots, one slot each
    next: u32,
    n: u32,
    started: Instant,
    shard_start: Vec<Option<String>>,
    /// (rc, seconds) of each shard that ended
    ended: Vec<Option<(i64, f64)>>,
    /// RED_NOW is printed once per logical step, at its first red part
    red_noted: bool,
    checking: bool,
    /// the `taskset` list this step's parts run under
    pin: Option<String>,
}

/// Run `f`; a panic is a part that never reported an exit: rc 125 and a note in its `.full` (engine v2 section 6).
pub(crate) fn guarded(full: &Path, f: impl FnOnce() -> (i64, f64)) -> (i64, f64) {
    let t0 = Instant::now();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            append_line(full, "chain: the worker for this part died before it reported an exit; it is red with rc 125");
            (125, t0.elapsed().as_secs_f64())
        }
    }
}

fn exec(step: &Step, cwd: &Path, out: &Path, part: Part, pin: Option<&str>) -> (i64, f64) {
    let t0 = Instant::now();
    let n = nshards(step);
    let i = if let Part::Shard(i) = part { i } else { 1 };
    // placeholders are replaced in the step's own command, whether or not it is split
    let t = |x: &str| if step.shards.is_some() { subst(x, i, n) } else { x.to_string() };
    let mut c = match (part, &step.shard_check, &step.argv, &step.run) {
        (Part::Check, Some(a), _, _) => {
            let mut c = Command::new(&a[0]);
            c.args(&a[1..]);
            let files: Vec<PathBuf> = (1..=n).map(|k| out.join(part_file(step, Part::Shard(k)))).collect();
            if let Ok(j) = std::env::join_paths(files) {
                c.env("GATETRAIN_SHARD_OUT", j);
            }
            c
        }
        (_, _, Some(a), _) => {
            let mut c = Command::new(t(&a[0]));
            c.args(a[1..].iter().map(|x| t(x)));
            c
        }
        // both streams share one file handle, so they interleave in the order they were written
        (_, _, None, Some(r)) => shell_command(&t(r)),
        _ => unreachable!("load() refuses a step with neither"),
    };
    if part != Part::Check && step.shards.is_some() {
        c.env("GATETRAIN_SHARD", i.to_string()).env("GATETRAIN_SHARDS", n.to_string());
    }
    if let Some(list) = pin {
        // `taskset -c LIST cmd args`: same environment, the process confined to the list
        let mut w = Command::new("taskset");
        w.arg("-c").arg(list).arg(c.get_program()).args(c.get_args());
        for (k, v) in c.get_envs() {
            match v {
                Some(v) => w.env(k, v),
                None => w.env_remove(k),
            };
        }
        c = w;
    }
    c.current_dir(cwd);
    let limit = step.timeout_s.unwrap_or(365.0 * 86400.0);
    let full = out.join(part_file(step, part));
    let (rc, note) = match run_to_file(c, &full, limit) {
        Ok(Some(code)) => (i64::from(code), None),
        Ok(None) => (124, Some(format!("chain: timed out after {limit} s; the process tree was killed (the output above is what it wrote)"))),
        Err(e) => (127, Some(format!("chain: cannot start: {e}"))),
    };
    if let Some(n) = note {
        append_line(&full, &n);
    }
    (rc, t0.elapsed().as_secs_f64())
}

/// Append one line to a step's `.full`, starting a new line when the step's own output did not end with one.
fn append_line(full: &Path, line: &str) {
    use std::io::Write;
    let ends_nl = std::fs::read(full).ok().is_none_or(|b| b.is_empty() || b.ends_with(b"\n"));
    match std::fs::OpenOptions::new().create(true).append(true).open(full) {
        Ok(mut f) => {
            let _ = writeln!(f, "{}{line}", if ends_nl { "" } else { "\n" });
        }
        Err(e) => eprintln!("chain: cannot write {}: {e}", full.display()),
    }
}

fn spawn_part(step: &Step, idx: usize, part: Part, cwd: &Path, out: &Path, tx: &mpsc::Sender<Done>, pin: Option<&str>) {
    let (step, cwd, out, tx, pin) = (step.clone(), cwd.to_path_buf(), out.to_path_buf(), tx.clone(), pin.map(str::to_string));
    std::thread::spawn(move || {
        let full = out.join(part_file(&step, part));
        let (rc, secs) = guarded(&full, || exec(&step, &cwd, &out, part, pin.as_deref()));
        let _ = tx.send(Done { idx, part, rc, secs });
    });
}

fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let o = LocalRunner.run_argv(cwd, &argv).ok()?;
    (o.code == 0).then(|| o.stdout.trim().to_string())
}

fn clean_host(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty() && !v.contains(char::is_whitespace)).then(|| v.to_string())
}

/// The machine's name from the OS, not the environment (`HOSTNAME` is a shell variable that is usually not exported): the kernel's
/// file on Linux, else the `hostname` command, else `uname -n`.
pub fn os_hostname() -> Option<String> {
    if let Some(h) = std::fs::read_to_string("/proc/sys/kernel/hostname").ok().and_then(|t| clean_host(&t)) {
        return Some(h);
    }
    for argv in [["hostname"].as_slice(), ["uname", "-n"].as_slice()] {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        if let Ok(o) = LocalRunner.run_argv(Path::new("."), &argv) {
            if o.code == 0 {
                if let Some(h) = clean_host(&o.stdout) {
                    return Some(h);
                }
            }
        }
    }
    None
}

/// The host name a run reports: `GATETRAIN_HOST` (set by `hold`), `COMPUTERNAME`, `HOSTNAME`, then the OS ([`os_hostname`]), else
/// `unknown`. The two lookups are parameters so a test can show each step of the fallback.
pub fn host_with(env: &dyn Fn(&str) -> Option<String>, os: &dyn Fn() -> Option<String>) -> String {
    ["GATETRAIN_HOST", "COMPUTERNAME", "HOSTNAME"].iter().find_map(|k| env(k).and_then(|v| clean_host(&v))).or_else(os).unwrap_or_else(|| "unknown".into())
}

pub fn host_name() -> String {
    host_with(&|k| std::env::var(k).ok(), &os_hostname)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Run the plan, calling `emit` once per log-format line. Err = config error (a name in only/skip that is no step, an unusable out folder).
pub fn run(plan: &Plan, digest: &str, o: &Opts, emit: &mut dyn FnMut(&str)) -> Result<Outcome, String> {
    for n in o.only.iter().chain(&o.skip).chain(&o.no_cache_for) {
        if !plan.steps.iter().any(|s| &s.name == n) {
            return Err(format!("--only/--skip/--no-cache-for names {n:?}, which is no step"));
        }
    }
    let budget = o.cores.max(1);
    let any_cores = plan.steps.iter().any(|s| s.cores.is_some());
    for s in &plan.steps {
        if let Some(c) = s.cores.filter(|&c| c as usize > budget) {
            return Err(format!("step {:?}: cores = {c} exceeds the core budget of {budget} (--cores)", s.name));
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
    // D446: the quarantine rows live today, read once; a missing file is a config error, a lapsed or malformed row is dropped and named
    let quarantine = if o.retry_flakes {
        match &o.quarantine {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|e| format!("cannot read the quarantine file {}: {e}", p.display()))?;
                let (live, dropped) = crate::diagnose::live_quarantine(&text, &Ts::now_utc().iso()[..10]);
                for (n, why) in dropped {
                    eprintln!("chain: quarantine {} line {n}: not honoured: {why}", p.display());
                }
                live
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let history: crate::diagnose::History = o.expected.iter().flatten().map(|(k, v)| (k.clone(), vec![*v])).collect();
    let host = o.host.as_deref().and_then(clean_host).unwrap_or_else(host_name);

    // every emitted line also feeds the log digest the ledger keeps
    let mut log_hash = Sha256::new();
    let mut say = |l: &str| {
        log_hash.update(l.as_bytes());
        log_hash.update(b"\n");
        emit(l);
    };

    // the tree must be clean before AND after; the out folder is not part of the tree it proves
    let out_abs = o.out.canonicalize().unwrap_or_else(|_| o.out.clone());
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
    // untracked files count whatever `status.showUntrackedFiles` says: a stamp is never written over a file git was told to hide
    let mut porcelain: Vec<String> = vec!["status".into(), "--porcelain".into(), "--untracked-files=all".into(), "--".into(), ".".into()];
    if let Ok(rel) = out_abs.strip_prefix(&cwd_abs) {
        if !rel.as_os_str().is_empty() {
            porcelain.push(format!(":(exclude){}", rel.to_string_lossy().replace('\\', "/")));
        }
    }
    let porcelain: Vec<&str> = porcelain.iter().map(String::as_str).collect();
    let sha = git_out(&cwd, &["rev-parse", "HEAD"]);
    let dirty_before = sha.as_ref().map(|_| git_out(&cwd, &porcelain).unwrap_or_else(|| "git status failed".into()));
    let mut branch: Option<String> = None;
    if let Some(sha) = &sha {
        let b = git_out(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into());
        say(&format!("branch {} at {} on {}", b.replace(char::is_whitespace, "_"), sha, host));
        branch = Some(b.replace(char::is_whitespace, "_"));
    }
    let started_at = Ts::now_utc().iso();
    say(&format!("CHAIN_MODE {}", o.mode));
    say(&format!("CHAIN_PARALLEL {par}"));
    say(&format!("CHAIN_ORDER {}", if o.expected.as_ref().is_some_and(|e| !e.is_empty()) { "longest" } else { "file" }));
    if any_cores {
        say(&format!("CHAIN_CORES {budget}"));
    }
    // where the per-step `.full` files are, so `diagnose` can find them from the log alone (an unknown line to every reader)
    say(&format!("CHAIN_OUT {}", out_abs.to_string_lossy().trim_start_matches(r"\\?\")));

    let n_steps = plan.steps.len();
    let mut sink = (o.samples > 0).then(|| SampleSink::new(&o.out, &host, o.samples));
    let mut state = vec![St::Pending; n_steps];
    let mut times: Vec<(usize, i64, f64)> = Vec::new();
    let mut reds: Vec<(String, i64)> = Vec::new();
    let mut keys: Vec<Option<String>> = vec![None; n_steps];
    let mut notes: Vec<Option<String>> = vec![None; n_steps];
    let mut cached: Vec<usize> = Vec::new();
    let mut sh: Vec<Option<Sh>> = (0..n_steps).map(|_| None).collect();
    let mut step_started: Vec<Option<String>> = vec![None; n_steps];
    let mut recs: Vec<StepRec> = Vec::new();
    let mut shard_recs: Vec<ShardRec> = Vec::new();
    let mut skips: Vec<(String, String)> = Vec::new();
    // retry state: the wall seconds of a step's first attempt, its pending retry record, and the finished ones
    let mut prior_secs: Vec<f64> = vec![0.0; n_steps];
    let mut retrying: Vec<Option<RetryRec>> = vec![None; n_steps];
    let mut retries: Vec<RetryRec> = Vec::new();
    let mut retry_started = 0usize;
    let t_run = Instant::now();
    // the core budget: which CPUs pinned steps hold, which each step holds, and the `taskset` list each running step was started with
    let mut busy = vec![false; budget];
    let mut held: Vec<Vec<usize>> = vec![Vec::new(); n_steps];
    let mut pin: Vec<Option<String>> = vec![None; n_steps];
    let pinning = any_cores && can_pin();
    let mut fast_red = false;
    for tier in [Tier::Fast, Tier::Slow] {
        let mut mine: Vec<usize> = selected.iter().copied().filter(|&i| plan.steps[i].tier == tier).collect();
        if let Some(e) = o.expected.as_ref().filter(|e| !e.is_empty()) {
            // longest expected first; the sort is stable, so ties and unknown steps keep file order
            let exp = |i: usize| e.get(&plan.steps[i].name).copied().unwrap_or(0.0);
            mine.sort_by(|&a, &b| exp(b).partial_cmp(&exp(a)).unwrap_or(std::cmp::Ordering::Equal));
        }
        if mine.is_empty() {
            continue;
        }
        if tier == Tier::Slow && fast_red && !o.keep_going {
            for &i in &mine {
                state[i] = St::Skipped;
                let why = "a fast step is red; --keep-going runs slow anyway";
                say(&format!("STEP_SKIPPED {} {why}", plan.steps[i].name));
                skips.push((plan.steps[i].name.clone(), why.into()));
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
                        let why = format!("needs {n}, which is not green");
                        say(&format!("STEP_SKIPPED {} {why}", plan.steps[i].name));
                        skips.push((plan.steps[i].name.clone(), why));
                        changed = true;
                    }
                }
            }
            // start what is ready, in file order: first the shards still queued for a slot, then new steps
            let mut hit = false;
            for &i in &mine {
                if running >= par {
                    break;
                }
                if state[i] == St::Running {
                    if let Some(s) = sh[i].as_mut() {
                        while running < par && s.next <= s.n {
                            launch_shard(&plan.steps[i], i, s, &cwd, &o.out, &tx, &mut say);
                            running += 1;
                        }
                    }
                    continue;
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
                if let Some(cdir) = &o.cache {
                    let step = &plan.steps[i];
                    if !step.inputs.is_empty() {
                        match cache_key(step, &cwd) {
                            Ok(k) => {
                                let entry = (!o.no_cache_for.contains(&step.name)).then(|| read_entry(cdir, &step.name, &k)).flatten();
                                if let Some((esha, eat)) = entry {
                                    let now = Ts::now_utc().iso();
                                    say(&format!("STEP_START {} {now}", step.name));
                                    say(&format!("STEP_CACHED {}", step.name));
                                    say(&format!("STEP_TIME {} 0.0s", step.name));
                                    say(&format!("STEP_EXIT {} 0", step.name));
                                    let full = o.out.join(format!("{}.full", step.name));
                                    let _ = std::fs::write(&full, format!("cached: green at {esha} {eat}, key {k}\n"));
                                    state[i] = St::Green;
                                    times.push((i, 0, 0.0));
                                    cached.push(i);
                                    recs.push(StepRec {
                                        name: step.name.clone(),
                                        tier: tier_name(step.tier),
                                        started_at: Some(now.clone()),
                                        ended_at: Some(now),
                                        rc: 0,
                                        secs: 0.0,
                                        cached: true,
                                        cached_from: Some(esha),
                                        key: Some(k),
                                        shards: i64::from(nshards(step)),
                                        log_ref: format!("{}.full", step.name),
                                    });
                                    hit = true;
                                    continue;
                                }
                                keys[i] = Some(k);
                            }
                            Err(why) => notes[i] = Some(format!("chain: not cached: {why}")),
                        }
                    }
                }
                if any_cores {
                    let want = plan.steps[i].cores.map_or(1, |c| c as usize);
                    let plain = (0..n_steps).filter(|&j| state[j] == St::Running && plan.steps[j].cores.is_none()).count();
                    if busy.iter().filter(|b| **b).count() + plain + want > budget {
                        continue;
                    }
                    if let Some(c) = plan.steps[i].cores {
                        let Some(cpus) = alloc_cpus(&busy, c as usize) else { continue };
                        for &k in &cpus {
                            busy[k] = true;
                        }
                        pin[i] = pinning.then(|| cpu_list(&cpus));
                        held[i] = cpus;
                    } else if pinning {
                        pin[i] = free_cpu_list(&busy);
                    }
                }
                state[i] = St::Running;
                let now = Ts::now_utc().iso();
                say(&format!("STEP_START {} {now}", plan.steps[i].name));
                step_started[i] = Some(now);
                if is_sharded(&plan.steps[i]) {
                    let n = nshards(&plan.steps[i]);
                    let list: String = (1..=n).map(|k| format!("{}\n", part_file(&plan.steps[i], Part::Shard(k)))).collect();
                    let _ = std::fs::write(o.out.join(format!("{}.full", plan.steps[i].name)), format!("{n} shards, each in its own file:\n{list}"));
                    let mut s = Sh { next: 1, n, started: Instant::now(), shard_start: vec![None; n as usize], ended: vec![None; n as usize], red_noted: false, checking: false, pin: pin[i].clone() };
                    while running < par && s.next <= s.n {
                        launch_shard(&plan.steps[i], i, &mut s, &cwd, &o.out, &tx, &mut say);
                        running += 1;
                    }
                    sh[i] = Some(s);
                } else {
                    running += 1;
                    spawn_part(&plan.steps[i], i, Part::Whole, &cwd, &o.out, &tx, pin[i].as_deref());
                }
            }
            if hit {
                continue; // a cached step is green already: settle skips and start its dependents
            }
            if running == 0 {
                break;
            }
            let wait = sink.as_ref().map_or(Duration::from_secs(3600 * 24 * 366), |s| s.every);
            let d = loop {
                match rx.recv_timeout(wait) {
                    Ok(d) => break d,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(s) = sink.as_mut() {
                            s.tick(state.iter().filter(|x| **x == St::Running).count());
                        }
                    }
                    Err(e) => return Err(format!("chain: lost a worker: {e}")),
                }
            };
            if let Some(s) = sink.as_mut() {
                s.maybe(state.iter().filter(|x| **x == St::Running).count());
            }
            running -= 1;
            let step = &plan.steps[d.idx];
            let name = &step.name;
            // `fin` = Some((rc, wall seconds)) once the logical step is over
            let mut red_file = format!("{name}.full");
            let fin: Option<(i64, f64)> = match d.part {
                Part::Whole => Some((d.rc, d.secs)),
                Part::Shard(k) => {
                    let s = sh[d.idx].as_mut().expect("a shard of a step that is running");
                    say(&format!("STEP_SHARD {name} {k}/{} EXIT {} {:.1}s", s.n, d.rc, d.secs));
                    s.ended[k as usize - 1] = Some((d.rc, d.secs));
                    shard_recs.push(ShardRec {
                        step: name.clone(),
                        shard: i64::from(k),
                        of: i64::from(s.n),
                        started_at: s.shard_start[k as usize - 1].clone().unwrap_or_default(),
                        secs: round1(d.secs),
                        rc: d.rc,
                        log_ref: part_file(step, Part::Shard(k)),
                    });
                    if d.rc != 0 && !s.red_noted {
                        s.red_noted = true;
                        say(&format!("RED_NOW {name} rc={} at {} (shard {k}/{}; full output: {})", d.rc, Ts::now_utc().iso(), s.n, part_file(step, Part::Shard(k))));
                    }
                    if s.ended.iter().all(Option::is_some) {
                        let lowest_red = s.ended.iter().flatten().find(|(rc, _)| *rc != 0).map(|(rc, _)| *rc);
                        match (lowest_red, &step.shard_check) {
                            (Some(rc), _) => Some((rc, s.started.elapsed().as_secs_f64())),
                            (None, Some(_)) => {
                                s.checking = true;
                                spawn_part(step, d.idx, Part::Check, &cwd, &o.out, &tx, pin[d.idx].as_deref());
                                running += 1;
                                None
                            }
                            (None, None) => Some((0, s.started.elapsed().as_secs_f64())),
                        }
                    } else {
                        None
                    }
                }
                Part::Check => {
                    red_file = part_file(step, Part::Check);
                    Some((d.rc, sh[d.idx].as_ref().map_or(d.secs, |s| s.started.elapsed().as_secs_f64())))
                }
            };
            let Some((rc, secs)) = fin else { continue };
            // D446: a red whole step may be re-run ONCE on the same sha, only when its own output says flake/infra or a live quarantine row names it
            if rc != 0 && o.retry_flakes && matches!(d.part, Part::Whole) && retrying[d.idx].is_none() && !is_sharded(step) {
                let full = o.out.join(format!("{name}.full"));
                let text = std::fs::read(&full).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                let lines: Vec<String> = text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
                if let Some(c) = crate::diagnose::retry_cause(name, rc, secs, &lines, &history, crate::diagnose::builtin_rules(), &quarantine) {
                    if retry_started >= MAX_RETRIED_STEPS {
                        say(&format!("STEP_RETRY_REFUSED {name} at most {MAX_RETRIED_STEPS} steps are retried per run"));
                    } else {
                        let test = c.test.replace(['\r', '\n'], " ");
                        let _ = std::fs::copy(&full, o.out.join(format!("{name}.attempt1.full")));
                        say(&format!("STEP_RETRY {name} attempt=1 rc={rc} class={} via={} test={test}", c.cls, c.via));
                        retry_started += 1;
                        prior_secs[d.idx] = secs;
                        retrying[d.idx] = Some(RetryRec { name: name.clone(), first_rc: rc, final_rc: rc, class: c.cls, via: c.via, test });
                        running += 1;
                        spawn_part(step, d.idx, Part::Whole, &cwd, &o.out, &tx, pin[d.idx].as_deref());
                        continue;
                    }
                }
            }
            let secs = secs + prior_secs[d.idx];
            for k in held[d.idx].drain(..) {
                busy[k] = false;
            }
            say(&format!("STEP_TIME {name} {secs:.1}s"));
            say(&format!("STEP_EXIT {name} {rc}"));
            times.push((d.idx, rc, secs));
            if let Some(n) = &notes[d.idx] {
                append_line(&o.out.join(format!("{name}.full")), n);
            }
            recs.push(StepRec {
                name: name.clone(),
                tier: tier_name(step.tier),
                started_at: step_started[d.idx].clone(),
                ended_at: Some(Ts::now_utc().iso()),
                rc,
                secs: round1(secs),
                cached: false,
                cached_from: None,
                key: keys[d.idx].clone(),
                shards: i64::from(nshards(step)),
                log_ref: format!("{name}.full"),
            });
            let was_retried = retrying[d.idx].take().map(|mut r| {
                r.final_rc = rc;
                retries.push(r);
            }).is_some();
            if rc == 0 {
                if let (Some(cdir), Some(k), Some(sha), false) = (&o.cache, &keys[d.idx], &sha, was_retried) {
                    if let Err(e) = write_entry(cdir, name, k, sha, secs, &host) {
                        eprintln!("chain: cannot write the cache entry for {name}: {e}");
                    }
                }
                state[d.idx] = St::Green;
            } else {
                state[d.idx] = St::Red;
                reds.push((name.clone(), rc));
                if tier == Tier::Fast {
                    fast_red = true;
                }
                if !sh[d.idx].as_ref().is_some_and(|s| s.red_noted) {
                    say(&format!("RED_NOW {name} rc={rc} at {} (full output: {red_file})", Ts::now_utc().iso()));
                }
            }
        }
    }
    if !retries.is_empty() {
        say(&format!("CHAIN_RETRIES {}", retries.iter().map(|r| format!("{}={}", r.name, if r.final_rc == 0 { "green" } else { "red" })).collect::<Vec<_>>().join(" ")));
    }
    say(&format!("RED_SUMMARY: {}", if reds.is_empty() { "none".to_string() } else { summary_in_file_order(plan, &reds) }));
    say("GATES_DONE");
    let log_sha256: String = log_hash.clone().finalize().iter().map(|b| format!("{b:02x}")).collect();

    let ended_at = Ts::now_utc().iso();
    let mut stamp_text: Option<(String, String)> = None;
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
                    let rec = recs.iter().find(|r| r.name == plan.steps[i].name);
                    serde_json::json!({
                        "name": plan.steps[i].name, "tier": tier_name(plan.steps[i].tier), "rc": rc, "seconds": round1(secs),
                        "key": rec.and_then(|r| r.key.clone()), "cached_from": rec.and_then(|r| r.cached_from.clone()), "shards": nshards(&plan.steps[i]),
                    })
                })
                .collect();
            let mut body = serde_json::json!({
                "v": 1, "kind": o.mode, "sha": sha, "host": host, "at": Ts::now_utc().iso(), "mode": o.mode, "par": par,
                "total_seconds": round1(t_run.elapsed().as_secs_f64()),
                "digest": digest, "steps": steps,
                "cached": cached.iter().map(|&i| plan.steps[i].name.clone()).collect::<Vec<_>>(),
                "prev": Vec::<String>::new(),
            });
            if !retries.is_empty() {
                body["retried"] = retries.iter().map(|r| serde_json::json!({"name": r.name, "first_rc": r.first_rc, "final_rc": r.final_rc, "class": r.class, "via": r.via, "test": r.test})).collect();
            }
            let text = serde_json::to_string_pretty(&body).unwrap_or_default() + "\n";
            let p = o.out.join(format!("stamp-{sha}.json"));
            std::fs::write(&p, &text).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
            // a stamp's own digest is the sha-256 of the file's bytes (engine v2 section 5.4)
            stamp_text = Some((digest_of(&text), text));
            Ok(p)
        }
    } else {
        Err("not a git repository: no stamp written".to_string())
    };
    let record = RunRec {
        sha,
        branch,
        host,
        mode: o.mode.clone(),
        par,
        started_at,
        ended_at,
        outcome: if reds.is_empty() { "green" } else { "red" },
        plan_digest: digest.to_string(),
        log_sha256,
        steps: recs,
        shards: shard_recs,
        skips,
        stamp: stamp_text,
        retries,
    };
    Ok(Outcome { reds, stamp, record })
}

fn tier_name(t: Tier) -> &'static str {
    if t == Tier::Fast {
        "fast"
    } else {
        "slow"
    }
}

fn launch_shard(step: &Step, idx: usize, s: &mut Sh, cwd: &Path, out: &Path, tx: &mpsc::Sender<Done>, say: &mut dyn FnMut(&str)) {
    let k = s.next;
    s.next += 1;
    let now = Ts::now_utc().iso();
    say(&format!("STEP_SHARD {} {k}/{} START {now}", step.name, s.n));
    s.shard_start[k as usize - 1] = Some(now);
    spawn_part(step, idx, Part::Shard(k), cwd, out, tx, s.pin.clone().as_deref());
}

/// The cache key of a step: sha-256 over its command, its directory relative to the repository top, salt and `git ls-files -s -- <inputs>` (mode, blob, path of every tracked
/// input). A sharded step's key also covers `shards` and `shard_check` (one key for the logical step). Err(why) = this run must not use or fill the cache: a dirty input
/// (modified or untracked), no tracked input, or git failing.
fn cache_key(step: &Step, cwd: &Path) -> Result<String, String> {
    let mut spec: Vec<&str> = vec!["--"];
    spec.extend(step.inputs.iter().map(String::as_str));
    let with = |head: &[&'static str]| -> Vec<&str> { head.iter().copied().chain(spec.iter().copied()).collect() };
    match git_out(cwd, &with(&["status", "--porcelain", "--untracked-files=all"])) {
        Some(d) if d.is_empty() => {}
        Some(_) => return Err("inputs are dirty (git status --porcelain is not empty for them); the step ran and no entry was written".into()),
        None => return Err("git status failed for the inputs".into()),
    }
    let ls = git_out(cwd, &with(&["ls-files", "-s"])).ok_or_else(|| "git ls-files failed for the inputs".to_string())?;
    let mut lines: Vec<&str> = ls.lines().collect();
    if lines.is_empty() {
        return Err("the inputs match no tracked file".into());
    }
    lines.sort_unstable();
    let mut h = Sha256::new();
    let mut part = |s: &str| {
        h.update((s.len() as u64).to_le_bytes());
        h.update(s.as_bytes());
    };
    part(&match (&step.run, &step.argv) {
        (Some(r), _) => format!("run:{r}"),
        (None, Some(a)) => format!("argv:{}", serde_json::to_string(a).unwrap_or_default()),
        _ => String::new(),
    });
    // the directory RELATIVE to the repository top (`git rev-parse --show-prefix`), never the absolute path: two worktrees of one
    // commit (the pre-check's and the proof's) must share a key (engine-v2 design, conflict X1)
    let prefix = git_out(cwd, &["rev-parse", "--show-prefix"]).ok_or_else(|| "git rev-parse --show-prefix failed".to_string())?;
    part(&format!("cwd:{prefix}"));
    part(step.cache_salt.as_deref().unwrap_or(""));
    // only a sharded step adds these, so every key an unsharded step had before stays what it was
    if let Some(n) = step.shards {
        part(&format!("shards:{n}"));
        if let Some(c) = &step.shard_check {
            part(&format!("shard_check:{}", serde_json::to_string(c).unwrap_or_default()));
        }
        if step.shard_env {
            part("shard_env");
        }
    }
    for l in lines {
        part(l);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// (sha, time) of a green entry for this key, if the file is there and says the same key.
fn read_entry(cache: &Path, step: &str, key: &str) -> Option<(String, String)> {
    let text = std::fs::read_to_string(cache.join(step).join(format!("{key}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    (v["key"].as_str()? == key).then(|| (v["sha"].as_str().unwrap_or("unknown").to_string(), v["at"].as_str().unwrap_or("unknown").to_string()))
}

fn write_entry(cache: &Path, step: &str, key: &str, sha: &str, secs: f64, host: &str) -> Result<(), String> {
    let dir = cache.join(step);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body = serde_json::json!({"sha": sha, "at": Ts::now_utc().iso(), "seconds": round1(secs), "key": key, "host": host});
    let tmp = dir.join(format!("{key}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&body).unwrap_or_default() + "\n").map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(format!("{key}.json"))).map_err(|e| e.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_that_dies_is_rc_125_with_a_note_in_its_full_file() {
        let d = std::env::temp_dir().join(format!("gt-guard-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let full = d.join("t.2of4.full");
        std::fs::write(&full, "partial output with no newline").unwrap();
        let (rc, _) = guarded(&full, || panic!("the worker died"));
        assert_eq!(rc, 125);
        let text = std::fs::read_to_string(&full).unwrap();
        assert!(text.starts_with("partial output with no newline\n") && text.contains("rc 125"), "{text}");
        assert_eq!(guarded(&full, || (3, 1.5)), (3, 1.5), "a normal part passes through");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn host_falls_back_in_order_env_then_os_then_unknown() {
        let none = |_: &str| None;
        assert_eq!(host_with(&none, &|| Some("os-name".into())), "os-name");
        assert_eq!(host_with(&none, &|| None), "unknown");
        let env = |k: &str| match k {
            "HOSTNAME" => Some("from-env".to_string()),
            "GATETRAIN_HOST" => Some("from-hold".to_string()),
            _ => None,
        };
        assert_eq!(host_with(&env, &|| Some("os-name".into())), "from-hold");
        let blank = |k: &str| (k == "COMPUTERNAME").then(|| "  ".to_string());
        assert_eq!(host_with(&blank, &|| Some("os-name".into())), "os-name", "a blank variable is not a name");
        let spaced = |k: &str| (k == "HOSTNAME").then(|| "two words".to_string());
        assert_eq!(host_with(&spaced, &|| Some("os-name".into())), "os-name", "a name with whitespace would break the log line");
    }

    #[test]
    fn the_os_hostname_is_a_real_name() {
        let h = os_hostname().expect("this machine has a host name");
        assert!(!h.is_empty() && !h.contains(char::is_whitespace));
    }
}

#[cfg(test)]
mod cpu_tests {
    use super::*;

    #[test]
    fn the_allocator_takes_the_first_contiguous_free_run() {
        let mut busy = vec![false; 8];
        assert_eq!(alloc_cpus(&busy, 3), Some(vec![0, 1, 2]));
        busy[1] = true;
        assert_eq!(alloc_cpus(&busy, 3), Some(vec![2, 3, 4]));
        assert_eq!(alloc_cpus(&busy, 1), Some(vec![0]));
        assert_eq!(alloc_cpus(&[true, false, true, false], 2), None, "free CPUs that are not contiguous do not make a run");
        assert_eq!(alloc_cpus(&busy, 9), None);
        assert_eq!(alloc_cpus(&busy, 0), None);
    }

    #[test]
    fn the_taskset_list_is_ranges_and_singles() {
        assert_eq!(cpu_list(&[0, 1, 2]), "0-2");
        assert_eq!(cpu_list(&[4]), "4");
        assert_eq!(cpu_list(&[0, 1, 3, 5, 6]), "0-1,3,5-6");
    }

    #[test]
    fn unpinned_steps_are_confined_to_what_pinned_steps_do_not_hold() {
        let mut busy = vec![false; 6];
        assert_eq!(free_cpu_list(&busy), None, "nothing reserved, nothing to confine");
        busy[1] = true;
        busy[2] = true;
        assert_eq!(free_cpu_list(&busy).as_deref(), Some("0,3-5"));
        assert_eq!(free_cpu_list(&[true, true]), None);
    }
}
