//! `gatetrain hold` and `gatetrain running` (design 4.3): the holder is the PARENT of the chain.
use anyhow::{bail, Context, Result};
use gatetrain_core::declog::{self, NewEntry};
use gatetrain_core::disk;
use gatetrain_core::hold::{self, Acquire, Info, Stale, State};
use gatetrain_core::ledger::Ledger;
use gatetrain_core::memory::{self, MemoryModel};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Exit code of a refused hold (EX_TEMPFAIL): the lock is held by someone live.
pub const EXIT_REFUSED: u8 = 75;
/// Exit code of a hold refused by the memory rule.
pub const EXIT_MEMORY: u8 = 76;
/// Exit code of a hold refused by the disk rule.
pub const EXIT_DISK: u8 = 77;
/// Exit code of a hold refused because the working directory is not the expected repository root.
pub const EXIT_CWD: u8 = 78;

pub struct HoldArgs {
    pub run_id: Option<String>,
    pub lock: Option<PathBuf>,
    pub db: PathBuf,
    pub hosts: Option<PathBuf>,
    pub host: Option<String>,
    pub wait_secs: u64,
    pub log: Option<PathBuf>,
    /// Step names the chain would run; their predicted memory is admitted against the host before the lock is taken.
    pub steps: Vec<String>,
    /// The repository root the chain runs in (pinned as the child's working directory).
    pub repo_root: Option<PathBuf>,
    /// Build folders the chain writes to (disk admission before, size recorded after).
    pub target_dir: Vec<PathBuf>,
    /// sampler interval in seconds (0 = off); it writes `<log>.samples.jsonl`, so it needs `log`
    pub samples: u64,
    pub cmd: Vec<String>,
}

/// The holder's sampler (engine v2 E7): the loop that already polls the child drives it; no thread.
struct Sampling {
    s: gatetrain_core::hw::Sampler,
    last: Instant,
    every: Duration,
    file: PathBuf,
    host: String,
    run_id: String,
    header: bool,
}

impl Sampling {
    fn maybe(&mut self) {
        if self.last.elapsed() < self.every {
            return;
        }
        let elapsed = self.last.elapsed().as_secs_f64();
        self.last = Instant::now();
        let r = self.s.sample(elapsed);
        let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) else { return };
        if !self.header {
            let _ = writeln!(f, "{}", r.unknown_json_line());
            self.header = true;
        }
        // the holder does not know which steps run: that figure is unknown, not zero
        let _ = writeln!(f, "{}", r.to_json_line(&self.host, &self.run_id, None, &hold::now_iso()));
    }
}

fn resolve(lock: &Option<PathBuf>, hosts: &Option<PathBuf>, host: &Option<String>) -> Result<(PathBuf, Option<hold::HostCapacity>)> {
    let cap = match (hosts, host) {
        (Some(h), Some(n)) => {
            let text = std::fs::read_to_string(h).with_context(|| format!("reading {}", h.display()))?;
            Some(hold::host_capacity(&text, n)?)
        }
        (None, None) => None,
        _ => bail!("--hosts and --host go together"),
    };
    let path = match (lock, cap.as_ref().and_then(|c| c.lock_file.clone())) {
        (Some(l), _) => l.clone(),
        (None, Some(l)) => PathBuf::from(l),
        (None, None) => bail!("no lock file: pass --lock, or --hosts and --host with a lock_file"),
    };
    Ok((path, cap))
}

pub fn status(lock: &Option<PathBuf>, hosts: &Option<PathBuf>, host: &Option<String>) -> Result<ExitCode> {
    let (path, _) = resolve(lock, hosts, host)?;
    Ok(match hold::status(&path)? {
        State::Free => {
            println!("FREE");
            ExitCode::from(1)
        }
        State::Held(i) => {
            println!("HELD {} pid {} since {}", i.run_id, i.pid, i.started_at);
            ExitCode::SUCCESS
        }
        State::Stale(Stale::Reboot(i)) => {
            println!("STOPPED {} reboot (boot id {} -> {}), started {}", i.run_id, i.boot_id, hold::boot_id(), i.started_at);
            ExitCode::from(1)
        }
        State::Stale(Stale::DeadHolder(i)) => {
            println!("STALE {} pid {} gone without release, started {}", i.run_id, i.pid, i.started_at);
            ExitCode::from(1)
        }
    })
}

fn entry(kind: &str, run: &str, outcome: &str, reason: String, evidence: serde_json::Value) -> NewEntry {
    NewEntry {
        at: hold::now_iso(),
        actor: "gatetrain-hold".into(),
        kind: kind.into(),
        subject: Some(run.into()),
        outcome: outcome.into(),
        reason,
        evidence: Some(evidence),
        ..Default::default()
    }
}

/// The log is shared by concurrent holders; a busy database is retried, never skipped.
fn append(db: &Path, e: &NewEntry) -> Result<()> {
    let mut last = None;
    for _ in 0..100 {
        let r = Ledger::open(db).and_then(|mut l| declog::append(&mut l, e));
        match r {
            Ok(_) => return Ok(()),
            Err(gatetrain_core::Error::Sqlite(x)) => {
                last = Some(x);
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(x) => return Err(x.into()),
        }
    }
    bail!("decision log busy: {}", last.map(|e| e.to_string()).unwrap_or_default())
}

fn say(log: &Option<PathBuf>, line: &str) -> Result<()> {
    println!("{line}");
    std::io::stdout().flush().ok();
    if let Some(p) = log {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).with_context(|| format!("holder log {}", p.display()))?;
        writeln!(f, "{line}")?;
    }
    Ok(())
}

fn evidence(i: &Info, lock: &Path) -> serde_json::Value {
    json!({"pid": i.pid, "boot_id": i.boot_id, "started_at": i.started_at, "lock": lock.display().to_string()})
}

pub fn run(a: HoldArgs) -> Result<ExitCode> {
    let run_id = a.run_id.clone().context("hold needs a run id")?;
    if a.cmd.is_empty() {
        bail!("hold needs a command after `--`");
    }
    let (lock_path, cap) = resolve(&a.lock, &a.hosts, &a.host)?;
    {
        // A fresh ledger file is brought to the current version; a newer one is refused by open().
        let mut l = Ledger::open(&a.db).with_context(|| format!("opening {}", a.db.display()))?;
        l.migrate()?;
    }
    if let Some(root) = &a.repo_root {
        // the chain runs where it was told to, or not at all (a build that fell back to the wrong folder filled a disk once, L70)
        if let Some(why) = gatetrain_core::hostlock::repo_root_problem(root) {
            let ev = json!({"repo_root": root.display().to_string(), "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(), "rule": "the working directory must be the top level of the expected repository"});
            append(&a.db, &entry("host.refuse-cwd", &run_id, "refused", why.clone(), ev))?;
            say(&a.log, &format!("REFUSED-CWD {run_id} {why}"))?;
            return Ok(ExitCode::from(EXIT_CWD));
        }
    }
    let model = match (&a.hosts, &a.host) {
        (Some(h), Some(n)) => Some(MemoryModel::from_hosts_toml(&std::fs::read_to_string(h)?, n)?),
        _ => None,
    };
    // a hold without a step list is admitted as one `(chain)`: its own recorded peak is the prediction (the holder records it after every run)
    let steps: Vec<String> = if a.steps.is_empty() { vec!["(chain)".to_string()] } else { a.steps.clone() };
    if let Some(m) = &model {
        let hist = Ledger::open(&a.db)?.step_memory_history()?;
        let ad = memory::admit(m, &steps, &hist);
        say(&a.log, &format!("MEMORY {}", ad.rule))?;
        if !ad.admitted {
            let cheap = memory::cheap_set(m, &steps, &hist);
            let ev = json!({"predicted_mb": (ad.predicted_gb * 1024.0).round() as i64, "available_mb": (ad.available_gb * 1024.0).round() as i64, "unpredicted": ad.unpredicted, "cheap_set": cheap, "rule": "predicted <= memory_gb - memory_reserve_gb"});
            append(&a.db, &entry("host.refuse-memory", &run_id, "refused", ad.rule.clone(), ev))?;
            say(&a.log, &format!("REFUSED-MEMORY {run_id} predicted {:.1} GB > available {:.1} GB", ad.predicted_gb, ad.available_gb))?;
            say(&a.log, &format!("CHEAP-SET {}", cheap.join(",")))?;
            return Ok(ExitCode::from(EXIT_MEMORY));
        }
    }
    if let (Some(h), Some(n)) = (&a.hosts, &a.host) {
        // both sides are canonicalised before matching: a relative spelling, a link or a short name must not slip past the rule
        let disks: Vec<disk::DiskRule> = disk::disks_from_hosts_toml(&std::fs::read_to_string(h)?, n)?.into_iter().map(|d| disk::DiskRule { path: disk::canon(&d.path), ..d }).collect();
        if !disks.is_empty() {
            let hist = Ledger::open(&a.db)?.step_memory_history()?;
            for t in &a.target_dir {
                let tc = disk::canon(&t.to_string_lossy());
                let key = disk::step_key(&tc);
                let predicted_mb = memory::typical_mb(hist.get(&key).map(Vec::as_slice).unwrap_or(&[]));
                let ad = disk::admit(&disks, &tc, predicted_mb, &disk::free_gb);
                say(&a.log, &format!("DISK {}", ad.rule))?;
                if !ad.admitted {
                    let ev = json!({"target_dir": t.display().to_string(), "disk": ad.disk, "free_mb": ad.free_gb.map(|f| (f * 1024.0).round() as i64), "predicted_mb": (ad.predicted_gb * 1024.0).round() as i64, "min_free_mb": (ad.min_free_gb * 1024.0).round() as i64, "rule": "free - predicted target size >= min_free_gb"});
                    append(&a.db, &entry("host.refuse-disk", &run_id, "refused", ad.rule.clone(), ev))?;
                    say(&a.log, &format!("REFUSED-DISK {run_id} {}", ad.rule))?;
                    return Ok(ExitCode::from(EXIT_DISK));
                }
            }
        }
    }
    let started = Instant::now();
    let lock = loop {
        match hold::try_acquire_for(&lock_path, &run_id, &a.target_dir.iter().map(|t| disk::canon(&t.to_string_lossy())).collect::<Vec<_>>())? {
            Acquire::Got(l) => break l,
            Acquire::Busy(i) => {
                if started.elapsed() >= Duration::from_secs(a.wait_secs) {
                    let line = format!("REFUSED {run_id} held by {} pid {} since {}", i.run_id, i.pid, i.started_at);
                    append(&a.db, &entry("host.refuse", &run_id, "refused", format!("lock held by {}", i.run_id), evidence(&i, &lock_path)))?;
                    say(&a.log, &line)?;
                    return Ok(ExitCode::from(EXIT_REFUSED));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };
    if let Some(s) = &lock.took_over {
        let old = s.info();
        let e = match s {
            Stale::Reboot(_) => entry("run.stopped", &old.run_id, "stopped", format!("host rebooted; lock of {} was left held (not a red)", old.run_id), evidence(old, &lock_path)),
            Stale::DeadHolder(_) => entry("host.takeover", &old.run_id, "taken-over", format!("holder pid {} gone without release", old.pid), evidence(old, &lock_path)),
        };
        append(&a.db, &e)?;
    }
    append(&a.db, &entry("host.hold", &run_id, "held", format!("holder pid {} took the lock", lock.info.pid), evidence(&lock.info, &lock_path)))?;
    say(&a.log, &format!("HOLDS {run_id}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        ctrlc::set_handler(move || s.store(true, Ordering::SeqCst)).context("installing the signal handler")?;
    }
    let mut c = Command::new(&a.cmd[0]);
    c.args(&a.cmd[1..]);
    if let Some(root) = &a.repo_root {
        c.current_dir(root).env("GATETRAIN_REPO_ROOT", root);
    }
    // Parallelism is min(core-based, memory-based); both come from hosts.toml.
    let par = match (cap.as_ref().and_then(|c| c.parallel), &model) {
        (Some(p), Some(m)) => {
            let x = memory::parallelism(m, p);
            if let Some(j) = x.cargo_jobs {
                c.env("CARGO_BUILD_JOBS", j.to_string());
            }
            Some(x.parallel)
        }
        (p, _) => p,
    };
    if let Some(p) = par {
        c.env("GATETRAIN_PARALLEL", p.to_string());
    }
    if let Some(n) = cap.as_ref().and_then(|c| c.cores) {
        c.env("GATETRAIN_CORES", n.to_string());
    }
    c.env("GATETRAIN_RUN_ID", &run_id);
    let mut sampling = match (a.samples, &a.log) {
        (n, Some(log)) if n > 0 => {
            c.env("GATETRAIN_SAMPLED", "1");
            let mut file = log.clone().into_os_string();
            file.push(".samples.jsonl");
            let mut s = gatetrain_core::hw::Sampler::host();
            s.sample(0.0); // the baseline the first delta is taken against
            Some(Sampling { s, last: Instant::now(), every: Duration::from_secs(n), file: PathBuf::from(file), host: a.host.clone().unwrap_or_else(gatetrain_core::chain::host_name), run_id: run_id.clone(), header: false })
        }
        _ => None,
    };
    let mut peak: Option<(f64, &'static str)> = None;
    let rc: i32 = match c.spawn() {
        Err(e) => {
            eprintln!("gatetrain: cannot start {:?}: {e}", a.cmd[0]);
            127
        }
        Ok(mut child) => {
            let mut meter = Meter::start(child.id());
            let mut last_sample = Instant::now();
            let rc = loop {
                if stop.load(Ordering::SeqCst) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break 130;
                }
                match child.try_wait()? {
                    Some(st) => break exit_code(st),
                    None => std::thread::sleep(Duration::from_millis(50)),
                }
                if let Some(s) = sampling.as_mut() {
                    s.maybe();
                }
                if last_sample.elapsed() >= Duration::from_secs(2) {
                    meter.sample();
                    last_sample = Instant::now();
                }
            };
            peak = meter.finish(&child);
            rc
        }
    };
    if let Some(s) = &sampling {
        if let Ok(text) = std::fs::read_to_string(&s.file) {
            let _ = Ledger::open(&a.db).and_then(|l| l.record_samples_for(&text, Some(&run_id)));
        }
    }
    if let Some((mb, source)) = peak {
        let _ = Ledger::open(&a.db).and_then(|l| l.record_step_peak(&run_id, "(chain)", mb, source, &hold::now_iso()));
        let _ = say(&a.log, &format!("PEAK {run_id} {mb:.0} MB ({source})"));
    }
    for t in &a.target_dir {
        // the size this folder reached is the prediction of the next run (the median of what is recorded)
        let mb = disk::dir_size_mb(t);
        let _ = Ledger::open(&a.db).and_then(|l| l.record_step_peak(&run_id, &disk::step_key(&disk::canon(&t.to_string_lossy())), mb, "target-size", &hold::now_iso()));
        let _ = say(&a.log, &format!("TARGET {run_id} {} {mb:.0} MB", t.display()));
    }
    let pid = lock.info.pid;
    lock.release()?; // sidecar says `released`, then the OS lock goes
    let how = if stop.load(Ordering::SeqCst) { "signal" } else { "child exit" };
    let logged = append(&a.db, &entry("host.release", &run_id, "released", format!("{how}, rc={rc}"), json!({"rc": rc, "pid": pid, "lock": lock_path.display().to_string()})));
    say(&a.log, &format!("RELEASED {run_id} rc={rc}"))?;
    logged?;
    Ok(ExitCode::from(rc.clamp(0, 255) as u8))
}

#[cfg(unix)]
fn exit_code(st: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(1))
}
#[cfg(not(unix))]
fn exit_code(st: std::process::ExitStatus) -> i32 {
    st.code().unwrap_or(1)
}

/// Peak memory of the child, where it can be measured: the cgroup `memory.peak` on Linux when it can be reset for this run,
/// else the largest sum of RSS over the process tree seen by the sampler; the peak working set of the child on Windows.
struct Meter {
    pid: u32,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    max_mb: f64,
    #[cfg(target_os = "linux")]
    cgroup: Option<PathBuf>,
}

impl Meter {
    fn start(pid: u32) -> Meter {
        #[cfg(target_os = "linux")]
        {
            // Writing to memory.peak resets it (kernel 6.12+); when that is refused the file would carry older history, so it is not used.
            let cgroup = memory::cgroup_peak_path().filter(|p| std::fs::write(p, "0").is_ok());
            Meter { pid, max_mb: 0.0, cgroup }
        }
        #[cfg(not(target_os = "linux"))]
        Meter { pid, max_mb: 0.0 }
    }
    fn sample(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.max_mb = self.max_mb.max(memory::tree_rss_mb(self.pid));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = self.pid;
    }
    fn finish(mut self, child: &std::process::Child) -> Option<(f64, &'static str)> {
        #[cfg(target_os = "linux")]
        {
            let _ = child;
            if let Some(b) = self.cgroup.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| t.trim().parse::<f64>().ok()) {
                return Some((b / 1048576.0, "cgroup"));
            }
            self.sample();
            return (self.max_mb > 0.0).then_some((self.max_mb, "rss-sampled"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            let _ = &mut self;
            return memory::peak_working_set_mb(child.as_raw_handle()).map(|mb| (mb, "working-set"));
        }
        #[allow(unreachable_code)]
        {
            let _ = (child, &mut self);
            None
        }
    }
}

/// `gatetrain memory record`: store one step peak (a chain reports its own steps; the holder only sees the whole chain).
pub fn memory_record(db: &Path, run: &str, step: &str, peak_mb: f64, source: &str) -> Result<ExitCode> {
    let mut l = Ledger::open(db).with_context(|| format!("opening {}", db.display()))?;
    l.migrate()?;
    l.record_step_peak(run, step, peak_mb, source, &hold::now_iso())?;
    println!("recorded {step} {peak_mb:.0} MB ({source}) for {run}");
    Ok(ExitCode::SUCCESS)
}

/// `gatetrain memory admit`: the same rule `hold --steps` applies, without launching anything.
pub fn memory_admit(db: &Path, hosts: &Path, host: &str, steps: &[String]) -> Result<ExitCode> {
    let m = MemoryModel::from_hosts_toml(&std::fs::read_to_string(hosts)?, host)?;
    let hist = if db.exists() { Ledger::open(db)?.step_memory_history()? } else { Default::default() };
    let ad = memory::admit(&m, steps, &hist);
    println!("{}", ad.rule);
    if ad.admitted {
        return Ok(ExitCode::SUCCESS);
    }
    println!("CHEAP-SET {}", memory::cheap_set(&m, steps, &hist).join(","));
    Ok(ExitCode::from(EXIT_MEMORY))
}
