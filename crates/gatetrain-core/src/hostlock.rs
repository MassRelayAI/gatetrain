//! The real seams of the batch state machine: [`RealLock`] is [`HostLock`] over the holder (`hold.rs`: an OS file lock, not a
//! lock file that merely exists) and [`MemoryAdmission`] is [`Admission`] over the memory rule (`memory.rs`) and the lock.
//!
//! "Is this run still running" is read off the lock itself: a lock that can be taken has no live holder, so a chain that died
//! without `GATES_DONE` is `STALE` (the batch records it as stopped, never as running and never as red).
use crate::batch::{Admission, HostLock, LaunchRequest};
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
}

impl MemoryAdmission {
    pub fn new(lock: PathBuf, db: PathBuf, hosts: Option<(PathBuf, String)>, chain_steps: Vec<String>) -> MemoryAdmission {
        MemoryAdmission { lock, db, hosts, chain_steps, target_dirs: vec![], repo_root: None, doctor: false }
    }
}

impl Admission for MemoryAdmission {
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
            // no step list means the chain as one `(chain)` step, admitted on its own recorded peak (as `hold` does)
            let steps: Vec<String> = if self.chain_steps.is_empty() { vec!["(chain)".to_string()] } else { self.chain_steps.clone() };
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
