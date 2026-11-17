//! `gatetrain probe` (engine v2 E6, section 10.1): a short, bounded benchmark of this host and its configured disks, stored in the
//! ledger. Refused while a proof holds the host (exit 75); there is no override. `--discover` lists mounted file systems that are not in
//! hosts.toml and writes nothing.
use anyhow::{Context, Result};
use clap::Args;
use gatetrain_core::hold::{self, State};
use gatetrain_core::hw::{self, ProbeCfg, Rot};
use gatetrain_core::ledger::{DiskProbeRow, HostProbeRow};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Args)]
pub struct ProbeArgs {
    /// hosts.toml: the configured disks of --host are probed
    #[arg(long)]
    config: Option<PathBuf>,
    /// the host's name in hosts.toml (default: this machine's name)
    #[arg(long)]
    host: Option<String>,
    /// probe this one folder on a person's word (also a network share, which is never probed otherwise)
    #[arg(long)]
    path: Option<PathBuf>,
    /// the most seconds the whole probe may take (each measurement that runs out is stored as `unknown: budget`)
    #[arg(long, default_value_t = 30.0)]
    budget_s: f64,
    /// list mounted file systems that are not in hosts.toml (size, type, rotational) and write nothing
    #[arg(long)]
    discover: bool,
    /// the ledger the probe is stored in
    #[arg(long, default_value = "gatetrain.db")]
    db: PathBuf,
    /// the host lock file (default: lock_file of --host in --config); the probe is refused while it is held
    #[arg(long)]
    lock: Option<PathBuf>,
    /// tuning: durable small writes per disk (default 200)
    #[arg(long, default_value_t = 200)]
    small_writes: usize,
    /// tuning: sequential write in MB per disk (default 256)
    #[arg(long, default_value_t = 256)]
    seq_mb: usize,
    /// tuning: small files per disk (default 2000)
    #[arg(long, default_value_t = 2000)]
    files: usize,
}

fn rot_text(r: &Rot) -> String {
    match r {
        Rot::Yes => "yes".into(),
        Rot::No => "no".into(),
        Rot::Unknown(why) => format!("unknown ({why})"),
    }
}

pub fn run(a: ProbeArgs) -> Result<ExitCode> {
    let cfg = match &a.config {
        Some(p) => Some(gatetrain_core::config::load(p).map_err(|e| anyhow::anyhow!("config error: {e}"))?),
        None => None,
    };
    let host_name = a.host.clone().unwrap_or_else(gatetrain_core::chain::host_name);
    let host_cfg = cfg.as_ref().and_then(|c| a.host.as_deref().and_then(|h| c.host(h)));
    if a.config.is_some() && a.host.is_some() && host_cfg.is_none() {
        eprintln!("probe: no host {host_name} in the config");
        return Ok(ExitCode::from(2));
    }
    if a.discover {
        return discover(host_cfg.map(|h| h.disks.iter().map(|d| d.path.clone()).collect()).unwrap_or_default());
    }
    // never during a proof: the lock is held, or the ledger shows a live run on this host
    let lock = a.lock.clone().or_else(|| host_cfg.and_then(|h| h.lock_file.clone()).map(PathBuf::from));
    if let Some(l) = &lock {
        if let State::Held(info) = hold::status(l)? {
            eprintln!("probe.host-busy: the host lock {} is held by run {}; a probe never runs during a proof (there is no override)", l.display(), info.run_id);
            return Ok(ExitCode::from(75));
        }
    }
    if a.db.exists() {
        if let Ok(l) = gatetrain_core::ledger::Ledger::open(&a.db) {
            let live: i64 = l.conn.query_row("SELECT COUNT(*) FROM run_current WHERE host=? AND outcome='running'", [&host_name], |r| r.get(0)).unwrap_or(0);
            if live > 0 {
                eprintln!("probe.host-busy: the ledger shows {live} running run(s) on {host_name}; a probe never runs during a proof");
                return Ok(ExitCode::from(75));
            }
        }
    }
    // which disks
    let mut disks: Vec<(PathBuf, f64)> = Vec::new();
    if let Some(p) = &a.path {
        let min = host_cfg.and_then(|h| h.disks.iter().find(|d| Path::new(&d.path) == p.as_path())).map_or(0.0, |d| d.min_free_gb.f());
        disks.push((p.clone(), min));
    } else if let Some(h) = host_cfg {
        disks.extend(h.disks.iter().map(|d| (PathBuf::from(&d.path), d.min_free_gb.f())));
    }
    let at = hold::now_iso();
    let started = std::time::Instant::now();
    // the host: cores, memory, sha-256 throughput on one thread and on all of them (5 s at most)
    let phase = (a.budget_s / 4.0).clamp(0.05, 2.5);
    let cpu = hw::probe_cpu(phase);
    let mem_gb = gatetrain_core::memory::detect_total_gb();
    let wsl = hw::is_wsl();
    let mut unknown = serde_json::Map::new();
    if mem_gb.is_none() {
        unknown.insert("mem_total_mb".into(), "no source on this platform".into());
    }
    if !wsl {
        unknown.insert("virt".into(), "virtualisation is detected only for the Linux subsystem on Windows".into());
    }
    let hrow = HostProbeRow {
        host: host_name.clone(),
        at: at.clone(),
        os: Some(std::env::consts::OS.into()),
        arch: Some(std::env::consts::ARCH.into()),
        virt: wsl.then(|| "wsl".to_string()),
        cores: Some(cpu.threads as i64),
        mem_total_mb: mem_gb.map(|g| g * 1024.0),
        cpu_one_mbps: Some(cpu.one_mbps),
        cpu_all_mbps: Some(cpu.all_mbps),
        host_class: None,
        tool_versions: None,
        duration_ms: Some(started.elapsed().as_millis() as i64),
        unknown: Some(serde_json::Value::Object(unknown).to_string()),
    };
    println!("host {host_name}: {}/{} {} threads, {} MB memory; sha-256 {:.0} MB/s on one thread, {:.0} MB/s on all", std::env::consts::OS, std::env::consts::ARCH, cpu.threads, mem_gb.map_or("unknown".into(), |g| format!("{:.0}", g * 1024.0)), cpu.one_mbps, cpu.all_mbps);
    let l = crate::cmd_runs::open_for_write(&a.db.to_string_lossy())?;
    l.record_host_probe(&hrow)?;
    let mut refused = 0;
    let each = ((a.budget_s - started.elapsed().as_secs_f64()) / disks.len().max(1) as f64).max(1.0);
    for (path, min_free) in &disks {
        let cfgp = ProbeCfg { budget_s: each, small_writes: a.small_writes, seq_mb: a.seq_mb, files: a.files, max_bytes: 512 * 1024 * 1024 };
        match hw::probe_disk(path, &cfgp, *min_free) {
            Err(why) => {
                refused += 1;
                eprintln!("disk {}: refused: {why}", path.display());
            }
            Ok(p) => {
                let (rot, fs, dev) = hw::rotational_of_path(path);
                let f = |x: Option<f64>, u: &str| x.map_or("unknown".to_string(), |v| format!("{v:.1} {u}"));
                println!(
                    "disk {}: durable write p50 {} p95 {}; sequential {}; small files {}; rotational {}; free {} of {}{}",
                    path.display(),
                    f(p.fsync_p50_ms, "ms"),
                    f(p.fsync_p95_ms, "ms"),
                    f(p.seq_write_mbps, "MB/s"),
                    p.smallfile_per_s.map_or("unknown".into(), |v| format!("{v:.0} files/s")),
                    rot_text(&rot),
                    f(p.free_gb, "GB"),
                    f(p.total_gb, "GB"),
                    if p.unknown.is_empty() { String::new() } else { format!("; unknown: {}", p.unknown.iter().map(|(k, v)| format!("{k} ({v})")).collect::<Vec<_>>().join(", ")) },
                );
                let mut unknown = p.unknown.clone();
                if let Rot::Unknown(w) = &rot {
                    unknown.insert("rotational".into(), w.clone());
                }
                l.record_disk_probe(&DiskProbeRow {
                    host: host_name.clone(),
                    at: at.clone(),
                    path: path.to_string_lossy().into_owned(),
                    device: dev,
                    fs_type: fs,
                    rotational: rot.as_db(),
                    rotational_source: Some(match &rot {
                        Rot::Unknown(w) => format!("unknown: {w}"),
                        _ => if cfg!(windows) { "Get-PhysicalDisk".to_string() } else { "/sys/block/*/queue/rotational".to_string() },
                    }),
                    fsync_p50_ms: p.fsync_p50_ms,
                    fsync_p95_ms: p.fsync_p95_ms,
                    seq_write_mbps: p.seq_write_mbps,
                    smallfile_per_s: p.smallfile_per_s,
                    free_gb: p.free_gb,
                    total_gb: p.total_gb,
                    bytes_written: Some(p.bytes_written as i64),
                    duration_ms: Some(p.duration_ms as i64),
                    unknown: Some(serde_json::to_string(&unknown).context("unknown")?),
                })?;
            }
        }
    }
    if disks.is_empty() {
        println!("no disk to probe: give --path, or --config and --host with [[hosts.disks]]");
    }
    Ok(ExitCode::from(u8::from(refused > 0)))
}

/// Mounted file systems that are not in hosts.toml. Writes nothing.
fn discover(configured: Vec<String>) -> Result<ExitCode> {
    let known = |p: &str| configured.iter().any(|c| p == c || p.starts_with(&format!("{}/", c.trim_end_matches('/'))));
    let mut n = 0;
    #[cfg(windows)]
    {
        for letter in 'A'..='Z' {
            let root = format!("{letter}:\\");
            if Path::new(&root).exists() && !known(&root) {
                let (free, total) = hw::disk_space(Path::new(&root));
                println!("drive {root} size {} GB free {} GB rotational {}", total.map_or("unknown".into(), |v| format!("{v:.0}")), free.map_or("unknown".into(), |v| format!("{v:.0}")), rot_text(&hw::rotational_windows(&root)));
                n += 1;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let mi = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        let wsl = hw::is_wsl();
        for l in mi.lines() {
            let f: Vec<&str> = l.split_whitespace().collect();
            let Some(dash) = f.iter().position(|x| *x == "-") else { continue };
            if f.len() < dash + 3 || !f[dash + 2].starts_with("/dev/") {
                continue;
            }
            let mp = f[4];
            if known(mp) {
                continue;
            }
            let m = hw::Mount { majmin: f[2].into(), mountpoint: mp.into(), fstype: f[dash + 1].into(), source: f[dash + 2].into() };
            let (_, total) = hw::disk_space(Path::new(mp));
            let rot = m.device().map(|d| hw::rotational_of(Path::new("/sys"), &d, wsl)).unwrap_or(Rot::Unknown("no device".into()));
            println!("mount {mp} {} size {} GB rotational {}", m.fstype, total.map_or("unknown".into(), |v| format!("{v:.0}")), rot_text(&rot));
            n += 1;
        }
    }
    if n == 0 {
        println!("no mounted file system outside hosts.toml");
    }
    Ok(ExitCode::SUCCESS)
}
