//! `tick` (engine v2 E10, section 4): one bounded pass, in this order: doctor per host, ingest the finished chain logs (and their
//! samples) the batches point at, advance every open batch through [`Engine::advance`] until it waits or is refused, `tidy --auto`
//! per host, at most one line of advice, and the cause-storm check over every running log. A report of 3 to 6 lines.
//!
//! What it never does (engine-v2 section 4, `engine-and-governance.md:629-631`): open a batch, choose lanes, apply advice, run a
//! probe, leave anything queued to fire later, or move a batch by any path but `advance` (so every guard and every ledger row is the
//! state machine's own). The ledger lock ([`try_lock`]) keeps two ticks from overlapping; scheduling is the operator's (cron, a timer).
use crate::advise::{self, Minutes};
use crate::batch::{Engine, Ev, Outcome};
use crate::config::{Config, Host};
use crate::diagnose::{self, Rule};
use crate::doctor::DResult;
use crate::history;
use crate::logfmt::{self, Ts};
use crate::runs;
use crate::Result;
use serde_json::{json, Value};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

/// Transitions one batch may take in one tick (open to closed is 10); a bound, not a target.
pub const MAX_STEPS: usize = 16;
/// Passes over the batches: a batch refused because its base was not proven yet is tried again after its base moved, in the same tick.
pub const MAX_PASSES: usize = 3;
/// Batch lines in the report; more batches fold into one line.
pub const MAX_BATCH_LINES: usize = 3;

/// Refusals that clear by themselves (a busy host, a base batch still on its way); every other refusal needs a person (exit 1).
const WAITS_ON_ITS_OWN: &[&str] = &["host.refuse", "host.refuse-proof-running", "host.refuse-memory", "pipeline.base-unproven", "pipeline.base-not-merged"];
/// Refusals that may clear when another batch moves in this tick.
const RETRY_AFTER_A_MOVE: &[&str] = &["pipeline.base-unproven", "pipeline.base-not-merged"];

// ---------------------------------------------------------------- the ledger lock

/// The ledger lock: an OS file lock (`File::try_lock`) on `<db>.tick.lock`, released when this value drops or the process dies.
/// The file holds nothing: on Windows a locked file cannot be read by other handles.
pub struct TickLock {
    _file: File,
    /// the sidecar this holder wrote; removed when it lets go, so a later holder is never described by it
    info: Option<PathBuf>,
}

impl Drop for TickLock {
    fn drop(&mut self) {
        if let Some(p) = &self.info {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub fn lock_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(".tick.lock");
    PathBuf::from(p)
}

/// `None` when another tick holds the lock.
pub fn try_lock(path: &Path) -> Result<Option<TickLock>> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d)?;
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(TickLock { _file: file, info: None })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// `<lock>.info`: who holds the ledger lock and since when, beside the lock file (a locked file cannot be read on Windows).
pub fn info_path(lock: &Path) -> PathBuf {
    let mut p = lock.as_os_str().to_owned();
    p.push(".info");
    PathBuf::from(p)
}

/// What the sidecar says about the holder.
#[derive(Debug, Clone, PartialEq)]
pub struct LockInfo {
    pub pid: i64,
    pub started: String,
    /// `tick`, or the `batch` command that holds it
    pub by: String,
}

impl LockInfo {
    /// Minutes since `started`, when it parses.
    pub fn age_min(&self) -> Option<f64> {
        let t = logfmt::parse_iso(&self.started).ok()?;
        Some(Ts::now_utc().secs_since(&t) / 60.0)
    }
}

pub fn read_info(lock: &Path) -> Option<LockInfo> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(info_path(lock)).ok()?).ok()?;
    Some(LockInfo { pid: v["pid"].as_i64()?, started: v["started"].as_str()?.to_string(), by: v["by"].as_str().unwrap_or("?").to_string() })
}

pub enum Taken {
    Got(TickLock),
    /// another process holds it; what its sidecar says, if anything
    Held(Option<LockInfo>),
}

/// Take the ledger lock and write the sidecar (pid, start time, `by`).
pub fn take(path: &Path, by: &str) -> Result<Taken> {
    match try_lock(path)? {
        Some(mut l) => {
            let info = json!({"pid": std::process::id(), "started": crate::hold::now_iso(), "by": by});
            std::fs::write(info_path(path), info.to_string())?;
            l.info = Some(info_path(path));
            Ok(Taken::Got(l))
        }
        None => Ok(Taken::Held(read_info(path))),
    }
}

/// [`take`], retrying for up to `wait_secs` while it is held.
pub fn take_waiting(path: &Path, by: &str, wait_secs: u64) -> Result<Taken> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    loop {
        match take(path, by)? {
            Taken::Held(i) if std::time::Instant::now() >= until => return Ok(Taken::Held(i)),
            Taken::Held(_) => std::thread::sleep(std::time::Duration::from_millis(200)),
            got => return Ok(got),
        }
    }
}

// ---------------------------------------------------------------- seams

/// What the tick asks of a host. The binary implements it over the existing runners; tests over canned answers.
pub trait HostSeams {
    /// The doctor checks of `h`; `None` when its reach has no runner.
    fn doctor(&self, h: &Host) -> Option<Vec<DResult>>;
    /// `tidy --auto` on `h` (the configured unattended classes only); a short note of what it did.
    fn tidy_auto(&self, h: &Host) -> String;
}

/// `tidy_auto_note` reads "<host>: 1.2 GB reclaimed (measured: ...); log ...": keep "1.2 GB reclaimed" and what follows the log part.
pub fn short_tidy_note(note: &str, host: &str) -> String {
    let n = note.strip_prefix(&format!("{host}: ")).unwrap_or(note);
    let head = n.split(" (").next().unwrap_or(n).trim();
    let tail: Vec<&str> = n.split("; ").skip(1).filter(|p| p.contains("failed") || p.contains("threshold")).collect();
    if tail.is_empty() {
        head.to_string()
    } else {
        format!("{head}, {}", tail.join(", "))
    }
}

pub struct TickOpts<'a> {
    /// hosts.toml: doctor, tidy and advice run for every enabled host
    pub hosts: &'a Config,
    pub seams: &'a dyn HostSeams,
    pub rules: &'a [Rule],
    /// `[chain] storm_reds`
    pub storm_reds: usize,
    pub now_us: i64,
    pub max_steps: usize,
}

// ---------------------------------------------------------------- the report

#[derive(Debug, Clone, PartialEq)]
pub enum End {
    /// Took transitions and ended where nothing more is to do (closed, abandoned, or idle).
    Rests(String),
    Waits(String),
    Blocked { rule: String, because: String, person: bool },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct BatchResult {
    pub batch: String,
    /// The state before the tick, then each state it reached.
    pub states: Vec<String>,
    pub end: End,
}

impl BatchResult {
    fn moved(&self) -> bool {
        self.states.len() > 1
    }
    fn needs_person(&self) -> bool {
        matches!(&self.end, End::Blocked { person: true, .. } | End::Failed(_))
    }
    fn line(&self) -> String {
        let path = self.states.join(" -> ");
        match &self.end {
            End::Rests(_) => format!("{}: {path}", self.batch),
            End::Waits(why) => format!("{}: {path}, waits: {why}", self.batch),
            End::Blocked { rule, because, .. } => format!("{}: {path}, BLOCKED {rule}: {because}", self.batch),
            End::Failed(why) => format!("{}: {path}, FAILED {why}", self.batch),
        }
    }
    fn end_word(&self) -> &'static str {
        match &self.end {
            End::Rests(_) => "rests",
            End::Waits(_) => "waits",
            End::Blocked { .. } => "blocked",
            End::Failed(_) => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostResult {
    pub host: String,
    pub reach: String,
    /// `None`: the host's reach has no runner
    pub doctor: Option<Vec<DResult>>,
    pub tidy: String,
}

#[derive(Debug, Clone)]
pub struct Storm {
    pub run: String,
    pub reds: usize,
    pub evidence: String,
}

#[derive(Debug, Clone, Default)]
pub struct TickReport {
    pub lines: Vec<String>,
    pub batches: Vec<BatchResult>,
    pub hosts: Vec<HostResult>,
    pub ingested: Vec<String>,
    /// finished logs that could not be read into the ledger: (run, why)
    pub not_ingested: Vec<(String, String)>,
    pub advice: Option<String>,
    pub storms: Vec<Storm>,
    /// `floor.log-append-only`: the decision log does not verify, so no batch was advanced
    pub log_broken: Option<String>,
}

impl TickReport {
    /// 0 normally, 1 when a batch needs a person (a blocking rule, a failed transition, the step cap) or the decision log is broken.
    pub fn exit_code(&self) -> u8 {
        u8::from(self.log_broken.is_some() || self.batches.iter().any(BatchResult::needs_person))
    }

    pub fn to_json(&self) -> String {
        let batches: Vec<Value> = self
            .batches
            .iter()
            .map(|b| {
                let (rule, why) = match &b.end {
                    End::Blocked { rule, because, .. } => (Some(rule.clone()), because.clone()),
                    End::Rests(w) | End::Waits(w) | End::Failed(w) => (None, w.clone()),
                };
                json!({"batch": b.batch, "states": b.states, "end": b.end_word(), "rule": rule, "why": why, "needs_person": b.needs_person()})
            })
            .collect();
        let hosts: Vec<Value> = self
            .hosts
            .iter()
            .map(|h| json!({"host": h.host, "doctor": h.doctor.as_ref().map(|d| d.iter().map(|r| json!({"check": r.check, "status": r.status, "message": r.message})).collect::<Vec<_>>()), "tidy": h.tidy}))
            .collect();
        let storms: Vec<Value> = self.storms.iter().map(|s| json!({"run": s.run, "reds": s.reds, "evidence": s.evidence})).collect();
        let not: Vec<Value> = self.not_ingested.iter().map(|(r, w)| json!({"run": r, "why": w})).collect();
        json!({"lines": self.lines, "batches": batches, "hosts": hosts, "ingested": self.ingested, "not_ingested": not, "advice": self.advice, "storms": storms, "log_broken": self.log_broken, "exit": self.exit_code()}).to_string()
    }
}

// ---------------------------------------------------------------- the pass

pub fn tick(e: &mut Engine, o: &TickOpts) -> Result<TickReport> {
    let mut rep = TickReport::default();
    let hosts: Vec<&Host> = o.hosts.hosts.iter().filter(|h| h.enabled).collect();
    // 0. the decision log verifies, or no automatic transition happens (floor.log-append-only)
    if let Err(b) = crate::declog::verify(&*e.ledger)? {
        rep.log_broken = Some(format!("BROKEN at seq {}: {}", b.seq, b.reason));
    }
    // 1. doctor per host
    for h in &hosts {
        rep.hosts.push(HostResult { host: h.name.clone(), reach: h.reach.clone(), doctor: o.seams.doctor(h), tidy: String::new() });
    }
    // 2. ingest the finished logs (and their samples) of every batch's runs; one bad log is named, never fatal
    let ids = e.batch_ids()?;
    for id in &ids {
        let Ok(evs) = e.events(id) else { continue };
        for (run, log) in runs_of(&evs) {
            match ingest_finished(e, &run, &log) {
                Ok(true) => rep.ingested.push(run),
                Ok(false) => {}
                Err(why) => rep.not_ingested.push((run, why)),
            }
        }
    }
    // 3. advance every open batch through the state machine's own `advance`; one broken batch fails alone
    let mut results: Vec<BatchResult> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for id in &ids {
        match e.fold(id) {
            Ok(f) if matches!(f.state.as_str(), "closed" | "abandoned") => {}
            Ok(f) => results.push(BatchResult { batch: id.clone(), states: vec![f.state], end: End::Rests(String::new()) }),
            Err(err) => {
                unreadable.push(id.clone());
                results.push(BatchResult { batch: id.clone(), states: vec!["?".into()], end: End::Failed(format!("the batch could not be read: {err}")) });
            }
        }
    }
    if rep.log_broken.is_none() {
        for pass in 0..MAX_PASSES {
            let mut any_moved = false;
            for r in results.iter_mut() {
                if unreadable.contains(&r.batch) {
                    continue;
                }
                let retry = matches!(&r.end, End::Blocked { rule, .. } if RETRY_AFTER_A_MOVE.contains(&rule.as_str()));
                if pass > 0 && !retry {
                    continue;
                }
                let before = r.states.len();
                if let Err(err) = drive(e, r, o.max_steps) {
                    r.end = End::Failed(err.to_string());
                }
                any_moved |= r.states.len() > before;
            }
            if !any_moved {
                break;
            }
        }
        judge_pipeline_waits(e, &mut results);
    }
    // 4. tidy --auto per host
    for (h, r) in hosts.iter().zip(rep.hosts.iter_mut()) {
        r.tidy = o.seams.tidy_auto(h);
    }
    // 5. at most one advice line (read-only: nothing is recorded, nothing applied)
    for h in &hosts {
        let Ok(inputs) = advise::gather(&*e.ledger, &h.name, Some(h), 3, o.now_us, Vec::new(), Vec::new(), String::new()) else { continue };
        if let Some(a) = advise::advise(&inputs).into_iter().next() {
            rep.advice = Some(format!("{} [{}] {} - {} ({})", h.name, a.rule, a.subject, a.move_line, minutes_text(&a.minutes)));
            break;
        }
    }
    // 6. the cause storm over every running log
    for r in &results {
        if let End::Waits(_) = r.end {
            let Ok(f) = e.fold(&r.batch) else { continue };
            for (rk, lk) in [("run_id", "log"), ("pf_run_id", "pf_log")] {
                let (Some(run), Some(log)) = (f.facts.get(rk).and_then(Value::as_str), f.facts.get(lk).and_then(Value::as_str)) else { continue };
                if let Some(s) = storm(run, Path::new(log), o.rules, o.storm_reds) {
                    if !rep.storms.iter().any(|x| x.run == s.run) {
                        rep.storms.push(s);
                    }
                }
            }
        }
    }
    rep.batches = results;
    rep.lines = lines(&rep);
    Ok(rep)
}

/// A batch refused because of its base waits only while the base is open and on its way: not red, preflight-red, held, abandoned,
/// closed, failed in this tick, or itself blocked on a person. Otherwise its base will not move by itself and a person decides.
fn judge_pipeline_waits(e: &Engine, results: &mut [BatchResult]) {
    let stuck: Vec<(String, bool)> = results.iter().map(|r| (r.batch.clone(), r.needs_person())).collect();
    for r in results.iter_mut() {
        let End::Blocked { rule, person, .. } = &mut r.end else { continue };
        if !RETRY_AFTER_A_MOVE.contains(&rule.as_str()) {
            continue;
        }
        let base = e.fold(&r.batch).ok().and_then(|f| f.facts.get("base_batch").and_then(Value::as_str).map(String::from));
        let on_its_way = base.as_deref().is_some_and(|b| {
            let state = e.fold(b).map(|f| f.state).unwrap_or_default();
            let blocked_here = stuck.iter().any(|(id, p)| id == b && *p);
            !state.is_empty() && !matches!(state.as_str(), "red" | "preflight-red" | "held" | "abandoned" | "closed") && !blocked_here
        });
        *person = !on_its_way;
    }
}

/// Advance one batch until it waits, is refused, fails, or rests; each transition is a row `advance` writes.
fn drive(e: &mut Engine, r: &mut BatchResult, max_steps: usize) -> Result<()> {
    for _ in 0..max_steps {
        match e.advance(&r.batch)? {
            Outcome::Advanced { to, .. } => {
                r.states.push(to.clone());
                r.end = End::Rests(String::new());
                if matches!(to.as_str(), "closed" | "abandoned") {
                    return Ok(());
                }
            }
            Outcome::Waiting(why) => {
                r.end = End::Waits(why);
                return Ok(());
            }
            Outcome::Idle(why) => {
                r.end = End::Rests(why);
                return Ok(());
            }
            Outcome::Refused { rule, because, .. } => {
                let person = !WAITS_ON_ITS_OWN.contains(&rule.as_str());
                let because = cause_of(e, &r.batch, &rule).unwrap_or(because);
                r.end = End::Blocked { rule, because, person };
                return Ok(());
            }
            Outcome::Failed(why) => {
                r.end = End::Failed(why);
                return Ok(());
            }
        }
    }
    // still moving after the cap: a loop or a bug until a person looked, never a silent wait
    r.end = End::Blocked { rule: "tick.step-cap".into(), because: format!("still moving after {max_steps} transitions in one tick; a person looks at `gatetrain batch status {}`", r.batch), person: true };
    Ok(())
}

/// For a red batch, the red steps the proof stored; the rule's own text otherwise.
fn cause_of(e: &Engine, id: &str, rule: &str) -> Option<String> {
    let key = match rule {
        "floor.zero-red" => "findings",
        "preflight.red" => "pf_findings",
        _ => return None,
    };
    let f = e.fold(id).ok()?;
    let reds = f.facts.get(key)?.as_array()?;
    if reds.is_empty() {
        return None;
    }
    let shown: Vec<String> = reds.iter().take(3).map(|r| format!("{} (rc {})", r["step"].as_str().unwrap_or("?"), r["rc"])).collect();
    let more = if reds.len() > 3 { format!(", +{} more", reds.len() - 3) } else { String::new() };
    let what = if key == "findings" { "a person commits a fix and restacks" } else { "a person commits a fix and restacks (a red pre-flight is not run again on the same tip)" };
    Some(format!("red step{} {}{more}; {what}", if reds.len() == 1 { "" } else { "s" }, shown.join(", ")))
}

/// (run id, log) of every launch the batch recorded: proofs (`run_id`, `log`) and launched pre-flights (`pf_run_id`, `pf_log`).
fn runs_of(events: &[Ev]) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for ev in events.iter().filter(|e| e.phase == "done") {
        for (rk, lk) in [("run_id", "log"), ("pf_run_id", "pf_log")] {
            if let (Some(r), Some(l)) = (ev.detail.get(rk).and_then(Value::as_str), ev.detail.get(lk).and_then(Value::as_str)) {
                if !out.iter().any(|(x, _)| x == r) {
                    out.push((r.to_string(), PathBuf::from(l)));
                }
            }
        }
    }
    out
}

/// A log with `GATES_DONE` whose run is not finished in the ledger yet goes in once (`runs::ingest`), with `<log>.samples.jsonl`
/// when present. A run known only as a `running` or `stopped` snapshot (`ingest --watch`) is replaced by its finished rows.
/// `Err(why)`: the log could not be read into the ledger.
fn ingest_finished(e: &Engine, run: &str, log: &Path) -> std::result::Result<bool, String> {
    let Ok(bytes) = std::fs::read(log) else { return Ok(false) };
    // cheap: a log still being written is not parsed at all
    if !bytes.windows(10).any(|w| w == b"GATES_DONE") {
        return Ok(false);
    }
    let p = logfmt::parse_log(&String::from_utf8_lossy(&bytes)).map_err(|err| format!("parse error: {err}"))?;
    if !p.gates_done {
        return Ok(false);
    }
    let replace = match runs::outcome_of(&*e.ledger, run).map_err(|err| err.to_string())? {
        None => false,
        Some(o) if o == "running" || o == "stopped" => true,
        Some(_) => return Ok(false),
    };
    let outcome = history::classify(&p, &Ts::now_utc(), 0.0);
    match runs::ingest(&*e.ledger, "the ledger", run, &p, outcome, None, replace, &runs::now_iso()).map_err(|err| err.to_string())? {
        runs::Ingest::Refused(why) => Err(why),
        runs::Ingest::Done => {
            let mut sf = log.as_os_str().to_owned();
            sf.push(".samples.jsonl");
            if let Ok(text) = std::fs::read_to_string(PathBuf::from(sf)) {
                e.ledger.record_samples_for(&text, Some(run)).map_err(|err| err.to_string())?;
            }
            Ok(true)
        }
    }
}

/// One cause explains at least `threshold` reds of a running log.
fn storm(run: &str, log: &Path, rules: &[Rule], threshold: usize) -> Option<Storm> {
    let text = String::from_utf8_lossy(&std::fs::read(log).ok()?).into_owned();
    let p = logfmt::parse_log(&text).ok()?;
    if p.gates_done {
        return None;
    }
    let findings = diagnose::diagnose_log_in(&text, &p, &diagnose::History::new(), rules, None);
    let (shared, _) = diagnose::shared_causes(&findings);
    let (evidence, steps) = shared.into_iter().next()?;
    (steps.len() >= threshold).then(|| Storm { run: run.to_string(), reds: steps.len(), evidence: cap(&evidence, 100) })
}

fn cap(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        t.to_string()
    } else {
        format!("{}...", t.chars().take(n).collect::<String>())
    }
}

fn minutes_text(m: &Minutes) -> String {
    match m {
        Minutes::Range(lo, hi) if (lo - hi).abs() < 0.05 => format!("{lo:.1} min saved per chain"),
        Minutes::Range(lo, hi) => format!("{lo:.1} to {hi:.1} min saved per chain"),
        Minutes::Unknown(why) => format!("minutes unknown: {why}"),
        Minutes::None => "prevents a problem".into(),
    }
}

fn doctor_text(h: &HostResult) -> String {
    let Some(d) = &h.doctor else { return String::new() };
    let mut parts = Vec::new();
    for status in ["FAIL", "WARN"] {
        let mut checks: Vec<&str> = Vec::new();
        for r in d.iter().filter(|r| r.status == status) {
            if !checks.contains(&r.check.as_str()) {
                checks.push(&r.check);
            }
        }
        let n = d.iter().filter(|r| r.status == status).count();
        if n > 0 {
            let more = if checks.len() > 3 { format!(", +{}", checks.len() - 3) } else { String::new() };
            parts.push(format!("{n} {status} ({}{more})", checks.iter().take(3).copied().collect::<Vec<_>>().join(", ")));
        }
    }
    if parts.is_empty() {
        "ok".into()
    } else {
        parts.join(" ")
    }
}

/// Batch lines (at most [`MAX_BATCH_LINES`]), one host line, one advice line, one storm line when there is a storm: 3 to 6 lines.
fn lines(rep: &TickReport) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(b) = &rep.log_broken {
        out.push(format!("log: {b}; floor.log-append-only: no batch is advanced (gatetrain log verify)"));
    } else if rep.batches.is_empty() {
        out.push("batches: none open (a person opens one with `gatetrain batch new`)".to_string());
    } else if rep.batches.len() <= MAX_BATCH_LINES {
        out.extend(rep.batches.iter().map(BatchResult::line));
    } else {
        let keep = MAX_BATCH_LINES - 1;
        out.extend(rep.batches[..keep].iter().map(BatchResult::line));
        let rest = &rep.batches[keep..];
        let n = |f: &dyn Fn(&BatchResult) -> bool| rest.iter().filter(|b| f(b)).count();
        let mut parts = Vec::new();
        for (count, word) in [
            (n(&|b| b.moved() && matches!(b.end, End::Rests(_))), "moved"),
            (n(&|b| matches!(b.end, End::Waits(_)) || matches!(&b.end, End::Blocked { person: false, .. })), "waiting"),
            (n(&|b| b.needs_person()), "blocked"),
            (n(&|b| !b.moved() && matches!(b.end, End::Rests(_))), "resting"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {word}"));
            }
        }
        out.push(format!("+{} more batch(es): {} (gatetrain batch status)", rest.len(), parts.join(", ")));
    }
    let mut hl: Vec<String> = rep
        .hosts
        .iter()
        .map(|h| match &h.doctor {
            Some(_) => format!("{} {}, tidy {}", h.host, doctor_text(h), h.tidy),
            None => format!("{} no runner (reach {}), tidy {}", h.host, crate::pycompat::repr_str(&h.reach), h.tidy),
        })
        .collect();
    if !rep.ingested.is_empty() {
        hl.push(format!("ingested {} run{}", rep.ingested.len(), if rep.ingested.len() == 1 { "" } else { "s" }));
    }
    if !rep.not_ingested.is_empty() {
        let shown: Vec<String> = rep.not_ingested.iter().take(2).map(|(r, w)| format!("{r} ({})", cap(w, 80))).collect();
        let more = if rep.not_ingested.len() > 2 { format!(", +{} more", rep.not_ingested.len() - 2) } else { String::new() };
        hl.push(format!("not ingested: {}{more}", shown.join(", ")));
    }
    out.push(if hl.is_empty() { "hosts: none enabled".to_string() } else { format!("hosts: {}", hl.join("; ")) });
    out.push(format!("advice: {}", rep.advice.as_deref().unwrap_or("none")));
    if let Some(s) = rep.storms.iter().max_by_key(|s| s.reds) {
        let more = if rep.storms.len() > 1 { format!(" (and {} more run(s))", rep.storms.len() - 1) } else { String::new() };
        out.push(format!("storm: one cause explains {} reds in run {}{more}: {}; stop it: gatetrain stop {} --yes", s.reds, s.run, s.evidence, s.run));
    }
    out
}
