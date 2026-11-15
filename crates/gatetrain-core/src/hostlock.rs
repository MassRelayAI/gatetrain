//! The real seams of the batch state machine: [`RealLock`] is [`HostLock`] over the holder (`hold.rs`: an OS file lock, not a
//! lock file that merely exists) and [`MemoryAdmission`] is [`Admission`] over the memory rule (`memory.rs`) and the lock.
//!
//! "Is this run still running" is read off the lock itself: a lock that can be taken has no live holder, so a chain that died
//! without `GATES_DONE` is `STALE` (the batch records it as stopped, never as running and never as red).
use crate::batch::{Admission, HostLock, LaunchRequest};
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

/// Admission: the memory rule for the chain's steps (`chain_steps`), and for role `proof` a free lock.
pub struct MemoryAdmission {
    pub lock: PathBuf,
    pub db: PathBuf,
    /// hosts.toml and the host name; without it only the lock is checked (no number is invented).
    pub hosts: Option<(PathBuf, String)>,
    pub chain_steps: Vec<String>,
}

impl Admission for MemoryAdmission {
    fn admit(&self, role: &str, _batch: &str) -> Result<Option<String>> {
        if role == "proof" {
            if let State::Held(i) = hold::status(&self.lock)? {
                return Ok(Some(format!("host lock held by {} (pid {}) since {}", i.run_id, i.pid, i.started_at)));
            }
        }
        let (Some((hosts, host)), false) = (&self.hosts, self.chain_steps.is_empty()) else { return Ok(None) };
        let m = MemoryModel::from_hosts_toml(&std::fs::read_to_string(hosts)?, host)?;
        let hist = if self.db.exists() { Ledger::open(&self.db)?.step_memory_history()? } else { Default::default() };
        let ad = memory::admit(&m, &self.chain_steps, &hist);
        Ok(if ad.admitted { None } else { Some(ad.rule) })
    }
}
