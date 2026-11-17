//! Hardware, measured (engine v2 section 10): the sampler (E7), rotational and the mount of a path (10.4), the bounded probe (E6).
//!
//! Unknown is a value (V10): a figure the platform cannot give is `None` with a reason in [`Reading::unknown`]; no rule in
//! `advise` turns it into a number. Every reader takes its root as a parameter so a test can feed fixture `/proc` and `/sys` trees.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

// ---------------------------------------------------------------------------------------------- the sampler

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskBusy {
    pub device: String,
    /// share of wall time the device spent doing I/O
    pub busy_pct: Option<f64>,
}

/// One sample. `None` = unknown, and `unknown[figure]` says why.
#[derive(Debug, Clone, Default)]
pub struct Reading {
    pub load_1m: Option<f64>,
    pub cpu_busy_pct: Option<f64>,
    pub iowait_pct: Option<f64>,
    pub stall_io_some: Option<f64>,
    pub stall_io_full: Option<f64>,
    pub stall_cpu_some: Option<f64>,
    pub stall_mem_some: Option<f64>,
    pub mem_free_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub disks: Vec<DiskBusy>,
    pub unknown: BTreeMap<String, String>,
}

impl Reading {
    fn set(&mut self, name: &str, v: Result<f64, String>, slot: fn(&mut Reading) -> &mut Option<f64>) {
        match v {
            Ok(x) => *slot(self) = Some(x),
            Err(why) => {
                self.unknown.insert(name.to_string(), why);
            }
        }
    }

    /// One JSON line of `samples.jsonl`: the time, the host, the run, the steps running now and every figure (null when unknown).
    pub fn to_json_line(&self, host: &str, run_id: &str, running_steps: usize, at: &str) -> String {
        let disks: Vec<Value> = self.disks.iter().map(|d| json!({"device": d.device, "busy_pct": d.busy_pct})).collect();
        json!({
            "at": at, "host": host, "run_id": run_id, "running_steps": running_steps,
            "load_1m": self.load_1m, "cpu_busy_pct": self.cpu_busy_pct, "iowait_pct": self.iowait_pct,
            "stall_io_some": self.stall_io_some, "stall_io_full": self.stall_io_full, "stall_cpu_some": self.stall_cpu_some, "stall_mem_some": self.stall_mem_some,
            "mem_free_mb": self.mem_free_mb, "mem_total_mb": self.mem_total_mb, "disks": disks,
        })
        .to_string()
    }

    /// The first line of the file: which figures this platform does not give, and why.
    pub fn unknown_json_line(&self) -> String {
        json!({ "unknown": self.unknown }).to_string()
    }
}

enum Src {
    Linux { proc: PathBuf, sys: PathBuf },
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    Mac,
    #[allow(dead_code)]
    Nothing,
}

#[derive(Default)]
struct Prev {
    cpu: Option<(f64, f64, f64)>, // total, idle, iowait ticks
    disks: HashMap<String, f64>,
    stall: HashMap<&'static str, f64>,
    #[cfg(windows)]
    win_cpu: Option<(u64, u64, u64)>,
}

pub struct Sampler {
    src: Src,
    prev: Prev,
    first: bool,
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

impl Sampler {
    /// A Linux source rooted at `proc` and `sys` (the real ones, or a fixture).
    pub fn linux_at(proc: &Path, sys: &Path) -> Sampler {
        Sampler { src: Src::Linux { proc: proc.into(), sys: sys.into() }, prev: Prev::default(), first: true }
    }

    /// The source of this machine.
    pub fn host() -> Sampler {
        if Path::new("/proc/stat").exists() {
            return Sampler::linux_at(Path::new("/proc"), Path::new("/sys"));
        }
        #[cfg(windows)]
        return Sampler { src: Src::Windows, prev: Prev::default(), first: true };
        #[cfg(target_os = "macos")]
        return Sampler { src: Src::Mac, prev: Prev::default(), first: true };
        #[allow(unreachable_code)]
        Sampler { src: Src::Nothing, prev: Prev::default(), first: true }
    }

    /// One reading. `elapsed_s` is the time since the previous one (rates are deltas); the first reading has no delta, so its rates are unknown.
    pub fn sample(&mut self, elapsed_s: f64) -> Reading {
        let first = std::mem::replace(&mut self.first, false);
        let mut r = Reading::default();
        match &self.src {
            Src::Linux { proc, sys } => {
                let (proc, sys) = (proc.clone(), sys.clone());
                self.linux(&mut r, &proc, &sys, elapsed_s, first);
            }
            #[cfg(windows)]
            Src::Windows => self.windows(&mut r, first),
            #[cfg(target_os = "macos")]
            Src::Mac => mac(&mut r),
            Src::Nothing => {
                for k in ["load_1m", "cpu_busy_pct", "iowait_pct", "mem_free_mb", "mem_total_mb"] {
                    r.unknown.insert(k.into(), "no sampler for this platform".into());
                }
            }
        }
        r
    }

    fn linux(&mut self, r: &mut Reading, proc: &Path, sys: &Path, elapsed_s: f64, first: bool) {
        let delta = |first: bool| -> Result<(), String> { if first { Err("the first reading has no delta".into()) } else { Ok(()) } };
        r.set("load_1m", read(&proc.join("loadavg")).and_then(|t| t.split_whitespace().next().and_then(|x| x.parse().ok()).ok_or_else(|| "loadavg unreadable".to_string())), |r| &mut r.load_1m);
        match read(&proc.join("meminfo")) {
            Ok(t) => {
                let kb = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<f64>().ok());
                r.set("mem_free_mb", kb("MemAvailable:").map(|k| k / 1024.0).ok_or_else(|| "no MemAvailable in /proc/meminfo".to_string()), |r| &mut r.mem_free_mb);
                r.set("mem_total_mb", kb("MemTotal:").map(|k| k / 1024.0).ok_or_else(|| "no MemTotal in /proc/meminfo".to_string()), |r| &mut r.mem_total_mb);
            }
            Err(e) => {
                r.unknown.insert("mem_free_mb".into(), e.clone());
                r.unknown.insert("mem_total_mb".into(), e);
            }
        }
        // CPU busy and iowait: /proc/stat tick deltas
        match read(&proc.join("stat")) {
            Ok(t) => {
                let f: Vec<f64> = t.lines().next().unwrap_or("").split_whitespace().skip(1).take(8).filter_map(|x| x.parse().ok()).collect();
                if f.len() >= 5 {
                    let (total, idle, iow) = (f.iter().sum::<f64>(), f[3], f[4]);
                    let prev = self.prev.cpu.replace((total, idle, iow));
                    match (delta(first), prev) {
                        (Ok(()), Some((pt, pi, pw))) if total > pt => {
                            let dt = total - pt;
                            r.cpu_busy_pct = Some(100.0 * (dt - (idle - pi) - (iow - pw)) / dt);
                            r.iowait_pct = Some(100.0 * (iow - pw) / dt);
                        }
                        (Err(e), _) => {
                            r.unknown.insert("cpu_busy_pct".into(), e.clone());
                            r.unknown.insert("iowait_pct".into(), e);
                        }
                        _ => {
                            r.unknown.insert("cpu_busy_pct".into(), "no CPU ticks passed".into());
                            r.unknown.insert("iowait_pct".into(), "no CPU ticks passed".into());
                        }
                    }
                } else {
                    r.unknown.insert("cpu_busy_pct".into(), "/proc/stat has no cpu line".into());
                    r.unknown.insert("iowait_pct".into(), "/proc/stat has no cpu line".into());
                }
            }
            Err(e) => {
                r.unknown.insert("cpu_busy_pct".into(), e.clone());
                r.unknown.insert("iowait_pct".into(), e);
            }
        }
        // stalls: /proc/pressure/* `total=` is microseconds stalled; the share of wall time is the delta over the interval
        for (name, file, key, full, slot) in [
            ("stall_io_some", "io", "io_some", false, (|r: &mut Reading| &mut r.stall_io_some) as fn(&mut Reading) -> &mut Option<f64>),
            ("stall_io_full", "io", "io_full", true, |r: &mut Reading| &mut r.stall_io_full),
            ("stall_cpu_some", "cpu", "cpu_some", false, |r: &mut Reading| &mut r.stall_cpu_some),
            ("stall_mem_some", "memory", "mem_some", false, |r: &mut Reading| &mut r.stall_mem_some),
        ] {
            let text = match read(&proc.join("pressure").join(file)) {
                Ok(t) => t,
                Err(_) => {
                    r.unknown.insert(name.into(), "no /proc/pressure files (a kernel without pressure stall information)".into());
                    continue;
                }
            };
            let kind = if full { "full " } else { "some " };
            let total = text.lines().find_map(|l| l.strip_prefix(kind)).and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("total="))).and_then(|v| v.parse::<f64>().ok());
            let Some(total) = total else {
                r.unknown.insert(name.into(), format!("/proc/pressure/{file} has no `{}` line", kind.trim()));
                continue;
            };
            let prev = self.prev.stall.insert(key, total);
            match (delta(first), prev) {
                (Ok(()), Some(p)) if elapsed_s > 0.0 => *slot(r) = Some(100.0 * (total - p) / (elapsed_s * 1e6)),
                (Err(e), _) => {
                    r.unknown.insert(name.into(), e);
                }
                _ => {
                    r.unknown.insert(name.into(), "no time passed".into());
                }
            }
        }
        // disks: time doing I/O per whole device
        if let Ok(t) = read(&proc.join("diskstats")) {
            for l in t.lines() {
                let f: Vec<&str> = l.split_whitespace().collect();
                if f.len() < 13 {
                    continue;
                }
                let name = f[2];
                if !sys.join("block").join(name).exists() || name.starts_with("loop") || name.starts_with("ram") {
                    continue; // partitions and virtual nodes are not disks
                }
                let Ok(ticks) = f[12].parse::<f64>() else { continue };
                let prev = self.prev.disks.insert(name.to_string(), ticks);
                let busy = match prev {
                    Some(p) if !first && elapsed_s > 0.0 => Some((100.0 * (ticks - p) / (elapsed_s * 1000.0)).clamp(0.0, 100.0)),
                    _ => None,
                };
                r.disks.push(DiskBusy { device: name.to_string(), busy_pct: busy });
            }
        } else {
            r.unknown.insert("disks".into(), "no /proc/diskstats".into());
        }
    }

    #[cfg(windows)]
    fn windows(&mut self, r: &mut Reading, first: bool) {
        #[repr(C)]
        struct FileTime {
            lo: u32,
            hi: u32,
        }
        #[repr(C)]
        struct MemStatus {
            length: u32,
            load: u32,
            total_phys: u64,
            avail_phys: u64,
            total_page: u64,
            avail_page: u64,
            total_virt: u64,
            avail_virt: u64,
            avail_ext: u64,
        }
        extern "system" {
            fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
            fn GlobalMemoryStatusEx(b: *mut MemStatus) -> i32;
        }
        let q = |f: &FileTime| (u64::from(f.hi) << 32) | u64::from(f.lo);
        let (mut i, mut k, mut u) = (FileTime { lo: 0, hi: 0 }, FileTime { lo: 0, hi: 0 }, FileTime { lo: 0, hi: 0 });
        // SAFETY: three valid out-pointers to FILETIME-shaped structs.
        if unsafe { GetSystemTimes(&mut i, &mut k, &mut u) } != 0 {
            let now = (q(&i), q(&k), q(&u));
            let prev = self.prev.win_cpu.replace(now);
            match (first, prev) {
                (false, Some((pi, pk, pu))) if now.1 + now.2 > pk + pu => {
                    let total = (now.1 + now.2 - pk - pu) as f64;
                    r.cpu_busy_pct = Some(100.0 * (total - (now.0 - pi) as f64) / total); // kernel time includes idle time
                }
                _ => {
                    r.unknown.insert("cpu_busy_pct".into(), "the first reading has no delta".into());
                }
            }
        } else {
            r.unknown.insert("cpu_busy_pct".into(), "GetSystemTimes failed".into());
        }
        let mut m = MemStatus { length: std::mem::size_of::<MemStatus>() as u32, load: 0, total_phys: 0, avail_phys: 0, total_page: 0, avail_page: 0, total_virt: 0, avail_virt: 0, avail_ext: 0 };
        // SAFETY: m is a correctly sized, initialised MEMORYSTATUSEX with `length` set.
        if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
            r.mem_free_mb = Some(m.avail_phys as f64 / 1048576.0);
            r.mem_total_mb = Some(m.total_phys as f64 / 1048576.0);
        } else {
            r.unknown.insert("mem_free_mb".into(), "GlobalMemoryStatusEx failed".into());
        }
        for (k, why) in [
            ("load_1m", "Windows has no load average"),
            ("iowait_pct", "Windows does not expose I/O wait"),
            ("stall_io_some", "Windows has no pressure stall information"),
            ("stall_io_full", "Windows has no pressure stall information"),
            ("stall_cpu_some", "Windows has no pressure stall information"),
            ("stall_mem_some", "Windows has no pressure stall information"),
            ("disks", "disk busy time needs the performance-counter API (not read in this version)"),
        ] {
            r.unknown.insert(k.into(), why.into());
        }
    }
}

/// macOS, **unverified** (no host to run on): load from `sysctl`, free memory estimated from `vm_stat`; everything else unknown.
#[cfg(target_os = "macos")]
fn mac(r: &mut Reading) {
    let run = |c: &str, a: &[&str]| std::process::Command::new(c).args(a).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    match run("sysctl", &["-n", "vm.loadavg"]).and_then(|t| t.trim_matches(|c: char| c == '{' || c.is_whitespace() || c == '}').split_whitespace().next().and_then(|x| x.parse::<f64>().ok())) {
        Some(l) => r.load_1m = Some(l),
        None => {
            r.unknown.insert("load_1m".into(), "sysctl vm.loadavg failed".into());
        }
    }
    match run("sysctl", &["-n", "hw.memsize"]).and_then(|t| t.trim().parse::<f64>().ok()) {
        Some(b) => r.mem_total_mb = Some(b / 1048576.0),
        None => {
            r.unknown.insert("mem_total_mb".into(), "sysctl hw.memsize failed".into());
        }
    }
    let est = run("vm_stat", &[]).and_then(|t| {
        let page: f64 = t.lines().next()?.split("page size of").nth(1)?.trim().trim_end_matches(" bytes)").trim().parse().ok()?;
        let pages = |k: &str| t.lines().find(|l| l.starts_with(k)).and_then(|l| l.split(':').nth(1)).and_then(|v| v.trim().trim_end_matches('.').parse::<f64>().ok());
        Some((pages("Pages free")? + pages("Pages inactive")?) * page / 1048576.0)
    });
    match est {
        Some(m) => r.mem_free_mb = Some(m), // an estimate: free plus inactive pages
        None => {
            r.unknown.insert("mem_free_mb".into(), "vm_stat unreadable".into());
        }
    }
    for k in ["cpu_busy_pct", "iowait_pct", "stall_io_some", "stall_io_full", "stall_cpu_some", "stall_mem_some", "disks"] {
        r.unknown.insert(k.into(), "not exposed on macOS in this version".into());
    }
}

// ---------------------------------------------------------------------------------------------- rotational and mounts

#[derive(Debug, Clone, PartialEq)]
pub enum Rot {
    Yes,
    No,
    Unknown(String),
}

impl Rot {
    /// 1, 0, or NULL for the ledger.
    pub fn as_db(&self) -> Option<i64> {
        match self {
            Rot::Yes => Some(1),
            Rot::No => Some(0),
            Rot::Unknown(_) => None,
        }
    }
}

/// True inside the Linux subsystem on Windows (the guest's disk is a file on a Windows disk).
pub fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version").map(|t| t.to_lowercase().contains("microsoft")).unwrap_or(false)
}

fn whole_disk(block: &Path, dev: &str) -> Option<String> {
    if block.join(dev).is_dir() {
        return Some(dev.to_string());
    }
    // a partition lives inside its disk's folder: /sys/block/sda/sda1
    std::fs::read_dir(block).ok()?.flatten().find(|e| e.path().join(dev).is_dir()).map(|e| e.file_name().to_string_lossy().into_owned())
}

/// Whether `dev` (a device or partition name such as `sda1`, `nvme0n1p1`, `dm-0`) is rotational, from `<sys>/block/<disk>/queue/rotational`,
/// through `slaves/` for a stacked device. A mixed or unresolved answer, a missing file or a value that is not 0 or 1 is `Unknown(reason)`.
pub fn rotational_of(sys: &Path, dev: &str, wsl: bool) -> Rot {
    if wsl {
        return Rot::Unknown("inside the Linux subsystem on Windows the disk is a virtual file on a Windows disk; ask the Windows host".into());
    }
    rot_inner(&sys.join("block"), dev, 0)
}

fn rot_inner(block: &Path, dev: &str, depth: u8) -> Rot {
    let Some(disk) = whole_disk(block, dev) else { return Rot::Unknown(format!("device {dev} not found under {}", block.display())) };
    let slaves: Vec<String> = std::fs::read_dir(block.join(&disk).join("slaves")).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    if !slaves.is_empty() {
        if depth >= 4 {
            return Rot::Unknown("stacked devices nest too deep".into());
        }
        let all: Vec<Rot> = slaves.iter().map(|s| rot_inner(block, s, depth + 1)).collect();
        return if all.iter().all(|x| *x == Rot::Yes) {
            Rot::Yes
        } else if all.iter().all(|x| *x == Rot::No) {
            Rot::No
        } else {
            Rot::Unknown(format!("mixed or unresolved devices under {disk}"))
        };
    }
    match std::fs::read_to_string(block.join(&disk).join("queue").join("rotational")).map(|t| t.trim().to_string()) {
        Ok(v) if v == "1" => Rot::Yes,
        Ok(v) if v == "0" => Rot::No,
        Ok(v) => Rot::Unknown(format!("rotational of {disk} reads {v:?}")),
        Err(e) => Rot::Unknown(format!("rotational of {disk} unreadable: {e}")),
    }
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub majmin: String,
    pub mountpoint: String,
    pub fstype: String,
    pub source: String,
}

impl Mount {
    /// A network or virtual file system: rotational is unknown, and it is probed only on a person's word (`--path`).
    pub fn network_or_virtual(&self) -> bool {
        let t = self.fstype.as_str();
        t.starts_with("nfs") || t.starts_with("smb") || t.starts_with("fuse") || ["cifs", "9p", "virtiofs", "drvfs", "vboxsf", "glusterfs", "ceph", "lustre", "afs"].contains(&t)
    }

    /// The device name under `/dev` (`sda1`), if the source is one.
    pub fn device(&self) -> Option<String> {
        self.source.strip_prefix("/dev/").map(|d| d.rsplit('/').next().unwrap_or(d).to_string())
    }
}

/// The mount that holds `path`: the longest mount point that prefixes it, from `/proc/self/mountinfo` text.
pub fn mount_of(mountinfo: &str, path: &str) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for l in mountinfo.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        let Some(dash) = f.iter().position(|x| *x == "-") else { continue };
        if f.len() < dash + 3 || dash < 5 {
            continue;
        }
        let mp = f[4];
        let hit = mp == "/" || path == mp || path.starts_with(&format!("{}/", mp.trim_end_matches('/')));
        if hit && best.as_ref().is_none_or(|b| mp.len() >= b.mountpoint.len()) {
            best = Some(Mount { majmin: f[2].into(), mountpoint: mp.into(), fstype: f[dash + 1].into(), source: f[dash + 2].into() });
        }
    }
    best
}

/// Windows: `Get-PhysicalDisk` media type of the drive holding `path`; `Unspecified`, a storage pool or a failure is unknown.
pub fn rotational_windows(path: &str) -> Rot {
    let Some(letter) = path.chars().next().filter(|c| c.is_ascii_alphabetic() && path.chars().nth(1) == Some(':')) else {
        return Rot::Unknown("no drive letter in the path (a network or relative path)".into());
    };
    let script = format!("(Get-Partition -DriveLetter {letter} | Get-Disk | Get-PhysicalDisk | Select-Object -First 1 -ExpandProperty MediaType)");
    let out = std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output();
    match out {
        Ok(o) if o.status.success() => match String::from_utf8_lossy(&o.stdout).trim() {
            "HDD" => Rot::Yes,
            "SSD" => Rot::No,
            other => Rot::Unknown(format!("Get-PhysicalDisk media type {other:?}")),
        },
        Ok(o) => Rot::Unknown(format!("Get-PhysicalDisk failed: {}", String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or(""))),
        Err(e) => Rot::Unknown(format!("powershell not run: {e}")),
    }
}

/// Rotational of the device under `path` on this machine, with the file system type. Never inferred from timings.
pub fn rotational_of_path(path: &Path) -> (Rot, Option<String>, Option<String>) {
    #[cfg(windows)]
    {
        return (rotational_windows(&path.to_string_lossy()), None, None);
    }
    #[allow(unreachable_code)]
    {
        let p = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let Ok(mi) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return (Rot::Unknown("no /proc/self/mountinfo on this platform (macOS: not read in this version)".into()), None, None);
        };
        let Some(m) = mount_of(&mi, &p.to_string_lossy()) else { return (Rot::Unknown("no mount holds the path".into()), None, None) };
        let dev = m.device();
        let rot = if m.network_or_virtual() {
            Rot::Unknown(format!("{} is a network or virtual file system", m.fstype))
        } else if let Some(d) = &dev {
            rotational_of(Path::new("/sys"), d, is_wsl())
        } else {
            Rot::Unknown(format!("the source {:?} is not a /dev device", m.source))
        };
        (rot, Some(m.fstype), dev)
    }
}

// ---------------------------------------------------------------------------------------------- the probe

#[derive(Debug, Clone)]
pub struct CpuProbe {
    pub one_mbps: f64,
    pub all_mbps: f64,
    pub threads: usize,
}

fn hash_for(secs: f64) -> f64 {
    let buf = vec![0xA5u8; 4 * 1024 * 1024];
    let t = Instant::now();
    let mut bytes = 0u64;
    let mut h = Sha256::new();
    while t.elapsed().as_secs_f64() < secs.max(0.001) {
        h.update(&buf);
        bytes += buf.len() as u64;
    }
    std::hint::black_box(h.finalize());
    bytes as f64 / 1048576.0 / t.elapsed().as_secs_f64()
}

/// sha-256 throughput in MB/s on one thread, then on every thread; each phase takes `phase_s` seconds (the CLI gives 2.5, so 5 s at most).
pub fn probe_cpu(phase_s: f64) -> CpuProbe {
    let one = hash_for(phase_s);
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let hs: Vec<_> = (0..n).map(|_| std::thread::spawn(move || hash_for(phase_s))).collect();
    let all: f64 = hs.into_iter().filter_map(|h| h.join().ok()).sum();
    CpuProbe { one_mbps: one, all_mbps: all, threads: n }
}

#[derive(Debug, Clone)]
pub struct ProbeCfg {
    pub budget_s: f64,
    pub small_writes: usize,
    pub seq_mb: usize,
    pub files: usize,
    /// never write more than this in one disk's probe
    pub max_bytes: u64,
}

impl Default for ProbeCfg {
    fn default() -> Self {
        ProbeCfg { budget_s: 30.0, small_writes: 200, seq_mb: 256, files: 2000, max_bytes: 512 * 1024 * 1024 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DiskProbe {
    pub fsync_p50_ms: Option<f64>,
    pub fsync_p95_ms: Option<f64>,
    pub seq_write_mbps: Option<f64>,
    pub smallfile_per_s: Option<f64>,
    pub free_gb: Option<f64>,
    pub total_gb: Option<f64>,
    pub bytes_written: u64,
    pub duration_ms: u64,
    /// figure name to reason (`budget`, ...)
    pub unknown: BTreeMap<String, String>,
}

/// (free GB, total GB) of the disk holding `path`.
pub fn disk_space(path: &Path) -> (Option<f64>, Option<f64>) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        }
        let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: w is NUL-terminated; the three out-pointers are valid u64s.
        if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0 {
            return (Some(avail as f64 / 1073741824.0), Some(total as f64 / 1073741824.0));
        }
        (None, None)
    }
    #[cfg(not(windows))]
    {
        let Some(o) = std::process::Command::new("df").args(["-Pk"]).arg(path).output().ok().filter(|o| o.status.success()) else { return (None, None) };
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) else { return (None, None) };
        let c: Vec<&str> = line.split_whitespace().collect();
        let gb = |i: usize| c.get(i).and_then(|x| x.parse::<f64>().ok()).map(|k| k / 1048576.0);
        (gb(3), gb(1))
    }
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

/// Probe the disk holding `path` in a folder `.gatetrain-probe-<pid>` under it. Refused (`Err("probe.disk-low: ..")`) below `min_free_gb + 1`
/// GB free, for a link, or for a path that is no folder. Bounded by `cfg.budget_s` and `cfg.max_bytes`: a measurement that runs out is
/// `unknown: budget`. The folder is removed at the end, and a folder a crashed probe left is removed first.
pub fn probe_disk(path: &Path, cfg: &ProbeCfg, min_free_gb: f64) -> Result<DiskProbe, String> {
    use std::io::Write;
    let md = std::fs::symlink_metadata(path).map_err(|e| format!("probe.path: {}: {e}", path.display()))?;
    if md.file_type().is_symlink() {
        return Err(format!("probe.path-link: {} is a link; probe the folder it points to by its real name", path.display()));
    }
    if !md.is_dir() {
        return Err(format!("probe.path: {} is not a folder", path.display()));
    }
    let (free, total) = disk_space(path);
    if let Some(f) = free {
        if f < min_free_gb + 1.0 {
            return Err(format!("probe.disk-low: {} has {f:.1} GB free, below min_free_gb {min_free_gb} + 1 GB", path.display()));
        }
    }
    // a folder a crashed probe left (our prefix only)
    for e in std::fs::read_dir(path).map_err(|e| e.to_string())?.flatten() {
        if e.file_name().to_string_lossy().starts_with(".gatetrain-probe-") && e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let dir = path.join(format!(".gatetrain-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("probe.path: cannot create {}: {e}", dir.display()))?;
    let _guard = Cleanup(dir.clone());
    let t0 = Instant::now();
    let over = || t0.elapsed().as_secs_f64() >= cfg.budget_s;
    let mut p = DiskProbe { free_gb: free, total_gb: total, ..Default::default() };
    // 1. durable small writes (the figure that separates a spinning disk from a solid-state one)
    let mut lat: Vec<f64> = Vec::new();
    {
        let mut f = std::fs::File::create(dir.join("small")).map_err(|e| e.to_string())?;
        let block = [0x5Au8; 4096];
        for _ in 0..cfg.small_writes.max(1) {
            let t = Instant::now();
            f.write_all(&block).and_then(|_| f.sync_data()).map_err(|e| e.to_string())?;
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
            p.bytes_written += 4096;
        }
    }
    p.fsync_p50_ms = Some(pct(&mut lat.clone(), 0.5));
    p.fsync_p95_ms = Some(pct(&mut lat, 0.95));
    // 2. sequential write: 1 MB blocks, one sync_all at the end
    if over() {
        p.unknown.insert("seq_write_mbps".into(), "budget".into());
    } else {
        let mut f = std::fs::File::create(dir.join("seq")).map_err(|e| e.to_string())?;
        let block = vec![0x3Cu8; 1024 * 1024];
        let want = (cfg.seq_mb as u64 * 1048576).min(cfg.max_bytes.saturating_sub(p.bytes_written));
        let (t, mut done) = (Instant::now(), 0u64);
        let mut ran_out = false;
        while done < want {
            if over() {
                ran_out = true;
                break;
            }
            f.write_all(&block).map_err(|e| e.to_string())?;
            done += block.len() as u64;
        }
        f.sync_all().map_err(|e| e.to_string())?;
        p.bytes_written += done;
        if ran_out || done == 0 {
            p.unknown.insert("seq_write_mbps".into(), "budget".into());
        } else {
            p.seq_write_mbps = Some(done as f64 / 1048576.0 / t.elapsed().as_secs_f64().max(1e-9));
        }
    }
    // 3. small files: create, write 4 KB, close; then delete
    if over() {
        p.unknown.insert("smallfile_per_s".into(), "budget".into());
    } else {
        let sub = dir.join("files");
        std::fs::create_dir_all(&sub).map_err(|e| e.to_string())?;
        let (t, mut n) = (Instant::now(), 0usize);
        let block = [0x77u8; 4096];
        let room = (cfg.max_bytes.saturating_sub(p.bytes_written) / 4096) as usize;
        for i in 0..cfg.files.min(room) {
            if i % 50 == 0 && over() {
                break;
            }
            std::fs::write(sub.join(format!("f{i}")), block).map_err(|e| e.to_string())?;
            n += 1;
        }
        for i in 0..n {
            let _ = std::fs::remove_file(sub.join(format!("f{i}")));
        }
        p.bytes_written += n as u64 * 4096;
        if n < cfg.files.min(room) || n == 0 {
            p.unknown.insert("smallfile_per_s".into(), "budget".into());
        } else {
            p.smallfile_per_s = Some(n as f64 / t.elapsed().as_secs_f64().max(1e-9));
        }
    }
    p.duration_ms = t0.elapsed().as_millis() as u64;
    Ok(p)
}
