//! The real seams of the batch state machine: [`RealLock`] is [`HostLock`] over the holder (`hold.rs`: an OS file lock, not a
//! lock file that merely exists) and [`MemoryAdmission`] is [`Admission`] over the memory rule (`memory.rs`) and the lock.
//!
//! "Is this run still running" is read off the lock itself: a lock that can be taken has no live holder, so a chain that died
//! without `GATES_DONE` is `STALE` (the batch records it as stopped, never as running and never as red).
use crate::batch::{Admission, HostLock, LaunchRequest};
use crate::tidy::close::CloseEnv;
use crate::disk;
use crate::doctor;
use crate::hold::{self, State};
use crate::ledger::Ledger;
use crate::memory::{self, MemoryModel};
use crate::{Error, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The lock, and how to start the holder. `holder` is the program and its leading arguments, normally
/// `[<gatetrain>, "hold"]`; the run id, `--lock`, `--db` and the chain command are appended.
pub struct RealLock {
    pub lock: PathBuf,
    pub db: PathBuf,
    pub hosts: Option<(PathBuf, String)>,
    pub holder: Vec<String>,
}

impl HostLock for RealLock {
    fn names_run(&self, run_id: &str) -> Result<bool> {
        Ok(matches!(hold::status(&self.lock)?, State::Held(i) if i.run_id == run_id))
    }

    fn launch(&self, r: &LaunchRequest) -> Result<()> {
        let (prog, lead) = self.holder.split_first().ok_or_else(|| Error::Batch("no holder command".into()))?;
        if r.command.is_empty() {
            return Err(Error::Batch("empty chain command".into()));
        }
        let out = std::fs::File::create(&r.log)?;
        let mut c = Command::new(prog);
        c.args(lead).arg(&r.run_id).arg("--lock").arg(&self.lock).arg("--db").arg(&self.db);
        if let Some((h, n)) = &self.hosts {
            c.arg("--hosts").arg(h).arg("--host").arg(n);
        }
        // The holder prints HOLDS / RELEASED and the chain's own output goes to the same log, which is what `parse` reads.
        c.arg("--").args(&r.command).current_dir(&r.workdir).envs(r.env.iter().map(|(k, v)| (k, v)));
        c.stdin(Stdio::null()).stdout(out.try_clone()?).stderr(out).spawn()?;
        Ok(())
    }
}

/// A boolean key of one host of a hosts file (`false` when absent or unreadable): `light_while_proving`.
pub fn host_flag(text: &str, host: &str, key: &str) -> bool {
    text.parse::<toml::Value>().ok().and_then(|v| v.get("hosts")?.as_array()?.iter().find(|h| h.get("name").and_then(|n| n.as_str()) == Some(host))?.get(key)?.as_bool()).unwrap_or(false)
}

/// `None` when `root` is the top level of a git repository, else why not (the check `hold --repo-root` makes).
pub fn repo_root_problem(root: &std::path::Path) -> Option<String> {
    if !root.is_dir() {
        return Some(format!("{} is not a folder", root.display()));
    }
    let top = match Command::new("git").arg("-C").arg(root).args(["rev-parse", "--show-toplevel"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Ok(_) => return Some(format!("{} is not inside a git repository", root.display())),
        Err(e) => return Some(format!("git could not be run to check {}: {e}", root.display())),
    };
    let same = |a: &std::path::Path, b: &std::path::Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same(root, std::path::Path::new(&top)) {
        None
    } else {
        Some(format!("{} is inside the repository {top} but is not its top level", root.display()))
    }
}

/// Admission: the same rules `gatetrain hold` applies, in the same order, so a batch is refused for the reasons (and with the exit codes)
/// a hold would be: the working directory is the repository top level, the chain's memory fits, the disk keeps its minimum after the
/// build folders grow, for role `proof` the lock is free, and the host passes `doctor`.
pub struct MemoryAdmission {
    pub lock: PathBuf,
    pub db: PathBuf,
    /// hosts.toml and the host name; without it only the lock and the working directory are checked (no number is invented).
    pub hosts: Option<(PathBuf, String)>,
    pub chain_steps: Vec<String>,
    /// The build folders the chain writes to (`[chain] target_dirs`).
    pub target_dirs: Vec<PathBuf>,
    /// The repository top level the chain must run in (`[chain] repo_root`).
    pub repo_root: Option<PathBuf>,
    /// Run `doctor` for the host (needs `hosts`).
    pub doctor: bool,
    /// The steps of the launched pre-flight (`[preflight] steps`): a light run is admitted on these, not on the chain's. Empty = the chain's.
    pub preflight_steps: Vec<String>,
    /// The host's own word that a light run may start while a proof holds its lock (host key `light_while_proving`, default false).
    pub light_while_proving: bool,
    /// The disk floor (engine-v2 T3, T4): on, the free space of every disk whose `use` covers the run, minus the run's predicted growth
    /// (the ledger's median for its target folders, else `needs_gb`; unknown refuses), must stay at or above `min_free_gb`.
    pub disk_floor: bool,
    /// `[needs] preflight_gb` and `chain_gb` of the hosts file: the growth assumed when the ledger has no history.
    pub needs_preflight_gb: Option<f64>,
    pub needs_chain_gb: Option<f64>,
    /// `tidy --auto` for this host (the unattended classes only), called by the engine's disk floor after a refusal when policy allows it.
    pub tidy: Option<Box<dyn Fn() -> Result<String>>>,
    /// T9 close's view of this host.
    pub close: Option<Box<dyn CloseEnv>>,
    /// The hosts file could not be loaded: the floor cannot be judged, so every run is refused (fail closed).
    pub floor_error: Option<String>,
}

impl MemoryAdmission {
    pub fn new(lock: PathBuf, db: PathBuf, hosts: Option<(PathBuf, String)>, chain_steps: Vec<String>) -> MemoryAdmission {
        MemoryAdmission { lock, db, hosts, chain_steps, target_dirs: vec![], repo_root: None, doctor: false, preflight_steps: vec![], light_while_proving: false, disk_floor: false, needs_preflight_gb: None, needs_chain_gb: None, tidy: None, close: None, floor_error: None }
    }
}

impl MemoryAdmission {
    /// `None` when every disk whose `use` covers the run keeps its `min_free_gb`, else why not. The growth is the ledger's median for the
    /// run's target folders that sit on that disk; with no history it is `[needs] preflight_gb` (light) or `chain_gb` (proof); with
    /// neither it is unknown, and unknown refuses.
    fn floor_problem(&self, text: &str, host: &str, role: &str, hist: &std::collections::HashMap<String, Vec<f64>>) -> Result<Option<String>> {
        let v: toml::Value = text.parse().map_err(|e: toml::de::Error| Error::Batch(format!("hosts.toml: {e}")))?;
        let uses = if role == "light" { "preflight" } else { "chain" };
        let h = v.get("hosts").and_then(|h| h.as_array()).and_then(|a| a.iter().find(|h| h.get("name").and_then(|n| n.as_str()) == Some(host)));
        let Some(h) = h else { return Ok(None) };
        let needs = if role == "light" { self.needs_preflight_gb } else { self.needs_chain_gb };
        for d in h.get("disks").and_then(|d| d.as_array()).into_iter().flatten() {
            let covers = d.get("use").and_then(|u| u.as_array()).is_some_and(|u| u.iter().any(|x| x.as_str() == Some(uses)));
            let (Some(path), true) = (d.get("path").and_then(|p| p.as_str()), covers) else { continue };
            let Some(min) = d.get("min_free_gb").and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64))) else { continue };
            let dpath = disk::canon(path);
            let mut grown_mb: Option<f64> = None;
            for t in &self.target_dirs {
                let tc = disk::canon(&t.to_string_lossy());
                let on_disk = disk::norm(&tc) == disk::norm(&dpath) || disk::norm(&tc).starts_with(&format!("{}/", disk::norm(&dpath).trim_end_matches('/')));
                if on_disk {
                    if let Some(h) = hist.get(&disk::step_key(&tc)).filter(|h| !h.is_empty()) {
                        grown_mb = Some(grown_mb.unwrap_or(0.0) + memory::typical_mb(h));
                    }
                }
            }
            let predicted_gb = match (grown_mb, needs) {
                (Some(mb), _) => mb / 1024.0,
                (None, Some(g)) => g,
                (None, None) => return Ok(Some(format!("disk {path}: the growth of this run is unknown (no history in the ledger and no [needs] value); a floor cannot be shown to hold, refused"))),
            };
            let Some(free) = disk::free_gb(&dpath) else {
                return Ok(Some(format!("disk {path}: free space could not be read and min_free_gb {min:.1} is set; refused")));
            };
            if free - predicted_gb < min {
                return Ok(Some(format!("disk {path}: free {free:.1} GB - predicted growth {predicted_gb:.1} GB = {:.1} GB < min_free_gb {min:.1}", free - predicted_gb)));
            }
        }
        Ok(None)
    }
}

impl Admission for MemoryAdmission {
    fn tidy_auto(&self, _role: &str) -> Result<Option<String>> {
        match &self.tidy {
            Some(f) => f().map(Some),
            None => Ok(None),
        }
    }

    fn close_env(&self) -> Option<&dyn CloseEnv> {
        self.close.as_deref()
    }

    fn admit(&self, role: &str, batch: &str) -> Result<Option<String>> {
        Ok(self.admit_rule(role, batch)?.map(|(_, why)| why))
    }

    fn admit_rule(&self, role: &str, _batch: &str) -> Result<Option<(String, String)>> {
        let refuse = |rule: &str, why: String| Ok(Some((rule.to_string(), why)));
        if let Some(root) = &self.repo_root {
            if let Some(why) = repo_root_problem(root) {
                return refuse("host.refuse-cwd", why);
            }
        }
        let hist = if self.db.exists() { Ledger::open(&self.db)?.step_memory_history()? } else { Default::default() };
        if let Some((hosts, host)) = &self.hosts {
            let text = std::fs::read_to_string(hosts)?;
            let m = MemoryModel::from_hosts_toml(&text, host)?;
            // no step list means the chain as one `(chain)` step, admitted on its own recorded peak (as `hold` does); a light run is
            // admitted on the pre-flight's own steps when they are known, not on the chain's
            let own = if role == "light" && !self.preflight_steps.is_empty() { &self.preflight_steps } else { &self.chain_steps };
            let steps: Vec<String> = if own.is_empty() { vec!["(chain)".to_string()] } else { own.clone() };
            let ad = memory::admit(&m, &steps, &hist);
            if !ad.admitted {
                return refuse("host.refuse-memory", ad.rule);
            }
            let disks: Vec<disk::DiskRule> = disk::disks_from_hosts_toml(&text, host)?.into_iter().map(|d| disk::DiskRule { path: disk::canon(&d.path), ..d }).collect();
            if !disks.is_empty() {
                for t in &self.target_dirs {
                    let tc = disk::canon(&t.to_string_lossy());
                    let predicted_mb = memory::typical_mb(hist.get(&disk::step_key(&tc)).map(Vec::as_slice).unwrap_or(&[]));
                    let da = disk::admit(&disks, &tc, predicted_mb, &disk::free_gb);
                    if !da.admitted {
                        return refuse("host.refuse-disk", da.rule);
                    }
                }
            }
        }
        if role == "proof" {
            if let State::Held(i) = hold::status(&self.lock)? {
                return refuse("host.refuse", format!("host lock held by {} (pid {}) since {}", i.run_id, i.pid, i.started_at));
            }
        }
        if self.disk_floor && (role == "light" || role == "proof") {
            if let Some((hosts, host)) = &self.hosts {
                let text = std::fs::read_to_string(hosts)?;
                if let Some(why) = self.floor_problem(&text, host, role, &hist)? {
                    return refuse("host.refuse-disk", why);
                }
            }
        }
        // a pre-flight, a lane run or a probe must not slow a proof down (lesson L26); the host may say otherwise (`light_while_proving`)
        if role == "light" && !self.light_while_proving {
            if let State::Held(i) = hold::status(&self.lock)? {
                return refuse("host.refuse-proof-running", format!("the host is proving: lock held by {} (pid {}) since {}; a light run waits (host key light_while_proving = true allows it)", i.run_id, i.pid, i.started_at));
            }
        }
        if let (true, Some((hosts, host))) = (self.doctor, &self.hosts) {
            let cfg = crate::config::load(hosts).map_err(|e| Error::Batch(format!("hosts.toml for doctor: {e}")))?;
            let results = doctor::run_doctor(&cfg, Some(host), &|h| crate::runner::for_reach(&h.reach)).map_err(Error::Batch)?;
            let fails: Vec<String> = results.iter().filter(|r| r.status == "FAIL").map(|r| format!("{}: {}", r.check, r.message)).collect();
            if !fails.is_empty() {
                return refuse("host.refuse-doctor", format!("doctor fails for {host}: {}", fails.join("; ")));
            }
        }
        Ok(None)
    }
}
