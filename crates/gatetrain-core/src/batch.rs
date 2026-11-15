//! The batch state machine (design 3). State is derived by folding the append-only `batch_events`; there is no status
//! column. Every transition that has an effect writes `intent` before and `done` after; a crash between the two is
//! resolved on the next `advance` by a read-only probe (design 3.4). Guards come from the floor rules (design 6.2).
//!
//! The holder, the host lock and admission are not here: they are the traits [`HostLock`] and [`Admission`], with
//! fakes for tests. The holder lane (rs2) implements them against the real lock file and `hosts.toml`.
use crate::batchcfg::BatchConfig;
use crate::declog::{self, NewEntry};
use crate::ledger::Ledger;
use crate::logfmt::parse_log;
use crate::{Error, Result};
use regex::Regex;
use rusqlite::params;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

/// The host lock and holder as the state machine sees them. rs2 implements this over the real lock file.
pub trait HostLock {
    /// Does a live lock name this run id? (the probe of T4 and the "still running" test of T5)
    fn names_run(&self, run_id: &str) -> Result<bool>;
    /// Start the chain under the holder. Must not block until the chain ends.
    fn launch(&self, req: &LaunchRequest) -> Result<()>;
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

/// A fake lock for tests: records launches; `named` is the set of run ids a live lock names.
#[cfg(any(test, feature = "test-fakes"))]
#[derive(Default)]
pub struct FakeLock {
    pub named: Mutex<HashSet<String>>,
    pub launches: Mutex<Vec<LaunchRequest>>,
}
#[cfg(any(test, feature = "test-fakes"))]
impl HostLock for FakeLock {
    fn names_run(&self, run_id: &str) -> Result<bool> {
        Ok(self.named.lock().unwrap().contains(run_id))
    }
    fn launch(&self, req: &LaunchRequest) -> Result<()> {
        self.launches.lock().unwrap().push(req.clone());
        self.named.lock().unwrap().insert(req.run_id.clone());
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
                    if !matches!(e.transition.as_str(), "hold" | "release" | "abandon") {
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
    Refused { rule: String, because: String, decision: i64 },
    Failed(String),
}

impl Outcome {
    /// 0 advanced or nothing to do, 1 failed, 3 refused by a rule (design 6.7); a refusal by the host seams uses the exit codes of
    /// `gatetrain hold`: 75 lock busy, 76 memory, 77 disk, 78 working directory, 79 doctor.
    pub fn exit_code(&self) -> u8 {
        match self {
            Outcome::Refused { rule, .. } => match rule.as_str() {
                "host.refuse" => 75,
                "host.refuse-memory" => 76,
                "host.refuse-disk" => 77,
                "host.refuse-cwd" => 78,
                "host.refuse-doctor" => 79,
                _ => 3,
            },
            Outcome::Failed(_) => 1,
            _ => 0,
        }
    }
}

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
    /// Test hook: abort with an error right after the effect of this transition, before `done` is written.
    pub crash_after_effect: Option<String>,
}

// ---------------------------------------------------------------- helpers

pub fn system_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
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

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, err) = git_raw(dir, args)?;
    if ok {
        Ok(out)
    } else {
        Err(Error::Batch(format!("git {}: {}", args.join(" "), err.trim())))
    }
}

fn git_raw(dir: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    Ok((o.status.success(), String::from_utf8_lossy(&o.stdout).trim().to_string(), String::from_utf8_lossy(&o.stderr).into_owned()))
}

/// Paths git reports as changed or untracked (new files listed individually).
fn dirty_paths(dir: &Path) -> Result<Vec<String>> {
    let o = Command::new("git").arg("-C").arg(dir).args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]).output()?;
    if !o.status.success() {
        return Err(Error::Batch(format!("git status: {}", String::from_utf8_lossy(&o.stderr).trim())));
    }
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
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
    let o = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    Ok((o.status.success(), o.stdout))
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
    let diff = git(dir, &["diff", "-U0", "--no-color", base, tip])?;
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
fn parse_iso(s: &str) -> Option<i64> {
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

fn run_plugin(cmd: &[String], dir: &Path, env: &[(String, String)]) -> Result<(i32, String)> {
    let (prog, args) = cmd.split_first().ok_or_else(|| Error::Batch("empty plugin command".into()))?;
    let o = Command::new(prog).args(args).current_dir(dir).envs(env.iter().map(|(k, v)| (k, v))).output()?;
    Ok((o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stderr).trim().to_string()))
}

fn substitute(cmd: &[String], sha: &str) -> Vec<String> {
    cmd.iter().map(|a| a.replace("{sha}", sha)).collect()
}

fn to_state(tr: &str) -> &'static str {
    match tr {
        "T1" => "stacked",
        "T2" => "settled",
        "T3" => "preflighted",
        "T4" => "proving",
        "T7" => "merged",
        "T8a" => "deploying",
        _ => "open",
    }
}

fn from_state(tr: &str) -> &'static str {
    match tr {
        "T1" => "open",
        "T2" => "stacked",
        "T3" => "settled",
        "T4" => "preflighted",
        "T7" => "proven",
        "T8a" => "merged",
        _ => "",
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

impl<'a> Engine<'a> {
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
                let (rc, err) = run_plugin(&cmd, wd, &[])?;
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
        Ok(Outcome::Refused { rule: rule.into(), because, decision })
    }

    // ------------------------------------------------------------ commands: new, status, hold, release, abandon

    pub fn new_batch(&mut self, id: &str, base: &str, workdir: &Path, lanes: &[String]) -> Result<()> {
        self.ledger.require_current()?;
        if !self.events(id)?.is_empty() {
            return Err(Error::Batch(format!("batch {id} already exists")));
        }
        let wd = std::path::absolute(workdir)?;
        let base_sha = git(&wd, &["rev-parse", &format!("{base}^{{commit}}")])?;
        let mut ls = vec![];
        for l in lanes {
            let tip = git(&wd, &["rev-parse", &format!("{l}^{{commit}}")])?;
            ls.push(json!({"ref": l, "tip": tip}));
        }
        let detail = json!({"base_ref": base, "base_sha": base_sha, "workdir": wd.to_string_lossy(), "lanes": ls});
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
        self.event(id, tr, "done", n, Some(to), None, None, None, Some(d), &json!({"from": f.state, "rule": rule, "reason": reason}))?;
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

    // ------------------------------------------------------------ advance

    /// One step: resume a pending intent, else take the transition the state allows, or refuse naming the rule.
    pub fn advance(&mut self, id: &str) -> Result<Outcome> {
        let f = self.fold(id)?;
        if let Some(p) = f.pending.clone() {
            return self.resume(id, &f, p);
        }
        match f.state.as_str() {
            "open" => self.go(id, &f, "T1"),
            "stacked" => self.go(id, &f, "T2"),
            "settled" => self.go(id, &f, "T3"),
            "preflighted" => self.go(id, &f, "T4"),
            "proving" => self.observe(id, &f),
            "red" => self.refuse(id, "floor.zero-red", "the proof has a red step; a red batch is not merged (restack after a patch)".into()),
            "proven" => self.go(id, &f, "T7"),
            "merged" => self.go(id, &f, "T8a"),
            "deploying" => self.health(id, &f),
            "live" => self.close(id, &f),
            "held" => self.refuse(id, "batch.held", "the batch is held; `batch release` returns it to its state".into()),
            s => Ok(Outcome::Idle(format!("batch {id} is {s}"))),
        }
    }

    fn go(&mut self, id: &str, f: &Folded, tr: &str) -> Result<Outcome> {
        let attempt = f.max_attempt.get(tr).copied().unwrap_or(0) + 1;
        match self.plan(tr, id, f, attempt)? {
            Planned::Refuse(rule, because) => self.refuse(id, &rule, because),
            Planned::Go(plan) => {
                self.event(id, tr, "intent", attempt, None, plan.input_sha.as_deref(), None, plan.run_id.as_deref(), None, &plan.detail)?;
                self.execute(id, f, plan)
            }
        }
    }

    /// Effect, then `done` (or `failed`).
    fn execute(&mut self, id: &str, f: &Folded, plan: Plan) -> Result<Outcome> {
        let r = self.effect(id, f, &plan);
        let done = match r {
            Ok(Ok(d)) => d,
            Ok(Err(msg)) | Err(Error::Batch(msg)) => {
                self.event(id, &plan.tr, "failed", plan.attempt, None, plan.input_sha.as_deref(), None, plan.run_id.as_deref(), None, &json!({"error": msg}))?;
                return Ok(Outcome::Failed(format!("{}: {msg}", plan.tr)));
            }
            Err(e) => return Err(e),
        };
        if self.crash_after_effect.as_deref() == Some(plan.tr.as_str()) {
            return Err(Error::Batch("simulated crash after the effect".into()));
        }
        self.finish(id, &plan, done.output, done.extra, "")
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

    fn resume(&mut self, id: &str, f: &Folded, p: Ev) -> Result<Outcome> {
        let plan = Plan { tr: p.transition.clone(), to: to_state(&p.transition).into(), input_sha: p.input_sha.clone(), run_id: p.run_id.clone(), attempt: p.attempt, detail: p.detail.clone() };
        match self.probe(f, &plan)? {
            Probe::Happened(out) => {
                let extra = if plan.tr == "T7" { json!({"resumed": "happened", "merge_new": out}) } else { json!({"resumed": "happened"}) };
                self.finish(id, &plan, out, extra, " (resumed: the probe found the effect done)")
            }
            Probe::NotHappened => self.execute(id, f, plan),
            Probe::Partly => {
                // Finish forward under a new attempt; the partial result is kept, never deleted.
                self.event(id, &plan.tr, "failed", plan.attempt, None, plan.input_sha.as_deref(), None, plan.run_id.as_deref(), None, &json!({"error": "partly done; finishing forward under a new attempt"}))?;
                let attempt = p.attempt + 1;
                let f2 = self.fold(id)?;
                match self.plan(&plan.tr, id, &f2, attempt)? {
                    Planned::Refuse(rule, because) => self.refuse(id, &rule, because),
                    Planned::Go(np) => {
                        self.event(id, &np.tr, "intent", attempt, None, np.input_sha.as_deref(), None, np.run_id.as_deref(), None, &np.detail)?;
                        self.execute(id, &f2, np)
                    }
                }
            }
        }
    }

    /// Read-only: git refs and commits, files on disk, the lock.
    fn probe(&self, f: &Folded, plan: &Plan) -> Result<Probe> {
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
            "T3" => {
                let sha = plan.input_sha.clone().unwrap_or_default();
                if self.stamp_path(&sha).exists() {
                    Probe::Happened(Some(sha))
                } else {
                    Probe::NotHappened
                }
            }
            "T4" => {
                if self.lock.names_run(plan.run_id.as_deref().unwrap_or_default())? {
                    Probe::Happened(None)
                } else {
                    Probe::NotHappened
                }
            }
            "T7" => {
                let new = plan.detail["merge_new"].as_str().unwrap_or_default();
                let cur = git(&wd, &["rev-parse", &format!("refs/heads/{}", self.cfg.protected)])?;
                if cur == new {
                    Probe::Happened(Some(new.into()))
                } else if git_raw(&wd, &["merge-base", "--is-ancestor", new, &cur])?.0 && !git(&wd, &["diff", "--name-only", new, &cur])?.lines().any(|p| !in_dirs(p, &self.cfg.generated_dirs)) {
                    Probe::Happened(Some(cur)) // the merge also carried the commit of the files the chain produced
                } else {
                    Probe::NotHappened
                }
            }
            _ => Probe::NotHappened, // T8a: the deploy command must be idempotent (design 7.6)
        })
    }

    // ------------------------------------------------------------ plans (the guards)

    fn stamp_path(&self, sha: &str) -> PathBuf {
        self.state_dir.join("stamps").join(format!("{sha}.stamp"))
    }

    fn tip(&self, f: &Folded) -> Result<String> {
        let wd = PathBuf::from(f.need("workdir")?);
        git(&wd, &["rev-parse", &format!("refs/heads/{}", f.need("branch")?)])
    }

    fn plan(&mut self, tr: &str, id: &str, f: &Folded, attempt: i64) -> Result<Planned> {
        let wd = PathBuf::from(f.need("workdir")?);
        let mk = |input: Option<String>, run: Option<String>, detail: Value| Planned::Go(Plan { tr: tr.into(), to: to_state(tr).into(), input_sha: input, run_id: run, attempt, detail });
        match tr {
            "T1" => {
                let base = f.need("base_sha")?;
                for l in f.facts.get("lanes").and_then(|l| l.as_array()).cloned().unwrap_or_default() {
                    let tip = l["tip"].as_str().unwrap_or_default();
                    let (anc, _, _) = git_raw(&wd, &["merge-base", "--is-ancestor", &base, tip])?;
                    if !anc {
                        return Ok(Planned::Refuse("stack.base-ancestor".into(), format!("the base {} is not an ancestor of lane {}", short(&base), l["ref"].as_str().unwrap_or("?"))));
                    }
                }
                Ok(mk(Some(base), None, json!({"branch": format!("batch/{id}/s{attempt}"), "stack_attempt": attempt})))
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
            "T3" => {
                if self.cfg.preflight_command.is_empty() {
                    return Ok(Planned::Refuse("config.preflight-command".into(), "gatetrain.toml has no [preflight] command".into()));
                }
                if let Some((rule, why)) = self.adm.admit_rule("light", id)? {
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
                if !self.stamp_path(&tip).exists() {
                    return Ok(Planned::Refuse("preflight.stamp-for-sha".into(), format!("no green preflight stamp for {}", short(&tip))));
                }
                let dirty = dirty_paths(&wd)?;
                if !dirty.is_empty() {
                    let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
                    return Ok(Planned::Refuse("floor.clean-tree".into(), format!("the checkout has uncommitted paths ({}); the chain would test files that are not in the sha that is merged", shown.join(", "))));
                }
                if self.cfg.chain_command.is_empty() {
                    return Ok(Planned::Refuse("config.chain-command".into(), "gatetrain.toml has no [chain] command".into()));
                }
                if let Some((rule, why)) = self.adm.admit_rule("proof", id)? {
                    return Ok(Planned::Refuse(rule, why));
                }
                let run = format!("{id}-p{attempt}");
                let log = self.state_dir.join("logs").join(format!("{run}.log"));
                Ok(mk(Some(tip.clone()), Some(run.clone()), json!({"run_id": run, "log": log.to_string_lossy(), "tested_sha": tip})))
            }
            "T7" => {
                let tip = self.tip(f)?;
                let tested = f.need("tested_sha")?;
                if tested != tip {
                    return Ok(Planned::Refuse("merge.tested-equals-merged".into(), format!("tested {}, tip {} (mode sha); restack and prove again", short(&tested), short(&tip))));
                }
                let pref = format!("refs/heads/{}", self.cfg.protected);
                let old = git(&wd, &["rev-parse", &pref])?;
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
                Ok(mk(Some(tip.clone()), None, json!({"merge_old": old, "merge_new": tip, "protected": self.cfg.protected, "mode": "sha", "checkout": checkout})))
            }
            "T8a" => {
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
                Ok(mk(Some(f.s("output_sha").map_or_else(|| f.need("merge_new"), Ok)?), None, json!({"targets": auto, "skipped_production": prod})))
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

    fn effect(&mut self, id: &str, f: &Folded, plan: &Plan) -> Result<std::result::Result<Done, String>> {
        let wd = PathBuf::from(f.need("workdir")?);
        match plan.tr.as_str() {
            "T1" => {
                let base = f.need("base_sha")?;
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
                    let (rc, err) = run_plugin(&self.cfg.stack_command, &wd, &env)?;
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
                    let (rc, err) = run_plugin(&self.cfg.regen_command, &wd, &env)?;
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
                    git(&wd, &["commit", "-q", "-m", &format!("chore(batch): regenerate generated files for {id} (pass {passes})")])?;
                    committed.extend(dirty);
                }
                let head = git(&wd, &["rev-parse", "HEAD"])?;
                Ok(Ok(Done { output: Some(head.clone()), extra: json!({"passes": passes, "paths": committed, "settled_tip": head}) }))
            }
            "T3" => {
                let sha = plan.input_sha.clone().unwrap_or_default();
                let env = self.env(id, f, &sha, "", "");
                let (rc, err) = run_plugin(&self.cfg.preflight_command, &wd, &env)?;
                if rc != 0 {
                    return Ok(Err(format!("preflight exited {rc}: {err}")));
                }
                let p = self.stamp_path(&sha);
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, format!("{sha}\n"))?;
                Ok(Ok(Done { output: Some(sha.clone()), extra: json!({"stamp": p.to_string_lossy()}) }))
            }
            "T4" => {
                let run = plan.run_id.clone().unwrap_or_default();
                let log = PathBuf::from(plan.detail["log"].as_str().unwrap_or_default());
                std::fs::create_dir_all(log.parent().unwrap())?;
                let sha = plan.input_sha.clone().unwrap_or_default();
                let req = LaunchRequest {
                    run_id: run.clone(),
                    batch: id.into(),
                    sha: sha.clone(),
                    base: f.s("base_sha").unwrap_or_default(),
                    command: self.cfg.chain_command.clone(),
                    log: log.clone(),
                    workdir: wd,
                    env: self.env(id, f, &sha, &run, &log.to_string_lossy()),
                };
                self.lock.launch(&req)?;
                Ok(Ok(Done { output: None, extra: json!({}) }))
            }
            "T7" => {
                let tested = plan.detail["merge_new"].as_str().unwrap_or_default().to_string();
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
                    git(&wd, &["commit", "-q", "-m", &format!("chore(batch): commit the generated files the chain produced for {id} (generated-delta)")])?;
                    if !self.cfg.regen_command.is_empty() {
                        let env = self.env(id, f, &tested, "", "");
                        let (rc, err) = run_plugin(&self.cfg.regen_command, &wd, &env)?;
                        if rc != 0 {
                            return Ok(Err(format!("regen command exited {rc} while re-settling the chain's generated files: {err}")));
                        }
                        let again = dirty_paths(&wd)?;
                        if !again.is_empty() {
                            return Ok(Err(format!("the generated files are not a fixpoint after the chain (the generators changed: {})", again.join(", "))));
                        }
                    }
                }
                let head = git(&wd, &["rev-parse", "HEAD"])?;
                let mut final_new = tested.clone();
                if head != tested {
                    let names = git(&wd, &["diff", "--name-only", &tested, &head])?;
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
                let sha = plan.input_sha.clone().unwrap_or_default();
                if !self.cfg.postmerge_command.is_empty() {
                    // the target checkout needs its dependencies and packages before anything is deployed from it
                    let dir = self.cfg.postmerge_dir.as_deref().map(PathBuf::from).unwrap_or_else(|| wd.clone());
                    let env = self.env(id, f, &sha, "", "");
                    let (rc, err) = run_plugin(&substitute(&self.cfg.postmerge_command, &sha), &dir, &env)?;
                    if rc != 0 {
                        return Ok(Err(format!("post-merge command exited {rc}: {err}")));
                    }
                }
                for n in plan.detail["targets"].as_array().cloned().unwrap_or_default() {
                    let name = n.as_str().unwrap_or_default();
                    let t = &self.cfg.targets[name];
                    let env = self.env(id, f, &sha, "", "");
                    let (rc, err) = run_plugin(&substitute(&t.command, &sha), &wd, &env)?;
                    if rc != 0 {
                        return Ok(Err(format!("deploy {name} exited {rc}: {err}")));
                    }
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
        if !cached.is_empty() {
            // a cached step counts only when its entry is verified for this sha on this host (another host's cache is not this host's)
            if self.cfg.cache_verify_command.is_empty() {
                return self.refuse(id, "proof.cached-unverified", format!("step(s) {} came from a step cache and no [chain] cache_verify_command proves the entries for {}", cached.join(", "), short(&tested)));
            }
            let wd = PathBuf::from(f.need("workdir")?);
            for s in &cached {
                let mut env = self.env(id, f, &tested, &run, &log.to_string_lossy());
                env.push(("GATETRAIN_STEP".into(), s.to_string()));
                let (rc, _) = run_plugin(&substitute(&self.cfg.cache_verify_command, &tested), &wd, &env)?;
                if rc != 0 {
                    return self.refuse(id, "proof.cached-unverified", format!("cached step {s} is not verified for {} (the verify command exited {rc})", short(&tested)));
                }
            }
        }
        let detail = json!({"proof": {"run_ids": [run.clone()], "tested_sha": tested.clone(), "cached_steps": cached, "steps": p.steps.len(), "generated_delta": generated}});
        self.plain(id, f, "T5", "proven", Some(tested), Some(run), detail, "allowed", "GATES_DONE, full mode, zero red, log commit equals the tested sha".into())
    }

    /// T8b: every automatic target reports healthy on the merged sha.
    fn health(&mut self, id: &str, f: &Folded) -> Result<Outcome> {
        let wd = PathBuf::from(f.need("workdir")?);
        let sha = f.need("merge_new")?;
        for n in f.facts.get("targets").and_then(|t| t.as_array()).cloned().unwrap_or_default() {
            let name = n.as_str().unwrap_or_default();
            let (health, tmo) = {
                let t = &self.cfg.targets[name];
                (t.health.clone(), t.health_timeout_min)
            };
            if health.is_empty() {
                return self.refuse(id, "deploy.health-missing", format!("target {name} has no health check; live cannot be confirmed"));
            }
            let env = self.env(id, f, &sha, "", "");
            let (rc, _) = run_plugin(&substitute(&health, &sha), &wd, &env)?;
            if rc != 0 {
                if tmo > 0.0 {
                    let since: Option<String> = self.ledger.conn.query_row("SELECT at FROM batch_events WHERE batch=? AND transition='T8a' AND phase='done' ORDER BY seq DESC LIMIT 1", [id], |r| r.get(0)).ok();
                    if let (Some(t0), Some(t1)) = (since.as_deref().and_then(parse_iso), parse_iso(&(self.now)())) {
                        if (t1 - t0) as f64 > tmo * 60.0 {
                            return self.refuse(id, "deploy.health-timeout", format!("target {name} did not report {} healthy within {tmo} min; the deploy is left as it is for a person to look at", short(&sha)));
                        }
                    }
                }
                return Ok(Outcome::Waiting(format!("target {name} does not report {} healthy yet", short(&sha))));
            }
        }
        self.plain(id, f, "T8b", "live", Some(sha), None, json!({}), "allowed", "every automatic target is healthy".into())
    }

    /// T9: closed. Tidy and archive tags are a later slice.
    fn close(&mut self, id: &str, f: &Folded) -> Result<Outcome> {
        self.plain(id, f, "T9", "closed", None, None, json!({}), "recorded", "closed".into())
    }
}
