//! Multi-host chains (docs/multi-host.md): ONE `gatetrain chain` run whose steps are spread over several hosts.
//!
//! The coordinator ([`run`]) places the steps of each tier on the accepted hosts (longest first, honouring slots and tags), steals work
//! for an idle host, and takes a step's result only from a host that reported the coordinator's exact commit and tool fingerprint. A
//! step lost by a host (a drop, a timeout, an answer that cannot be read) is never green: it runs again on another qualified host, or
//! it is red. A step that went red on a host is red. The transport is a trait ([`Transport`]); the real one (ssh and the `agent`
//! subcommand) lives in the CLI crate, the tests use a fake.
use crate::chain::{clean_host, git_out, host_name, nshards, round1, summary_in_file_order, tier_name, HostEvent, HostRec, Outcome, Plan, RunRec, ShardRec, StepRec, Tier};
use crate::logfmt::Ts;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::Instant;

/// The agent protocol version; a host that speaks another is refused.
pub const PROTOCOL: u32 = 1;

/// rc of a step every qualified host lost (never green).
pub const RC_LOST: i64 = 125;
/// rc of a step no accepted host qualifies for.
pub const RC_NO_HOST: i64 = 126;

/// `<arch>-<os>` of this binary: `x86_64-linux`, `aarch64-macos`, `x86_64-windows`.
pub fn platform_tag() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// True when a tag has the form of a platform tag (`<arch>-<os>` with a known os).
pub fn is_platform_tag(t: &str) -> bool {
    ["-linux", "-windows", "-macos", "-freebsd"].iter().any(|s| t.ends_with(s) && t.len() > s.len())
}

/// What a host is: its tool versions and the gatetrain that runs the steps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Fingerprint {
    /// sha-256 over the version and every (command, output) pair
    pub hash: String,
    pub version: String,
    /// (command, trimmed stdout), in the configured order
    pub items: Vec<(String, String)>,
}

impl Fingerprint {
    pub fn of(version: &str, items: Vec<(String, String)>) -> Fingerprint {
        let canon = serde_json::to_string(&serde_json::json!({"v": PROTOCOL, "version": version, "items": items})).unwrap_or_default();
        let hash = Sha256::digest(canon.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        Fingerprint { hash, version: version.to_string(), items }
    }

    /// The fingerprint over only these commands, in this order (a command this one did not run is left out).
    pub fn subset(&self, probes: &[String]) -> Fingerprint {
        let items = probes.iter().filter_map(|p| self.items.iter().find(|(c, _)| c == p).cloned()).collect();
        Fingerprint::of(&self.version, items)
    }

    /// The output of one command, if it ran.
    pub fn get(&self, probe: &str) -> Option<&str> {
        self.items.iter().find(|(c, _)| c == probe).map(|(_, o)| o.as_str())
    }

    /// Why `other` is not this fingerprint (the first difference), None when equal. Names the command, never a value beyond its output.
    pub fn differs(&self, other: &Fingerprint) -> Option<String> {
        if self.hash == other.hash {
            return None;
        }
        if self.version != other.version {
            return Some(format!("gatetrain {} against the coordinator's {}", other.version, self.version));
        }
        for (i, (cmd, out)) in self.items.iter().enumerate() {
            match other.items.get(i) {
                Some((c2, o2)) if c2 == cmd && o2 == out => {}
                Some((c2, o2)) if c2 == cmd => return Some(format!("`{cmd}` printed {:?} against the coordinator's {:?}", short(o2), short(out))),
                _ => return Some(format!("the fingerprint commands differ at `{cmd}`")),
            }
        }
        Some("the fingerprint differs".into())
    }
}

fn short(s: &str) -> String {
    let one = s.lines().next().unwrap_or("");
    one.chars().take(80).collect()
}

/// Run every probe (a shell line) in `cwd`; its trimmed stdout is the item. A probe that fails or times out is an error.
pub fn fingerprint(cwd: &Path, probes: &[String]) -> Result<Fingerprint, String> {
    let mut items = Vec::new();
    for p in probes {
        let mut c = crate::runner::shell_command(p);
        c.current_dir(cwd);
        match crate::runner::run_cmd(c, Some(60.0))? {
            crate::runner::RunOut::Done { rc: 0, out } => items.push((p.clone(), out.trim().replace("\r\n", "\n"))),
            crate::runner::RunOut::Done { rc, .. } => return Err(format!("fingerprint command `{p}` exited {rc}")),
            crate::runner::RunOut::TimedOut => return Err(format!("fingerprint command `{p}` timed out")),
        }
    }
    Ok(Fingerprint::of(env!("CARGO_PKG_VERSION"), items))
}

/// What `agent hello` reports (one `AGENT_HELLO {json}` line).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Hello {
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub sha: String,
    pub fingerprint: Option<Fingerprint>,
    #[serde(default)]
    pub platform: String,
    /// set when the agent refused (a dirty work dir, a commit it cannot get, a failing probe)
    pub error: Option<String>,
}

/// One shard of a remote step, as the agent reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardOut {
    pub shard: i64,
    pub of: i64,
    pub started_at: String,
    pub secs: f64,
    pub rc: i64,
    pub log_ref: String,
}

/// What `agent run` reports (one `AGENT_RESULT {json}` line).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentResult {
    #[serde(default)]
    pub protocol: u32,
    #[serde(default)]
    pub sha: String,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub step: String,
    pub rc: Option<i64>,
    #[serde(default)]
    pub secs: f64,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    #[serde(default)]
    pub shards: Vec<ShardOut>,
    /// the step's output files, relative to `out`
    #[serde(default)]
    pub files: Vec<String>,
    /// the agent's output folder
    #[serde(default)]
    pub out: String,
    /// set when the agent refused to run (the host is no longer equivalent)
    pub error: Option<String>,
}

/// A host as the scheduler sees it.
#[derive(Debug, Clone)]
pub struct HostSpec {
    pub name: String,
    /// effective tags (declared, plus the reported platform tag)
    pub tags: Vec<String>,
    pub slots: usize,
    pub cores: Option<usize>,
    /// the coordinator itself
    pub local: bool,
}

/// One step handed to a host.
#[derive(Debug, Clone)]
pub struct StepReq {
    pub step: String,
    pub sha: String,
    pub run_id: String,
    /// `--par` of the agent's run: the step's shard count
    pub par: usize,
    pub timeout_s: Option<f64>,
    /// the fingerprint commands this host was accepted with (the agent answers them again before it runs)
    pub probes: Vec<String>,
}

/// The door to the hosts. `Err` from [`Transport::run_step`] means the step was LOST (no valid answer); a red step is `Ok`.
pub trait Transport: Send + Sync {
    /// Check a host: get the commit there (push or fetch, then a checkout), run its `prepare`, answer these fingerprint commands.
    fn hello(&self, host: &HostSpec, sha: &str, probes: &[String]) -> Result<Hello, String>;
    /// Run one step; `line` gets every line the host streams (progress only, never a verdict).
    fn run_step(&self, host: &HostSpec, req: &StepReq, line: &mut dyn FnMut(&str)) -> Result<AgentResult, String>;
    /// Copy a finished step's output files into `out` (the coordinator's `--out`).
    fn fetch(&self, host: &HostSpec, res: &AgentResult, out: &Path) -> Result<(), String>;
    /// The run is over (best effort clean-up on the host).
    fn finish(&self, _host: &HostSpec, _run_id: &str) {}
}

/// `[multihost]` of hosts.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct MhConfig {
    /// commands every host must answer exactly as the coordinator does
    pub fingerprint: Vec<String>,
    /// commands a host must also answer exactly for one of its tags to count (`[multihost.tag_fingerprint]`); a host whose answers
    /// differ keeps its other tags and loses this one
    pub tag_fingerprint: Vec<(String, Vec<String>)>,
    pub local_tags: Vec<String>,
    pub max_host_failures: usize,
    pub max_step_attempts: usize,
}

impl Default for MhConfig {
    fn default() -> Self {
        MhConfig { fingerprint: Vec::new(), tag_fingerprint: Vec::new(), local_tags: Vec::new(), max_host_failures: 2, max_step_attempts: 3 }
    }
}

/// Read `[multihost]` from the hosts.toml text (strict: an unknown key is an error).
pub fn mh_config(text: &str) -> Result<MhConfig, String> {
    let t: toml::Table = text.parse().map_err(|e: toml::de::Error| format!("hosts.toml: {e}"))?;
    let mut c = MhConfig::default();
    let Some(v) = t.get("multihost") else { return Ok(c) };
    let m = v.as_table().ok_or("multihost: must be a table")?;
    for (k, v) in m {
        let list = |v: &toml::Value| -> Result<Vec<String>, String> {
            v.as_array().filter(|a| a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty()))).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).ok_or_else(|| format!("multihost.{k}: must be a list of non-empty strings"))
        };
        let pos = |v: &toml::Value| -> Result<usize, String> { v.as_integer().filter(|n| *n >= 1).map(|n| n as usize).ok_or_else(|| format!("multihost.{k}: must be a whole number of at least 1")) };
        match k.as_str() {
            "fingerprint" => c.fingerprint = list(v)?,
            "tag_fingerprint" => {
                let t = v.as_table().ok_or("multihost.tag_fingerprint: must be a table of tag = [commands]")?;
                for (tag, cmds) in t {
                    let l = cmds.as_array().filter(|a| a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty()))).ok_or_else(|| format!("multihost.tag_fingerprint.{tag}: must be a list of non-empty strings"))?;
                    c.tag_fingerprint.push((tag.clone(), l.iter().filter_map(|x| x.as_str().map(String::from)).collect()));
                }
            }
            "local_tags" => c.local_tags = list(v)?,
            "max_host_failures" => c.max_host_failures = pos(v)?,
            "max_step_attempts" => c.max_step_attempts = pos(v)?,
            _ => return Err(format!("multihost: unknown key {k:?}; allowed fingerprint, tag_fingerprint, local_tags, max_host_failures, max_step_attempts")),
        }
    }
    Ok(c)
}

impl MhConfig {
    /// The commands a host with these tags is checked with: the common ones, then each tag's, without repeats.
    pub fn probes_for(&self, tags: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let tagged = self.tag_fingerprint.iter().filter(|(t, _)| tags.contains(t)).flat_map(|(_, c)| c.iter());
        for p in self.fingerprint.iter().chain(tagged) {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// Every command of the config (what the coordinator runs once for itself).
    pub fn all_probes(&self) -> Vec<String> {
        let tags: Vec<String> = self.tag_fingerprint.iter().map(|(t, _)| t.clone()).collect();
        self.probes_for(&tags)
    }

    fn tag_probes(&self, tag: &str) -> Vec<String> {
        self.tag_fingerprint.iter().filter(|(t, _)| t == tag).flat_map(|(_, c)| c.iter().cloned()).collect()
    }
}

/// The tags a step needs: its `requires`, else the coordinator's platform tag.
pub fn requires_of(step: &crate::chain::Step, platform: &str) -> Vec<String> {
    if step.requires.is_empty() {
        vec![platform.to_string()]
    } else {
        step.requires.clone()
    }
}

/// A step may run on a host: every required tag is there, and the step's `cores` fit when the host declares cores.
pub fn qualifies(requires: &[String], cores: Option<u32>, h: &HostSpec) -> bool {
    requires.iter().all(|t| h.tags.contains(t)) && match (cores, h.cores) {
        (Some(c), Some(hc)) => c as usize <= hc,
        _ => true,
    }
}

/// Longest-first packing: each step (index, weight), longest first (stable), goes to the qualified host whose expected finish
/// `(queued weight + weight) / slots` is smallest (ties: host order). Returns the queue of each host and the steps no host qualifies for.
pub fn pack(items: &[(usize, f64)], hosts: &[HostSpec], can: &dyn Fn(usize, usize) -> bool) -> (Vec<VecDeque<usize>>, Vec<usize>) {
    let mut order: Vec<(usize, f64)> = items.to_vec();
    order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut queues: Vec<VecDeque<usize>> = vec![VecDeque::new(); hosts.len()];
    let mut load = vec![0.0f64; hosts.len()];
    let mut unplaced = Vec::new();
    for (s, w) in order {
        let best = (0..hosts.len()).filter(|&h| hosts[h].slots > 0 && can(s, h)).min_by(|&a, &b| {
            let fa = (load[a] + w) / hosts[a].slots as f64;
            let fb = (load[b] + w) / hosts[b].slots as f64;
            fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
        });
        match best {
            Some(h) => {
                queues[h].push_back(s);
                load[h] += w;
            }
            None => unplaced.push(s),
        }
    }
    (queues, unplaced)
}

/// How a slot got its step.
#[derive(Debug, Clone, PartialEq)]
pub enum Take {
    /// from its own queue
    Own,
    /// a step that was lost (or whose host was removed) and is waiting for any qualified host
    Orphan,
    /// from another host's queue
    Stolen(usize),
}

/// The queues of one tier: one per host (longest first), plus the orphans.
#[derive(Debug, Clone)]
pub struct Board {
    pub queues: Vec<VecDeque<usize>>,
    pub orphans: VecDeque<usize>,
    pub weight: HashMap<usize, f64>,
}

impl Board {
    pub fn new(queues: Vec<VecDeque<usize>>, orphans: Vec<usize>, weight: HashMap<usize, f64>) -> Board {
        Board { queues, orphans: orphans.into(), weight }
    }

    fn queued(&self, h: usize) -> f64 {
        self.queues[h].iter().map(|s| self.weight.get(s).copied().unwrap_or(1.0)).sum()
    }

    /// The next step for a free slot of host `h`: an orphan first, then the first ready step of its own queue, then the longest ready
    /// step it qualifies for from the host with the most queued weight.
    pub fn take(&mut self, h: usize, ready: &dyn Fn(usize) -> bool, can: &dyn Fn(usize, usize) -> bool) -> Option<(usize, Take)> {
        if let Some(i) = self.orphans.iter().position(|&s| ready(s) && can(s, h)) {
            return self.orphans.remove(i).map(|s| (s, Take::Orphan));
        }
        if let Some(i) = self.queues[h].iter().position(|&s| ready(s) && can(s, h)) {
            return self.queues[h].remove(i).map(|s| (s, Take::Own));
        }
        let mut victims: Vec<usize> = (0..self.queues.len()).filter(|&v| v != h && !self.queues[v].is_empty()).collect();
        victims.sort_by(|&a, &b| self.queued(b).partial_cmp(&self.queued(a)).unwrap_or(std::cmp::Ordering::Equal));
        for v in victims {
            // the queue is longest first, so the first ready step it may run is the longest
            if let Some(i) = self.queues[v].iter().position(|&s| ready(s) && can(s, h)) {
                return self.queues[v].remove(i).map(|s| (s, Take::Stolen(v)));
            }
        }
        None
    }

    /// A lost step goes to the front of the orphans.
    pub fn requeue(&mut self, s: usize) {
        self.orphans.push_front(s);
    }

    /// A removed host's queue becomes orphans, in its order.
    pub fn drop_host(&mut self, h: usize) {
        let q: Vec<usize> = self.queues[h].drain(..).collect();
        self.orphans.extend(q);
    }

    pub fn remove(&mut self, s: usize) {
        self.orphans.retain(|&x| x != s);
        for q in &mut self.queues {
            q.retain(|&x| x != s);
        }
    }
}

/// Everything a multi-host run needs besides the plan.
pub struct MhOpts {
    pub out: PathBuf,
    pub keep_going: bool,
    pub only: Vec<String>,
    pub skip: Vec<String>,
    pub mode: String,
    /// the coordinator's name
    pub host: Option<String>,
    pub run_id: String,
    /// expected seconds per step name; a missing step weighs the median of the known ones, no history = 1 each
    pub weights: HashMap<String, f64>,
    /// the coordinator's own slots (0 = it only dispatches) and extra tags
    pub local_slots: usize,
    pub local_cores: Option<usize>,
    /// the worker hosts with their declared tags (the platform tag is added from their hello)
    pub workers: Vec<HostSpec>,
    pub cfg: MhConfig,
    /// the coordinator's fingerprint over every command of the config ([`MhConfig::all_probes`])
    pub fingerprint: Fingerprint,
    pub platform: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum St {
    Pending,
    Running,
    Green,
    Red,
    Skipped,
}

enum Ev {
    Line(usize, usize, String),
    Done(usize, usize, Box<Result<AgentResult, String>>),
}

/// The checks every result passes before it counts: the commit, the fingerprint, the step, a known exit code. Err(why, remove the host now).
fn check_result(r: &AgentResult, sha: &str, fp: &str, step: &str) -> Result<i64, (String, bool)> {
    if let Some(e) = &r.error {
        return Err((format!("the agent refused: {e}"), true));
    }
    if r.protocol != PROTOCOL {
        return Err((format!("agent protocol {} is not {PROTOCOL}", r.protocol), true));
    }
    if r.sha != sha {
        return Err((format!("the result names commit {} instead of {}", short(&r.sha), short(sha)), true));
    }
    if r.fingerprint != fp {
        return Err(("the result carries another fingerprint than the one accepted".into(), true));
    }
    if r.step != step {
        return Err((format!("the result is for step {:?}, not {step:?}", r.step), false));
    }
    r.rc.ok_or_else(|| ("the result has no exit code (unknown status)".to_string(), false))
}

/// Run a plan over the coordinator and the accepted workers. Err = configuration error (exit 2).
pub fn run(plan: &Plan, digest: &str, o: &MhOpts, t: Arc<dyn Transport>, emit: &mut dyn FnMut(&str)) -> Result<Outcome, String> {
    for n in o.only.iter().chain(&o.skip) {
        if !plan.steps.iter().any(|s| &s.name == n) {
            return Err(format!("--only/--skip names {n:?}, which is no step"));
        }
    }
    std::fs::create_dir_all(&o.out).map_err(|e| format!("cannot create {}: {e}", o.out.display()))?;
    let cwd = PathBuf::from(plan.cwd.clone().unwrap_or_else(|| ".".into()));
    let coord = o.host.as_deref().and_then(clean_host).unwrap_or_else(host_name);
    let out_abs = o.out.canonicalize().unwrap_or_else(|_| o.out.clone());
    let sha = git_out(&cwd, &["rev-parse", "HEAD"]).ok_or("a multi-host run needs a git repository: every host must run one commit")?;
    let dirty = |when: &str| -> Result<(), String> {
        match crate::chain::tree_status(&cwd, &out_abs) {
            Some(s) if s.is_empty() => Ok(()),
            Some(_) => Err(format!("the tree is dirty {when} (git status --porcelain is not empty): a multi-host run needs a clean tree, every host runs the committed one")),
            None => Err("git status failed".into()),
        }
    };
    dirty("before the run")?;
    let selected: Vec<usize> = (0..plan.steps.len()).filter(|&i| (o.only.is_empty() || o.only.contains(&plan.steps[i].name)) && !o.skip.contains(&plan.steps[i].name)).collect();

    let mut log_hash = Sha256::new();
    let mut say = |l: &str| {
        log_hash.update(l.as_bytes());
        log_hash.update(b"\n");
        emit(l);
    };
    let branch = git_out(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into()).replace(char::is_whitespace, "_");
    say(&format!("branch {branch} at {sha} on {coord}"));
    let started_at = Ts::now_utc().iso();
    let t_run = Instant::now();
    let mut events: Vec<HostEvent> = Vec::new();
    let ev = |events: &mut Vec<HostEvent>, host: &str, step: Option<&str>, event: &str, reason: &str| {
        events.push(HostEvent { at: Ts::now_utc().iso(), host: host.into(), step: step.map(String::from), event: event.into(), reason: reason.into() });
    };

    // ---- the hosts: the coordinator, then every worker that answers hello with our commit and fingerprint
    let mut hosts: Vec<HostSpec> = Vec::new();
    let mut fps: Vec<String> = Vec::new();
    let mut probe_lists: Vec<Vec<String>> = Vec::new();
    let mut host_recs: Vec<HostRec> = Vec::new();
    let mut local_tags = vec![o.platform.clone()];
    local_tags.extend(o.cfg.local_tags.iter().cloned());
    let local_probes = o.cfg.probes_for(&local_tags);
    let local_fp = o.fingerprint.subset(&local_probes).hash;
    host_recs.push(HostRec { host: coord.clone(), role: "coordinator".into(), sha: Some(sha.clone()), fingerprint: Some(local_fp.clone()), tags: local_tags.clone(), slots: o.local_slots as i64, accepted: o.local_slots > 0, reason: (o.local_slots == 0).then(|| "--local-slots 0: the coordinator only dispatches".into()) });
    if o.local_slots > 0 {
        hosts.push(HostSpec { name: coord.clone(), tags: local_tags, slots: o.local_slots, cores: o.local_cores, local: true });
        fps.push(local_fp);
        probe_lists.push(local_probes);
    }
    // each worker answers the common commands and those of every tag it declares (and of our platform tag)
    let asked: Vec<Vec<String>> = o.workers.iter().map(|w| {
        let mut cand = w.tags.clone();
        if !cand.contains(&o.platform) {
            cand.push(o.platform.clone());
        }
        o.cfg.probes_for(&cand)
    }).collect();
    let hellos: Vec<Result<Hello, String>> = std::thread::scope(|sc| {
        let hs: Vec<_> = o.workers.iter().zip(&asked).map(|(w, probes)| { let t = t.clone(); let sha = sha.clone(); sc.spawn(move || t.hello(w, &sha, probes)) }).collect();
        hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("the hello thread died".into()))).collect()
    });
    let mine_out = |p: &str| o.fingerprint.get(p).map(str::to_string);
    for (w, hello) in o.workers.iter().zip(hellos) {
        type Accepted = (Vec<String>, String, Vec<String>, Vec<(String, String)>);
        let verdict: Result<Accepted, String> = match &hello {
            Err(e) => Err(format!("no answer: {e}")),
            Ok(h) if h.error.is_some() => Err(format!("the agent refused: {}", h.error.as_deref().unwrap_or_default())),
            Ok(h) if h.protocol != PROTOCOL => Err(format!("agent protocol {} is not {PROTOCOL}", h.protocol)),
            Ok(h) if h.sha != sha => Err(format!("it checked out {} instead of {}", if h.sha.is_empty() { "nothing" } else { &h.sha }, sha)),
            Ok(h) => match &h.fingerprint {
                None => Err("it reported no fingerprint".into()),
                Some(f) if f.version != o.fingerprint.version => Err(format!("fingerprint mismatch: gatetrain {} against the coordinator's {}", f.version, o.fingerprint.version)),
                Some(f) => {
                    let differs = |p: &String| (f.get(p).map(str::to_string) != mine_out(p)).then(|| format!("`{p}` printed {:?} against the coordinator's {:?}", short(f.get(p).unwrap_or("nothing")), short(&mine_out(p).unwrap_or_default())));
                    let wrong: Vec<&String> = w.tags.iter().filter(|t| is_platform_tag(t) && **t != h.platform).collect();
                    if let Some(why) = o.cfg.fingerprint.iter().find_map(differs) {
                        Err(format!("fingerprint mismatch: {why}"))
                    } else if !wrong.is_empty() || h.platform.is_empty() {
                        Err(format!("it declares {} but reports platform {:?}", wrong.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","), h.platform))
                    } else {
                        let mut cand = w.tags.clone();
                        if !cand.contains(&h.platform) {
                            cand.push(h.platform.clone());
                        }
                        let (mut kept, mut dropped) = (Vec::new(), Vec::new());
                        for t in cand {
                            match o.cfg.tag_probes(&t).iter().find_map(differs) {
                                Some(why) => dropped.push((t, why)),
                                None => kept.push(t),
                            }
                        }
                        let list = o.cfg.probes_for(&kept);
                        let expected = o.fingerprint.subset(&list).hash;
                        if f.subset(&list).hash != expected {
                            Err("fingerprint mismatch: its answers do not hash to the coordinator's".into())
                        } else {
                            Ok((kept, expected, list, dropped))
                        }
                    }
                }
            },
        };
        let reported = hello.as_ref().ok();
        match verdict {
            Ok((tags, fp, list, dropped)) => {
                for (t, why) in &dropped {
                    say(&format!("HOST_TAG_DROPPED {} {t} {why}", w.name));
                    ev(&mut events, &w.name, None, "tag-dropped", &format!("{t}: {why}"));
                }
                say(&format!("HOST_ACCEPTED {} slots={} tags={} fingerprint={}", w.name, w.slots, tags.join(","), &fp[..12.min(fp.len())]));
                let reason = (!dropped.is_empty()).then(|| format!("tags dropped: {}", dropped.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(",")));
                host_recs.push(HostRec { host: w.name.clone(), role: "worker".into(), sha: Some(sha.clone()), fingerprint: Some(fp.clone()), tags: tags.clone(), slots: w.slots as i64, accepted: true, reason });
                hosts.push(HostSpec { name: w.name.clone(), tags, slots: w.slots, cores: w.cores, local: false });
                fps.push(fp);
                probe_lists.push(list);
            }
            Err(why) => {
                let why = why.replace(['\r', '\n'], " ");
                say(&format!("HOST_REFUSED {} {why}", w.name));
                ev(&mut events, &w.name, None, "refused", &why);
                host_recs.push(HostRec {
                    host: w.name.clone(),
                    role: "worker".into(),
                    sha: reported.map(|h| h.sha.clone()).filter(|s| !s.is_empty()),
                    fingerprint: reported.and_then(|h| h.fingerprint.as_ref().map(|f| f.hash.clone())),
                    tags: w.tags.clone(),
                    slots: w.slots as i64,
                    accepted: false,
                    reason: Some(why),
                });
            }
        }
    }
    let total_slots: usize = hosts.iter().map(|h| h.slots).sum();
    say(&format!("CHAIN_MODE {}", o.mode));
    say(&format!("CHAIN_PARALLEL {}", total_slots.max(1)));
    say(&format!("CHAIN_ORDER {}", if o.weights.is_empty() { "file" } else { "longest" }));
    say(&format!("CHAIN_HOSTS {}", hosts.iter().map(|h| format!("{}={}", h.name, h.slots)).collect::<Vec<_>>().join(" ")));
    say(&format!("CHAIN_OUT {}", out_abs.to_string_lossy().trim_start_matches(r"\\?\")));

    // weights: the ledger's medians, a missing step the median of the known ones, no history 1 each
    let known: Vec<f64> = o.weights.values().copied().collect();
    let fallback = if known.is_empty() { 1.0 } else { crate::stats::median(&known) };
    let weight = |i: usize| o.weights.get(&plan.steps[i].name).copied().unwrap_or(fallback);

    let n = plan.steps.len();
    let mut state = vec![St::Pending; n];
    let mut reds: Vec<(String, i64)> = Vec::new();
    let mut recs: Vec<StepRec> = Vec::new();
    let mut shard_recs: Vec<ShardRec> = Vec::new();
    let mut skips: Vec<(String, String)> = Vec::new();
    let mut times: Vec<(usize, i64, f64, usize)> = Vec::new();
    let mut started: Vec<Option<(String, Instant)>> = vec![None; n];
    let mut lost_on: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    let mut attempts = vec![0usize; n];
    let mut alive = vec![true; hosts.len()];
    let mut host_fail = vec![0usize; hosts.len()];
    let mut busy = vec![0usize; hosts.len()];
    let requires: Vec<Vec<String>> = plan.steps.iter().map(|s| requires_of(s, &o.platform)).collect();
    let mut fast_red = false;
    let (tx, rx) = mpsc::channel::<Ev>();
    let mut agent_logs: HashMap<(usize, usize), std::fs::File> = HashMap::new();

    for tier in [Tier::Fast, Tier::Slow] {
        let mine: Vec<usize> = selected.iter().copied().filter(|&i| plan.steps[i].tier == tier).collect();
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
        let static_can = |s: usize, h: usize| qualifies(&requires[s], plan.steps[s].cores, &hosts[h]);
        let items: Vec<(usize, f64)> = mine.iter().map(|&i| (i, weight(i))).collect();
        let (queues, unplaced) = pack(&items, &hosts, &static_can);
        for (h, q) in queues.iter().enumerate() {
            for &s in q {
                ev(&mut events, &hosts[h].name, Some(&plan.steps[s].name), "placed", &format!("longest first, weight {:.1}", weight(s)));
            }
        }
        let mut board = Board::new(queues, unplaced, items.iter().copied().collect());
        let mut running = 0usize;
        loop {
            // settle skips: a pending step whose need is red or skipped never runs
            let mut changed = true;
            while changed {
                changed = false;
                for &i in &mine {
                    if state[i] != St::Pending {
                        continue;
                    }
                    let bad = plan.steps[i].needs.iter().find(|nm| plan.steps.iter().position(|s| &s.name == *nm).is_some_and(|j| matches!(state[j], St::Red | St::Skipped)));
                    if let Some(nm) = bad {
                        state[i] = St::Skipped;
                        board.remove(i);
                        let why = format!("needs {nm}, which is not green");
                        say(&format!("STEP_SKIPPED {} {why}", plan.steps[i].name));
                        skips.push((plan.steps[i].name.clone(), why));
                        changed = true;
                    }
                }
            }
            let can = |s: usize, h: usize| alive[h] && !lost_on[s].contains(&h) && static_can(s, h);
            // a pending step no live host may run is red: lost everywhere (125) or never placeable (126)
            for &i in &mine {
                if state[i] == St::Pending && !(0..hosts.len()).any(|h| can(i, h)) {
                    let (rc, why) = if attempts[i] > 0 { (RC_LOST, format!("lost on every qualified host ({} attempt(s))", attempts[i])) } else { (RC_NO_HOST, format!("no accepted host has the tags {}", requires[i].join(","))) };
                    board.remove(i);
                    let name = &plan.steps[i].name;
                    if started[i].is_none() {
                        say(&format!("STEP_START {name} {}", Ts::now_utc().iso()));
                    }
                    let secs = started[i].as_ref().map_or(0.0, |(_, t0)| t0.elapsed().as_secs_f64());
                    say(&format!("STEP_TIME {name} {secs:.1}s"));
                    say(&format!("STEP_EXIT {name} {rc}"));
                    say(&format!("RED_NOW {name} rc={rc} at {} ({why})", Ts::now_utc().iso()));
                    ev(&mut events, &coord, Some(name), "unplaceable", &why);
                    append(&o.out.join(format!("{name}.full")), &format!("chain: {why}; the step is red, it never counts green"));
                    state[i] = St::Red;
                    reds.push((name.clone(), rc));
                    if tier == Tier::Fast {
                        fast_red = true;
                    }
                    recs.push(StepRec { name: name.clone(), tier: tier_name(tier), started_at: started[i].as_ref().map(|s| s.0.clone()), ended_at: Some(Ts::now_utc().iso()), rc, secs: round1(secs), cached: false, cached_from: None, key: None, shards: i64::from(nshards(&plan.steps[i])), log_ref: format!("{name}.full"), host: None, fingerprint: None });
                    times.push((i, rc, secs, usize::MAX));
                }
            }
            // fill every free slot of every live host
            let ready = |s: usize| state[s] == St::Pending && plan.steps[s].needs.iter().all(|nm| plan.steps.iter().position(|x| &x.name == nm).is_none_or(|j| state[j] == St::Green || !selected.contains(&j)));
            let mut starts: Vec<(usize, usize, Take)> = Vec::new();
            for h in 0..hosts.len() {
                while alive[h] && busy[h] + starts.iter().filter(|x| x.1 == h).count() < hosts[h].slots {
                    let taken: Vec<usize> = starts.iter().map(|x| x.0).collect();
                    match board.take(h, &|s| ready(s) && !taken.contains(&s), &can) {
                        Some((s, how)) => starts.push((s, h, how)),
                        None => break,
                    }
                }
            }
            for (s, h, how) in starts {
                let name = plan.steps[s].name.clone();
                if started[s].is_none() {
                    let now = Ts::now_utc().iso();
                    say(&format!("STEP_START {name} {now}"));
                    started[s] = Some((now, Instant::now()));
                }
                say(&format!("STEP_HOST {name} {}", hosts[h].name));
                match &how {
                    Take::Stolen(v) => {
                        say(&format!("STEP_STOLEN {name} from={} by={}", hosts[*v].name, hosts[h].name));
                        ev(&mut events, &hosts[h].name, Some(&name), "stolen", &format!("from {}: {} had no ready step of its own", hosts[*v].name, hosts[h].name));
                    }
                    Take::Orphan => ev(&mut events, &hosts[h].name, Some(&name), "placed", "re-run of a lost step, or the queue of a removed host"),
                    Take::Own => {}
                }
                state[s] = St::Running;
                busy[h] += 1;
                running += 1;
                let req = StepReq { step: name, sha: sha.clone(), run_id: o.run_id.clone(), par: nshards(&plan.steps[s]) as usize, timeout_s: plan.steps[s].timeout_s, probes: probe_lists[h].clone() };
                let (t, host, tx) = (t.clone(), hosts[h].clone(), tx.clone());
                std::thread::spawn(move || {
                    let tx2 = tx.clone();
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| t.run_step(&host, &req, &mut |l| {
                        let _ = tx2.send(Ev::Line(s, h, l.to_string()));
                    })))
                    .unwrap_or_else(|_| Err("the transport for this step died".into()));
                    let _ = tx.send(Ev::Done(s, h, Box::new(res)));
                });
            }
            if running == 0 {
                break;
            }
            let (s, h, res) = loop {
                match rx.recv() {
                    Ok(Ev::Line(s, h, l)) => {
                        // progress only: kept next to the step's output, never read as a verdict
                        let f = agent_logs.entry((s, h)).or_insert_with(|| {
                            std::fs::OpenOptions::new().create(true).append(true).open(o.out.join(format!("{}.{}.agent.log", plan.steps[s].name, hosts[h].name))).unwrap_or_else(|_| null_sink())
                        });
                        let _ = std::io::Write::write_all(f, format!("{l}\n").as_bytes());
                    }
                    Ok(Ev::Done(s, h, r)) => break (s, h, *r),
                    Err(e) => return Err(format!("chain: lost a worker thread: {e}")),
                }
            };
            running -= 1;
            busy[h] -= 1;
            let name = plan.steps[s].name.clone();
            let verdict = match &res {
                Ok(r) => check_result(r, &sha, &fps[h], &name),
                Err(e) => Err((e.clone(), false)),
            };
            match verdict {
                Ok(rc) => {
                    let r = res.as_ref().expect("a checked result");
                    if !hosts[h].local {
                        if let Err(e) = t.fetch(&hosts[h], r, &o.out) {
                            append(&o.out.join(format!("{name}.full")), &format!("chain: the output of this step could not be copied from {}: {e}", hosts[h].name));
                        }
                    }
                    let secs = started[s].as_ref().map_or(r.secs, |(_, t0)| t0.elapsed().as_secs_f64());
                    for sh in &r.shards {
                        say(&format!("STEP_SHARD {name} {}/{} START {}", sh.shard, sh.of, sh.started_at));
                        say(&format!("STEP_SHARD {name} {}/{} EXIT {} {:.1}s", sh.shard, sh.of, sh.rc, sh.secs));
                        shard_recs.push(ShardRec { step: name.clone(), shard: sh.shard, of: sh.of, started_at: sh.started_at.clone(), secs: round1(sh.secs), rc: sh.rc, log_ref: sh.log_ref.clone() });
                    }
                    say(&format!("STEP_TIME {name} {secs:.1}s"));
                    say(&format!("STEP_EXIT {name} {rc}"));
                    recs.push(StepRec {
                        name: name.clone(),
                        tier: tier_name(tier),
                        started_at: started[s].as_ref().map(|x| x.0.clone()),
                        ended_at: Some(Ts::now_utc().iso()),
                        rc,
                        secs: round1(secs),
                        cached: false,
                        cached_from: None,
                        key: None,
                        shards: i64::from(nshards(&plan.steps[s])),
                        log_ref: format!("{name}.full"),
                        host: Some(hosts[h].name.clone()),
                        fingerprint: Some(fps[h].clone()),
                    });
                    times.push((s, rc, secs, h));
                    if rc == 0 {
                        state[s] = St::Green;
                    } else {
                        // red is red: never tried on another host
                        state[s] = St::Red;
                        reds.push((name.clone(), rc));
                        if tier == Tier::Fast {
                            fast_red = true;
                        }
                        say(&format!("RED_NOW {name} rc={rc} at {} (host {}; full output: {name}.full)", Ts::now_utc().iso(), hosts[h].name));
                    }
                }
                Err((why, remove_now)) => {
                    let why = why.replace(['\r', '\n'], " ");
                    say(&format!("STEP_HOST_LOST {name} {} {why}", hosts[h].name));
                    ev(&mut events, &hosts[h].name, Some(&name), "lost", &why);
                    host_fail[h] += 1;
                    lost_on[s].insert(h);
                    attempts[s] += 1;
                    if alive[h] && (remove_now || host_fail[h] >= o.cfg.max_host_failures) {
                        alive[h] = false;
                        board.drop_host(h);
                        let r = if remove_now { why.clone() } else { format!("{} step(s) lost (max_host_failures = {})", host_fail[h], o.cfg.max_host_failures) };
                        say(&format!("HOST_REMOVED {} {r}", hosts[h].name));
                        ev(&mut events, &hosts[h].name, None, "removed", &r);
                        if let Some(rec) = host_recs.iter_mut().find(|x| x.host == hosts[h].name) {
                            rec.reason = Some(format!("removed during the run: {r}"));
                        }
                    }
                    if attempts[s] >= o.cfg.max_step_attempts {
                        let why = format!("lost {} time(s) (max_step_attempts = {})", attempts[s], o.cfg.max_step_attempts);
                        let secs = started[s].as_ref().map_or(0.0, |(_, t0)| t0.elapsed().as_secs_f64());
                        say(&format!("STEP_TIME {name} {secs:.1}s"));
                        say(&format!("STEP_EXIT {name} {RC_LOST}"));
                        say(&format!("RED_NOW {name} rc={RC_LOST} at {} ({why})", Ts::now_utc().iso()));
                        append(&o.out.join(format!("{name}.full")), &format!("chain: {why}; the step is red, it never counts green"));
                        state[s] = St::Red;
                        reds.push((name.clone(), RC_LOST));
                        if tier == Tier::Fast {
                            fast_red = true;
                        }
                        recs.push(StepRec { name: name.clone(), tier: tier_name(tier), started_at: started[s].as_ref().map(|x| x.0.clone()), ended_at: Some(Ts::now_utc().iso()), rc: RC_LOST, secs: round1(secs), cached: false, cached_from: None, key: None, shards: i64::from(nshards(&plan.steps[s])), log_ref: format!("{name}.full"), host: None, fingerprint: None });
                        times.push((s, RC_LOST, secs, usize::MAX));
                    } else {
                        state[s] = St::Pending;
                        board.requeue(s);
                    }
                }
            }
        }
    }
    drop(agent_logs);
    for h in hosts.iter().filter(|h| !h.local) {
        t.finish(h, &o.run_id);
    }
    say(&format!("RED_SUMMARY: {}", if reds.is_empty() { "none".to_string() } else { summary_in_file_order(plan, &reds) }));
    say("GATES_DONE");
    let log_sha256: String = log_hash.clone().finalize().iter().map(|b| format!("{b:02x}")).collect();
    let ended_at = Ts::now_utc().iso();

    let mut stamp_text = None;
    let stamp = if !reds.is_empty() {
        Err("the run is red".to_string())
    } else if let Err(why) = dirty("after the run") {
        Err(format!("{why}: no stamp written"))
    } else {
        let steps: Vec<serde_json::Value> = times
            .iter()
            .map(|&(i, rc, secs, h)| {
                serde_json::json!({
                    "name": plan.steps[i].name, "tier": tier_name(plan.steps[i].tier), "rc": rc, "seconds": round1(secs), "key": null, "cached_from": null,
                    "shards": nshards(&plan.steps[i]), "host": hosts.get(h).map(|x| x.name.clone()), "fingerprint": fps.get(h).cloned(),
                })
            })
            .collect();
        let hosts_j: Vec<serde_json::Value> = host_recs
            .iter()
            .map(|r| serde_json::json!({"name": r.host, "role": r.role, "tags": r.tags, "slots": r.slots, "sha": r.sha, "fingerprint": r.fingerprint, "accepted": r.accepted, "reason": r.reason}))
            .collect();
        let body = serde_json::json!({
            "v": 1, "kind": o.mode, "sha": sha, "host": coord, "at": Ts::now_utc().iso(), "mode": o.mode, "par": total_slots,
            "total_seconds": round1(t_run.elapsed().as_secs_f64()), "digest": digest, "steps": steps, "cached": Vec::<String>::new(),
            "prev": Vec::<String>::new(), "multi_host": true, "run_id": o.run_id, "hosts": hosts_j,
        });
        let text = serde_json::to_string_pretty(&body).unwrap_or_default() + "\n";
        let p = o.out.join(format!("stamp-{sha}.json"));
        std::fs::write(&p, &text).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        stamp_text = Some((crate::chain::digest_of(&text), text));
        Ok(p)
    };
    let record = RunRec {
        sha: Some(sha),
        branch: Some(branch),
        host: coord,
        mode: o.mode.clone(),
        par: total_slots,
        started_at,
        ended_at,
        outcome: if reds.is_empty() { "green" } else { "red" },
        plan_digest: digest.to_string(),
        log_sha256,
        steps: recs,
        shards: shard_recs,
        skips,
        stamp: stamp_text,
        retries: Vec::new(),
        hosts: host_recs,
        host_events: events,
    };
    Ok(Outcome { reds, stamp, record })
}

fn null_sink() -> std::fs::File {
    #[cfg(windows)]
    let null = "NUL";
    #[cfg(not(windows))]
    let null = "/dev/null";
    std::fs::OpenOptions::new().write(true).open(null).expect("the null device")
}

fn append(full: &Path, line: &str) {
    use std::io::Write;
    let ends_nl = std::fs::read(full).ok().is_none_or(|b| b.is_empty() || b.ends_with(b"\n"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(full) {
        let _ = writeln!(f, "{}{line}", if ends_nl { "" } else { "\n" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, tags: &[&str], slots: usize) -> HostSpec {
        HostSpec { name: name.into(), tags: tags.iter().map(|s| s.to_string()).collect(), slots, cores: None, local: false }
    }

    #[test]
    fn longest_first_packing_balances_by_slots() {
        let hosts = [h("a", &["t"], 1), h("b", &["t"], 1)];
        // longest first: 5 -> a, 4 -> b, 3 -> b (7 < 8), 3 -> a (8 < 10), 1 -> b (8 < 9)
        let items = [(0, 3.0), (1, 5.0), (2, 1.0), (3, 4.0), (4, 3.0)];
        let (q, un) = pack(&items, &hosts, &|_, _| true);
        assert!(un.is_empty());
        assert_eq!(q[0].iter().copied().collect::<Vec<_>>(), vec![1, 4]);
        assert_eq!(q[1].iter().copied().collect::<Vec<_>>(), vec![3, 0, 2]);
    }

    #[test]
    fn a_host_with_more_slots_takes_more_weight() {
        let hosts = [h("big", &["t"], 3), h("small", &["t"], 1)];
        let items: Vec<(usize, f64)> = (0..8).map(|i| (i, 1.0)).collect();
        let (q, _) = pack(&items, &hosts, &|_, _| true);
        assert_eq!((q[0].len(), q[1].len()), (6, 2));
    }

    #[test]
    fn placement_follows_the_tags() {
        let hosts = [h("lin", &["x86_64-linux"], 4), h("js", &["x86_64-linux", "js"], 1)];
        let req = [vec!["x86_64-linux".to_string(), "js".to_string()], vec!["x86_64-linux".to_string()], vec!["gpu".to_string()]];
        let can = |s: usize, hh: usize| qualifies(&req[s], None, &hosts[hh]);
        let (q, un) = pack(&[(0, 1.0), (1, 1.0), (2, 1.0)], &hosts, &can);
        assert_eq!(q[1].iter().copied().collect::<Vec<_>>(), vec![0], "the js step goes only to the js host");
        assert_eq!(q[0].iter().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(un, vec![2], "no host has gpu");
    }

    #[test]
    fn cores_must_fit_a_host_that_declares_them() {
        let mut x = h("x", &["t"], 1);
        x.cores = Some(4);
        assert!(qualifies(&["t".into()], Some(4), &x));
        assert!(!qualifies(&["t".into()], Some(5), &x));
        x.cores = None;
        assert!(qualifies(&["t".into()], Some(64), &x), "a host without cores is not limited");
    }

    #[test]
    fn an_idle_host_steals_the_longest_ready_step_from_the_most_loaded() {
        let w: HashMap<usize, f64> = [(0, 9.0), (1, 5.0), (2, 2.0), (3, 1.0)].into_iter().collect();
        let mut b = Board::new(vec![VecDeque::from(vec![1, 2]), VecDeque::from(vec![3]), VecDeque::new()], vec![], w);
        assert_eq!(b.take(2, &|_| true, &|_, _| true), Some((1, Take::Stolen(0))));
        assert_eq!(b.take(2, &|s| s != 2, &|_, _| true), Some((3, Take::Stolen(1))), "a step that is not ready is not taken");
        assert_eq!(b.take(1, &|_| true, &|_, _| true), Some((2, Take::Stolen(0))));
        assert_eq!(b.take(1, &|_| true, &|_, _| true), None);
    }

    #[test]
    fn orphans_come_first_and_a_removed_host_queue_becomes_orphans() {
        let mut b = Board::new(vec![VecDeque::from(vec![0, 1]), VecDeque::from(vec![2])], vec![], HashMap::new());
        b.requeue(2);
        assert_eq!(b.take(0, &|_| true, &|_, _| true), Some((2, Take::Orphan)));
        b.drop_host(0);
        assert_eq!(b.orphans.iter().copied().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(b.take(1, &|_| true, &|s, _| s != 0), Some((1, Take::Orphan)), "an orphan only to a host that may run it");
    }

    #[test]
    fn fingerprints_name_the_first_difference() {
        let a = Fingerprint::of("0.3.0", vec![("rustc -V".into(), "rustc 1.90.0".into()), ("node -v".into(), "v22".into())]);
        let b = Fingerprint::of("0.3.0", vec![("rustc -V".into(), "rustc 1.91.0".into()), ("node -v".into(), "v22".into())]);
        assert_eq!(a.differs(&a.clone()), None);
        assert!(a.differs(&b).unwrap().contains("rustc -V"), "{:?}", a.differs(&b));
        let c = Fingerprint::of("0.2.2", a.items.clone());
        assert!(a.differs(&c).unwrap().contains("gatetrain 0.2.2"));
    }

    #[test]
    fn the_multihost_table_is_strict() {
        assert_eq!(mh_config("").unwrap(), MhConfig::default());
        let c = mh_config("[multihost]\nfingerprint = ['rustc -V']\nmax_host_failures = 3\n").unwrap();
        assert_eq!((c.fingerprint, c.max_host_failures, c.max_step_attempts), (vec!["rustc -V".to_string()], 3, 3));
        assert!(mh_config("[multihost]\nfingerprnt = []\n").unwrap_err().contains("unknown key"));
        assert!(mh_config("[multihost]\nmax_step_attempts = 0\n").is_err());
    }

    #[test]
    fn platform_tags_are_recognised() {
        assert!(is_platform_tag(&platform_tag()));
        assert!(is_platform_tag("aarch64-linux") && !is_platform_tag("js") && !is_platform_tag("-linux"));
    }
}
