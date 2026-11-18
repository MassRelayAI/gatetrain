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
        Ok(()) => Ok(Some(TickLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
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
    pub advice: Option<String>,
    pub storms: Vec<Storm>,
}

impl TickReport {
    /// 0 normally, 1 when a batch is blocked by a rule that needs a person (or a transition failed).
    pub fn exit_code(&self) -> u8 {
        u8::from(self.batches.iter().any(BatchResult::needs_person))
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
        json!({"lines": self.lines, "batches": batches, "hosts": hosts, "ingested": self.ingested, "advice": self.advice, "storms": storms, "exit": self.exit_code()}).to_string()
    }
}

// ---------------------------------------------------------------- the pass

pub fn tick(e: &mut Engine, o: &TickOpts) -> Result<TickReport> {
    let mut rep = TickReport::default();
    let hosts: Vec<&Host> = o.hosts.hosts.iter().filter(|h| h.enabled).collect();
    // 1. doctor per host
    for h in &hosts {
        rep.hosts.push(HostResult { host: h.name.clone(), reach: h.reach.clone(), doctor: o.seams.doctor(h), tidy: String::new() });
    }
    // 2. ingest the finished logs (and their samples) of every batch's runs
    let ids = e.batch_ids()?;
    for id in &ids {
        for (run, log) in runs_of(&e.events(id)?) {
            if ingest_finished(e, &run, &log)? {
                rep.ingested.push(run);
            }
        }
    }
    // 3. advance every open batch through the state machine's own `advance`
    let open: Vec<String> = ids.into_iter().filter(|id| e.fold(id).map(|f| !matches!(f.state.as_str(), "closed" | "abandoned")).unwrap_or(false)).collect();
    let mut results: Vec<BatchResult> = Vec::new();
    for id in &open {
        let start = e.fold(id)?.state;
        results.push(BatchResult { batch: id.clone(), states: vec![start], end: End::Rests(String::new()) });
    }
    for pass in 0..MAX_PASSES {
        let mut any_moved = false;
        for r in results.iter_mut() {
            let retry = matches!(&r.end, End::Blocked { rule, .. } if RETRY_AFTER_A_MOVE.contains(&rule.as_str()));
            if pass > 0 && !retry {
                continue;
            }
            let before = r.states.len();
            drive(e, r)?;
            any_moved |= r.states.len() > before;
        }
        if !any_moved {
            break;
        }
    }
    // 4. tidy --auto per host
    for (h, r) in hosts.iter().zip(rep.hosts.iter_mut()) {
        r.tidy = o.seams.tidy_auto(h);
    }
    // 5. at most one advice line (read-only: nothing is recorded, nothing applied)
    for h in &hosts {
        let inputs = advise::gather(&*e.ledger, &h.name, Some(h), 3, o.now_us, Vec::new(), Vec::new(), String::new())?;
        if let Some(a) = advise::advise(&inputs).into_iter().next() {
            rep.advice = Some(format!("{} [{}] {} - {} ({})", h.name, a.rule, a.subject, a.move_line, minutes_text(&a.minutes)));
            break;
        }
    }
    // 6. the cause storm over every running log
    for r in &results {
        if let End::Waits(_) = r.end {
            let f = e.fold(&r.batch)?;
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

/// Advance one batch until it waits, is refused, fails, or rests; each transition is a row `advance` writes.
fn drive(e: &mut Engine, r: &mut BatchResult) -> Result<()> {
    for _ in 0..MAX_STEPS {
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
    r.end = End::Waits(format!("stopped after {MAX_STEPS} transitions in one tick; the next tick goes on"));
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

/// A log with `GATES_DONE` whose run is not in the ledger yet goes in once (`runs::ingest`), with `<log>.samples.jsonl` when present.
fn ingest_finished(e: &Engine, run: &str, log: &Path) -> Result<bool> {
    let Ok(bytes) = std::fs::read(log) else { return Ok(false) };
    let p = logfmt::parse_log(&String::from_utf8_lossy(&bytes))?;
    if !p.gates_done || runs::outcome_of(&*e.ledger, run)?.is_some() {
        return Ok(false);
    }
    let outcome = history::classify(&p, &Ts::now_utc(), 0.0);
    match runs::ingest(&*e.ledger, "the ledger", run, &p, outcome, None, false, &runs::now_iso())? {
        runs::Ingest::Refused(_) => Ok(false),
        runs::Ingest::Done => {
            let mut sf = log.as_os_str().to_owned();
            sf.push(".samples.jsonl");
            if let Ok(text) = std::fs::read_to_string(PathBuf::from(sf)) {
                e.ledger.record_samples_for(&text, Some(run))?;
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
    if rep.batches.is_empty() {
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
    out.push(if hl.is_empty() { "hosts: none enabled".to_string() } else { format!("hosts: {}", hl.join("; ")) });
    out.push(format!("advice: {}", rep.advice.as_deref().unwrap_or("none")));
    if let Some(s) = rep.storms.iter().max_by_key(|s| s.reds) {
        let more = if rep.storms.len() > 1 { format!(" (and {} more run(s))", rep.storms.len() - 1) } else { String::new() };
        out.push(format!("storm: one cause explains {} reds in run {}{more}: {}; stop it: gatetrain stop {} --yes", s.reds, s.run, s.evidence, s.run));
    }
    out
}
