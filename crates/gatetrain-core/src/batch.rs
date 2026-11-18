//! The batch state machine (design 3). State is derived by folding the append-only `batch_events`; there is no status
//! column. Every transition that has an effect writes `intent` before and `done` after; a crash between the two is
//! resolved on the next `advance` by a read-only probe (design 3.4). Guards come from the floor rules (design 6.2).
//!
//! The holder, the host lock and admission are not here: they are the traits [`HostLock`] and [`Admission`], with
//! fakes for tests. The holder lane (rs2) implements them against the real lock file and `hosts.toml`.
use crate::batchcfg::BatchConfig;
use crate::declog::{self, NewEntry};
use crate::evidence;
use crate::tidy::close::{self, CloseEnv, OwnedItem, OwnedSet};
use crate::ledger::Ledger;
use crate::logfmt::parse_log;
use crate::{Error, Result};
use regex::Regex;
use rusqlite::params;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
#[cfg(any(test, feature = "test-fakes"))]
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(test, feature = "test-fakes"))]
use std::sync::Mutex;

// ---------------------------------------------------------------- seams for the holder lane (rs2)

/// What the engine asks the holder to start (design 4.3). The run id is fixed before launch.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub run_id: String,
    pub batch: String,
    pub sha: String,
    pub base: String,
    pub command: Vec<String>,
    pub log: PathBuf,
    pub workdir: PathBuf,
    pub env: Vec<(String, String)>,
    /// `proof` (T4, T6a) or `light` (the launched pre-flight, T3): which host and lock the run belongs on.
    pub role: String,
}

/// The host lock and holder as the state machine sees them. rs2 implements this over the real lock file.
pub trait HostLock {
    /// Does a live lock name this run id? (the probe of T4 and the "still running" test of T5)
    fn names_run(&self, run_id: &str) -> Result<bool>;
    /// Start the chain under the holder. Must not block until the chain ends; the engine waits for the holder to name the run.
    fn launch(&self, req: &LaunchRequest) -> Result<()>;
    /// Stop a launched holder that did not name its run in time, so a launch that failed leaves nothing running. Default: nothing.
    fn abort(&self, _run_id: &str) -> Result<()> {
        Ok(())
    }
}

/// "A host has a free slot and passes doctor for this role" (T3 role `light`, T4 role `proof`). rs2 implements it.
pub trait Admission {
    /// `None` admits; `Some(reason)` refuses with that reason.
    fn admit(&self, role: &str, batch: &str) -> Result<Option<String>>;
    /// The same decision with the rule that refused: `host.refuse` (lock busy, exit 75), `host.refuse-memory` (76), `host.refuse-disk` (77),
    /// `host.refuse-cwd` (78), `host.refuse-doctor` (79), else `host.admission`. These are the exit codes of `gatetrain hold`.
    fn admit_rule(&self, role: &str, batch: &str) -> Result<Option<(String, String)>> {
        Ok(self.admit(role, batch)?.map(|why| ("host.admission".to_string(), why)))
    }
    /// Run `tidy --auto` once on the host the run needs (only the unattended classes, never a bulk prune). `None` = this seam cannot tidy;
    /// `Some(note)` = it did, with what it reclaimed. Called by the disk floor after a `host.refuse-disk`, when policy allows it.
    fn tidy_auto(&self, _role: &str) -> Result<Option<String>> {
        Ok(None)
    }
    /// Why there is no close environment, when the seam knows (an unknown reach, a tidy config that does not build).
    fn close_env_error(&self) -> Option<String> {
        None
    }
    /// What T9 close probes and acts through; `None` = this seam cannot tidy (close then refuses).
    fn close_env(&self) -> Option<&dyn CloseEnv> {
        None
    }
}

/// Admits everything: a test double, compiled only for tests (feature `test-fakes`); the engine of the binary is given the real seams.
#[cfg(any(test, feature = "test-fakes"))]
pub struct AllowAll;
#[cfg(any(test, feature = "test-fakes"))]
impl Admission for AllowAll {
    fn admit(&self, _: &str, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// A fake lock for tests: records launches; `named` is the set of run ids a live lock names. `late_polls` = n > 0 models a holder that
/// was spawned but takes the host lock late: a launched run is named only after n `names_run` questions (`usize::MAX`: never).
#[cfg(any(test, feature = "test-fakes"))]
#[derive(Default)]
pub struct FakeLock {
    pub named: Mutex<HashSet<String>>,
    pub launches: Mutex<Vec<LaunchRequest>>,
    pub late_polls: Mutex<usize>,
    pub starting: Mutex<HashMap<String, usize>>,
    pub aborted: Mutex<Vec<String>>,
}
#[cfg(any(test, feature = "test-fakes"))]
impl HostLock for FakeLock {
    fn names_run(&self, run_id: &str) -> Result<bool> {
        if self.named.lock().unwrap().contains(run_id) {
            return Ok(true);
        }
        let mut st = self.starting.lock().unwrap();
        if let Some(n) = st.get_mut(run_id) {
            if *n == usize::MAX {
                return Ok(false);
            }
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.remove(run_id);
                self.named.lock().unwrap().insert(run_id.to_string());
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn launch(&self, req: &LaunchRequest) -> Result<()> {
        self.launches.lock().unwrap().push(req.clone());
        let late = *self.late_polls.lock().unwrap();
        if late == 0 {
            self.named.lock().unwrap().insert(req.run_id.clone());
        } else {
            self.starting.lock().unwrap().insert(req.run_id.clone(), late);
        }
        Ok(())
    }
    fn abort(&self, run_id: &str) -> Result<()> {
        self.starting.lock().unwrap().remove(run_id);
        self.aborted.lock().unwrap().push(run_id.to_string());
        Ok(())
    }
}

/// Fake admission: refuses one role with a reason.
#[cfg(any(test, feature = "test-fakes"))]
pub struct DenyRole(pub &'static str, pub &'static str);
#[cfg(any(test, feature = "test-fakes"))]
impl Admission for DenyRole {
    fn admit(&self, role: &str, _: &str) -> Result<Option<String>> {
        Ok(if role == self.0 { Some(self.1.to_string()) } else { None })
    }
}

// ---------------------------------------------------------------- events and folding

#[derive(Debug, Clone)]
pub struct Ev {
    pub seq: i64,
    pub transition: String,
    pub phase: String,
    pub attempt: i64,
    pub state_to: Option<String>,
    pub input_sha: Option<String>,
    pub output_sha: Option<String>,
    pub run_id: Option<String>,
    pub decision_seq: Option<i64>,
    pub detail: Value,
}

#[derive(Debug, Clone)]
pub struct Folded {
    pub state: String,
    pub held_from: Option<String>,
    pub pending: Option<Ev>,
    pub facts: Map<String, Value>,
    pub max_attempt: HashMap<String, i64>,
    pub events: usize,
}

impl Folded {
    fn s(&self, k: &str) -> Option<String> {
        self.facts.get(k).and_then(|v| v.as_str()).map(String::from)
    }
    fn need(&self, k: &str) -> Result<String> {
        self.s(k).ok_or_else(|| Error::Batch(format!("batch fact `{k}` is missing")))
    }
}

/// Fold the events of one batch: the state is the newest `done`, a later `intent` without a `done` is pending.
pub fn fold(events: &[Ev]) -> Option<Folded> {
    if events.is_empty() {
        return None;
    }
    let mut f = Folded { state: String::new(), held_from: None, pending: None, facts: Map::new(), max_attempt: HashMap::new(), events: events.len() };
    for e in events {
        match e.phase.as_str() {
            "intent" => {
                f.pending = Some(e.clone());
                let m = f.max_attempt.entry(e.transition.clone()).or_insert(0);
                *m = (*m).max(e.attempt);
            }
            "done" | "failed" => {
                if f.pending.as_ref().is_some_and(|p| p.transition == e.transition && p.attempt == e.attempt) {
                    f.pending = None;
                }
                if e.phase == "done" {
                    let m = f.max_attempt.entry(e.transition.clone()).or_insert(0);
                    *m = (*m).max(e.attempt);
                    if let Some(to) = &e.state_to {
                        if to == "held" {
                            f.held_from = Some(f.state.clone());
                        }
                        f.state = to.clone();
                    }
                    if !matches!(e.transition.as_str(), "hold" | "release" | "abandon" | "restack" | "retry") {
                        if let Some(o) = e.detail.as_object() {
                            for (k, v) in o {
                                f.facts.insert(k.clone(), v.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(f)
}

// ---------------------------------------------------------------- outcomes

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Advanced { from: String, to: String, note: String },
    Waiting(String),
    Idle(String),
    /// `clears_by_itself`: the refusal goes away without a person (a busy host, a base batch still on its way); a tick waits on it.
    Refused { rule: String, because: String, decision: i64, clears_by_itself: bool },
    Failed(String),
}

impl Outcome {
    /// 0 advanced or nothing to do, 1 failed, 3 refused by a rule (design 6.7); a refusal by the host seams uses the exit codes of
    /// `gatetrain hold`: 75 lock busy, 76 memory, 77 disk, 78 working directory, 79 doctor.
    pub fn exit_code(&self) -> u8 {
        match self {
            Outcome::Refused { rule, .. } => match rule.as_str() {
                "host.refuse" | "host.refuse-proof-running" => 75,
                "host.refuse-memory" => 76,
                "host.refuse-disk" | "host.disk-unknown" => 77,
                "host.refuse-cwd" => 78,
                "host.refuse-doctor" => 79,
                _ => 3,
            },
            Outcome::Failed(_) => 1,
            _ => 0,
        }
    }
}

/// Refusals that clear by themselves (a busy host, a base batch still on its way); every other refusal needs a person.
pub const CLEARS_BY_ITSELF: &[&str] = &["host.refuse", "host.refuse-proof-running", "host.refuse-memory", "pipeline.base-unproven", "pipeline.base-not-merged", "deploy.still-running", "deploy.another-batch-deploying"];

/// The transition with an effect that `advance` takes from `state`, if any.
fn next_effect(state: &str) -> Option<&'static str> {
    Some(match state {
        "open" => "T1",
        "stacked" => "T2",
        "settled" => "T3",
        "preflighted" => "T4",
        "red" => "T6a",
        "proven" => "T7",
        "merged" => "T8a",
        _ => return None,
    })
}

/// The checkpoint subject of the post-merge command (a target name never starts with `(`).
const POSTMERGE: &str = "(postmerge)";

enum Planned {
    Go(Plan),
    Refuse(String, String),
}

#[derive(Debug, Clone)]
struct Plan {
    tr: String,
    to: String,
    input_sha: Option<String>,
    run_id: Option<String>,
    attempt: i64,
    detail: Value,
}

struct Done {
    output: Option<String>,
    extra: Value,
}

enum Probe {
    Happened(Option<String>),
    NotHappened,
    Partly,
    /// The probe cannot tell whether the effect happened (a holder that opened its log but holds no lock yet, a deploy target a crash
    /// may have left half done): the attempt is closed as `interrupted` and is run again only when the effect is declared idempotent.
    Unknown(String),
    /// The effect is still running in a process a dead gatetrain left behind: neither settled nor safe to start again, so it waits.
    Running(String),
    /// A record of the effect that nothing can verify (no pid, no start token, unreadable): it never clears by itself, a person decides.
    Unverifiable(String),
}

/// What clearing a batch's deploy records did.
enum Cleared {
    /// the batch has none
    None,
    /// a record still runs or cannot be verified: the refusal
    Refused(Outcome),
    /// cleared, under the override row `decision`
    Done { decision: i64, what: Vec<String>, message: String },
}

/// What a record of a running T8a step (`<state>/running/<target>.json`) says about its process.
enum Rec {
    /// The process and its group are gone (or a later process holds the pid).
    Gone,
    /// It still runs: its leader under its start token, or a member of its process group.
    Live(String),
    /// Nothing can tell whether it runs: no pid or no start token was recorded, or the record cannot be read.
    Unverifiable(String),
}

pub struct Engine<'a> {
    pub ledger: &'a mut Ledger,
    pub cfg: &'a BatchConfig,
    pub lock: &'a dyn HostLock,
    pub adm: &'a dyn Admission,
    pub now: &'a dyn Fn() -> String,
    /// Stamps and chain logs live here.
    pub state_dir: PathBuf,
    pub actor: String,
    /// The heartbeat of the ledger lock this engine runs under ([`crate::tick::TickLock::beat`]): while a plugin runs it names the
    /// batch, the transition, the step and the step's deadline, so a holder that beats but is stuck past its deadline is seen.
    pub progress: Option<crate::tick::Beat>,
}

/// Crash injection for tests only (feature `test-fakes`): the engine "dies" at a named point, so no row after it is written. The point
/// is set per thread, so tests that run side by side never see each other's crash. Points: `<tr>` right after the effect of a
/// transition, before its `done`; `before:<tr>` right after its intent, before the effect; `T8a:<target>` right after that target
/// deployed. The production build has no such field, struct or error.
#[cfg(any(test, feature = "test-fakes"))]
pub mod crash_seam {
    use std::cell::RefCell;
    thread_local! {
        static AT: RefCell<Option<String>> = const { RefCell::new(None) };
    }
    /// Crash at `point` from now on in this thread (`None`: never).
    pub fn set(point: Option<String>) {
        AT.with(|a| *a.borrow_mut() = point);
    }
    pub(crate) fn hit(point: &str) -> crate::Result<()> {
        if AT.with(|a| a.borrow().as_deref() == Some(point)) {
            return Err(crate::Error::SimulatedCrash(point.to_string()));
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-fakes"))]
fn crash_point(point: &str) -> Result<()> {
    crash_seam::hit(point)
}

#[cfg(not(any(test, feature = "test-fakes")))]
#[inline(always)]
fn crash_point(_point: &str) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------- helpers

pub fn system_now() -> String {
    iso_of(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SS+00:00`.
pub fn iso_of(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// What a git call runs under: its time limit, the heartbeat cell it is named in, and the batch it is for. Set per thread by the
/// engine's entry points ([`Engine::git_scope`]); without one a git call still has the default `git_min` limit (30 min).
struct GitCtx {
    secs: f64,
    progress: Option<crate::tick::Beat>,
    batch: String,
}

thread_local! {
    /// Did the current T8a attempt start a plugin (post-merge, a deploy)? A failure after that is `failed-dirty`.
    static T8A_SPAWNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static GIT_CTX: std::cell::RefCell<Option<GitCtx>> = const { std::cell::RefCell::new(None) };
}

/// Restores the git context it replaced when it drops.
struct GitScope(Option<GitCtx>);

impl Drop for GitScope {
    fn drop(&mut self) {
        let prev = self.0.take();
        GIT_CTX.with(|c| *c.borrow_mut() = prev);
    }
}

/// Every git call of the engine: a time limit (`[timeouts] git_min`; git runs hooks, which can hang), a process group of its
/// own so the limit ends the hook too, and named in the heartbeat while it runs (`<batch> git <subcommand>`, its deadline).
fn git_run(dir: &Path, args: &[&str]) -> Result<(bool, Vec<u8>, Vec<u8>)> {
    let (secs, progress, batch) = GIT_CTX.with(|c| c.borrow().as_ref().map(|g| (g.secs, g.progress.clone(), g.batch.clone()))).unwrap_or((30.0 * 60.0, None, String::new()));
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(args);
    let sub = args.iter().find(|a| !a.starts_with('-') && !a.contains('=')).copied().unwrap_or("");
    let since = epoch_ms();
    if let Some(b) = &progress {
        if let Ok(mut g) = b.lock() {
            *g = Some(json!({"effect": format!("{batch} git {sub}").trim_start().to_string(), "since_ms": since, "deadline_ms": since + (secs * 1000.0) as i64}));
        }
    }
    let r = crate::runner::run_isolated_raw(c, secs);
    if let Some(b) = &progress {
        if let Ok(mut g) = b.lock() {
            *g = None;
        }
    }
    match r.map_err(|e| Error::Batch(format!("git {}: {e}", args.join(" "))))? {
        crate::runner::IsolatedRaw::Done { rc, stdout, stderr } => Ok((rc == 0, stdout, stderr)),
        crate::runner::IsolatedRaw::TimedOut => Err(Error::Batch(format!(
            "git {} timed out after {} min ([timeouts] git_min; its hooks included); the local process tree was signalled",
            args.join(" "),
            secs / 60.0
        ))),
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, err) = git_raw(dir, args)?;
    if ok {
        Ok(out)
    } else {
        Err(Error::Batch(format!("git {}: {}", args.join(" "), err.trim())))
    }
}

fn git_raw(dir: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let (ok, out, err) = git_run(dir, args)?;
    Ok((ok, String::from_utf8_lossy(&out).trim().to_string(), String::from_utf8_lossy(&err).into_owned()))
}

/// Paths git reports as changed or untracked (new files listed individually).
fn dirty_paths(dir: &Path) -> Result<Vec<String>> {
    let (ok, out, err) = git_run(dir, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"])?;
    if !ok {
        return Err(Error::Batch(format!("git status: {}", String::from_utf8_lossy(&err).trim())));
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    let mut out = vec![];
    let mut it = text.split('\0').filter(|s| !s.is_empty());
    while let Some(e) = it.next() {
        if e.len() < 4 {
            continue;
        }
        out.push(e[3..].to_string());
        if matches!(e.as_bytes()[0], b'R' | b'C') {
            it.next(); // the rename source
        }
    }
    Ok(out)
}

/// Raw bytes of a git command (a blob must not be trimmed or re-encoded).
fn git_bytes(dir: &Path, args: &[&str]) -> Result<(bool, Vec<u8>)> {
    let (ok, out, _) = git_run(dir, args)?;
    Ok((ok, out))
}

/// Paths with an unresolved merge conflict.
fn unmerged_paths(dir: &Path) -> Result<Vec<String>> {
    Ok(git(dir, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// `**/` any folders, `**` anything, `*` anything but a slash, `?` one character but a slash.
fn glob_match(glob: &str, path: &str) -> bool {
    let mut re = String::from("^");
    let cs: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '*' if cs.get(i + 1) == Some(&'*') => {
                if cs.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).map(|r| r.is_match(path)).unwrap_or(false)
}

/// A conflict marker line left in a file (the start or the end of a conflict).
fn has_markers(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> "))
}

/// Files whose diff `base..tip` ADDS both a start and an end conflict marker (a file with markers was committed).
fn marker_paths(dir: &Path, base: &str, tip: &str) -> Result<Vec<String>> {
    let diff = git(dir, &["diff", "-U0", "--no-color", "--no-renames", base, tip])?;
    let (mut out, mut cur, mut start, mut end) = (vec![], None::<String>, false, false);
    let flush = |cur: &mut Option<String>, start: &mut bool, end: &mut bool, out: &mut Vec<String>| {
        if let Some(c) = cur.take() {
            if *start && *end {
                out.push(c);
            }
        }
        *start = false;
        *end = false;
    };
    for l in diff.lines() {
        if let Some(p) = l.strip_prefix("+++ b/") {
            flush(&mut cur, &mut start, &mut end, &mut out);
            cur = Some(p.to_string());
        } else if l.starts_with("+<<<<<<< ") {
            start = true;
        } else if l.starts_with("+>>>>>>> ") {
            end = true;
        }
    }
    flush(&mut cur, &mut start, &mut end, &mut out);
    Ok(out)
}

/// The worktree that has the protected branch checked out, if any.
fn protected_checkout(dir: &Path, protected: &str) -> Result<Option<String>> {
    let out = git(dir, &["worktree", "list", "--porcelain"])?;
    let want = format!("branch refs/heads/{protected}");
    let mut path: Option<String> = None;
    for l in out.lines() {
        if let Some(p) = l.strip_prefix("worktree ") {
            path = Some(p.trim().to_string());
        } else if l.trim() == want {
            return Ok(path);
        }
    }
    Ok(None)
}

/// `YYYY-MM-DDTHH:MM:SS[.fff](Z|+HH:MM|-HH:MM)` as seconds since the epoch.
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut secs = days * 86400 + h * 3600 + mi * 60 + se;
    let mut rest = s.get(19..)?;
    if let Some(r) = rest.strip_prefix('.') {
        rest = r.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let off = n(s.len() - rest.len() + 1, s.len() - rest.len() + 3)? * 3600 + n(s.len() - rest.len() + 4, s.len() - rest.len() + 6)? * 60;
        secs -= if sign == '+' { off } else { -off };
    }
    Some(secs)
}

fn in_dirs(path: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|d| path.starts_with(&format!("{}/", d.trim_end_matches('/'))))
}

/// The default limit of a deploy command without `timeout_min` (a tick refuses such a target, `[tick] enabled`).
pub const DEPLOY_TIMEOUT_MIN: f64 = 60.0;

fn epoch_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Write a record whole: a temporary file in the same folder, flushed to disk, then renamed over the record, so a reader (or a crash)
/// never sees half of one.
fn write_record(path: &Path, v: &Value) -> std::result::Result<(), String> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("record")));
    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    f.write_all(v.to_string().as_bytes()).and_then(|_| f.sync_all()).map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Does the holder's log say it holds `run` (`HOLDS <run>`, written once the holder took the host lock)?
fn holder_named(log: &Path, run: &str) -> bool {
    let want = format!("HOLDS {run}");
    std::fs::read(log).map(|b| String::from_utf8_lossy(&b).lines().any(|l| l.trim() == want)).unwrap_or(false)
}

fn substitute(cmd: &[String], sha: &str) -> Vec<String> {
    cmd.iter().map(|a| a.replace("{sha}", sha)).collect()
}

fn to_state(tr: &str) -> &'static str {
    match tr {
        "T1" => "stacked",
        "T2" => "settled",
        "T3" => "preflighted",
        "T4" | "T6a" => "proving",
        "T7" => "merged",
        "T8a" => "deploying",
        _ => "open",
    }
}

/// The state a transition leads to: a launched pre-flight (T3 with a run id) leads to `preflighting`, not `preflighted`.
fn to_for(tr: &str, detail: &Value) -> &'static str {
    if tr == "T3" && detail.get("pf_run_id").is_some() {
        "preflighting"
    } else {
        to_state(tr)
    }
}

fn from_state(tr: &str) -> &'static str {
    match tr {
        "T1" => "open",
        "T2" => "stacked",
        "T3" => "settled",
        "T4" => "preflighted",
        "T6a" => "red",
        "T7" => "proven",
        "T8a" => "merged",
        _ => "",
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

impl<'a> Engine<'a> {
    /// Every plugin command runs here: with a time limit (`min` minutes), in a process group (a job object on Windows) of its own so
    /// a timeout ends everything it started, and named in the heartbeat while it runs (`<batch> <transition> <step>`, its deadline).
    /// `Ok(Err(why))` = it ran out of time, or the limit is not a usable number (fail closed: nothing ran); `Ok(Ok((rc, why)))` = it
    /// ended, with its stderr (else its stdout) as the reason. The deadline in the heartbeat is wall-clock milliseconds, because
    /// another process reads it; the limit itself is measured on this process's monotonic clock.
    #[allow(clippy::too_many_arguments)]
    fn plugin(&self, id: &str, tr: &str, step: &str, key: &str, cmd: &[String], dir: &Path, env: &[(String, String)], min: f64) -> Result<std::result::Result<(i32, String), String>> {
        self.plugin_as(id, tr, step, key, cmd, dir, env, min, false)
    }

    /// [`Engine::plugin`], for a step that may outlive this process (`survives_parent`). A T8a step (the post-merge command, a probe, a
    /// deploy) also records the process it runs in `<state>/running/<target>.json` (one file per target, whichever batch runs it: the
    /// batch, the sha, the pid and its start token) while it runs, so if this process dies the next advance of ANY batch waits for
    /// that deploy instead of starting a second one beside it on the same target.
    #[allow(clippy::too_many_arguments)]
    fn plugin_as(&self, id: &str, tr: &str, step: &str, key: &str, cmd: &[String], dir: &Path, env: &[(String, String)], min: f64, survives_parent: bool) -> Result<std::result::Result<(i32, String), String>> {
        let noun = if step == "postmerge" { "post-merge".to_string() } else { step.to_string() };
        let secs = min * 60.0;
        if !(secs.is_finite() && secs > 0.0) || std::time::Duration::try_from_secs_f64(secs).is_err() {
            return Ok(Err(format!("{noun}: {min} min{key} is not a usable time limit; nothing was run")));
        }
        let (prog, args) = cmd.split_first().ok_or_else(|| Error::Batch(format!("{noun}: empty plugin command")))?;
        let mut c = Command::new(prog);
        c.args(args).current_dir(dir).envs(env.iter().map(|(k, v)| (k, v)));
        let since = epoch_ms();
        let target = step.strip_prefix("deploy ").or_else(|| step.strip_prefix("probe ")).unwrap_or(if step == "postmerge" { POSTMERGE } else { step });
        let sha = env.iter().find(|(k, _)| k == "GATETRAIN_SHA").map(|(_, v)| v.clone()).unwrap_or_default();
        let record = (tr == "T8a").then(|| self.running_path(target));
        let marker = json!({"step": step, "batch": id, "target": target, "sha": sha, "since_ms": since, "survives_parent": survives_parent});
        if let Some(p) = &record {
            // the intent marker, before anything runs: a crash from here until the pid is written leaves a record without a pid,
            // which every later advance reads as "may be running" (it waits), never as "did not happen"
            if let Err(e) = write_record(p, &marker) {
                return Ok(Err(format!("{noun}: its record {} cannot be written ({e}); nothing was run", p.display())));
            }
            crash_point(&format!("T8a:starting:{step}"))?;
        }
        if let Some(b) = &self.progress {
            if let Ok(mut g) = b.lock() {
                *g = Some(json!({"effect": format!("{id} {tr} {step}"), "since_ms": since, "deadline_ms": since + (secs * 1000.0) as i64}));
            }
        }
        let crashed: std::cell::RefCell<Option<Error>> = std::cell::RefCell::new(None);
        // the pid (and its start token) is recorded before the plugin runs on: on Windows while it is still suspended; a record that
        // cannot be written ends it and fails the step
        let mut on_start = |pid: u32| -> std::result::Result<(), String> {
            if tr == "T8a" {
                T8A_SPAWNED.with(|c| c.set(true));
            }
            if let Some(p) = &record {
                let mut v = marker.clone();
                v["pid"] = json!(pid);
                v["token"] = json!(crate::runner::process_token(pid));
                crash_point(&format!("T8a:record:{step}"))
                    .map_err(|_| "the test seam refused the write".to_string())
                    .and_then(|_| write_record(p, &v))
                    .map_err(|e| format!("the pid {pid} could not be recorded in {} ({e}); the plugin was ended", p.display()))?;
            }
            Ok(())
        };
        let mut on_spawn = |_pid: u32| -> std::result::Result<(), String> {
            if let Err(e) = crash_point(&format!("{tr}:spawned:{step}")) {
                *crashed.borrow_mut() = Some(e);
                return Err("the parent died".into());
            }
            Ok(())
        };
        let r = crate::runner::run_isolated_with(c, secs, crate::runner::Opts { survives_parent, on_spawn: Some(&mut on_spawn), on_start: Some(&mut on_start) });
        if let Some(e) = crashed.borrow_mut().take() {
            return Err(e); // a test's crash: the process "died" with the plugin running, its record left behind
        }
        if let Some(p) = &record {
            // a T8a step that succeeded marks its record `deployed <sha>` as its last act, before the caller writes the checkpoint row:
            // a crash in between leaves a record that the next advance turns into that checkpoint (`adopt_deployed`), so
            // `deploy.superseded` and the probe see the deploy. Any other ending removes the record.
            let ok = matches!(&r, Ok(crate::runner::IsolatedRaw::Done { rc: 0, .. }));
            let marked = ok && {
                let mut v = marker.clone();
                v["deployed"] = json!(sha);
                write_record(p, &v).is_ok()
            };
            if !marked {
                let _ = std::fs::remove_file(p);
            }
        }
        if let Some(b) = &self.progress {
            if let Ok(mut g) = b.lock() {
                *g = None;
            }
        }
        Ok(match r.map_err(|e| Error::Batch(format!("{noun}: {e}")))? {
            crate::runner::IsolatedRaw::Done { rc, stdout, stderr } => {
                let (o, e) = (String::from_utf8_lossy(&stdout).trim().to_string(), String::from_utf8_lossy(&stderr).trim().to_string());
                Ok((rc, if e.is_empty() { o } else { e }))
            }
            crate::runner::IsolatedRaw::TimedOut => Err(format!("{noun} timed out after {min} min{key}; the local process tree was signalled; a remote or detached deploy may still run")),
        })
    }

    /// Where a running T8a step on `target` (a deploy target, or [`POSTMERGE`]) is recorded, whichever batch runs it: a deploy that
    /// is running is a fact about the target, not about one batch.
    fn running_path(&self, target: &str) -> PathBuf {
        let safe: String = target.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_string() } else { format!("%{:02x}", c as u32) }).collect();
        self.state_dir.join("running").join(format!("{safe}.json"))
    }

    /// Every record of a running T8a step, of every batch (`<state>/running/*.json`; a file of an older release is read too).
    fn running_records(&self) -> Vec<(PathBuf, Value)> {
        let Ok(dir) = std::fs::read_dir(self.state_dir.join("running")) else { return vec![] };
        let mut out: Vec<(PathBuf, Value)> = dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')))
            .map(|p| {
                let v = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).unwrap_or(Value::Null);
                (p, v)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// What a record says about its process. The leader is the recorded pid under its recorded start token: a record without a pid
    /// (gatetrain stopped between the intent marker and the spawn), without a token (it could not be read at spawn) or that cannot be
    /// read is not trusted (`Unverifiable`); a pid that a later process holds (another token) is gone, its group too (the kernel does
    /// not reuse a pid while its process group lives). Its process group counts only for a step that does not survive its parent: a
    /// `survives_parent` deploy leaves its service running on purpose, so only its leader is judged.
    fn record_state(&self, path: &Path, v: &Value) -> Rec {
        let step = v["step"].as_str().unwrap_or("a deploy step");
        let Some(batch) = v["batch"].as_str() else {
            return Rec::Unverifiable(format!("the record {} cannot be read; nothing clears it by itself: a person checks that no deploy runs and removes it", path.display()));
        };
        // a held, abandoned or closed batch is never retried: its record is cleared by `clear-record`
        let parked = self.fold(batch).is_ok_and(|f| matches!(f.state.as_str(), "held" | "abandoned" | "closed"));
        let cmd = |flag: &str| if parked { format!("`gatetrain batch clear-record {batch} --reason <why>{flag}`") } else { format!("`gatetrain batch advance {batch} --retry{flag} --reason <why>`") };
        if v["deployed"].as_str().is_some() {
            return Rec::Gone; // the step ended and succeeded; only its checkpoint row is missing (`adopt_deployed`)
        }
        let unverifiable = format!("A person checks that it is not running and runs {}, which clears the record", cmd(" --clear-unverifiable-record"));
        let Some(pid) = v["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) else {
            return Rec::Unverifiable(format!("{step} of batch {batch} was starting when its gatetrain stopped, before its pid was recorded: it may be running, and nothing can tell, so nothing clears it by itself. {unverifiable}"));
        };
        let (group, end) = if cfg!(unix) { (format!(" (process group {pid})"), format!("`kill -KILL -- -{pid}`")) } else { (String::new(), format!("`taskkill /T /F /PID {pid}`")) };
        let Some(token) = v["token"].as_u64() else {
            return Rec::Unverifiable(format!("{step} of batch {batch} was recorded as pid {pid}{group} without a start token, so it cannot be told from a later process that reuses the pid, and nothing clears it by itself. {unverifiable}"));
        };
        let clear = format!("If it is stuck, a person ends it ({end}) and runs {}, which clears the record", cmd(""));
        if crate::runner::process_alive(pid, Some(token)) {
            return Rec::Live(format!("{step} of batch {batch} is still running as pid {pid}{group}, started by a gatetrain that is gone; the next step waits for it. {clear}"));
        }
        if crate::runner::process_token(pid).is_some_and(|now| now != token) {
            return Rec::Gone; // a later process holds the pid: the recorded one and its group are gone
        }
        let survives = v["survives_parent"].as_bool().unwrap_or(false);
        if !survives && crate::runner::group_alive(pid) {
            return Rec::Live(format!("{step} of batch {batch}: its leader pid {pid} ended, but its process group {pid} still has members; the next step waits for them. {clear}"));
        }
        Rec::Gone
    }

    /// A T8a step of batch `id` a dead process left running: `Some((rule, what))` while it lives (`deploy.still-running`, clears by
    /// itself) or while nothing can tell (`deploy.record-unverifiable`, a person decides at once). A live record is named first.
    fn deploy_still_running(&self, id: &str) -> Option<(&'static str, String)> {
        Self::held_by(self.running_records().iter().filter(|(_, v)| v["batch"].as_str() == Some(id)).map(|(p, v)| self.record_state(p, v)), "deploy.still-running")
    }

    /// The first record that holds: a live one under `live_rule`, else an unverifiable one under `deploy.record-unverifiable`.
    fn held_by(recs: impl Iterator<Item = Rec>, live_rule: &'static str) -> Option<(&'static str, String)> {
        let mut unseen = None;
        for r in recs {
            match r {
                Rec::Live(w) => return Some((live_rule, w)),
                Rec::Unverifiable(w) => {
                    unseen.get_or_insert(w);
                }
                Rec::Gone => {}
            }
        }
        unseen.map(|w| ("deploy.record-unverifiable", w))
    }

    /// Another batch deploys, or will before this one may (MAJOR 1 of the enable-readiness review): a T8a step of another batch that a
    /// dead process left running on any target, or another batch in `merged` whose deploy is not settled (not started, pending, or
    /// interrupted and due to run again; one whose deploy failed and needs a person does not hold the others, `deploy.superseded`
    /// guards it instead). For T8a only a batch merged before this one counts, so the older merge deploys first. A T8a is also held by
    /// another batch in `deploying` on a target this one deploys (its health checks have not passed), so a health check never runs
    /// against a newer sha than the one it checks.
    fn another_batch_deploying(&self, id: &str, tr: &str) -> Result<Option<(&'static str, String)>> {
        if let Some(held) = Self::held_by(self.running_records().iter().filter(|(_, v)| v["batch"].as_str() != Some(id)).map(|(p, v)| self.record_state(p, v)), "deploy.another-batch-deploying") {
            return Ok(Some(held));
        }
        let merged_at = |evs: &[Ev]| evs.iter().rev().find(|e| e.transition == "T7" && e.phase == "done").map(|e| e.seq);
        let mine = merged_at(&self.events(id)?);
        let ours: Vec<&String> = self.cfg.targets.iter().filter(|(_, t)| t.tier != "production").map(|(n, _)| n).collect();
        for b in self.batch_ids()?.into_iter().filter(|b| b != id) {
            let evs = self.events(&b)?;
            let Some(f) = fold(&evs) else { continue };
            // a batch held while its health checks ran (held from `deploying`) holds the target as if it still ran them (round 3, E)
            let held_from = (f.state == "held").then(|| evs.iter().rev().find(|e| e.transition == "hold" && e.phase == "done").and_then(|e| e.detail["from"].as_str().map(String::from))).flatten();
            if tr == "T8a" && (f.state == "deploying" || held_from.as_deref() == Some("deploying")) {
                let theirs: Vec<String> = f.facts.get("targets").and_then(Value::as_array).into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
                let shared: Vec<&str> = theirs.iter().filter(|t| ours.contains(t)).map(String::as_str).collect();
                if !shared.is_empty() {
                    return Ok(Some((
                        "deploy.another-batch-deploying",
                        format!("batch {b} is deploying {}{} and its health checks have not passed; this batch deploys after it is live (or a person abandons it), so a health check never runs against a newer sha", shared.join(", "), if held_from.is_some() { " (held)" } else { "" }),
                    )));
                }
                continue;
            }
            if f.state != "merged" || self.t8a_needs_person(&b)? {
                continue;
            }
            if tr == "T8a" && !matches!((merged_at(&evs), mine), (Some(theirs), Some(m)) if theirs < m) {
                continue;
            }
            let how = if f.pending.as_ref().is_some_and(|p| p.transition == "T8a") { "its deploy was interrupted and is not settled" } else { "it has not deployed yet" };
            return Ok(Some(("deploy.another-batch-deploying", format!("batch {b} is merged and {how}; this batch {} after it settles, so the newer sha is the last one deployed", if tr == "T7" { "merges" } else { "deploys" }))));
        }
        Ok(None)
    }

    /// Does the T8a of batch `b` wait for a person (its failed attempts spent the budget, or it was interrupted too often)?
    fn t8a_needs_person(&self, b: &str) -> Result<bool> {
        let (crashes, _) = self.repeats(b, "T8a")?;
        Ok(crashes >= self.cfg.max_interrupts || self.failures_since(b, "T8a")?.len() > self.cfg.retry_budget("T8a"))
    }

    /// A deploy of `sha` that a newer batch already overtook on one of `targets`: the newest checkpoint of that target is another
    /// batch's, for a sha that `sha` does not contain (it is ahead of `sha`, or unrelated to it). `Some(why)`: refused, never run.
    fn superseded(&self, id: &str, wd: &Path, sha: &str, targets: &[String]) -> Result<Option<String>> {
        for t in targets {
            if self.checkpointed(id, sha, t)? {
                continue;
            }
            let newest: Option<(String, String)> = self
                .ledger
                .conn
                .query_row(
                    "SELECT batch, json_extract(evidence,'$.sha') FROM decisions WHERE kind='deploy' AND rule='deploy.checkpoint' AND subject=? AND batch<>? ORDER BY seq DESC LIMIT 1",
                    params![t, id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            let Some((b, theirs)) = newest else { continue };
            if theirs == sha || git_raw(wd, &["merge-base", "--is-ancestor", &theirs, sha]).map(|r| r.0).unwrap_or(false) {
                continue; // what the target serves is this sha or older: deploying it moves the target forward
            }
            return Ok(Some(format!(
                "target {t} already serves {} of batch {b}, which {} does not contain; deploying it would put an older sha over a newer one. A person decides (`gatetrain batch abandon {id} --reason <why>` once the target is right)",
                short(&theirs),
                short(sha)
            )));
        }
        Ok(None)
    }

    /// The limit of a plugin without one of its own (`[timeouts] plugin_min`).
    fn plugin_min(&self) -> f64 {
        self.cfg.plugin_timeout_min
    }

    /// The git context of one engine call: every git call below it runs with `[timeouts] git_min` and is named in the heartbeat.
    fn git_scope(&self, id: &str) -> GitScope {
        let ctx = GitCtx { secs: self.cfg.git_timeout_min * 60.0, progress: self.progress.clone(), batch: id.to_string() };
        GitScope(GIT_CTX.with(|c| c.borrow_mut().replace(ctx)))
    }

    /// A cherry-pick stopped on conflicts: hubs are rebuilt by their command, other paths are taken from a side by their resolver,
    /// anything else (or a result that still holds conflict markers) stops the stack. `Err(why)` leaves the pick for the caller to abort.
    fn resolve(&self, id: &str, wd: &Path, first_err: &str) -> Result<std::result::Result<(), String>> {
        let paths = unmerged_paths(wd)?;
        if paths.is_empty() || (self.cfg.hubs.is_empty() && self.cfg.resolvers.is_empty()) {
            return Ok(Err(first_err.to_string()));
        }
        let tmp = self.state_dir.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        for (i, p) in paths.iter().enumerate() {
            if let Some(h) = self.cfg.hubs.iter().find(|h| glob_match(&h.glob, p)) {
                let stage = |n: u8, name: &str| -> Result<PathBuf> {
                    let f = tmp.join(format!("{id}-{i}-{name}"));
                    let (ok, bytes) = git_bytes(wd, &["show", &format!(":{n}:{p}")])?;
                    std::fs::write(&f, if ok { bytes } else { vec![] })?; // an add/add conflict has no base
                    Ok(f)
                };
                let (b, o, t) = (stage(1, "base")?, stage(2, "ours")?, stage(3, "theirs")?);
                let target = wd.join(p);
                let cmd: Vec<String> = h
                    .rebuild
                    .iter()
                    .map(|a| {
                        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/"); // forward slashes work in a POSIX shell and in Windows programs alike
                        a.replace("{base}", &fwd(&b)).replace("{ours}", &fwd(&o)).replace("{theirs}", &fwd(&t)).replace("{path}", &fwd(&target))
                    })
                    .collect();
                let (rc, err) = match self.plugin(id, "T1", "hub rebuild", " ([timeouts] plugin_min)", &cmd, wd, &[], self.plugin_min())? {
                    Ok(x) => x,
                    Err(why) => return Ok(Err(why)),
                };
                if rc != 0 {
                    return Ok(Err(format!("the hub rebuild for {p} exited {rc}: {err}")));
                }
            } else if let Some(r) = self.cfg.resolvers.iter().find(|r| glob_match(&r.glob, p)) {
                match r.take.as_str() {
                    "lane" => git(wd, &["checkout", "--theirs", "--", p]).map(|_| ())?,
                    "stack" => git(wd, &["checkout", "--ours", "--", p]).map(|_| ())?,
                    _ => return Ok(Err(format!("{p} conflicts and its resolver says manual"))),
                }
            } else {
                return Ok(Err(format!("{p} conflicts and no hub or resolver covers it")));
            }
            // the hub guard: a resolved file never keeps conflict markers
            let text = std::fs::read(wd.join(p)).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            if has_markers(&text) {
                return Ok(Err(format!("{p} still holds conflict markers after its resolver")));
            }
            git(wd, &["add", "--", p])?;
        }
        let (ok, _, err) = git_raw(wd, &["-c", "core.editor=true", "cherry-pick", "--continue"])?;
        if !ok {
            return Ok(Err(format!("cherry-pick --continue failed: {}", err.lines().next().unwrap_or(""))));
        }
        Ok(Ok(()))
    }

    // ------------------------------------------------------------ persistence

    pub fn events(&self, id: &str) -> Result<Vec<Ev>> {
        let mut st = self.ledger.conn.prepare(
            "SELECT seq,transition,phase,attempt,state_to,input_sha,output_sha,run_id,decision_seq,detail FROM batch_events WHERE batch=? ORDER BY seq",
        )?;
        let rows = st.query_map([id], |r| {
            let d: String = r.get(9)?;
            Ok(Ev {
                seq: r.get(0)?,
                transition: r.get(1)?,
                phase: r.get(2)?,
                attempt: r.get(3)?,
                state_to: r.get(4)?,
                input_sha: r.get(5)?,
                output_sha: r.get(6)?,
                run_id: r.get(7)?,
                decision_seq: r.get(8)?,
                detail: serde_json::from_str(&d).unwrap_or(Value::Null),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn fold(&self, id: &str) -> Result<Folded> {
        fold(&self.events(id)?).ok_or_else(|| Error::Batch(format!("no such batch: {id}")))
    }

    #[allow(clippy::too_many_arguments)]
    fn event(&mut self, id: &str, tr: &str, phase: &str, attempt: i64, state_to: Option<&str>, input: Option<&str>, output: Option<&str>, run_id: Option<&str>, decision: Option<i64>, detail: &Value) -> Result<i64> {
        self.ledger.require_current()?;
        let at = (self.now)();
        self.ledger.conn.execute(
            "INSERT INTO batch_events(batch,transition,phase,attempt,state_to,input_sha,output_sha,run_id,host,at,decision_seq,detail) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![id, tr, phase, attempt, state_to, input, output, run_id, "local", at, decision, detail.to_string()],
        )?;
        Ok(self.ledger.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(&mut self, kind: &str, id: &str, subject: Option<String>, rule: &str, outcome: &str, reason: String, evidence: Option<Value>) -> Result<i64> {
        let e = NewEntry {
            at: (self.now)(),
            actor: self.actor.clone(),
            kind: kind.into(),
            batch: Some(id.into()),
            subject,
            rule: Some(rule.into()),
            evidence,
            outcome: outcome.into(),
            reason,
            ..Default::default()
        };
        Ok(declog::append(self.ledger, &e)?.0)
    }

    /// A refusal: logged once (a repeat of the identical last refusal is not logged again), the batch stays where it is.
    fn refuse(&mut self, id: &str, rule: &str, because: String) -> Result<Outcome> {
        let last: Option<(i64, String, Option<String>, String)> = self
            .ledger
            .conn
            .query_row("SELECT seq,kind,rule,reason FROM decisions WHERE batch=? ORDER BY seq DESC LIMIT 1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .ok();
        let decision = match last {
            Some((seq, k, r, reason)) if k == "refusal" && r.as_deref() == Some(rule) && reason == because => seq,
            _ => self.decide("refusal", id, None, rule, "refused", because.clone(), None)?,
        };
        Ok(Outcome::Refused { rule: rule.into(), because, decision, clears_by_itself: CLEARS_BY_ITSELF.contains(&rule) })
    }

    // ------------------------------------------------------------ commands: new, status, hold, release, abandon

    pub fn new_batch(&mut self, id: &str, base: &str, workdir: &Path, lanes: &[String]) -> Result<()> {
        let _git = self.git_scope(id);
        self.ledger.require_current()?;
        if !self.events(id)?.is_empty() {
            return Err(Error::Batch(format!("batch {id} already exists")));
        }
        let wd = std::path::absolute(workdir)?;
        // `--base <batch-id>`: stacked on the tip of a batch that is preflighted, proving or proven (engine-v2 section 8)
        let mut pipeline = json!({});
        let base_sha = if !self.events(base)?.is_empty() {
            let bf = self.fold(base)?;
            if !matches!(bf.state.as_str(), "preflighted" | "proving" | "proven") {
                return Err(Error::Batch(format!("pipeline.base-state: batch {base} is {}; a batch is stacked on a batch that is preflighted, proving or proven", bf.state)));
            }
            let unmerged = |s: &str| !matches!(s, "merged" | "deploying" | "live" | "closed" | "abandoned");
            let (mut ahead, mut cur) = (1usize, bf.clone());
            while let Some(below) = cur.s("base_batch") {
                cur = self.fold(&below)?;
                if unmerged(&cur.state) {
                    ahead += 1;
                }
            }
            if ahead > self.cfg.max_ahead {
                return Err(Error::Batch(format!("pipeline.max-ahead: {ahead} unmerged batch(es) would sit below this one, [pipeline] max_ahead is {}", self.cfg.max_ahead)));
            }
            let tip = git(&wd, &["rev-parse", &format!("refs/heads/{}", bf.need("branch")?)])?;
            let floor = bf.s("floor_sha").or_else(|| bf.s("base_sha")).unwrap_or_else(|| tip.clone());
            pipeline = json!({"base_batch": base, "floor_sha": floor});
            tip
        } else {
            git(&wd, &["rev-parse", &format!("{base}^{{commit}}")])?
        };
        let mut ls = vec![];
        for l in lanes {
            let tip = git(&wd, &["rev-parse", &format!("{l}^{{commit}}")])?;
            ls.push(json!({"ref": l, "tip": tip}));
        }
        let mut detail = json!({"base_ref": base, "base_sha": base_sha, "workdir": wd.to_string_lossy(), "lanes": ls});
        if let (Some(d), Some(x)) = (detail.as_object_mut(), pipeline.as_object()) {
            d.extend(x.clone());
        }
        let d = self.decide("transition", id, Some(base_sha.clone()), "transition.open", "allowed", format!("batch opened on {base} with {} lanes", lanes.len()), None)?;
        self.event(id, "open", "done", 1, Some("open"), Some(&base_sha), None, None, Some(d), &detail)?;
        Ok(())
    }

    /// The state with what is running where; the `Value` is what `--json` prints.
    pub fn status(&self, id: &str) -> Result<Value> {
        let f = self.fold(id)?;
        Ok(json!({
            "batch": id, "state": f.state, "held_from": f.held_from,
            "pending_intent": f.pending.as_ref().map(|p| json!({"transition": p.transition, "attempt": p.attempt})),
            "facts": f.facts, "events": f.events,
        }))
    }

    pub fn batch_ids(&self) -> Result<Vec<String>> {
        let mut st = self.ledger.conn.prepare("SELECT DISTINCT batch FROM batch_events ORDER BY batch")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn side(&mut self, id: &str, tr: &str, to: &str, rule: &str, reason: &str) -> Result<Outcome> {
        let f = self.fold(id)?;
        let d = self.decide("override", id, None, rule, "recorded", reason.into(), Some(json!({"from": f.state})))?;
        let n = f.max_attempt.get(tr).copied().unwrap_or(0) + 1;
        let mut detail = json!({"from": f.state, "rule": rule, "reason": reason});
        if let Some(p) = &f.pending {
            // a side event never settles an intent: it names the one it leaves pending, which `release` and `advance` resume
            detail["pending_intent"] = json!({"transition": p.transition, "attempt": p.attempt});
        }
        self.event(id, tr, "done", n, Some(to), None, None, None, Some(d), &detail)?;
        Ok(Outcome::Advanced { from: f.state, to: to.into(), note: reason.into() })
    }

    pub fn hold(&mut self, id: &str, reason: &str) -> Result<Outcome> {
        let f = self.fold(id)?;
        if matches!(f.state.as_str(), "held" | "abandoned" | "closed") {
            return Ok(Outcome::Idle(format!("batch {id} is {}", f.state)));
        }
        self.side(id, "hold", "held", "hold.person", reason)
    }

    pub fn release(&mut self, id: &str) -> Result<Outcome> {
        let f = self.fold(id)?;
        match (f.state.as_str(), f.held_from.clone()) {
            ("held", Some(back)) => self.side(id, "release", &back, "hold.release", "released by a person"),
            _ => Ok(Outcome::Idle(format!("batch {id} is not held"))),
        }
    }

    pub fn abandon(&mut self, id: &str, reason: &str) -> Result<Outcome> {
        let f = self.fold(id)?;
        if matches!(f.state.as_str(), "abandoned" | "closed") {
            return Ok(Outcome::Idle(format!("batch {id} is {}", f.state)));
        }
        // floor.no-destroy: abandoning deletes no branch; the stack branches stay as they are.
        self.side(id, "abandon", "abandoned", "abandon.person", reason)
    }

    /// Runs of this batch that reached a verdict or are still running: every launch (T4, T6a) minus the stopped ones (T5s).
    /// A stopped run is infrastructure and is not a full run (design 3.4, 5.3).
    fn full_runs(&self, id: &str) -> Result<usize> {
        let ev = self.events(id)?;
        let n = |t: &str| ev.iter().filter(|e| e.transition == t && e.phase == "done").count();
        Ok((n("T4") + n("T6a")).saturating_sub(n("T5s")))
    }

    /// `budget.full-runs`: refuses the launch that would be run N+1 of `[chain] max_full_runs_per_batch`.
    fn full_run_budget(&self, id: &str) -> Result<Option<Planned>> {
        let Some(max) = self.cfg.max_full_runs_per_batch else { return Ok(None) };
        let used = self.full_runs(id)?;
        Ok((used >= max).then(|| Planned::Refuse("budget.full-runs".into(), format!("the batch has used {used} of {max} full proof runs ([chain] max_full_runs_per_batch); a person must raise the budget or abandon the batch"))))
    }

    /// The run number of the next launch: T4 and T6a share one sequence so a retry never reuses a run id or a log file.
    fn next_run_no(&self, f: &Folded) -> i64 {
        f.max_attempt.get("T4").copied().unwrap_or(0) + f.max_attempt.get("T6a").copied().unwrap_or(0) + 1
    }

    /// A held, abandoned or closed batch is never moved or resumed: not by `advance`, a tick, `retry` or `restack`, and not by the
    /// resume of an intent a crash left pending (the resume's `done` would overwrite the hold). `None` = the batch may move.
    fn parked(&mut self, id: &str, f: &Folded) -> Result<Option<Outcome>> {
        Ok(match f.state.as_str() {
            "held" => {
                let pend = f.pending.as_ref().map(|p| format!(" (the {} intent of attempt {} stays pending until then)", p.transition, p.attempt)).unwrap_or_default();
                Some(self.refuse(id, "batch.held", format!("the batch is held; `batch release` returns it to its state{pend}"))?)
            }
            "abandoned" | "closed" => Some(Outcome::Idle(format!("batch {id} is {}", f.state))),
            _ => None,
        })
    }

    /// `batch advance --retry --reason`: a person lets one more attempt of the transition whose last attempt failed through. It is an
    /// override row (`retry.person`) and a side event `retry` that leaves the state alone; the next `advance` takes the attempt.
    pub fn retry_failed(&mut self, id: &str, reason: &str) -> Result<Outcome> {
        self.retry_failed_with(id, reason, false)
    }

    /// `batch clear-record <id> --reason`: a person clears the deploy records batch `id` left behind, in any state (a held, abandoned or
    /// closed batch too, which `--retry` never reaches), under the same liveness check as a retry. An override row
    /// (`record.clear-person`, `cleared_records`) and nothing else: no batch moves.
    pub fn clear_record(&mut self, id: &str, reason: &str, clear_unverifiable: bool) -> Result<Outcome> {
        self.adopt_deployed()?;
        let f = self.fold(id)?;
        if reason.trim().is_empty() {
            return Err(Error::Batch("clearing a record needs a reason (--reason)".into()));
        }
        Ok(match self.clear_own_records(id, &f, reason, clear_unverifiable, "record.clear-person")? {
            Cleared::None => Outcome::Idle(format!("batch {id} has no deploy record to clear")),
            Cleared::Refused(o) => o,
            Cleared::Done { message, .. } => Outcome::Idle(message),
        })
    }

    /// Clear the records of batch `id` (`<state>/running/*.json` naming it), each judged first: a live one refuses (`deploy.record-live`),
    /// an unverifiable one refuses unless `clear_unverifiable` (`deploy.record-unverifiable`); otherwise an override row under `rule`.
    fn clear_own_records(&mut self, id: &str, f: &Folded, reason: &str, clear_unverifiable: bool, rule: &str) -> Result<Cleared> {
        let mine: Vec<(PathBuf, Value)> = self.running_records().into_iter().filter(|(_, v)| v["batch"].as_str() == Some(id)).collect();
        if mine.is_empty() {
            return Ok(Cleared::None);
        }
        let (mut live, mut unseen) = (vec![], vec![]);
        for (p, v) in &mine {
            match self.record_state(p, v) {
                Rec::Live(w) => live.push(w),
                Rec::Unverifiable(w) => unseen.push(w),
                Rec::Gone => {}
            }
        }
        if !live.is_empty() {
            return Ok(Cleared::Refused(self.refuse(id, "deploy.record-live", format!("{}; the record is not cleared while it runs", live.join("; ")))?));
        }
        if !unseen.is_empty() && !clear_unverifiable {
            return Ok(Cleared::Refused(self.refuse(id, "deploy.record-unverifiable", format!("{}; without `--clear-unverifiable-record` the record is not cleared", unseen.join("; ")))?));
        }
        let what: Vec<String> = mine
            .iter()
            .map(|(_, v)| {
                let step = v["step"].as_str().unwrap_or("a deploy step");
                match v["pid"].as_u64() {
                    Some(pid) if cfg!(unix) => format!("{step} (pid {pid}, process group {pid})"),
                    Some(pid) => format!("{step} (pid {pid})"),
                    None => format!("{step} (no pid was recorded)"),
                }
            })
            .collect();
        let blind = !unseen.is_empty();
        let d = self.decide("override", id, None, rule, "recorded", reason.trim().into(), Some(json!({"transition": "T8a", "from": f.state, "cleared_records": what, "unverifiable": blind})))?;
        for (p, _) in &mine {
            std::fs::remove_file(p)?;
        }
        let warn = if blind { "; it could not be verified (--clear-unverifiable-record): if it still runs, a second deploy may start beside an unseen one" } else { "" };
        let message = format!("cleared the record of {}{warn}; the next advance probes the deploy: {}", what.join(", "), reason.trim());
        Ok(Cleared::Done { decision: d, what, message })
    }

    /// [`Engine::retry_failed`]; `clear_unverifiable` (`--clear-unverifiable-record`) also clears a record that cannot be told from a
    /// live deploy (no pid or no start token was recorded). A record whose process (or, for a step that does not survive its parent,
    /// its process group) still runs is never cleared: the retry is refused (`deploy.record-live`) and names what to end.
    pub fn retry_failed_with(&mut self, id: &str, reason: &str, clear_unverifiable: bool) -> Result<Outcome> {
        let _git = self.git_scope(id);
        self.adopt_deployed()?;
        let f = self.fold(id)?;
        if let Some(o) = self.parked(id, &f)? {
            return Ok(o);
        }
        if reason.trim().is_empty() {
            return Err(Error::Batch("a retry override needs a reason (--reason)".into()));
        }
        // a record of a T8a step this batch left behind (a deploy a dead gatetrain started) holds every deploy until it ends; a person
        // who checked that it is gone clears it here, with an override row that names what was cleared
        match self.clear_own_records(id, &f, reason, clear_unverifiable, "retry.person")? {
            Cleared::None => {}
            Cleared::Refused(o) => return Ok(o),
            Cleared::Done { decision, what, message } => {
                let n = f.max_attempt.get("retry").copied().unwrap_or(0) + 1;
                self.event(id, "retry", "done", n, Some(&f.state), None, None, None, Some(decision), &json!({"transition": "T8a", "reason": reason.trim(), "cleared_records": what}))?;
                return Ok(Outcome::Idle(message));
            }
        }
        let Some(tr) = next_effect(&f.state) else { return Ok(Outcome::Idle(format!("batch {id} is {}: no transition with an effect to retry", f.state))) };
        let (crashes, stopped) = self.repeats(id, tr)?;
        if self.failures_since(id, tr)?.is_empty() && crashes.max(stopped) < self.cfg.max_interrupts {
            return Ok(Outcome::Idle(format!("the last attempt of {tr} did not fail: nothing to retry")));
        }
        let d = self.decide("override", id, None, "retry.person", "recorded", reason.trim().into(), Some(json!({"transition": tr, "from": f.state})))?;
        let n = f.max_attempt.get("retry").copied().unwrap_or(0) + 1;
        self.event(id, "retry", "done", n, Some(&f.state), None, None, None, Some(d), &json!({"transition": tr, "reason": reason.trim()}))?;
        Ok(Outcome::Idle(format!("one more attempt of {tr} is allowed: {}", reason.trim())))
    }

    /// The failed attempts of `tr` that still count: after the row that put the batch in its state (a hold, a release or a retry
    /// override is not such a row) and after the newest person's retry override. A crash is not a failure of the effect: an attempt
    /// closed as `interrupted` does not count, unless the probe could not settle it. Oldest first.
    fn failures_since(&self, id: &str, tr: &str) -> Result<Vec<Ev>> {
        let ev = self.events(id)?;
        let entered = ev.iter().rev().find(|e| e.phase == "done" && !matches!(e.transition.as_str(), "hold" | "release" | "retry")).map_or(0, |e| e.seq);
        let overridden = ev.iter().rev().find(|e| e.transition == "retry" && e.phase == "done" && e.detail["transition"].as_str() == Some(tr)).map_or(0, |e| e.seq);
        let since = entered.max(overridden);
        let settled_crash = |e: &Ev| e.detail["outcome"].as_str() == Some("interrupted") && e.detail["settled"].as_bool() != Some(false);
        Ok(ev.into_iter().filter(|e| e.seq > since && e.transition == tr && e.phase == "failed" && !settled_crash(e)).collect())
    }

    /// Repeats that are not failures of the effect but still need a person when they go on: consecutive crashes of `tr` (settled
    /// interruptions) and, for a launch, consecutive stopped runs (T5s), each since the newest person's override for `tr`.
    fn repeats(&self, id: &str, tr: &str) -> Result<(usize, usize)> {
        let ev = self.events(id)?;
        let entered = ev.iter().rev().find(|e| e.phase == "done" && !matches!(e.transition.as_str(), "hold" | "release" | "retry")).map_or(0, |e| e.seq);
        let overridden = ev.iter().rev().find(|e| e.transition == "retry" && e.phase == "done" && e.detail["transition"].as_str() == Some(tr)).map_or(0, |e| e.seq);
        let since = entered.max(overridden);
        let crashes = ev.iter().filter(|e| e.seq > since && e.transition == tr && e.phase == "failed" && e.detail["outcome"].as_str() == Some("interrupted") && e.detail["settled"].as_bool() != Some(false)).count();
        let mut stopped = 0;
        if matches!(tr, "T4" | "T6a") {
            for e in ev.iter().rev().filter(|e| e.seq > overridden && e.phase == "done") {
                match e.transition.as_str() {
                    "T5s" => stopped += 1,
                    "T4" | "T6a" | "hold" | "release" | "retry" => {}
                    _ => break,
                }
            }
        }
        Ok((crashes, stopped))
    }

    /// A failed merge or deploy may have happened after all: the fast-forward moved the protected branch and only the check after it
    /// failed (a hook left a file in the checkout), or every target had deployed when a later step failed. Before anything is retried
    /// or refused, the failed attempt's plan is rebuilt from its intent and probed: an effect that happened finishes that attempt.
    fn settle_failed(&mut self, id: &str, f: &Folded, tr: &str) -> Result<Option<Outcome>> {
        if !matches!(tr, "T7" | "T8a") {
            return Ok(None);
        }
        let Some(last) = self.failures_since(id, tr)?.pop() else { return Ok(None) };
        let Some(intent) = self.events(id)?.into_iter().find(|e| e.transition == tr && e.phase == "intent" && e.attempt == last.attempt) else { return Ok(None) };
        let plan = Plan { tr: tr.into(), to: to_for(tr, &intent.detail).into(), input_sha: intent.input_sha.clone(), run_id: intent.run_id.clone(), attempt: intent.attempt, detail: intent.detail.clone() };
        if let Probe::Happened(out) = self.probe(id, f, &plan)? {
            let extra = if tr == "T7" { json!({"resumed": "failed-but-happened", "merge_new": out}) } else { json!({"resumed": "failed-but-happened"}) };
            let warning = last.detail.get("error").and_then(Value::as_str).unwrap_or_default().to_string();
            let mut extra = extra;
            extra["warning"] = json!(warning);
            return Ok(Some(self.finish(id, &plan, out, extra, " (the failed attempt's effect had happened; its error is kept as a warning)")?));
        }
        Ok(None)
    }

    /// Does `tip` differ from `tested` only by files under the generated directories, with `tested` an ancestor of it?
    fn generated_only(&self, wd: &Path, tested: &str, tip: &str) -> Result<bool> {
        if self.cfg.generated_dirs.is_empty() || !git_raw(wd, &["merge-base", "--is-ancestor", tested, tip])?.0 {
            return Ok(false);
        }
        let names = git(wd, &["diff", "--name-only", "--no-renames", tested, tip])?;
        Ok(names.lines().all(|p| in_dirs(p, &self.cfg.generated_dirs)))
    }

    /// The engine's one retry decision, used by every path that starts an attempt (`advance`, a tick, `retry`): a transition whose
    /// failed attempts exceed its `[retry] budget` (default 0) is refused (`batch.failed-needs-person`) until a person's override.
    fn failure_gate(&mut self, id: &str, tr: &str) -> Result<Option<Outcome>> {
        let cap = self.cfg.max_interrupts;
        let (crashes, stopped) = self.repeats(id, tr)?;
        if crashes >= cap {
            let because = format!("{tr} was interrupted {crashes} times in a row ([retry] max_interrupts = {cap}): something kills the engine during it; a person looks, then runs `gatetrain batch advance {id} --retry --reason <why>`");
            return Ok(Some(self.refuse(id, "batch.interrupt-cap", because)?));
        }
        if stopped >= cap {
            let because = format!("the proof run stopped without GATES_DONE {stopped} times in a row ([retry] max_interrupts = {cap}): the holder or the host keeps dying; a person looks, then runs `gatetrain batch advance {id} --retry --reason <why>`");
            return Ok(Some(self.refuse(id, "batch.stopped-cap", because)?));
        }
        let failed = self.failures_since(id, tr)?;
        let Some(last) = failed.last() else { return Ok(None) };
        let budget = self.cfg.retry_budget(tr);
        if failed.len() <= budget {
            let why = format!("attempt {} of {tr} after {} failed attempt(s), within [retry] budget {tr} = {budget}", last.attempt + 1, failed.len());
            self.decide("retry", id, None, "retry.budget", "allowed", why, Some(json!({"transition": tr, "failed": failed.len()})))?;
            return Ok(None);
        }
        let err = last.detail.get("error").and_then(Value::as_str).unwrap_or("no error recorded");
        let err: String = if err.chars().count() > 120 { format!("{}...", err.chars().take(120).collect::<String>()) } else { err.to_string() };
        let because = format!("the last attempt of {tr} failed ({err}); [retry] budget {tr} = {budget} is spent; a person runs `gatetrain batch advance {id} --retry --reason <why>` to try again");
        Ok(Some(self.refuse(id, "batch.failed-needs-person", because)?))
    }

    /// Retry a red proof (T6a, red -> proving): a full rerun, refused while a cause of the red run is open (`retry.causes-open`).
    /// A cause is addressed when the batch tip moved past the tested commit, or a person recorded the run's red as a flake or
    /// infrastructure with a reason (`note_cause`).
    pub fn retry(&mut self, id: &str) -> Result<Outcome> {
        let _git = self.git_scope(id);
        let f = self.fold(id)?;
        if let Some(o) = self.parked(id, &f)? {
            return Ok(o);
        }
        if let Some(p) = f.pending.clone() {
            return self.resume(id, &f, p);
        }
        if f.state != "red" {
            return Ok(Outcome::Idle(format!("batch {id} is {}, not red", f.state)));
        }
        self.go(id, &f, "T6a")
    }

    /// A person records that the cause of the last red run is a `flake` or `infra`, with a reason. It is a decision-log override
    /// (`retry.cause-noted`) naming the red run; it covers that run only, and it leaves the state alone.
    pub fn note_cause(&mut self, id: &str, class: &str, reason: &str) -> Result<Outcome> {
        let f = self.fold(id)?;
        if f.state != "red" {
            return Ok(Outcome::Idle(format!("batch {id} is {}, not red: nothing to note", f.state)));
        }
        if !matches!(class, "flake" | "infra") {
            return Err(Error::Batch(format!("a cause is noted as `flake` or `infra`, not `{class}`")));
        }
        if reason.trim().is_empty() {
            return Err(Error::Batch("a cause note needs a reason".into()));
        }
        let (run, tested) = (f.need("run_id")?, f.need("tested_sha")?);
        self.decide("override", id, Some(tested.clone()), "retry.cause-noted", "recorded", format!("{class}: {}", reason.trim()), Some(json!({"run_id": run, "class": class, "tested_sha": tested})))?;
        Ok(Outcome::Idle(format!("noted: the red of run {run} is a {class}")))
    }

    /// Is the red of `run` covered by a recorded note?
    fn cause_noted(&self, id: &str, run: &str) -> Result<bool> {
        let mut st = self.ledger.conn.prepare("SELECT evidence FROM decisions WHERE batch=? AND kind='override' AND rule='retry.cause-noted'")?;
        let rows = st.query_map([id], |r| r.get::<_, Option<String>>(0))?;
        for ev in rows {
            if let Some(v) = ev?.and_then(|e| serde_json::from_str::<Value>(&e).ok()) {
                if v["run_id"].as_str() == Some(run) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // ------------------------------------------------------------ advance

    /// One step: resume a pending intent, else take the transition the state allows, or refuse naming the rule.
    pub fn advance(&mut self, id: &str) -> Result<Outcome> {
        let _git = self.git_scope(id);
        self.adopt_deployed()?;
        let f = self.fold(id)?;
        if let Some(o) = self.parked(id, &f)? {
            return Ok(o);
        }
        if let Some(p) = f.pending.clone() {
            return self.resume(id, &f, p);
        }
        match f.state.as_str() {
            "open" => self.go(id, &f, "T1"),
            "stacked" => self.go(id, &f, "T2"),
            "settled" => self.go(id, &f, "T3"),
            "preflighting" => self.observe_preflight(id, &f),
            "preflight-red" => self.refuse(id, "preflight.red", "the pre-flight run has a red step; commit a fix and restack (a red pre-flight is not run again on the same tip)".into()),
            "preflighted" => self.go(id, &f, "T4"),
            "proving" => self.observe(id, &f),
            "red" => self.refuse(id, "floor.zero-red", "the proof has a red step; a red batch is not merged (restack after a patch)".into()),
            "proven" => self.go(id, &f, "T7"),
            "merged" => self.go(id, &f, "T8a"),
            "deploying" => self.health(id, &f),
            "live" => self.close(id, &f),
            s => Ok(Outcome::Idle(format!("batch {id} is {s}"))),
        }
    }

    fn go(&mut self, id: &str, f: &Folded, tr: &str) -> Result<Outcome> {
        if let Some(o) = self.settle_failed(id, f, tr)? {
            return Ok(o);
        }
        if let Some(o) = self.failure_gate(id, tr)? {
            return Ok(o);
        }
        let attempt = f.max_attempt.get(tr).copied().unwrap_or(0) + 1;
        match self.plan(tr, id, f, attempt)? {
            Planned::Refuse(rule, because) => self.refuse(id, &rule, because),
            Planned::Go(plan) => {
                self.event(id, tr, "intent", attempt, None, plan.input_sha.as_deref(), None, plan.run_id.as_deref(), None, &plan.detail)?;
                crash_point(&format!("before:{tr}"))?;
                self.execute(id, f, plan)
            }
        }
    }

    /// Effect, then `done` (or `failed`).
    fn execute(&mut self, id: &str, f: &Folded, plan: Plan) -> Result<Outcome> {
        let r = self.effect(id, f, &plan);
        let done = match r {
            Ok(Ok(d)) => d,
            // a test hook's crash leaves the intent as a real crash would: the next advance probes it
            #[cfg(any(test, feature = "test-fakes"))]
            Err(e @ Error::SimulatedCrash(_)) => return Err(e),
            // every other error of the effect (a command that cannot be spawned, a missing folder, a marker that cannot be written)
            // is a failed attempt with its row, so no caller runs the effect again without a person
            Ok(Err(msg)) | Err(Error::Batch(msg)) => return self.failed(id, f, &plan, msg),
            Err(e) => return self.failed(id, f, &plan, e.to_string()),
        };
        crash_point(&plan.tr)?;
        self.finish(id, &plan, done.output, done.extra, "")
    }

    /// A failed attempt: `failed-clean` when the world is as it was before the attempt, `failed-dirty` when the attempt left something
    /// behind (a stack branch, a commit, a moved ref, a deployed target), so a person knows what a retry starts from.
    fn failed(&mut self, id: &str, f: &Folded, plan: &Plan, msg: String) -> Result<Outcome> {
        let outcome = if self.left_dirty(id, f, plan) { "failed-dirty" } else { "failed-clean" };
        self.event(id, &plan.tr, "failed", plan.attempt, None, plan.input_sha.as_deref(), None, plan.run_id.as_deref(), None, &json!({"error": msg, "outcome": outcome}))?;
        Ok(Outcome::Failed(format!("{}: {msg}", plan.tr)))
    }

    /// Did the failed attempt leave the world changed? A check that cannot be made counts as changed.
    fn left_dirty(&self, id: &str, f: &Folded, plan: &Plan) -> bool {
        let Some(wd) = f.s("workdir").map(PathBuf::from) else { return true };
        let rev = |r: &str| git(&wd, &["rev-parse", "--verify", "-q", r]).ok();
        match plan.tr.as_str() {
            "T1" => rev(&format!("refs/heads/{}", plan.detail["branch"].as_str().unwrap_or_default())).is_some(),
            "T2" => rev("HEAD") != plan.input_sha || dirty_paths(&wd).map(|d| !d.is_empty()).unwrap_or(true),
            "T7" => {
                let branch = f.s("branch").unwrap_or_default();
                rev(&format!("refs/heads/{branch}")) != plan.input_sha || rev(&format!("refs/heads/{}", self.cfg.protected)).as_deref() != plan.detail["merge_old"].as_str()
            }
            // a plugin that ran (post-merge, a deploy, even one that failed or timed out) may have changed the target
            "T8a" => T8A_SPAWNED.with(|c| c.get()) || self.checkpoints_of(id, plan.attempt).map(|n| n > 0).unwrap_or(true),
            _ => false,
        }
    }

    /// Close the pending attempt as `interrupted`: a crash, not a failure of the effect. `settled` = the probe (or the effect's declared
    /// idempotence) says running it again is safe; an unsettled interruption counts against the `[retry] budget` like a failure.
    fn interrupted(&mut self, id: &str, plan: &Plan, settled: bool, why: &str) -> Result<()> {
        let detail = json!({"error": format!("interrupted: {why}"), "outcome": "interrupted", "settled": settled});
        self.event(id, &plan.tr, "failed", plan.attempt, None, plan.input_sha.as_deref(), None, plan.run_id.as_deref(), None, &detail)?;
        Ok(())
    }

    /// May the effect of this plan run twice without harm? A regen fixpoint and a blocking pre-flight are; a deploy is when every
    /// target of the attempt is declared `idempotent`; a launch, a stack and a merge are not (their probes settle them instead).
    fn idempotent(&self, plan: &Plan) -> bool {
        match plan.tr.as_str() {
            "T2" => true,
            "T3" => plan.detail.get("pf_run_id").is_none(),
            "T8a" => plan.detail["targets"].as_array().into_iter().flatten().all(|n| self.cfg.targets.get(n.as_str().unwrap_or_default()).is_some_and(|t| t.idempotent)),
            _ => false,
        }
    }

    fn finish(&mut self, id: &str, plan: &Plan, output: Option<String>, extra: Value, why: &str) -> Result<Outcome> {
        let mut detail = plan.detail.clone();
        if let (Some(d), Some(x)) = (detail.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                d.insert(k.clone(), v.clone());
            }
        }
        if let (Some(d), Some(o)) = (detail.as_object_mut(), &output) {
            d.insert("output_sha".into(), json!(o));
        }
        let from = from_state(&plan.tr);
        let reason = format!("{from} -> {}{why}", plan.to);
        let d = self.decide("transition", id, output.clone().or(plan.input_sha.clone()), &format!("transition.{}", plan.tr), "allowed", reason, Some(json!({"attempt": plan.attempt})))?;
        self.event(id, &plan.tr, "done", plan.attempt, Some(&plan.to), plan.input_sha.as_deref(), output.as_deref(), plan.run_id.as_deref(), Some(d), &detail)?;
        Ok(Outcome::Advanced { from: from.into(), to: plan.to.clone(), note: why.trim().to_string() })
    }

    // ------------------------------------------------------------ resume (design 3.4)

    /// An intent with no `done`: the probe settles it. `happened` finishes the attempt; anything else closes it as `interrupted` and the
    /// transition starts again as a new attempt through `go` (its guards, its retry decision, a new run id), never the same attempt
    /// twice. An effect the probe cannot settle runs again only when it is declared idempotent; otherwise it waits for a person.
    fn resume(&mut self, id: &str, f: &Folded, p: Ev) -> Result<Outcome> {
        let plan = Plan { tr: p.transition.clone(), to: to_for(&p.transition, &p.detail).into(), input_sha: p.input_sha.clone(), run_id: p.run_id.clone(), attempt: p.attempt, detail: p.detail.clone() };
        let probed = self.probe(id, f, &plan)?;
        if plan.tr == "T8a" && !matches!(probed, Probe::Happened(_) | Probe::Running(_) | Probe::Unverifiable(_)) {
            // a resumed deploy whose sha a newer batch already overtook is never run again
            let targets: Vec<String> = plan.detail["targets"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
            let wd = PathBuf::from(f.need("workdir")?);
            if let Some(why) = self.superseded(id, &wd, plan.input_sha.as_deref().unwrap_or_default(), &targets)? {
                return self.refuse(id, "deploy.superseded", why);
            }
        }
        match probed {
            Probe::Happened(out) => {
                let extra = if plan.tr == "T7" { json!({"resumed": "happened", "merge_new": out}) } else { json!({"resumed": "happened"}) };
                return self.finish(id, &plan, out, extra, " (resumed: the probe found the effect done)");
            }
            Probe::Running(why) => return Ok(Outcome::Waiting(why)),
            Probe::Unverifiable(why) => return self.refuse(id, "deploy.record-unverifiable", why),
            Probe::NotHappened => self.interrupted(id, &plan, true, "the probe found the effect not done; it runs again under a new attempt")?,
            // finish forward under a new attempt; the partial result is kept, never deleted
            Probe::Partly => self.interrupted(id, &plan, true, "partly done; finishing forward under a new attempt")?,
            Probe::Unknown(why) => {
                // a person's retry override written after this intent settles it: they looked
                let person = self.events(id)?.iter().any(|e| e.seq > p.seq && e.transition == "retry" && e.phase == "done" && e.detail["transition"].as_str() == Some(plan.tr.as_str()));
                let again = self.idempotent(&plan) || person;
                let what = if person {
                    "; a person's retry override after it settles it, so it runs again under a new attempt"
                } else if again {
                    "; the effect is declared idempotent, so it runs again under a new attempt"
                } else {
                    "; a person settles it"
                };
                self.interrupted(id, &plan, again, &format!("{why}{what}"))?
            }
        }
        let f2 = self.fold(id)?;
        if let Some(o) = self.parked(id, &f2)? {
            return Ok(o);
        }
        self.go(id, &f2, &plan.tr)
    }

    /// Read-only: git refs and commits, files on disk, the lock.
    fn probe(&self, id: &str, f: &Folded, plan: &Plan) -> Result<Probe> {
        let wd = PathBuf::from(f.need("workdir")?);
        Ok(match plan.tr.as_str() {
            "T1" => {
                let branch = plan.detail["branch"].as_str().unwrap_or_default().to_string();
                let (exists, _, _) = git_raw(&wd, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}")])?;
                if !exists {
                    Probe::NotHappened
                } else {
                    let mut all = true;
                    for l in f.facts.get("lanes").and_then(|l| l.as_array()).cloned().unwrap_or_default() {
                        // `git cherry` marks a commit not yet applied with `+`
                        let c = git(&wd, &["cherry", &branch, l["tip"].as_str().unwrap_or_default()])?;
                        if c.lines().any(|x| x.starts_with('+')) {
                            all = false;
                        }
                    }
                    if all {
                        Probe::Happened(Some(git(&wd, &["rev-parse", &branch])?))
                    } else {
                        Probe::Partly
                    }
                }
            }
            "T2" => {
                let head = git(&wd, &["rev-parse", "HEAD"])?;
                if dirty_paths(&wd)?.is_empty() && Some(&head) == plan.input_sha.as_ref() {
                    Probe::NotHappened // a rerun is a fixpoint check: no change, no commit
                } else {
                    Probe::Partly // commits or leftovers exist; the fixpoint is not proven
                }
            }
            // a launched pre-flight: adopted when a live lock names it (see `launch_probe`)
            "T3" if plan.detail.get("pf_run_id").is_some() => self.launch_probe(plan, "pf_log")?,
            "T3" => {
                let sha = plan.input_sha.clone().unwrap_or_default();
                if self.stamp_path(&sha).exists() {
                    Probe::Happened(Some(sha))
                } else {
                    Probe::NotHappened
                }
            }
            "T4" | "T6a" => self.launch_probe(plan, "log")?,
            "T7" => {
                let new = plan.detail["merge_new"].as_str().unwrap_or_default();
                let cur = git(&wd, &["rev-parse", &format!("refs/heads/{}", self.cfg.protected)])?;
                if cur == new {
                    Probe::Happened(Some(new.into()))
                } else if git_raw(&wd, &["merge-base", "--is-ancestor", new, &cur])?.0 && !git(&wd, &["diff", "--name-only", "--no-renames", new, &cur])?.lines().any(|p| !in_dirs(p, &self.cfg.generated_dirs)) {
                    Probe::Happened(Some(cur)) // the merge also carried the commit of the files the chain produced
                } else {
                    Probe::NotHappened
                }
            }
            "T8a" if self.deploy_still_running(id).is_some() => match self.deploy_still_running(id) {
                Some(("deploy.record-unverifiable", why)) => Probe::Unverifiable(why),
                Some((_, why)) => Probe::Running(why),
                None => Probe::NotHappened,
            },
            "T8a" => {
                // the per-target checkpoints of the ledger, for this sha: every one there is done; the first one missing is the one a crash
                // may have left half done (they run in order), and the ones after it never started
                let sha = plan.input_sha.clone().unwrap_or_default();
                let mut doubt: Option<String> = None;
                for name in plan.detail["targets"].as_array().into_iter().flatten().filter_map(|n| n.as_str()) {
                    if !self.checkpointed(id, &sha, name)? {
                        doubt = Some(name.to_string());
                        break;
                    }
                }
                let postmerged = self.cfg.postmerge_command.is_empty() || self.checkpointed(id, &sha, POSTMERGE)?;
                match doubt {
                    None if postmerged => Probe::Happened(plan.input_sha.clone()),
                    // the post-merge command prepares a checkout and runs again (it is idempotent by contract, like regen)
                    None => Probe::NotHappened,
                    Some(_) if !postmerged => Probe::NotHappened,
                    // the next attempt settles it: its probe is asked first, or it is safe to deploy twice
                    Some(name) if self.cfg.targets.get(&name).is_some_and(|t| t.idempotent || !t.probe.is_empty()) => Probe::NotHappened,
                    Some(name) => Probe::Unknown(format!("a crash may have left deploy target {name} of {} half deployed, and it is neither declared idempotent nor probed", short(&sha))),
                }
            }
            _ => Probe::NotHappened,
        })
    }

    /// The probe of a launch (T4, T6a, a launched T3): a live lock that names the run, or the holder's `HOLDS <run>` line in the log,
    /// means it was launched (T5 reads how it ended); no log means the holder never started; a log without that line means it was
    /// spawned and may still take the lock, which no probe can settle.
    fn launch_probe(&self, plan: &Plan, log_key: &str) -> Result<Probe> {
        let run = plan.run_id.as_deref().unwrap_or_default();
        let log = PathBuf::from(plan.detail[log_key].as_str().unwrap_or_default());
        if self.lock.names_run(run)? || holder_named(&log, run) {
            return Ok(Probe::Happened(None));
        }
        Ok(match std::fs::metadata(&log) {
            Err(_) => Probe::NotHappened,
            Ok(_) => Probe::Unknown(format!("the holder of run {run} opened its log {} but has not named the run; it may still start", log.display())),
        })
    }

    /// The launch handshake (engine-v2 4.3): a launch returns only once the holder names its run, by the host lock or by the
    /// `HOLDS <run>` line it writes to the log (a chain that ends before the next look still leaves that line). A holder that does not
    /// within `[chain] launch_ack_secs` is stopped and the launch fails clean, so the same tick never takes the run for stopped (T5s)
    /// and launches it again. The wait is measured on the monotonic clock of this process.
    fn await_ack(&self, run: &str, log: &Path) -> Result<std::result::Result<(), String>> {
        let secs = self.cfg.launch_ack_secs;
        let limit = std::time::Duration::try_from_secs_f64(secs).unwrap_or(std::time::Duration::from_secs(60));
        let t0 = std::time::Instant::now();
        loop {
            if self.lock.names_run(run)? || holder_named(log, run) {
                return Ok(Ok(()));
            }
            let spent = t0.elapsed();
            if spent >= limit {
                self.lock.abort(run)?;
                return Ok(Err(format!("the holder did not name run {run} within {secs} s ([chain] launch_ack_secs); it was stopped, so nothing runs under that id")));
            }
            std::thread::sleep((limit - spent).min(std::time::Duration::from_millis(50)));
        }
    }

    // ------------------------------------------------------------ plans (the guards)

    /// A deploy checkpoint: a `deploy` decision row (`deploy.checkpoint`, subject the target or [`POSTMERGE`]) written right after it
    /// succeeded for `sha`. The T8a probe and every later attempt for the same sha read it, so a target is never deployed twice.
    fn checkpoint(&mut self, id: &str, sha: &str, attempt: i64, target: &str) -> Result<i64> {
        let what = if target == POSTMERGE { "the post-merge command ran".to_string() } else { format!("target {target} deployed") };
        self.decide("deploy", id, Some(target.into()), "deploy.checkpoint", "recorded", format!("{what} for {}", short(sha)), Some(json!({"sha": sha, "attempt": attempt, "target": target})))
    }

    /// The checkpoint of a T8a step that just succeeded, then its record (marked `deployed` by [`Engine::plugin_as`]) is removed.
    fn checkpoint_step(&mut self, id: &str, sha: &str, attempt: i64, target: &str) -> Result<i64> {
        let d = self.checkpoint(id, sha, attempt, target)?;
        let p = self.running_path(target);
        if std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|v| v["batch"].as_str() == Some(id) && v["deployed"].as_str() == Some(sha)) {
            let _ = std::fs::remove_file(&p);
        }
        Ok(d)
    }

    /// MINOR 8 of round 2: a record marked `deployed <sha>` is a T8a step that succeeded and whose checkpoint row a crash prevented.
    /// Its checkpoint is written now (for the batch, the sha and the target the record names, under that batch's T8a attempt) and the
    /// record removed, before any guard reads the checkpoints. Run at the start of every command that moves or clears a batch.
    fn adopt_deployed(&mut self) -> Result<()> {
        for (p, v) in self.running_records() {
            let (Some(b), Some(sha), Some(t)) = (v["batch"].as_str(), v["deployed"].as_str(), v["target"].as_str()) else { continue };
            if !self.checkpointed(b, sha, t)? {
                let attempt = self.fold(b).ok().map(|f| f.pending.as_ref().filter(|e| e.transition == "T8a").map(|e| e.attempt).unwrap_or_else(|| f.max_attempt.get("T8a").copied().unwrap_or(0))).unwrap_or(0);
                let what = if t == POSTMERGE { "the post-merge command ran".to_string() } else { format!("target {t} deployed") };
                self.decide(
                    "deploy",
                    b,
                    Some(t.into()),
                    "deploy.checkpoint",
                    "recorded",
                    format!("{what} for {} (from its record: gatetrain stopped after the step, before this row)", short(sha)),
                    Some(json!({"sha": sha, "attempt": attempt, "target": t, "from_record": true})),
                )?;
            }
            std::fs::remove_file(&p)?;
        }
        Ok(())
    }

    fn checkpointed(&self, id: &str, sha: &str, target: &str) -> Result<bool> {
        let n: i64 = self.ledger.conn.query_row(
            "SELECT COUNT(*) FROM decisions WHERE batch=? AND kind='deploy' AND rule='deploy.checkpoint' AND subject=? AND json_extract(evidence,'$.sha')=?",
            params![id, target, sha],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Checkpoints written by this attempt (a failed T8a that wrote one left a deploy behind: `failed-dirty`).
    fn checkpoints_of(&self, id: &str, attempt: i64) -> Result<i64> {
        Ok(self.ledger.conn.query_row(
            "SELECT COUNT(*) FROM decisions WHERE batch=? AND kind='deploy' AND rule='deploy.checkpoint' AND json_extract(evidence,'$.attempt')=?",
            params![id, attempt],
            |r| r.get(0),
        )?)
    }

    fn stamp_path(&self, sha: &str) -> PathBuf {
        self.state_dir.join("stamps").join(format!("{sha}.stamp"))
    }

    /// `[preflight] chain_command` is set: the pre-flight is a launched run (state `preflighting`), not a blocking command.
    fn launched_preflight(&self) -> bool {
        !self.cfg.preflight_chain_command.is_empty()
    }

    /// Admission with the disk floor (engine-v2 T3, T4): when the host refuses for disk and policy allows `tidy:auto` (`[disk] tidy_auto`),
    /// tidy once through the seam, measure again, and admit or refuse with `host.refuse-disk`. One tidy row (`tidy.auto-disk`, outcome
    /// `recovered` or `still-below`); the refusal row is the caller's. Still below after allowed tidies on two batches in a row is a
    /// founder escalation row (`host.disk-escalation`).
    fn admit_floor(&mut self, role: &str, id: &str) -> Result<Option<(String, String)>> {
        let first = self.adm.admit_rule(role, id)?;
        let Some((rule, _)) = &first else { return Ok(None) };
        if rule != "host.refuse-disk" || !self.cfg.tidy_auto {
            return Ok(first);
        }
        // once per batch: a second refusal of the same batch is a refusal, not another tidy
        let ran: i64 = self.ledger.conn.query_row("SELECT COUNT(*) FROM decisions WHERE rule='tidy.auto-disk' AND batch=?", [id], |r| r.get(0)).unwrap_or(0);
        if ran > 0 {
            return Ok(first);
        }
        let note = match self.adm.tidy_auto(role) {
            Ok(Some(n)) => n,
            Ok(None) => return Ok(first),
            Err(e) => {
                // a tidy that failed is a row and the original refusal, never an engine error
                self.decide("tidy", id, None, "tidy.auto-disk", "failed", format!("tidy --auto could not run: {e}"), Some(json!({"role": role})))?;
                return Ok(first);
            }
        };
        let again = self.adm.admit_rule(role, id)?;
        let prev: Option<String> = self.ledger.conn.query_row("SELECT outcome FROM decisions WHERE rule='tidy.auto-disk' AND batch<>? ORDER BY seq DESC LIMIT 1", [id], |r| r.get(0)).ok();
        // only a measured shortfall is "still below"; a floor that cannot be judged after the tidy is unknown and never escalates
        let below = again.as_ref().is_some_and(|(r, _)| r == "host.refuse-disk");
        let (outcome, state) = match &again {
            None => ("recovered", "above its floor".to_string()),
            Some(_) if below => ("still-below", "still below its floor".to_string()),
            Some((r, _)) => ("unknown", format!("not judged after it ({r})")),
        };
        self.decide("tidy", id, None, "tidy.auto-disk", outcome, format!("{note}; the disk is {state}"), Some(json!({"role": role})))?;
        let Some((r2, w2)) = again else { return Ok(None) };
        if !below {
            return Ok(Some((r2, w2)));
        }
        if prev.as_deref() == Some("still-below") {
            let already: i64 = self.ledger.conn.query_row("SELECT COUNT(*) FROM decisions WHERE rule='host.disk-escalation' AND batch=?", [id], |r| r.get(0)).unwrap_or(0);
            if already == 0 {
                self.decide("escalation", id, None, "host.disk-escalation", "founder", format!("the disk is still below its floor after allowed tidies on two batches in a row: {w2}"), None)?;
            }
        }
        Ok(Some((r2, format!("{w2} (still below after tidy:auto)"))))
    }

    /// `None` when the pre-flight of `tip` is proven, else why not. A launched pre-flight is proven by the stamp `gatetrain chain` wrote
    /// (`stamp-<sha>.json` naming the sha and the digest of the configured steps file); the engine's own `<sha>.stamp` is the older form.
    fn stamp_problem(&self, tip: &str, wd: &Path) -> Option<String> {
        if !self.launched_preflight() {
            return (!self.stamp_path(tip).exists()).then(|| format!("no green preflight stamp for {}", short(tip)));
        }
        self.chain_stamp_problem(tip, wd).map(|why| format!("no green preflight stamp for {}: {why}", short(tip)))
    }

    fn chain_stamp_problem(&self, sha: &str, wd: &Path) -> Option<String> {
        let dir = self.abs(wd, self.cfg.preflight_stamp_dir.as_deref().unwrap_or_default());
        let file = dir.join(format!("stamp-{sha}.json"));
        let Ok(text) = std::fs::read_to_string(&file) else { return Some(format!("no chain stamp {} (a green run of gatetrain chain writes it)", file.display())) };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { return Some(format!("the chain stamp {} is not JSON", file.display())) };
        if v["sha"].as_str() != Some(sha) {
            return Some(format!("the chain stamp {} names another commit", file.display()));
        }
        if let Some(want) = self.cfg.preflight_steps_file.as_deref() {
            let have = evidence::file_digest(&self.abs(wd, want));
            if have.is_none() || v["digest"].as_str() != have.as_deref() {
                return Some(format!("the stamp was written for another steps file than {want} (digest {} against {})", short(v["digest"].as_str().unwrap_or("none")), short(have.as_deref().unwrap_or("unreadable"))));
            }
        }
        None
    }

    /// The tip of the batch this one is stacked on, else the recorded base (what a stack or a restack is cut from).
    fn current_base(&self, f: &Folded) -> Result<String> {
        match f.s("base_batch") {
            Some(b) => {
                let bf = self.fold(&b)?;
                git(&PathBuf::from(bf.need("workdir")?), &["rev-parse", &format!("refs/heads/{}", bf.need("branch")?)])
            }
            None => f.need("base_sha"),
        }
    }

    /// `pipeline.base-moved` and `pipeline.base-unproven` (engine-v2 section 8) for a batch stacked on another batch.
    fn pipeline_guard(&self, f: &Folded) -> Result<Option<Planned>> {
        let Some(b) = f.s("base_batch") else { return Ok(None) };
        if self.current_base(f)? != f.need("base_sha")? {
            return Ok(Some(Planned::Refuse("pipeline.base-moved".into(), format!("batch {b} moved its tip after this batch was stacked on it; `batch restack` stacks it again on the new tip (its pre-flight reuses unchanged steps by key)"))));
        }
        let bs = self.fold(&b)?.state;
        if !self.cfg.prove_ahead && !matches!(bs.as_str(), "proven" | "merged" | "deploying" | "live" | "closed") {
            return Ok(Some(Planned::Refuse("pipeline.base-unproven".into(), format!("the base batch {b} is {bs}, not proven; a proof on an unproven base may be wasted ([pipeline] prove_ahead = true allows it)"))));
        }
        Ok(None)
    }

    /// Stack this batch again on the new tip of its base batch: a side event back to `open`; the next advance stacks (T1, a new attempt).
    pub fn restack(&mut self, id: &str) -> Result<Outcome> {
        let _git = self.git_scope(id);
        let f = self.fold(id)?;
        if let Some(o) = self.parked(id, &f)? {
            return Ok(o);
        }
        if let Some(p) = f.pending.clone() {
            return self.resume(id, &f, p);
        }
        let Some(b) = f.s("base_batch") else { return Ok(Outcome::Idle(format!("batch {id} is not stacked on another batch"))) };
        if !matches!(f.state.as_str(), "stacked" | "settled" | "preflighted" | "preflight-red") {
            return self.refuse(id, "pipeline.restack-state", format!("batch {id} is {}; only a stacked, settled or preflighted batch is restacked (stop the run first)", f.state));
        }
        let now = self.current_base(&f)?;
        if now == f.need("base_sha")? {
            return Ok(Outcome::Idle(format!("the tip of {b} has not moved")));
        }
        self.side(id, "restack", "open", "pipeline.restack", &format!("restack on the new tip {} of {b}", short(&now)))
    }

    fn tip(&self, f: &Folded) -> Result<String> {
        let wd = PathBuf::from(f.need("workdir")?);
        git(&wd, &["rev-parse", &format!("refs/heads/{}", f.need("branch")?)])
    }

    /// Lanes of the batch whose tip has no lane stamp (`stamp-<tip>.json` written by `gatetrain chain`, naming that sha).
    fn lanes_without_stamp(&self, f: &Folded, wd: &Path) -> Vec<String> {
        let dir = self.cfg.lane_stamp_dir.as_deref().map(Path::new).map(|d| if d.is_absolute() { d.to_path_buf() } else { wd.join(d) }).unwrap_or_default();
        let mut missing = vec![];
        for l in f.facts.get("lanes").and_then(|l| l.as_array()).cloned().unwrap_or_default() {
            let tip = l["tip"].as_str().unwrap_or_default();
            let ok = std::fs::read_to_string(dir.join(format!("stamp-{tip}.json"))).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).is_some_and(|v| v["sha"].as_str() == Some(tip));
            if !ok {
                missing.push(format!("{} ({})", l["ref"].as_str().unwrap_or("?"), short(tip)));
            }
        }
        missing
    }

    fn plan(&mut self, tr: &str, id: &str, f: &Folded, attempt: i64) -> Result<Planned> {
        let wd = PathBuf::from(f.need("workdir")?);
        let mk = |input: Option<String>, run: Option<String>, detail: Value| Planned::Go(Plan { tr: tr.into(), to: to_state(tr).into(), input_sha: input, run_id: run, attempt, detail });
        match tr {
            "T1" => {
                let base = self.current_base(f)?;
                // a lane of a batch stacked on another batch was cut below that batch's tip: its floor is the root base
                let floor = f.s("floor_sha").unwrap_or_else(|| base.clone());
                for l in f.facts.get("lanes").and_then(|l| l.as_array()).cloned().unwrap_or_default() {
                    let tip = l["tip"].as_str().unwrap_or_default();
                    let (anc, _, _) = git_raw(&wd, &["merge-base", "--is-ancestor", &floor, tip])?;
                    if !anc {
                        return Ok(Planned::Refuse("stack.base-ancestor".into(), format!("the base {} is not an ancestor of lane {}", short(&base), l["ref"].as_str().unwrap_or("?"))));
                    }
                }
                if self.cfg.require_lane_stamp {
                    let missing = self.lanes_without_stamp(f, &wd);
                    if !missing.is_empty() {
                        return Ok(Planned::Refuse("stack.lane-stamp".into(), format!("no green lane stamp ([lanes] require_stamp) for: {}", missing.join(", "))));
                    }
                }
                Ok(mk(Some(base.clone()), None, json!({"branch": format!("batch/{id}/s{attempt}"), "stack_attempt": attempt, "base_sha": base})))
            }
            "T2" => {
                let branch = f.need("branch")?;
                let on = git(&wd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
                if on != branch {
                    return Ok(Planned::Refuse("stack.branch-checked-out".into(), format!("the checkout is on {on}, the batch branch is {branch}")));
                }
                let marked = marker_paths(&wd, &f.need("base_sha")?, &self.tip(f)?)?;
                if !marked.is_empty() {
                    return Ok(Planned::Refuse("stack.conflict-markers".into(), format!("the stack commits conflict markers in: {}", marked.join(", "))));
                }
                let stray: Vec<String> = dirty_paths(&wd)?.into_iter().filter(|p| !in_dirs(p, &self.cfg.generated_dirs)).collect();
                if !stray.is_empty() {
                    return Ok(Planned::Refuse("floor.named-paths".into(), format!("paths outside the generated directories are changed: {}", stray.join(", "))));
                }
                Ok(mk(Some(self.tip(f)?), None, json!({})))
            }
            "T3" if self.launched_preflight() => {
                if let Some((rule, why)) = self.admit_floor("light", id)? {
                    return Ok(Planned::Refuse(rule, why));
                }
                let tip = self.tip(f)?;
                if let Some(st) = f.s("settled_tip").filter(|st| *st != tip) {
                    return Ok(Planned::Refuse("settle.tip-moved".into(), format!("the batch branch moved from the settled {} to {} after settle; settle must be the last commit before the chain", short(&st), short(&tip))));
                }
                let run = format!("{id}-f{attempt}");
                let log = self.state_dir.join("logs").join(format!("{run}.log"));
                Ok(Planned::Go(Plan { tr: tr.into(), to: "preflighting".into(), input_sha: Some(tip.clone()), run_id: Some(run.clone()), attempt, detail: json!({"pf_run_id": run, "pf_log": log.to_string_lossy(), "pf_sha": tip}) }))
            }
            "T3" => {
                if self.cfg.preflight_command.is_empty() {
                    return Ok(Planned::Refuse("config.preflight-command".into(), "gatetrain.toml has no [preflight] command".into()));
                }
                if let Some((rule, why)) = self.admit_floor("light", id)? {
                    return Ok(Planned::Refuse(rule, why));
                }
                let tip = self.tip(f)?;
                if let Some(st) = f.s("settled_tip").filter(|st| *st != tip) {
                    return Ok(Planned::Refuse("settle.tip-moved".into(), format!("the batch branch moved from the settled {} to {} after settle; settle must be the last commit before the chain", short(&st), short(&tip))));
                }
                Ok(mk(Some(tip), None, json!({})))
            }
            "T4" => {
                let tip = self.tip(f)?;
                if let Some(st) = f.s("settled_tip").filter(|st| *st != tip) {
                    return Ok(Planned::Refuse("settle.tip-moved".into(), format!("the batch branch moved from the settled {} to {} after settle; settle must be the last commit before the chain", short(&st), short(&tip))));
                }
                if let Some(why) = self.stamp_problem(&tip, &wd) {
                    return Ok(Planned::Refuse("preflight.stamp-for-sha".into(), why));
                }
                if let Some(r) = self.pipeline_guard(f)? {
                    return Ok(r);
                }
                let dirty = dirty_paths(&wd)?;
                if !dirty.is_empty() {
                    let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
                    return Ok(Planned::Refuse("floor.clean-tree".into(), format!("the checkout has uncommitted paths ({}); the chain would test files that are not in the sha that is merged", shown.join(", "))));
                }
                if self.cfg.chain_command.is_empty() {
                    return Ok(Planned::Refuse("config.chain-command".into(), "gatetrain.toml has no [chain] command".into()));
                }
                if let Some((rule, why)) = self.admit_floor("proof", id)? {
                    return Ok(Planned::Refuse(rule, why));
                }
                if let Some(r) = self.full_run_budget(id)? {
                    return Ok(r);
                }
                let run = format!("{id}-p{}", self.next_run_no(f));
                let log = self.state_dir.join("logs").join(format!("{run}.log"));
                Ok(mk(Some(tip.clone()), Some(run.clone()), json!({"run_id": run, "log": log.to_string_lossy(), "tested_sha": tip})))
            }
            "T6a" => {
                let tip = self.tip(f)?;
                let (run0, tested) = (f.need("run_id")?, f.need("tested_sha")?);
                if tip == tested && !self.cause_noted(id, &run0)? {
                    return Ok(Planned::Refuse("retry.causes-open".into(), format!("run {run0} was red on {} and the batch tip has not moved; commit a fix, or record the cause as a flake or infrastructure with a reason", short(&tested))));
                }
                if let Some(r) = self.full_run_budget(id)? {
                    return Ok(r);
                }
                if let Some(why) = self.stamp_problem(&tip, &wd) {
                    return Ok(Planned::Refuse("preflight.stamp-for-sha".into(), why));
                }
                if let Some(r) = self.pipeline_guard(f)? {
                    return Ok(r);
                }
                let dirty = dirty_paths(&wd)?;
                if !dirty.is_empty() {
                    let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
                    return Ok(Planned::Refuse("floor.clean-tree".into(), format!("the checkout has uncommitted paths ({}); the chain would test files that are not in the sha that is merged", shown.join(", "))));
                }
                if self.cfg.chain_command.is_empty() {
                    return Ok(Planned::Refuse("config.chain-command".into(), "gatetrain.toml has no [chain] command".into()));
                }
                if let Some((rule, why)) = self.admit_floor("proof", id)? {
                    return Ok(Planned::Refuse(rule, why));
                }
                let run = format!("{id}-p{}", self.next_run_no(f));
                let log = self.state_dir.join("logs").join(format!("{run}.log"));
                // settled_tip follows the tip: a stopped retry returns to `preflighted`, whose launch guard compares it
                Ok(mk(Some(tip.clone()), Some(run.clone()), json!({"run_id": run, "log": log.to_string_lossy(), "tested_sha": tip, "settled_tip": tip, "target_dirs": self.cfg.chain_target_dirs})))
            }
            "T7" => {
                let tip = self.tip(f)?;
                let tested = f.need("tested_sha")?;
                // the tip may be ahead of the tested sha only by the engine's own generated-delta commit (an earlier T7 attempt made it):
                // only files under the generated directories differ, and the effect proves them a fixpoint again before it merges
                if tested != tip && !self.generated_only(&wd, &tested, &tip)? {
                    return Ok(Planned::Refuse("merge.tested-equals-merged".into(), format!("tested {}, tip {} (mode sha); restack and prove again", short(&tested), short(&tip))));
                }
                let pref = format!("refs/heads/{}", self.cfg.protected);
                let old = git(&wd, &["rev-parse", &pref])?;
                if let Some(base) = f.s("base_batch") {
                    // a batch stacked on another carries the base's commits: a fast-forward to its tip would merge them unproven
                    let on = f.s("base_sha").is_some_and(|b| git_raw(&wd, &["merge-base", "--is-ancestor", &b, &old]).map(|r| r.0).unwrap_or(false));
                    if !on {
                        return Ok(Planned::Refuse("pipeline.base-not-merged".into(), format!("batch {base} is the base of this batch and is not on {} yet; merge {base} first (a fast-forward to this tip would carry its unproven commits)", self.cfg.protected)));
                    }
                }
                if let Some((rule, why)) = self.another_batch_deploying(id, "T7")? {
                    return Ok(Planned::Refuse(rule.into(), why));
                }
                let (ff, _, _) = git_raw(&wd, &["merge-base", "--is-ancestor", &old, &tip])?;
                if !ff {
                    return Ok(Planned::Refuse("merge.fast-forward".into(), format!("{} cannot fast-forward to {}; the protected branch moved", self.cfg.protected, short(&tip))));
                }
                // a checked-out protected branch must move with its working tree: fast-forward it there instead of moving the bare ref
                let checkout = protected_checkout(&wd, &self.cfg.protected)?;
                if let Some(c) = &checkout {
                    let dirty = dirty_paths(Path::new(c))?;
                    if !dirty.is_empty() {
                        let shown: Vec<&str> = dirty.iter().take(3).map(String::as_str).collect();
                        return Ok(Planned::Refuse("merge.checkout-dirty".into(), format!("{} is checked out in {c} with uncommitted changes ({}); commit or stash there so the merge can update it", self.cfg.protected, shown.join(", "))));
                    }
                }
                Ok(mk(Some(tip.clone()), None, json!({"merge_old": old, "merge_new": tip, "tested": tested, "protected": self.cfg.protected, "mode": "sha", "checkout": checkout})))
            }
            "T8a" => {
                if let Some((rule, why)) = self.deploy_still_running(id) {
                    return Ok(Planned::Refuse(rule.into(), why));
                }
                if let Some((rule, why)) = self.another_batch_deploying(id, "T8a")? {
                    return Ok(Planned::Refuse(rule.into(), why));
                }
                let (mut auto, mut prod) = (vec![], vec![]);
                for (n, t) in &self.cfg.targets {
                    if t.tier == "production" {
                        prod.push(n.clone());
                    } else {
                        auto.push(n.clone());
                    }
                }
                for n in &prod {
                    // floor.production-human: recorded, never deployed by the engine
                    self.decide("refusal", id, Some(n.clone()), "floor.production-human", "refused", format!("target {n} is production and needs a human; skipped"), None)?;
                }
                let sha = f.s("output_sha").map_or_else(|| f.need("merge_new"), Ok)?;
                if let Some(why) = self.superseded(id, &wd, &sha, &auto)? {
                    return Ok(Planned::Refuse("deploy.superseded".into(), why));
                }
                Ok(mk(Some(sha), None, json!({"targets": auto, "skipped_production": prod})))
            }
            _ => Err(Error::Batch(format!("no plan for {tr}"))),
        }
    }

    // ------------------------------------------------------------ effects

    fn env(&self, id: &str, f: &Folded, sha: &str, run_id: &str, log: &str) -> Vec<(String, String)> {
        vec![
            ("GATETRAIN_BATCH".into(), id.into()),
            ("GATETRAIN_SHA".into(), sha.into()),
            ("GATETRAIN_RUN_ID".into(), run_id.into()),
            ("GATETRAIN_LOG".into(), log.into()),
            ("GATETRAIN_BASE".into(), f.s("base_sha").unwrap_or_default()),
        ]
    }

    /// `<state>/evidence/<run>.json`: the results for `sha` that pass every condition of 7.2 but the key (the chain compares keys).
    fn write_evidence(&self, wd: &Path, sha: &str, run: &str) -> Result<PathBuf> {
        let dirs: Vec<PathBuf> = self.cfg.evidence_stamp_dirs.iter().map(|d| self.abs(wd, d)).collect();
        let plan = self.cfg.steps_file.as_deref().and_then(|s| evidence::file_digest(&self.abs(wd, s)));
        let host = self.cfg.proof_host.clone().unwrap_or_default();
        let ctx = evidence::Ctx { sha, host: &host, host_class: self.cfg.proof_host_class.as_deref(), plan_digest: plan.as_deref(), now: parse_iso(&(self.now)()), cfg: self.cfg, state_dir: &self.state_dir };
        let rows = evidence::select_for_launch(&evidence::load_results(&dirs), &ctx, &parse_iso);
        let file = self.state_dir.join("evidence").join(format!("{run}.json"));
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, serde_json::to_string_pretty(&evidence::evidence_file(sha, &rows))?)?;
        Ok(file)
    }

    fn effect(&mut self, id: &str, f: &Folded, plan: &Plan) -> Result<std::result::Result<Done, String>> {
        let wd = PathBuf::from(f.need("workdir")?);
        match plan.tr.as_str() {
            "T1" => {
                let base = plan.detail["base_sha"].as_str().map(String::from).map_or_else(|| f.need("base_sha"), Ok)?;
                let branch = plan.detail["branch"].as_str().unwrap_or_default().to_string();
                git(&wd, &["switch", "-c", &branch, &base])?;
                let lanes = f.facts.get("lanes").and_then(|l| l.as_array()).cloned().unwrap_or_default();
                let mut subjects = vec![];
                if !self.cfg.stack_command.is_empty() {
                    // the repository's own stack script (hub rebuild, hub guard) builds the stack; the engine checks what it left
                    let refs: Vec<String> = lanes.iter().map(|l| l["ref"].as_str().unwrap_or_default().to_string()).collect();
                    let tips: Vec<String> = lanes.iter().map(|l| format!("{}={}", l["ref"].as_str().unwrap_or_default(), l["tip"].as_str().unwrap_or_default())).collect();
                    let mut env = self.env(id, f, &base, "", "");
                    env.push(("GATETRAIN_BRANCH".into(), branch.clone()));
                    env.push(("GATETRAIN_LANES".into(), refs.join(" ")));
                    env.push(("GATETRAIN_LANE_TIPS".into(), tips.join(" ")));
                    let (rc, err) = match self.plugin(id, "T1", "stack command", " ([timeouts] plugin_min)", &self.cfg.stack_command, &wd, &env, self.plugin_min())? {
                        Ok(x) => x,
                        Err(why) => return Ok(Err(why)),
                    };
                    if rc != 0 {
                        return Ok(Err(format!("stack command exited {rc}: {err}")));
                    }
                    let on = git(&wd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
                    if on != branch {
                        return Ok(Err(format!("stack command left the checkout on {on}, not on {branch}")));
                    }
                    let left = dirty_paths(&wd)?;
                    if !left.is_empty() {
                        return Ok(Err(format!("stack command left uncommitted paths: {}", left.join(", "))));
                    }
                    for l in &lanes {
                        subjects.push((l["ref"].as_str().unwrap_or("?").to_string(), git(&wd, &["log", "-1", "--format=%s", l["tip"].as_str().unwrap_or_default()])?));
                    }
                } else {
                    for l in &lanes {
                        let tip = l["tip"].as_str().unwrap_or_default().to_string();
                        let commits = git(&wd, &["rev-list", "--reverse", "--no-merges", &format!("{base}..{tip}")])?;
                        for c in commits.lines() {
                            let (ok, _, err) = git_raw(&wd, &["cherry-pick", c])?;
                            if !ok {
                                let first = err.lines().next().unwrap_or("").to_string();
                                let why = match self.resolve(id, &wd, &first) {
                                    Ok(Ok(())) => continue,
                                    Ok(Err(why)) => why,
                                    Err(e) => e.to_string(),
                                };
                                let _ = git_raw(&wd, &["cherry-pick", "--abort"]);
                                return Ok(Err(format!("conflict applying {} from lane {}: {why}", short(c), l["ref"].as_str().unwrap_or("?"))));
                            }
                        }
                        subjects.push((l["ref"].as_str().unwrap_or("?").to_string(), git(&wd, &["log", "-1", "--format=%s", &tip])?));
                    }
                }
                let have = git(&wd, &["log", "--format=%s", &format!("{base}..{branch}")])?;
                for (lane, s) in subjects {
                    if !have.lines().any(|h| h == s) {
                        return Ok(Err(format!("lane {lane}: tip subject {s:?} is not in the stack (L33)")));
                    }
                }
                let tip = git(&wd, &["rev-parse", &branch])?;
                Ok(Ok(Done { output: Some(tip), extra: json!({"branch": branch}) }))
            }
            "T2" => {
                let dirs = self.cfg.generated_dirs.clone();
                if self.cfg.regen_command.is_empty() {
                    let head = git(&wd, &["rev-parse", "HEAD"])?;
                    return Ok(Ok(Done { output: Some(head.clone()), extra: json!({"passes": 0, "paths": [], "settled_tip": head}) }));
                }
                let env = self.env(id, f, &plan.input_sha.clone().unwrap_or_default(), "", "");
                let (mut committed, mut passes) = (vec![], 0usize);
                loop {
                    if passes >= self.cfg.max_passes {
                        return Ok(Err(format!("no fixpoint after {} regen passes", self.cfg.max_passes)));
                    }
                    passes += 1;
                    let (rc, err) = match self.plugin(id, "T2", "regen", " ([timeouts] plugin_min)", &self.cfg.regen_command, &wd, &env, self.plugin_min())? {
                        Ok(x) => x,
                        Err(why) => return Ok(Err(why)),
                    };
                    if rc != 0 {
                        return Ok(Err(format!("regen command exited {rc}: {err}")));
                    }
                    let dirty = dirty_paths(&wd)?;
                    if dirty.is_empty() {
                        break;
                    }
                    let stray: Vec<&String> = dirty.iter().filter(|p| !in_dirs(p, &dirs)).collect();
                    if !stray.is_empty() {
                        return Ok(Err(format!("floor.named-paths: regen changed paths outside the generated directories: {}", stray.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
                    }
                    // stage by name, new files included; never `add -A`
                    let mut args = vec!["add", "--"];
                    args.extend(dirty.iter().map(String::as_str));
                    git(&wd, &args)?;
                    let mut c = vec!["commit".to_string(), "-q".into(), "-m".into(), format!("chore(batch): regenerate generated files for {id} (pass {passes})"), "--".into()];
                    c.extend(dirty.iter().cloned());
                    git(&wd, &c.iter().map(String::as_str).collect::<Vec<_>>())?; // an explicit pathspec: only these files, whatever else is staged
                    committed.extend(dirty);
                }
                let head = git(&wd, &["rev-parse", "HEAD"])?;
                Ok(Ok(Done { output: Some(head.clone()), extra: json!({"passes": passes, "paths": committed, "settled_tip": head}) }))
            }
            "T3" if plan.detail.get("pf_run_id").is_some() => {
                let run = plan.run_id.clone().unwrap_or_default();
                let log = PathBuf::from(plan.detail["pf_log"].as_str().unwrap_or_default());
                std::fs::create_dir_all(log.parent().unwrap())?;
                let sha = plan.input_sha.clone().unwrap_or_default();
                let command: Vec<String> = self.cfg.preflight_chain_command.iter().map(|a| a.replace("{sha}", &sha).replace("{run}", &run).replace("{log}", &log.to_string_lossy())).collect();
                let env = self.env(id, f, &sha, &run, &log.to_string_lossy());
                self.lock.launch(&LaunchRequest { run_id: run.clone(), batch: id.into(), sha, base: f.s("base_sha").unwrap_or_default(), command, log: log.clone(), workdir: wd, env, role: "light".into() })?;
                if let Err(why) = self.await_ack(&run, &log)? {
                    return Ok(Err(why));
                }
                Ok(Ok(Done { output: None, extra: json!({}) }))
            }
            "T3" => {
                let sha = plan.input_sha.clone().unwrap_or_default();
                let env = self.env(id, f, &sha, "", "");
                let (rc, err) = match self.plugin(id, "T3", "preflight", " ([preflight] timeout_min)", &self.cfg.preflight_command, &wd, &env, self.cfg.preflight_timeout_min)? {
                    Ok(x) => x,
                    Err(why) => return Ok(Err(why)),
                };
                if rc != 0 {
                    return Ok(Err(format!("preflight exited {rc}: {err}")));
                }
                let p = self.stamp_path(&sha);
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, format!("{sha}\n"))?;
                Ok(Ok(Done { output: Some(sha.clone()), extra: json!({"stamp": p.to_string_lossy()}) }))
            }
            "T4" | "T6a" => {
                let run = plan.run_id.clone().unwrap_or_default();
                let log = PathBuf::from(plan.detail["log"].as_str().unwrap_or_default());
                std::fs::create_dir_all(log.parent().unwrap())?;
                let sha = plan.input_sha.clone().unwrap_or_default();
                let mut env = self.env(id, f, &sha, &run, &log.to_string_lossy());
                let mut command = self.cfg.chain_command.clone();
                // the evidence the chain may use (engine-v2 7.3): written by the engine from its own stamps, offered, never trusted at T5
                if !self.cfg.evidence_stamp_dirs.is_empty() || command.iter().any(|a| a.contains("{evidence}")) {
                    let file = self.write_evidence(&wd, &sha, &run)?;
                    command = command.iter().map(|a| a.replace("{evidence}", &file.to_string_lossy())).collect();
                    env.push(("GATETRAIN_EVIDENCE".into(), file.to_string_lossy().into_owned()));
                }
                let req = LaunchRequest { run_id: run.clone(), batch: id.into(), sha: sha.clone(), base: f.s("base_sha").unwrap_or_default(), command, log: log.clone(), workdir: wd, env, role: "proof".into() };
                self.lock.launch(&req)?;
                if let Err(why) = self.await_ack(&run, &log)? {
                    return Ok(Err(why));
                }
                Ok(Ok(Done { output: None, extra: json!({}) }))
            }
            "T7" => {
                // what the chain proved: the tip, or (after an attempt that committed the generated delta) an ancestor of it
                let tested = plan.detail["tested"].as_str().or(plan.detail["merge_new"].as_str()).unwrap_or_default().to_string();
                let old = plan.detail["merge_old"].as_str().unwrap_or_default();
                // generated-delta: files the chain itself produced are committed BY NAME (never `add -A`), the generators run again and must
                // change nothing, and only generated files may differ from what was tested
                let dirty = dirty_paths(&wd)?;
                if !dirty.is_empty() {
                    let stray: Vec<&String> = dirty.iter().filter(|p| !in_dirs(p, &self.cfg.generated_dirs)).collect();
                    if !stray.is_empty() {
                        return Ok(Err(format!("chain.tree-differs: the tree holds changes outside the generated directories: {}", stray.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))));
                    }
                    let mut args = vec!["add", "--"];
                    args.extend(dirty.iter().map(String::as_str));
                    git(&wd, &args)?;
                    let mut c = vec!["commit".to_string(), "-q".into(), "-m".into(), format!("chore(batch): commit the generated files the chain produced for {id} (generated-delta)"), "--".into()];
                    c.extend(dirty.iter().cloned());
                    git(&wd, &c.iter().map(String::as_str).collect::<Vec<_>>())?; // an explicit pathspec: only these files, whatever else is staged
                }
                // whenever the merge would carry generated files the chain did not test (this attempt's commit or an earlier one's), the
                // generators run again and must change nothing
                if git(&wd, &["rev-parse", "HEAD"])? != tested && !self.cfg.regen_command.is_empty() {
                    let env = self.env(id, f, &tested, "", "");
                    let (rc, err) = match self.plugin(id, "T7", "regen", " ([timeouts] plugin_min)", &self.cfg.regen_command, &wd, &env, self.plugin_min())? {
                        Ok(x) => x,
                        Err(why) => return Ok(Err(why)),
                    };
                    if rc != 0 {
                        return Ok(Err(format!("regen command exited {rc} while re-settling the chain's generated files: {err}")));
                    }
                    let again = dirty_paths(&wd)?;
                    if !again.is_empty() {
                        return Ok(Err(format!("the generated files are not a fixpoint after the chain (the generators changed: {})", again.join(", "))));
                    }
                }
                let head = git(&wd, &["rev-parse", "HEAD"])?;
                let mut final_new = tested.clone();
                if head != tested {
                    let names = git(&wd, &["diff", "--name-only", "--no-renames", &tested, &head])?;
                    let bad: Vec<&str> = names.lines().filter(|p| !in_dirs(p, &self.cfg.generated_dirs)).collect();
                    if !bad.is_empty() {
                        return Ok(Err(format!("chain.tree-differs: the merge would carry changes beyond the generated files that were tested against: {}", bad.join(", "))));
                    }
                    final_new = head;
                }
                let new: &str = &final_new;
                if let Some(c) = plan.detail["checkout"].as_str() {
                    // the branch is checked out in `c`: a fast-forward there moves the ref and the working tree together
                    let (ok, _, err) = git_raw(Path::new(c), &["merge", "--ff-only", new])?;
                    if !ok {
                        return Ok(Err(format!("fast-forward of {} in {c} failed: {}", self.cfg.protected, err.trim())));
                    }
                    let cur = git(&wd, &["rev-parse", &format!("refs/heads/{}", self.cfg.protected)])?;
                    if cur != new || !dirty_paths(Path::new(c))?.is_empty() {
                        return Ok(Err(format!("after the fast-forward {c} is not at {} with a clean tree", short(new))));
                    }
                } else {
                    // compare-and-swap: update-ref fails when the ref no longer holds `old`
                    let (ok, _, err) = git_raw(&wd, &["update-ref", &format!("refs/heads/{}", self.cfg.protected), new, old])?;
                    if !ok {
                        return Ok(Err(format!("compare-and-swap of {} failed: {}", self.cfg.protected, err.trim())));
                    }
                }
                Ok(Ok(Done { output: Some(new.into()), extra: json!({"merge_new": new}) }))
            }
            "T8a" => {
                T8A_SPAWNED.with(|c| c.set(false));
                let sha = plan.input_sha.clone().unwrap_or_default();
                if !self.cfg.postmerge_command.is_empty() && !self.checkpointed(id, &sha, POSTMERGE)? {
                    // the target checkout needs its dependencies and packages before anything is deployed from it
                    let dir = self.cfg.postmerge_dir.as_deref().map(PathBuf::from).unwrap_or_else(|| wd.clone());
                    let env = self.env(id, f, &sha, "", "");
                    let (rc, err) = match self.plugin(id, "T8a", "postmerge", " ([postmerge] timeout_min)", &substitute(&self.cfg.postmerge_command, &sha), &dir, &env, self.cfg.postmerge_timeout_min)? {
                        Ok(x) => x,
                        Err(why) => return Ok(Err(why)),
                    };
                    if rc != 0 {
                        return Ok(Err(format!("post-merge command exited {rc}: {err}")));
                    }
                    self.checkpoint_step(id, &sha, plan.attempt, POSTMERGE)?;
                }
                for n in plan.detail["targets"].as_array().cloned().unwrap_or_default() {
                    let name = n.as_str().unwrap_or_default();
                    if self.checkpointed(id, &sha, name)? {
                        continue; // deployed by an earlier attempt for this sha: never twice
                    }
                    let Some(t) = self.cfg.targets.get(name) else {
                        return Ok(Err(format!("deploy.target-unknown: target {name} is not in gatetrain.toml any more; a person decides what this batch deploys")));
                    };
                    let env = self.env(id, f, &sha, "", "");
                    if !t.probe.is_empty() {
                        // no checkpoint: ask the target whether it already serves the sha (a crash after its deploy, before its row)
                        let (rc, _) = match self.plugin(id, "T8a", &format!("probe {name}"), " ([timeouts] plugin_min)", &substitute(&t.probe, &sha), &wd, &env, self.plugin_min())? {
                            Ok(x) => x,
                            Err(why) => return Ok(Err(why)),
                        };
                        if rc == 0 {
                            self.checkpoint_step(id, &sha, plan.attempt, name)?;
                            continue;
                        }
                    }
                    let Some(t) = self.cfg.targets.get(name) else { return Ok(Err(format!("deploy.target-unknown: target {name} is not in gatetrain.toml any more"))) };
                    let cmd = substitute(&t.command, &sha);
                    let (min, survives) = (t.timeout_min.unwrap_or(DEPLOY_TIMEOUT_MIN), t.survives_parent);
                    let (rc, err) = match self.plugin_as(id, "T8a", &format!("deploy {name}"), &format!(" ([deploy.{name}] timeout_min)"), &cmd, &wd, &env, min, survives)? {
                        Ok(x) => x,
                        Err(why) => return Ok(Err(why)),
                    };
                    if rc != 0 {
                        return Ok(Err(format!("deploy {name} exited {rc}: {err}")));
                    }
                    // test seam: the process dies right after this target deployed, before its checkpoint and before `done`
                    crash_point(&format!("T8a:{name}"))?;
                    self.checkpoint_step(id, &sha, plan.attempt, name)?;
                }
                Ok(Ok(Done { output: Some(sha), extra: json!({}) }))
            }
            t => Err(Error::Batch(format!("no effect for {t}"))),
        }
    }

    // ------------------------------------------------------------ reads without an intent (T5, T8b, T9)

    #[allow(clippy::too_many_arguments)]
    fn plain(&mut self, id: &str, f: &Folded, tr: &str, to: &str, input: Option<String>, run: Option<String>, detail: Value, outcome: &str, note: String) -> Result<Outcome> {
        let from = f.state.clone();
        let n = f.max_attempt.get(tr).copied().unwrap_or(0) + 1;
        let d = self.decide("transition", id, input.clone(), &format!("transition.{tr}"), outcome, format!("{from} -> {to}: {note}"), Some(json!({"attempt": n})))?;
        self.event(id, tr, "done", n, Some(to), input.as_deref(), None, run.as_deref(), Some(d), &detail)?;
        Ok(Outcome::Advanced { from, to: to.into(), note })
    }

    /// The one evidence rule (engine-v2 7.2) for the cached steps of a proof log. `Err(because)` = refuse with `proof.cached-unverified`;
    /// `Ok(rows)` = every cached step is accepted, with the evidence rows to record in the proof.
    ///
    /// A line `STEP_CACHED <step> <key> from <run>` is judged by [`evidence::verify`] against the engine's own stamps; the proof host is
    /// never trusted to vouch for itself. A bare `STEP_CACHED <step>` carries no key (no `inputs`, condition 7): it counts only when the
    /// repository's own `[chain] cache_verify_command` vouches for it, as before.
    #[allow(clippy::too_many_arguments)]
    fn verify_cached(&self, id: &str, f: &Folded, text: &str, p: &crate::logfmt::ParsedLog, cached: &[&str], tested: &str, run: &str, log: &Path) -> Result<std::result::Result<Vec<Value>, String>> {
        let refuse = |why: &str, b: String| Ok(Err(format!("{b} (why: {why})")));
        if self.cfg.allow_cached_steps == "never" {
            return refuse("policy-never", format!("step(s) {} came from a step cache and [merge] allow_cached_steps is never", cached.join(", ")));
        }
        let wd = PathBuf::from(f.need("workdir")?);
        let lines = evidence::cached_lines(text);
        let keyed = |s: &str| lines.iter().find(|l| l.0 == s).and_then(|l| l.1.clone());
        let dirs: Vec<PathBuf> = self.cfg.evidence_stamp_dirs.iter().map(|d| self.abs(&wd, d)).collect();
        let results = evidence::load_results(&dirs);
        let plan = self.cfg.steps_file.as_deref().and_then(|s| evidence::file_digest(&self.abs(&wd, s)));
        let host = p.host.clone().or_else(|| self.cfg.proof_host.clone()).unwrap_or_default();
        let ctx = evidence::Ctx { sha: tested, host: &host, host_class: self.cfg.proof_host_class.as_deref(), plan_digest: plan.as_deref(), now: parse_iso(&(self.now)()), cfg: self.cfg, state_dir: &self.state_dir };
        let mut ok = vec![];
        let mut keyless: Vec<&str> = vec![];
        for s in cached {
            match keyed(s) {
                Some(key) => match evidence::verify(s, &key, &results, &ctx, &parse_iso) {
                    evidence::Verdict::Accepted(a) => ok.push(json!({"step": s, "source_run": a.source_run, "input_key": a.input_key, "source_host": a.source_host, "stamp_digest": a.stamp_digest, "verified_by": a.verified_by})),
                    evidence::Verdict::Refused { why, detail } => return refuse(why, format!("cached step {s} is not accepted for {}: {detail}", short(tested))),
                },
                None => keyless.push(s),
            }
        }
        if !keyless.is_empty() {
            // a cached step counts only when its entry is verified for this sha on this host (another host's cache is not this host's)
            if self.cfg.cache_verify_command.is_empty() {
                return refuse("evidence-undeclared", format!("step(s) {} came from a step cache and no [chain] cache_verify_command proves the entries for {}", keyless.join(", "), short(tested)));
            }
            for s in keyless {
                let mut env = self.env(id, f, tested, run, &log.to_string_lossy());
                env.push(("GATETRAIN_STEP".into(), s.to_string()));
                let rc = match self.plugin(id, "T5", "cache verify", " ([timeouts] plugin_min)", &substitute(&self.cfg.cache_verify_command, tested), &wd, &env, self.plugin_min())? {
                    Ok((rc, _)) => rc,
                    Err(why) => return refuse("evidence-missing", format!("cached step {s} is not verified for {}: {why}", short(tested))),
                };
                if rc != 0 {
                    return refuse("evidence-missing", format!("cached step {s} is not verified for {} (the verify command exited {rc})", short(tested)));
                }
                ok.push(json!({"step": s, "verified_by": "command"}));
            }
        }
        Ok(Ok(ok))
    }

    /// A configured path: absolute as it is, else relative to the batch checkout.
    fn abs(&self, wd: &Path, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            wd.join(p)
        }
    }

    /// T3o: a pure read of the launched pre-flight's log, its stamp and the lock.
    fn observe_preflight(&mut self, id: &str, f: &Folded) -> Result<Outcome> {
        let run = f.need("pf_run_id")?;
        let sha = f.need("pf_sha")?;
        let log = PathBuf::from(f.need("pf_log")?);
        let text = std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let p = parse_log(&text)?;
        if !p.gates_done {
            if self.lock.names_run(&run)? {
                return Ok(Outcome::Waiting(format!("pre-flight {run} is still running")));
            }
            // stopped, not red: the next launch is a new run id and a new log
            return self.plain(id, f, "T3s", "settled", Some(sha), Some(run.clone()), json!({"stopped_run": run}), "recorded", format!("pre-flight {run} stopped without GATES_DONE"));
        }
        evidence::record_shadow(&self.state_dir, &run, &text)?;
        if p.mode.as_deref() != Some("preflight") {
            return self.refuse(id, "floor.preflight-mode", format!("the pre-flight run's mode is {:?}, not preflight", p.mode.unwrap_or_default()));
        }
        let mut reds: Vec<Value> = p.red_now.iter().map(|(s, rc, _)| json!({"step": s, "rc": rc})).collect();
        for s in &p.steps {
            if s.exit_code.is_some_and(|c| c != 0) && !reds.iter().any(|r| r["step"] == json!(s.name)) {
                reds.push(json!({"step": s.name, "rc": s.exit_code}));
            }
        }
        if !reds.is_empty() {
            let note = format!("{} red step(s) in the pre-flight", reds.len());
            return self.plain(id, f, "T3r", "preflight-red", Some(sha), Some(run), json!({"pf_findings": reds}), "recorded", note);
        }
        let logged = p.commit.clone().unwrap_or_default();
        if logged.len() < 7 || !sha.starts_with(&logged) {
            return self.refuse(id, "floor.proven-sha", format!("the pre-flight log ran commit {logged:?}, the batch tip it was launched for is {}", short(&sha)));
        }
        let tip = self.tip(f)?;
        if tip != sha {
            return self.refuse(id, "settle.tip-moved", format!("the batch branch moved from {} to {} while its pre-flight ran", short(&sha), short(&tip)));
        }
        let wd = PathBuf::from(f.need("workdir")?);
        if let Some(why) = self.chain_stamp_problem(&sha, &wd) {
            return self.refuse(id, "preflight.stamp-for-sha", format!("no green preflight stamp for {}: {why}", short(&sha)));
        }
        if !self.cfg.legacy_stamp_command.is_empty() {
            // the bridge for a launcher that still demands the repository's old stamp: runs once the chain stamp is read, idempotent by contract
            let stamp = self.abs(&wd, self.cfg.preflight_stamp_dir.as_deref().unwrap_or_default()).join(format!("stamp-{sha}.json"));
            let cmd: Vec<String> = self.cfg.legacy_stamp_command.iter().map(|a| a.replace("{sha}", &sha).replace("{results}", &stamp.to_string_lossy())).collect();
            let (rc, err) = match self.plugin(id, "T3o", "legacy stamp", " ([timeouts] plugin_min)", &cmd, &wd, &self.env(id, f, &sha, &run, &log.to_string_lossy()), self.plugin_min())? {
                Ok(x) => x,
                Err(why) => (-1, why),
            };
            if rc != 0 {
                return self.refuse(id, "preflight.legacy-stamp", format!("[preflight] legacy_stamp_command exited {rc}: {err}; the batch stays preflighting and the command is tried again"));
            }
        }
        self.plain(id, f, "T3o", "preflighted", Some(sha), Some(run), json!({"pf_stamped": true}), "allowed", "GATES_DONE, preflight mode, zero red, a chain stamp for the tip".into())
    }

    /// T5: a pure read of the chain log and the lock.
    fn observe(&mut self, id: &str, f: &Folded) -> Result<Outcome> {
        let run = f.need("run_id")?;
        let tested = f.need("tested_sha")?;
        let log = PathBuf::from(f.need("log")?);
        let text = std::fs::read(&log).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let p = parse_log(&text)?;
        if !p.gates_done {
            if self.lock.names_run(&run)? {
                return Ok(Outcome::Waiting(format!("run {run} is still proving")));
            }
            // T5s: no GATES_DONE and no live lock: stopped, not red (L42); the next launch is a new run id and a new log
            let note = format!("run {run} stopped without GATES_DONE");
            return self.plain(id, f, "T5s", "preflighted", Some(tested), Some(run.clone()), json!({"stopped_run": run}), "recorded", note);
        }
        evidence::record_shadow(&self.state_dir, &run, &text)?;
        if p.mode.as_deref() != Some("full") {
            return self.refuse(id, "floor.full-mode", format!("the run's mode is {:?}, only a full run is a proof", p.mode.unwrap_or_default()));
        }
        let mut reds: Vec<Value> = p.red_now.iter().map(|(s, rc, _)| json!({"step": s, "rc": rc})).collect();
        for s in &p.steps {
            if s.exit_code.is_some_and(|c| c != 0) && !reds.iter().any(|r| r["step"] == json!(s.name)) {
                reds.push(json!({"step": s.name, "rc": s.exit_code}));
            }
        }
        if !reds.is_empty() {
            let note = format!("{} red step(s); findings stored, diagnose is a later slice", reds.len());
            return self.plain(id, f, "T5r", "red", Some(tested), Some(run), json!({"findings": reds}), "recorded", note);
        }
        let logged = p.commit.clone().unwrap_or_default();
        if logged.len() < 7 || !tested.starts_with(&logged) {
            return self.refuse(id, "floor.proven-sha", format!("the log ran commit {logged:?}, the recorded tested sha is {}", short(&tested)));
        }
        // the tree after the chain: only generated files may differ from the settled tip (generated-delta); anything else means the tested
        // tree is not the tip that would be merged
        let dirty = dirty_paths(Path::new(&f.need("workdir")?))?;
        let (generated, stray): (Vec<String>, Vec<String>) = dirty.into_iter().partition(|p| in_dirs(p, &self.cfg.generated_dirs));
        if !stray.is_empty() {
            let shown: Vec<&str> = stray.iter().take(5).map(String::as_str).collect();
            return self.refuse(id, "chain.tree-differs", format!("after the chain the tree holds changes outside the generated directories ({}), so the tested tree is not the settled tip {}", shown.join(", "), short(&tested)));
        }
        let cached: Vec<&str> = p.steps.iter().filter(|s| s.cached).map(|s| s.name.as_str()).collect();
        let mut accepted: Vec<Value> = vec![];
        if !cached.is_empty() {
            match self.verify_cached(id, f, &text, &p, &cached, &tested, &run, &log)? {
                Err(because) => return self.refuse(id, "proof.cached-unverified", because),
                Ok(ok) => accepted = ok,
            }
        }
        let detail = json!({"proof": {"run_ids": [run.clone()], "tested_sha": tested.clone(), "cached_steps": cached, "evidence": accepted, "steps": p.steps.len(), "generated_delta": generated}});
        self.plain(id, f, "T5", "proven", Some(tested), Some(run), detail, "allowed", "GATES_DONE, full mode, zero red, log commit equals the tested sha".into())
    }

    /// T8b: every automatic target reports healthy on the merged sha.
    fn health(&mut self, id: &str, f: &Folded) -> Result<Outcome> {
        let wd = PathBuf::from(f.need("workdir")?);
        let sha = f.need("merge_new")?;
        for n in f.facts.get("targets").and_then(|t| t.as_array()).cloned().unwrap_or_default() {
            let name = n.as_str().unwrap_or_default();
            let Some((health, tmo)) = self.cfg.targets.get(name).map(|t| (t.health.clone(), t.health_timeout_min)) else {
                return self.refuse(id, "deploy.target-unknown", format!("target {name} was deployed by this batch but is not in gatetrain.toml any more; live cannot be confirmed, a person decides"));
            };
            if health.is_empty() {
                return self.refuse(id, "deploy.health-missing", format!("target {name} has no health check; live cannot be confirmed"));
            }
            let env = self.env(id, f, &sha, "", "");
            // a health call that times out is an answer: not healthy. It runs into the same age check as a failing one, so a check
            // that hangs on every call is refused at the health timeout instead of waiting for ever
            let (rc, hung) = match self.plugin(id, "T8b", &format!("health {name}"), " ([timeouts] plugin_min)", &substitute(&health, &sha), &wd, &env, self.plugin_min())? {
                Ok((rc, _)) => (rc, None),
                Err(why) => (-1, Some(why.trim_start_matches(&format!("health {name} ")).replacen("timed out", "health timed out", 1))),
            };
            if rc != 0 {
                // the one health timer: the target's own limit, else `[tick] max_wait_minutes`; it runs from the row that put the batch
                // in `deploying` (a release starts it again), read from the ledger
                let tmo = if tmo > 0.0 { tmo } else { self.cfg.max_wait_minutes };
                let since: Option<String> = self.ledger.conn.query_row("SELECT at FROM batch_events WHERE batch=? AND phase='done' ORDER BY seq DESC LIMIT 1", [id], |r| r.get(0)).ok();
                if let (Some(t0), Some(t1)) = (since.as_deref().and_then(parse_iso), parse_iso(&(self.now)())) {
                    if (t1 - t0) as f64 > tmo * 60.0 {
                        let last = hung.as_deref().map(|h| format!(" (the last call: {h})")).unwrap_or_default();
                        return self.refuse(id, "deploy.health-timeout", format!("target {name} did not report {} healthy within {tmo} min{last}; the deploy is left as it is for a person to look at", short(&sha)));
                    }
                }
                if let Some(h) = hung {
                    return Ok(Outcome::Waiting(format!("target {name}: {h}")));
                }
                return Ok(Outcome::Waiting(format!("target {name} does not report {} healthy yet", short(&sha))));
            }
        }
        self.plain(id, f, "T8b", "live", Some(sha), None, json!({}), "allowed", "every automatic target is healthy".into())
    }

    /// T9: close. Without `[close] tidy` the batch closes as it always did; with it only the batch's own recorded items are tidied
    /// (`tidy/close.rs`) and the batch stays `live` while anything is refused or left over (a decision row each, tried again later).
    fn close(&mut self, id: &str, f: &Folded) -> Result<Outcome> {
        if !self.cfg.close_tidy {
            return self.plain(id, f, "T9", "closed", None, None, json!({}), "recorded", "closed".into());
        }
        let adm = self.adm;
        let Some(env) = adm.close_env() else {
            // no environment will appear by waiting: a person fixes the host's reach or its tidy config
            let why = adm.close_env_error().unwrap_or_else(|| "the seam has none (`gatetrain batch close` with --hosts and --host)".into());
            return self.refuse(id, "close.no-environment", format!("batch {id}: close needs a tidy environment for this host: {why}"));
        };
        let set = self.owned_set(id, f, env)?;
        let rep = close::run_close(&set, env);
        let last = self.close_rows(id, &rep)?;
        if rep.clean() {
            let note = format!("owned set tidied: {} moved, {} already gone", rep.done.len(), rep.gone.len());
            return self.plain(id, f, "T9", "closed", None, None, json!({"tidied": rep.done, "gone": rep.gone}), "recorded", note);
        }
        let (code, why) = rep.refusals.first().map(|r| (r.code.clone(), r.why.clone())).unwrap_or_else(|| ("tidy.check-not-clean".into(), rep.residue.first().cloned().unwrap_or_default()));
        Ok(Outcome::Refused { clears_by_itself: false, rule: code, because: format!("{why} ({} refusal(s), {} left over; the batch stays live and is retried)", rep.refusals.len(), rep.residue.len()), decision: last.unwrap_or(0) })
    }

    /// The rows of one close run: a refusal row per refusal and per left-over item, a `tidy.moved` row per item that went. The last
    /// refusal row's seq, if any.
    fn close_rows(&mut self, id: &str, rep: &close::CloseReport) -> Result<Option<i64>> {
        let mut last = None;
        for r in &rep.refusals {
            last = Some(self.close_row("refusal", id, &r.code, &r.key, &r.why, "refused")?);
        }
        for r in &rep.residue {
            last = Some(self.close_row("refusal", id, "tidy.check-not-clean", r, &format!("`check` still shows {r}"), "refused")?);
        }
        for k in &rep.done {
            self.close_row("tidy", id, "tidy.moved", k, &format!("{k} moved to the recycle bin by exact name"), "allowed")?;
        }
        Ok(last)
    }

    /// A close row, written once: an identical (batch, rule, subject, reason) row already in the log is not repeated on a retry.
    fn close_row(&mut self, kind: &str, id: &str, rule: &str, key: &str, why: &str, outcome: &str) -> Result<i64> {
        let have: Option<i64> = self.ledger.conn.query_row("SELECT seq FROM decisions WHERE batch=? AND rule=? AND subject=? AND reason=? ORDER BY seq DESC LIMIT 1", params![id, rule, key, why], |r| r.get(0)).ok();
        match have {
            Some(seq) if kind == "refusal" => Ok(seq),
            _ => self.decide(kind, id, Some(key.into()), rule, outcome, why.into(), None),
        }
    }

    /// The batch's own recorded items, and only those.
    fn owned_set(&self, id: &str, f: &Folded, env: &dyn CloseEnv) -> Result<OwnedSet> {
        let ev = self.events(id)?;
        let mut set = OwnedSet { roots: self.cfg.close_roots.clone(), shared_targets: self.cfg.chain_target_dirs.clone(), ..Default::default() };
        let mut runs: Vec<String> = vec![];
        for e in &ev {
            if let Some(r) = &e.run_id {
                if !runs.contains(r) {
                    runs.push(r.clone());
                }
            }
            if e.phase == "done" {
                if let Some(b) = e.detail["branch"].as_str() {
                    set.branches.push(b.to_string());
                }
            }
        }
        let mut lanes: Vec<String> = vec![];
        for l in f.facts.get("lanes").and_then(|l| l.as_array()).cloned().unwrap_or_default() {
            if let Some(r) = l["ref"].as_str() {
                lanes.push(r.to_string());
            }
        }
        set.branches.extend(lanes.iter().cloned());
        let wd = f.s("workdir").unwrap_or_default();
        if self.cfg.close_own_workdir && !wd.is_empty() {
            set.chain_checkout = Some(wd.clone());
            set.items.push(OwnedItem::new("worktree", &wd));
        }
        for w in env.worktrees_on(&set.branches)? {
            // the batch's own checkout is its owned set only on request: the lane worktrees are looked up by branch, and that lookup finds it
            let is_wd = !wd.is_empty() && crate::tidy::ops::norm(&w) == crate::tidy::ops::norm(&wd);
            if (is_wd && !self.cfg.close_own_workdir) || set.items.iter().any(|i| i.key == w) {
                continue;
            }
            set.items.push(OwnedItem::new("worktree", &w));
        }
        for r in &runs {
            // recorded by the run, one per line; the shared `[chain] target_dirs` are never the batch's to move
            for (sub, kind) in [("targets", "target"), ("containers", "container"), ("tmpdirs", "tmp")] {
                let file = self.state_dir.join(sub).join(format!("{r}.txt"));
                for l in std::fs::read_to_string(&file).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()) {
                    set.items.push(OwnedItem::new(kind, l));
                }
            }
        }
        // the branches the batch is made of: its lane refs under line/ and the stack branches the engine itself recorded; none is derived by name
        let mut branches: Vec<String> = lanes.iter().filter(|l| l.starts_with("line/")).cloned().collect();
        branches.extend(set.branches.iter().filter(|b| b.starts_with(&format!("batch/{id}/"))).cloned());
        branches.dedup();
        for b in branches {
            if !set.items.iter().any(|i| i.kind == "branch" && i.key == b) {
                set.items.push(OwnedItem::new("branch", &b));
            }
        }
        Ok(set)
    }

    /// A person closes a live batch with whatever is left (`batch close --with-residue`): the owned set is tidied as far as the refusals
    /// allow, every refusal is already a row, and the override row names what stays behind.
    pub fn close_with_residue(&mut self, id: &str, reason: &str) -> Result<Outcome> {
        let _git = self.git_scope(id);
        let f = self.fold(id)?;
        if f.state != "live" {
            return Ok(Outcome::Idle(format!("batch {id} is {}, not live", f.state)));
        }
        if reason.trim().is_empty() {
            return Err(Error::Batch("closing with residue needs a reason".into()));
        }
        let mut left = vec![];
        let adm = self.adm;
        // `[close] tidy = false` means close never tidies, by hand or not; with it on, the run writes the same rows as `close`
        if let (true, Some(env)) = (self.cfg.close_tidy, adm.close_env()) {
            let set = self.owned_set(id, &f, env)?;
            let rep = close::run_close(&set, env);
            self.close_rows(id, &rep)?;
            left = rep.refusals.iter().map(|r| format!("{} ({})", r.key, r.code)).chain(rep.residue.iter().cloned()).collect();
        }
        self.decide("override", id, None, "close.with-residue", "recorded", reason.trim().into(), Some(json!({"residue": left})))?;
        self.plain(id, &f, "T9", "closed", None, None, json!({"residue": left}), "recorded", format!("closed with residue: {}", reason.trim()))
    }

    /// `batch close`: take the T9 step now (a live batch only).
    pub fn close_batch(&mut self, id: &str) -> Result<Outcome> {
        let _git = self.git_scope(id);
        let f = self.fold(id)?;
        if f.state != "live" {
            return Ok(Outcome::Idle(format!("batch {id} is {}, not live", f.state)));
        }
        self.close(id, &f)
    }
}
