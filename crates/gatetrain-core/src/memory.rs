//! Memory in the host model (design 4.2, GT-RS2 addition). Four jobs:
//! 1. what the host has: `memory_gb` (or detected), minus `memory_reserve_gb` for the owner's own apps, capped by a WSL VM limit;
//! 2. what a step typically needs: the median of its recorded peaks (`ledger::step_memory`), like the duration medians;
//! 3. admission: the predicted memory of what would run against what is available;
//! 4. parallelism: min(core-based, memory-based), plus a suggested `CARGO_BUILD_JOBS`.
//!
//! Every number comes from `hosts.toml` or from history. A step with no history counts `step_default_gb` when the host sets it
//! and is listed as unpredicted either way; no constant is written in code.
use crate::{Error, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryModel {
    /// True when `hosts.toml` sets any memory key for the host; with none, memory is simply not checked (nothing is invented).
    pub configured: bool,
    /// Total memory of the machine or VM; `None` when it was neither configured nor detectable.
    pub total_gb: Option<f64>,
    /// A WSL VM cap (`.wslconfig` memory or `wsl_memory_gb`), when the host is a WSL guest.
    pub vm_cap_gb: Option<f64>,
    pub reserve_gb: f64,
    pub per_step_gb: Option<f64>,
    pub per_cargo_job_gb: Option<f64>,
    pub step_default_gb: Option<f64>,
}

impl MemoryModel {
    pub fn from_hosts_toml(text: &str, host: &str) -> Result<MemoryModel> {
        let v: toml::Value = text.parse().map_err(|e: toml::de::Error| Error::Canon(format!("hosts.toml: {e}")))?;
        let h = v
            .get("hosts")
            .and_then(|h| h.as_array())
            .and_then(|a| a.iter().find(|h| h.get("name").and_then(|n| n.as_str()) == Some(host)))
            .ok_or_else(|| Error::Canon(format!("hosts.toml: no host named {host:?}")))?;
        let f = |k: &str| h.get(k).and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)));
        let wsl = h.get("wsl").and_then(|x| x.as_bool()).unwrap_or(false);
        let configured = ["memory_gb", "memory_reserve_gb", "wsl_memory_gb", "memory_per_step_gb", "memory_per_cargo_job_gb", "step_default_gb"].iter().any(|k| h.get(*k).is_some());
        let vm_cap_gb = f("wsl_memory_gb").or_else(|| if wsl { wslconfig_from_home() } else { None });
        Ok(MemoryModel {
            configured,
            total_gb: f("memory_gb").or_else(detect_total_gb),
            vm_cap_gb,
            reserve_gb: f("memory_reserve_gb").unwrap_or(0.0),
            per_step_gb: f("memory_per_step_gb"),
            per_cargo_job_gb: f("memory_per_cargo_job_gb"),
            step_default_gb: f("step_default_gb"),
        })
    }

    /// `min(total, vm cap) - reserve`, never below 0. `None` when the total is neither configured nor detectable: unknown memory is
    /// NOT unlimited memory (admission refuses, parallelism is not lowered by a number nobody has).
    pub fn available_gb(&self) -> Option<f64> {
        let total = match (self.total_gb, self.vm_cap_gb) {
            (Some(t), Some(c)) => t.min(c),
            (Some(t), None) => t,
            (None, Some(c)) => c,
            (None, None) => return None,
        };
        Some((total - self.reserve_gb).max(0.0))
    }
}

/// The median of recorded peaks (megabytes).
pub fn typical_mb(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    match s.len() {
        0 => 0.0,
        n if n % 2 == 1 => s[n / 2],
        n => (s[n / 2 - 1] + s[n / 2]) / 2.0,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub admitted: bool,
    pub predicted_gb: f64,
    pub available_gb: f64,
    /// Steps with no history (they counted `step_default_gb`, or 0 when the host has none).
    pub unpredicted: Vec<String>,
    /// The rule and the numbers, ready to print and to put in the decision log.
    pub rule: String,
}

fn predict_gb(m: &MemoryModel, step: &str, hist: &HashMap<String, Vec<f64>>) -> (f64, bool) {
    match hist.get(step).filter(|v| !v.is_empty()) {
        Some(v) => (typical_mb(v) / 1024.0, true),
        None => (m.step_default_gb.unwrap_or(0.0), false),
    }
}

pub fn admit(m: &MemoryModel, steps: &[String], hist: &HashMap<String, Vec<f64>>) -> Admission {
    if !m.configured {
        return Admission { admitted: true, predicted_gb: 0.0, available_gb: 0.0, unpredicted: vec![], rule: "no memory settings for this host in hosts.toml: memory is not checked".into() };
    }
    let mut sum = 0.0;
    let mut unpredicted = Vec::new();
    for s in steps {
        let (g, known) = predict_gb(m, s, hist);
        sum += g;
        if !known {
            unpredicted.push(s.clone());
        }
    }
    let Some(avail) = m.available_gb() else {
        return Admission {
            admitted: false,
            predicted_gb: sum,
            available_gb: 0.0,
            unpredicted,
            rule: "REFUSED: the total memory of this host is unknown (memory_gb is not set in hosts.toml and could not be detected); unknown memory is not unlimited memory".into(),
        };
    };
    if !unpredicted.is_empty() && m.step_default_gb.is_none() {
        return Admission {
            admitted: false,
            predicted_gb: sum,
            available_gb: avail,
            rule: format!("REFUSED: {} step(s) have no recorded peak ({}) and step_default_gb is not set in hosts.toml; set it (it is required once memory is configured)", unpredicted.len(), unpredicted.join(", ")),
            unpredicted,
        };
    }
    let admitted = sum <= avail;
    let total = m.total_gb.map_or("unknown".to_string(), |t| format!("{t:.1}"));
    let rule = format!(
        "admit when predicted <= memory_gb - memory_reserve_gb: predicted {sum:.1} GB for {} steps ({} without history) vs available {avail:.1} GB (memory {total} GB, reserve {:.1} GB{}): {}",
        steps.len(),
        unpredicted.len(),
        m.reserve_gb,
        m.vm_cap_gb.map_or(String::new(), |c| format!(", VM cap {c:.1} GB")),
        if admitted { "admitted" } else { "REFUSED" }
    );
    Admission { admitted, predicted_gb: sum, available_gb: avail, unpredicted, rule }
}

/// The largest set of cheapest steps that fits (greedy, cheapest first); what to run when the full set is refused.
pub fn cheap_set(m: &MemoryModel, steps: &[String], hist: &HashMap<String, Vec<f64>>) -> Vec<String> {
    let mut v: Vec<(f64, &String)> = steps.iter().map(|s| (predict_gb(m, s, hist).0, s)).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    let (mut sum, mut out) = (0.0, Vec::new());
    let avail = m.available_gb().unwrap_or(0.0);
    for (g, s) in v {
        if sum + g > avail {
            break;
        }
        sum += g;
        out.push(s.clone());
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parallelism {
    pub core_based: u64,
    pub memory_based: Option<u64>,
    /// min of the two.
    pub parallel: u64,
    /// Suggested `CARGO_BUILD_JOBS` from `memory_per_cargo_job_gb`; `None` when not configured.
    pub cargo_jobs: Option<u64>,
}

pub fn parallelism(m: &MemoryModel, core_based: u64) -> Parallelism {
    let avail = m.available_gb();
    let by = |per: Option<f64>| per.filter(|p| *p > 0.0).zip(avail).map(|(p, a)| ((a / p).floor() as u64).max(1));
    let memory_based = by(m.per_step_gb);
    Parallelism { core_based, memory_based, parallel: memory_based.map_or(core_based, |x| x.min(core_based)).max(1), cargo_jobs: by(m.per_cargo_job_gb) }
}

/// `[wsl2] memory=24GB` (or MB) from a `.wslconfig`.
pub fn wslconfig_memory_gb(text: &str) -> Option<f64> {
    for line in text.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            if k.trim().eq_ignore_ascii_case("memory") {
                let v = v.trim().to_ascii_uppercase();
                let num = |suffixes: &[&str]| suffixes.iter().find_map(|s| v.strip_suffix(s)).and_then(|n| n.trim().parse::<f64>().ok());
                return num(&["GB", "G"]).or_else(|| num(&["MB", "M"]).map(|m| m / 1024.0)).or_else(|| num(&["KB", "K"]).map(|k| k / 1048576.0));
            }
        }
    }
    None
}

fn wslconfig_from_home() -> Option<f64> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    wslconfig_memory_gb(&std::fs::read_to_string(std::path::Path::new(&home).join(".wslconfig")).ok()?)
}

/// `MemTotal` in gigabytes from /proc/meminfo text (inside WSL this is already the VM cap).
pub fn meminfo_total_gb(text: &str) -> Option<f64> {
    let l = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    Some(l.split_whitespace().nth(1)?.parse::<f64>().ok()? / 1048576.0)
}

pub fn detect_total_gb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        return meminfo_total_gb(&std::fs::read_to_string("/proc/meminfo").ok()?);
    }
    #[cfg(windows)]
    {
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
            fn GlobalMemoryStatusEx(b: *mut MemStatus) -> i32;
        }
        let mut s = MemStatus { length: std::mem::size_of::<MemStatus>() as u32, load: 0, total_phys: 0, avail_phys: 0, total_page: 0, avail_page: 0, total_virt: 0, avail_virt: 0, avail_ext: 0 };
        // SAFETY: s is a correctly sized, initialised MEMORYSTATUSEX with `length` set, as the API requires.
        return if unsafe { GlobalMemoryStatusEx(&mut s) } != 0 { Some(s.total_phys as f64 / 1073741824.0) } else { None };
    }
    #[allow(unreachable_code)]
    None
}

/// Memory of a process tree on Linux by walking /proc (sum of VmRSS of `root` and its descendants), in megabytes.
/// The holder samples this every few seconds when no cgroup `memory.peak` is readable.
#[cfg(target_os = "linux")]
pub fn tree_rss_mb(root: u32) -> f64 {
    let mut ppid: HashMap<u32, u32> = HashMap::new();
    let mut rss: HashMap<u32, f64> = HashMap::new();
    if let Ok(d) = std::fs::read_dir("/proc") {
        for e in d.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Ok(st) = std::fs::read_to_string(e.path().join("status")) else { continue };
            for l in st.lines() {
                if let Some(v) = l.strip_prefix("PPid:") {
                    ppid.insert(pid, v.trim().parse().unwrap_or(0));
                } else if let Some(v) = l.strip_prefix("VmRSS:") {
                    rss.insert(pid, v.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0);
                }
            }
        }
    }
    let mut set = vec![root];
    let mut i = 0;
    while i < set.len() {
        let p = set[i];
        set.extend(ppid.iter().filter(|(_, pp)| **pp == p).map(|(c, _)| *c));
        i += 1;
    }
    set.iter().filter_map(|p| rss.get(p)).sum()
}

/// cgroup v2 `memory.peak` file of this process's cgroup, when the kernel has it.
#[cfg(target_os = "linux")]
pub fn cgroup_peak_path() -> Option<std::path::PathBuf> {
    let c = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = c.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let p = std::path::Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')).join("memory.peak");
    p.is_file().then_some(p)
}

/// Peak working set of a process on Windows, in megabytes (the process itself, not its children).
#[cfg(windows)]
pub fn peak_working_set_mb(handle: std::os::windows::io::RawHandle) -> Option<f64> {
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        rest: [usize; 7],
    }
    extern "system" {
        fn K32GetProcessMemoryInfo(h: *mut std::ffi::c_void, c: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, page_fault_count: 0, peak_working_set: 0, rest: [0; 7] };
    let cb = c.cb;
    // SAFETY: the handle is a live child handle owned by std::process::Child; c is sized as `cb` says
    // (PROCESS_MEMORY_COUNTERS is 72 bytes on x64: two u32, then nine usize).
    (unsafe { K32GetProcessMemoryInfo(handle as *mut _, &mut c, cb) } != 0).then(|| c.peak_working_set as f64 / 1048576.0)
}
