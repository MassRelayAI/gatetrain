//! The host lock and the holder (design 4.3). Slice 2.
//!
//! The lock is a real OS file lock (`File::try_lock`, std 1.89) on `<lock>`, taken by the PARENT of the chain.
//! The holder's facts (run id, pid, boot id, start time, state) live in the sidecar `<lock>.info`, not in the
//! lock file: on Windows `LockFileEx` makes a locked file unreadable to other handles, so the lock file itself
//! carries nothing and the sidecar is always readable.
//!
//! Lock semantics per OS (documented, tested on both):
//! - Linux/macOS: `flock(2)` advisory lock on the open file description; the kernel drops it when the holder
//!   process dies, however it dies. Works across processes.
//! - Windows: `LockFileEx` exclusive lock on the whole file; mandatory for other handles; the OS drops it when the
//!   handle closes or the process dies.
//!
//! So "is the holder alive" is answered by the lock itself: a lock that can be taken has no live holder. The pid is
//! recorded for people and reports; it is never probed (no `kill -0`, no process-table grep). The boot id catches
//! the one case the lock cannot: a reboot, after which the old holder is gone for a reason other than the chain.
use crate::{Error, Result};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What the holder wrote about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub run_id: String,
    pub pid: u32,
    pub boot_id: String,
    pub started_at: String,
    /// `held` while running, `released` after a clean release. `held` with a free lock is a stale lock.
    pub state: String,
}

impl Info {
    pub fn to_json(&self) -> Value {
        json!({"run_id": self.run_id, "pid": self.pid, "boot_id": self.boot_id, "started_at": self.started_at, "state": self.state})
    }
    fn from_json(v: &Value) -> Option<Info> {
        Some(Info {
            run_id: v.get("run_id")?.as_str()?.to_string(),
            pid: v.get("pid")?.as_u64()? as u32,
            boot_id: v.get("boot_id")?.as_str()?.to_string(),
            started_at: v.get("started_at")?.as_str()?.to_string(),
            state: v.get("state")?.as_str()?.to_string(),
        })
    }
    fn unknown() -> Info {
        Info { run_id: "?".into(), pid: 0, boot_id: "?".into(), started_at: "?".into(), state: "held".into() }
    }
}

/// Why a stale lock is stale.
#[derive(Debug, Clone, PartialEq)]
pub enum Stale {
    /// The host rebooted since the holder wrote its facts: the run is `stopped`, not red (L42).
    Reboot(Info),
    /// Same boot, holder gone without a `released` line: it crashed or was killed.
    DeadHolder(Info),
}

impl Stale {
    pub fn info(&self) -> &Info {
        match self {
            Stale::Reboot(i) | Stale::DeadHolder(i) => i,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Free,
    Held(Info),
    Stale(Stale),
}

pub fn info_path(lock: &Path) -> PathBuf {
    let mut s = lock.as_os_str().to_os_string();
    s.push(".info");
    PathBuf::from(s)
}

fn read_info(lock: &Path) -> Option<Info> {
    let text = std::fs::read_to_string(info_path(lock)).ok()?;
    Info::from_json(&serde_json::from_str(&text).ok()?)
}

fn write_info(lock: &Path, i: &Info) -> Result<()> {
    let mut tmp = info_path(lock).into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, serde_json::to_vec(&i.to_json())?)?;
    std::fs::rename(&tmp, info_path(lock))?; // atomic replace: a reader never sees half a file
    Ok(())
}

/// The host's boot id. `GATETRAIN_BOOT_ID` overrides it (tests, and hosts with no probe).
/// Linux: the kernel's boot id. Windows: `win:<boot unix time>` from the tick counter (compared with a tolerance).
/// Elsewhere: `unknown`, which never reports a change.
pub fn boot_id() -> String {
    if let Ok(v) = std::env::var("GATETRAIN_BOOT_ID") {
        return v;
    }
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        return s.trim().to_string();
    }
    #[cfg(windows)]
    {
        extern "system" {
            fn GetTickCount64() -> u64;
        }
        // SAFETY: GetTickCount64 takes no arguments and cannot fail.
        let ms = unsafe { GetTickCount64() };
        return format!("win:{}", now_secs().saturating_sub(ms / 1000));
    }
    #[allow(unreachable_code)]
    "unknown".to_string()
}

/// Did the host reboot between two boot ids? Windows ids are tick-derived and jitter by a second or two.
pub fn boot_changed(a: &str, b: &str) -> bool {
    if a == "unknown" || b == "unknown" {
        return false;
    }
    let win = |s: &str| s.strip_prefix("win:").and_then(|x| x.parse::<i64>().ok());
    match (win(a), win(b)) {
        (Some(x), Some(y)) => (x - y).abs() > 120,
        _ => a != b,
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// ISO 8601 UTC, second resolution (no date library needed for one format).
pub fn now_iso() -> String {
    let s = now_secs();
    let (days, rem) = ((s / 86400) as i64, s % 86400);
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
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn open_lock_file(lock: &Path) -> Result<File> {
    if let Some(dir) = lock.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    Ok(OpenOptions::new().read(true).write(true).create(true).truncate(false).open(lock)?)
}

fn classify_prior(prior: Option<Info>) -> State {
    match prior {
        Some(i) if i.state == "held" => {
            if boot_changed(&i.boot_id, &boot_id()) {
                State::Stale(Stale::Reboot(i))
            } else {
                State::Stale(Stale::DeadHolder(i))
            }
        }
        _ => State::Free,
    }
}

/// Read the lock without taking it over: never greps ps. Takes the lock for an instant to test it, then lets go.
/// (A holder that tries in that instant sees `Busy`; `hold --wait` retries.)
pub fn status(lock: &Path) -> Result<State> {
    if !lock.exists() {
        return Ok(State::Free);
    }
    let f = open_lock_file(lock)?;
    match f.try_lock() {
        Ok(()) => {
            let st = classify_prior(read_info(lock));
            let _ = f.unlock();
            Ok(st)
        }
        Err(TryLockError::WouldBlock) => Ok(State::Held(read_info(lock).unwrap_or_else(Info::unknown))),
        Err(TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}

/// A taken lock. Dropping it releases the OS lock; call `release` first for the clean `released` line.
pub struct Lock {
    file: File,
    path: PathBuf,
    pub info: Info,
    /// What was found there before we took it (a stale lock taken over), for the decision log.
    pub took_over: Option<Stale>,
}

pub enum Acquire {
    Got(Lock),
    /// Someone live holds it.
    Busy(Info),
}

/// Try once. A lock whose holder died is taken over, and `took_over` says why.
pub fn try_acquire(lock: &Path, run_id: &str) -> Result<Acquire> {
    let f = open_lock_file(lock)?;
    match f.try_lock() {
        Ok(()) => {
            let took_over = match classify_prior(read_info(lock)) {
                State::Stale(s) => Some(s),
                _ => None,
            };
            let info = Info { run_id: run_id.to_string(), pid: std::process::id(), boot_id: boot_id(), started_at: now_iso(), state: "held".into() };
            write_info(lock, &info)?;
            Ok(Acquire::Got(Lock { file: f, path: lock.to_path_buf(), info, took_over }))
        }
        Err(TryLockError::WouldBlock) => {
            // The holder writes its facts just after locking; give it a moment.
            for _ in 0..20 {
                if let Some(i) = read_info(lock) {
                    if i.state == "held" {
                        return Ok(Acquire::Busy(i));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Ok(Acquire::Busy(Info::unknown()))
        }
        Err(TryLockError::Error(e)) => Err(Error::Io(e)),
    }
}

impl Lock {
    /// Mark the sidecar `released`, then let go of the OS lock.
    pub fn release(self) -> Result<()> {
        let mut i = self.info.clone();
        i.state = "released".into();
        write_info(&self.path, &i)?;
        self.file.unlock()?;
        Ok(())
    }
}

/// The host's parallelism, from `hosts.toml` only (design 4.2). No number is written in code.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCapacity {
    pub parallel: Option<u64>,
    pub cores: Option<u64>,
    pub lock_file: Option<String>,
}

/// `GATETRAIN_PARALLEL` = `max_parallel_steps`, lowered to the cores left after `reserve_cores` when `cores` is given
/// (never below 1). `GATETRAIN_CORES` = `cores - reserve_cores`. Absent keys export nothing.
pub fn host_capacity(hosts_toml: &str, host: &str) -> Result<HostCapacity> {
    let v: toml::Value = hosts_toml.parse().map_err(|e: toml::de::Error| Error::Canon(format!("hosts.toml: {e}")))?;
    let h = v
        .get("hosts")
        .and_then(|h| h.as_array())
        .and_then(|a| a.iter().find(|h| h.get("name").and_then(|n| n.as_str()) == Some(host)))
        .ok_or_else(|| Error::Canon(format!("hosts.toml: no host named {host:?}")))?;
    let int = |k: &str| h.get(k).and_then(|x| x.as_integer()).map(|x| x.max(0) as u64);
    let cores = int("cores").map(|c| c.saturating_sub(int("reserve_cores").unwrap_or(0)).max(1));
    let parallel = match (int("max_parallel_steps"), cores) {
        (Some(p), Some(c)) => Some(p.min(c).max(1)),
        (Some(p), None) => Some(p.max(1)),
        (None, _) => None,
    };
    Ok(HostCapacity { parallel, cores, lock_file: h.get("lock_file").and_then(|x| x.as_str()).map(String::from) })
}
