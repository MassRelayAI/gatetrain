//! The admission guard (docs/admission.md): before a new step starts, read the host and HOLD the start while the host or a protected
//! service is in trouble. A hold is a buffer, never a failure: running steps keep running, nothing is killed, and after `max_hold_s` of
//! continuous holding the run goes on at one step at a time (`GUARD_HOLD_TIMEOUT`).
//!
//! Monitor first. Every reading is printed as a `GUARD_MONITOR` line every `monitor_every_s`; only the thresholds that are SET hold:
//! - `[[guard.disk]] path, min_free_gb`: free space below the floor;
//! - `[[guard.probe]] url, max_ms, fails`: a protected service answered slower than `max_ms`, with a 5xx, or not at all, `fails` probes in a row;
//! - `max_psi_mem_full`, `max_psi_io_full`, `max_psi_cpu_some`: the `avg10` of /proc/pressure/{memory,io,cpu} (`full`, `full`, `some`) above it;
//! - `max_load`: the 1-minute load average above it.
//!
//! A reading that cannot be taken (no /proc/pressure, no `df`, no `curl`) is printed as `?` and never holds: the guard watches, it does
//! not invent trouble. The settings live in the `[guard]` table of a hosts file (the one `tidy --config` reads; tidy ignores the table)
//! and are re-read before every check: an edit takes effect at the next step and is printed as `GUARD_CONFIG changed: <diff>`. An edit
//! that does not parse keeps the previous settings and says so. Command-line flags override the file.
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One `[[guard.disk]]`: hold while `path` has less than `min_free_gb` free.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskFloor {
    pub path: String,
    pub min_free_gb: f64,
}

fn d_max_ms() -> u64 {
    2000
}
fn d_fails() -> u32 {
    3
}

/// One `[[guard.probe]]`: a protected service. Bad = slower than `max_ms`, a 5xx, or no answer; `fails` bad probes in a row hold.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeRule {
    pub url: String,
    #[serde(default = "d_max_ms")]
    pub max_ms: u64,
    #[serde(default = "d_fails")]
    pub fails: u32,
}

/// `[guard.caps]`: limits a LAUNCHER applies (a systemd scope, nice/ionice, container caps). gatetrain only prints them
/// (`gatetrain guard env`); all are off unless set.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    /// run the drive in a `systemd-run --user --scope` with the limits below
    #[serde(default)]
    pub scope: bool,
    pub cpu_quota: Option<String>,
    pub memory_max: Option<String>,
    pub memory_high: Option<String>,
    pub nice: Option<i32>,
    #[serde(default)]
    pub ionice_idle: bool,
    /// `docker run --cpus` / `--memory` for the throwaway containers a gate starts
    pub container_cpus: Option<String>,
    pub container_memory: Option<String>,
}

fn d_every() -> f64 {
    5.0
}
fn d_max_hold() -> f64 {
    1800.0
}
fn d_monitor() -> f64 {
    300.0
}
fn d_probe_every() -> f64 {
    30.0
}

/// The `[guard]` table. Every threshold is optional; an unset one only monitors.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// seconds between checks while holding
    #[serde(default = "d_every")]
    pub every_s: f64,
    /// seconds of continuous holding after which the run goes on at one step at a time
    #[serde(default = "d_max_hold")]
    pub max_hold_s: f64,
    /// seconds between two GUARD_MONITOR lines
    #[serde(default = "d_monitor")]
    pub monitor_every_s: f64,
    /// seconds between two probes of the same service
    #[serde(default = "d_probe_every")]
    pub probe_every_s: f64,
    pub max_psi_mem_full: Option<f64>,
    pub max_psi_io_full: Option<f64>,
    pub max_psi_cpu_some: Option<f64>,
    pub max_load: Option<f64>,
    #[serde(default)]
    pub disk: Vec<DiskFloor>,
    #[serde(default)]
    pub probe: Vec<ProbeRule>,
    #[serde(default)]
    pub caps: Caps,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            every_s: d_every(),
            max_hold_s: d_max_hold(),
            monitor_every_s: d_monitor(),
            probe_every_s: d_probe_every(),
            max_psi_mem_full: None,
            max_psi_io_full: None,
            max_psi_cpu_some: None,
            max_load: None,
            disk: vec![],
            probe: vec![],
            caps: Caps::default(),
        }
    }
}

#[derive(Deserialize)]
struct FileTop {
    guard: Option<Settings>,
}

/// Parse the `[guard]` table of a hosts file (other tables are ignored). No table = the defaults (monitor only).
pub fn parse(text: &str) -> Result<Settings, String> {
    let top: FileTop = toml::from_str(text).map_err(|e| format!("[guard]: {}", e.message()))?;
    let s = top.guard.unwrap_or_default();
    validate(&s)?;
    Ok(s)
}

fn validate(s: &Settings) -> Result<(), String> {
    for (k, v) in [("every_s", s.every_s), ("max_hold_s", s.max_hold_s), ("monitor_every_s", s.monitor_every_s), ("probe_every_s", s.probe_every_s)] {
        if !(v.is_finite() && v > 0.0) {
            return Err(format!("[guard] {k} must be a positive number, got {v}"));
        }
    }
    for d in &s.disk {
        if d.path.is_empty() || !(d.min_free_gb.is_finite() && d.min_free_gb >= 0.0) {
            return Err(format!("[[guard.disk]] {:?}: path must be set and min_free_gb >= 0", d.path));
        }
    }
    for p in &s.probe {
        if !(p.url.starts_with("http://") || p.url.starts_with("https://")) || p.url.chars().any(|c| c.is_whitespace()) {
            return Err(format!("[[guard.probe]] url {:?} must be an http(s) URL", p.url));
        }
        if p.fails == 0 || p.max_ms == 0 {
            return Err(format!("[[guard.probe]] {}: max_ms and fails must be at least 1", p.url));
        }
    }
    let ok = |v: &Option<String>| v.as_deref().is_none_or(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_alphanumeric() || c == '%' || c == '.'));
    for (k, v) in [("cpu_quota", &s.caps.cpu_quota), ("memory_max", &s.caps.memory_max), ("memory_high", &s.caps.memory_high), ("container_cpus", &s.caps.container_cpus), ("container_memory", &s.caps.container_memory)] {
        if !ok(v) {
            return Err(format!("[guard.caps] {k}: only letters, digits, '.' and '%' (for example \"2400%\", \"64G\", \"2\")"));
        }
    }
    Ok(())
}

/// Command-line overrides: each set field replaces the file's value (a disk floor replaces the same path's or is added; probe URLs are added).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub max_psi_mem_full: Option<f64>,
    pub max_psi_io_full: Option<f64>,
    pub max_psi_cpu_some: Option<f64>,
    pub max_load: Option<f64>,
    pub min_free_disk: Vec<DiskFloor>,
    pub probe: Vec<String>,
    pub probe_max_ms: Option<u64>,
    pub probe_fails: Option<u32>,
    pub every_s: Option<f64>,
    pub max_hold_s: Option<f64>,
    pub monitor_every_s: Option<f64>,
    pub probe_every_s: Option<f64>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.max_psi_mem_full.is_none()
            && self.max_psi_io_full.is_none()
            && self.max_psi_cpu_some.is_none()
            && self.max_load.is_none()
            && self.min_free_disk.is_empty()
            && self.probe.is_empty()
            && self.probe_max_ms.is_none()
            && self.probe_fails.is_none()
            && self.every_s.is_none()
            && self.max_hold_s.is_none()
            && self.monitor_every_s.is_none()
            && self.probe_every_s.is_none()
    }
    pub fn apply(&self, s: &mut Settings) {
        macro_rules! set {
            ($($f:ident),*) => { $( if let Some(v) = self.$f { s.$f = Some(v); } )* };
        }
        set!(max_psi_mem_full, max_psi_io_full, max_psi_cpu_some, max_load);
        macro_rules! num {
            ($($f:ident),*) => { $( if let Some(v) = self.$f { s.$f = v; } )* };
        }
        num!(every_s, max_hold_s, monitor_every_s, probe_every_s);
        for d in &self.min_free_disk {
            match s.disk.iter_mut().find(|x| x.path == d.path) {
                Some(x) => x.min_free_gb = d.min_free_gb,
                None => s.disk.push(d.clone()),
            }
        }
        for u in &self.probe {
            if !s.probe.iter().any(|p| &p.url == u) {
                s.probe.push(ProbeRule { url: u.clone(), max_ms: d_max_ms(), fails: d_fails() });
            }
        }
        for p in &mut s.probe {
            if let Some(m) = self.probe_max_ms {
                p.max_ms = m;
            }
            if let Some(f) = self.probe_fails {
                p.fails = f;
            }
        }
    }
}

/// `PATH=GB` (the last `=` splits, so a path may hold one).
pub fn parse_disk_floor(s: &str) -> Result<DiskFloor, String> {
    let (p, g) = s.rsplit_once('=').ok_or_else(|| format!("--min-free-disk {s:?}: expected PATH=GB"))?;
    let gb: f64 = g.trim().parse().map_err(|_| format!("--min-free-disk {s:?}: {g:?} is not a number of GB"))?;
    if p.is_empty() || !(gb.is_finite() && gb >= 0.0) {
        return Err(format!("--min-free-disk {s:?}: expected PATH=GB with GB >= 0"));
    }
    Ok(DiskFloor { path: p.to_string(), min_free_gb: gb })
}

/// Every setting as `key=value`, for the change diff.
pub fn flat(s: &Settings) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let o = |v: Option<f64>| v.map_or("unset".to_string(), |x| format!("{x}"));
    m.insert("every_s".into(), format!("{}", s.every_s));
    m.insert("max_hold_s".into(), format!("{}", s.max_hold_s));
    m.insert("monitor_every_s".into(), format!("{}", s.monitor_every_s));
    m.insert("probe_every_s".into(), format!("{}", s.probe_every_s));
    m.insert("max_psi_mem_full".into(), o(s.max_psi_mem_full));
    m.insert("max_psi_io_full".into(), o(s.max_psi_io_full));
    m.insert("max_psi_cpu_some".into(), o(s.max_psi_cpu_some));
    m.insert("max_load".into(), o(s.max_load));
    for d in &s.disk {
        m.insert(format!("disk[{}].min_free_gb", d.path), format!("{}", d.min_free_gb));
    }
    for p in &s.probe {
        m.insert(format!("probe[{}]", p.url), format!("max_ms={} fails={}", p.max_ms, p.fails));
    }
    let c = &s.caps;
    let so = |v: &Option<String>| v.clone().unwrap_or_else(|| "unset".into());
    m.insert("caps.scope".into(), c.scope.to_string());
    m.insert("caps.cpu_quota".into(), so(&c.cpu_quota));
    m.insert("caps.memory_max".into(), so(&c.memory_max));
    m.insert("caps.memory_high".into(), so(&c.memory_high));
    m.insert("caps.nice".into(), c.nice.map_or("unset".into(), |n| n.to_string()));
    m.insert("caps.ionice_idle".into(), c.ionice_idle.to_string());
    m.insert("caps.container_cpus".into(), so(&c.container_cpus));
    m.insert("caps.container_memory".into(), so(&c.container_memory));
    m
}

/// `a: 1 -> 2; b: unset -> 3` over the keys that differ (an absent key reads `absent`).
pub fn diff(old: &Settings, new: &Settings) -> String {
    let (a, b) = (flat(old), flat(new));
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| format!("{k}: {} -> {}", a.get(k).map_or("absent", |s| s.as_str()), b.get(k).map_or("absent", |s| s.as_str())))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The launcher's view of `[guard.caps]` as shell assignments (`gatetrain guard env`); unset values print empty.
pub fn caps_env(s: &Settings) -> String {
    let c = &s.caps;
    let v = |x: &Option<String>| x.clone().unwrap_or_default();
    format!(
        "GUARD_SCOPE={}\nGUARD_CPU_QUOTA={}\nGUARD_MEMORY_MAX={}\nGUARD_MEMORY_HIGH={}\nGUARD_NICE={}\nGUARD_IONICE_IDLE={}\nGUARD_CONTAINER_CPUS={}\nGUARD_CONTAINER_MEMORY={}\n",
        u8::from(c.scope),
        v(&c.cpu_quota),
        v(&c.memory_max),
        v(&c.memory_high),
        c.nice.map(|n| n.to_string()).unwrap_or_default(),
        u8::from(c.ionice_idle),
        v(&c.container_cpus),
        v(&c.container_memory),
    )
}

/// What the host said at one check. `None` = could not be read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Readings {
    pub mem_full10: Option<f64>,
    pub io_full10: Option<f64>,
    pub cpu_some10: Option<f64>,
    pub load1: Option<f64>,
    /// (path, free GB, total GB)
    pub disks: Vec<(String, Option<f64>, Option<f64>)>,
}

/// `avg10` of the `some` or `full` line of a /proc/pressure file.
pub fn psi_avg10(text: &str, kind: &str) -> Option<f64> {
    text.lines().find_map(|l| l.strip_prefix(kind).and_then(|r| r.strip_prefix(' '))).and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("avg10="))).and_then(|v| v.parse().ok())
}

/// Read PSI and the load average under `proc_root` (normally `/proc`).
pub fn read_proc(proc_root: &Path) -> Readings {
    let rd = |p: &[&str]| std::fs::read_to_string(p.iter().fold(proc_root.to_path_buf(), |a, x| a.join(x))).ok();
    Readings {
        mem_full10: rd(&["pressure", "memory"]).and_then(|t| psi_avg10(&t, "full")),
        io_full10: rd(&["pressure", "io"]).and_then(|t| psi_avg10(&t, "full")),
        cpu_some10: rd(&["pressure", "cpu"]).and_then(|t| psi_avg10(&t, "some")),
        load1: rd(&["loadavg"]).and_then(|t| t.split_whitespace().next().and_then(|x| x.parse().ok())),
        disks: vec![],
    }
}

/// (free GB, total GB) of the file system holding `path`.
#[cfg(not(windows))]
pub fn disk_space(path: &str) -> Option<(f64, Option<f64>)> {
    let o = std::process::Command::new("df").args(["-Pk", path]).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    let free = cols.get(3)?.parse::<f64>().ok()? / 1048576.0;
    Some((free, cols.get(1).and_then(|t| t.parse::<f64>().ok()).map(|t| t / 1048576.0)))
}

#[cfg(windows)]
pub fn disk_space(path: &str) -> Option<(f64, Option<f64>)> {
    crate::disk::free_gb(path).map(|f| (f, None))
}

/// One probe of a protected service.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeSample {
    /// an HTTP answer: status and milliseconds
    Answer { status: u16, ms: u64 },
    /// no answer (timeout, refused, DNS)
    NoAnswer(String),
    /// the probe could not be taken (no `curl`): never counted as bad
    Unknown(String),
}

impl ProbeSample {
    pub fn bad(&self, max_ms: u64) -> bool {
        match self {
            ProbeSample::Answer { status, ms } => *status >= 500 || *ms > max_ms,
            ProbeSample::NoAnswer(_) => true,
            ProbeSample::Unknown(_) => false,
        }
    }
    pub fn short(&self) -> String {
        match self {
            ProbeSample::Answer { status, ms } => format!("{ms}ms/{status}"),
            ProbeSample::NoAnswer(e) => format!("no-answer({e})"),
            ProbeSample::Unknown(_) => "?".into(),
        }
    }
}

/// GET `url` with `curl`, giving up after three times `max_ms` (at least 2 s, at most 30 s).
pub fn curl_probe(url: &str, max_ms: u64) -> ProbeSample {
    let limit = (max_ms.saturating_mul(3)).clamp(2000, 30000) as f64 / 1000.0;
    let o = std::process::Command::new("curl").args(["-sS", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "-w", "%{http_code} %{time_total}", "--max-time", &format!("{limit}"), url]).output();
    let o = match o {
        Ok(o) => o,
        Err(e) => return ProbeSample::Unknown(format!("cannot run curl: {e}")),
    };
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let mut w = text.split_whitespace();
    let status: u16 = w.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let secs: f64 = w.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    if !o.status.success() || status == 0 {
        let err = String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("").trim().replace(char::is_whitespace, "_");
        return ProbeSample::NoAnswer(format!("curl rc={} {}", o.status.code().unwrap_or(-1), err.chars().take(80).collect::<String>()));
    }
    ProbeSample::Answer { status, ms: (secs * 1000.0).round() as u64 }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// What the scheduler may do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// start steps as usual
    Open,
    /// start nothing new
    Hold,
    /// held longer than `max_hold_s`: start a step only when none runs
    Par1,
}

/// Where the guard's settings come from.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    /// the hosts file whose `[guard]` table is read before every check; None = flags only
    pub config: Option<PathBuf>,
    pub overrides: Overrides,
    /// the /proc to read (tests point it at a folder of fake files)
    pub proc_root: Option<PathBuf>,
}

impl Spec {
    pub fn enabled(&self) -> bool {
        self.config.is_some() || !self.overrides.is_empty()
    }
}

type FreeFn = Box<dyn Fn(&str) -> Option<(f64, Option<f64>)> + Send>;
type ProbeFn = Box<dyn Fn(&str, u64) -> ProbeSample + Send>;

/// The guard's state across checks.
pub struct Guard {
    spec: Spec,
    pub settings: Settings,
    cfg_text: Option<String>,
    cfg_note: Option<String>,
    free_of: FreeFn,
    probe_of: ProbeFn,
    /// consecutive bad probes, and the last sample, per URL
    probe_bad: BTreeMap<String, (u32, ProbeSample)>,
    last_probe: Option<Instant>,
    hold_since: Option<Instant>,
    hold_why: String,
    timed_out: bool,
    last_monitor: Option<Instant>,
    /// print GUARD_MONITOR lines on their interval (off for `guard wait`, which prints only while it holds)
    pub monitor: bool,
}

impl Guard {
    /// The real sources: `df` and `curl`. An unreadable or invalid config at the START is an error (a typo must not pass silently).
    pub fn new(spec: Spec) -> Result<Guard, String> {
        Guard::with_sources(spec, Box::new(disk_space), Box::new(curl_probe))
    }

    pub fn with_sources(spec: Spec, free_of: FreeFn, probe_of: ProbeFn) -> Result<Guard, String> {
        let (settings, cfg_text) = match &spec.config {
            Some(p) => {
                let t = std::fs::read_to_string(p).map_err(|e| format!("guard config {}: {e}", p.display()))?;
                (parse(&t).map_err(|e| format!("guard config {}: {e}", p.display()))?, Some(t))
            }
            None => (Settings::default(), None),
        };
        let mut s = settings;
        spec.overrides.apply(&mut s);
        validate(&s)?;
        Ok(Guard { spec, settings: s, cfg_text, cfg_note: None, free_of, probe_of, probe_bad: BTreeMap::new(), last_probe: None, hold_since: None, hold_why: String::new(), timed_out: false, last_monitor: None, monitor: true })
    }

    /// The settings line printed once at the start.
    pub fn describe(&self) -> String {
        let s = &self.settings;
        let o = |v: Option<f64>| v.map_or("-".to_string(), |x| format!("{x}"));
        let disks: Vec<String> = s.disk.iter().map(|d| format!("{}>={}GB", d.path, d.min_free_gb)).collect();
        let probes: Vec<String> = s.probe.iter().map(|p| format!("{}<={}ms*{}", p.url, p.max_ms, p.fails)).collect();
        format!(
            "GUARD_ON config={} hold: disk=[{}] probe=[{}] psi_mem_full>{} psi_io_full>{} psi_cpu_some>{} load>{}; max_hold={}s every={}s monitor={}s",
            self.spec.config.as_ref().map_or("-".into(), |p| p.display().to_string()),
            disks.join(","),
            probes.join(","),
            o(s.max_psi_mem_full),
            o(s.max_psi_io_full),
            o(s.max_psi_cpu_some),
            o(s.max_load),
            s.max_hold_s,
            s.every_s,
            s.monitor_every_s
        )
    }

    /// How long the scheduler may wait for a step before it checks again.
    pub fn wait(&self) -> Duration {
        let s = if self.hold_since.is_some() { self.settings.every_s } else { self.settings.monitor_every_s.min(self.settings.every_s.max(self.settings.probe_every_s)) };
        Duration::from_secs_f64(s.max(0.05))
    }

    pub fn holding(&self) -> bool {
        self.hold_since.is_some()
    }

    fn reload(&mut self, say: &mut dyn FnMut(&str)) {
        let Some(p) = self.spec.config.clone() else { return };
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) => {
                let n = format!("GUARD_CONFIG unreadable ({e}); keeping the previous settings");
                if self.cfg_note.as_deref() != Some(&n) {
                    say(&n);
                    self.cfg_note = Some(n);
                }
                return;
            }
        };
        if self.cfg_text.as_deref() == Some(&text) {
            return;
        }
        let parsed = parse(&text).and_then(|mut s| {
            self.spec.overrides.apply(&mut s);
            validate(&s).map(|_| s)
        });
        match parsed {
            Ok(s) => {
                let d = diff(&self.settings, &s);
                say(&format!("GUARD_CONFIG changed: {}", if d.is_empty() { "nothing that matters".into() } else { d }));
                self.settings = s;
                self.cfg_text = Some(text);
                self.cfg_note = None;
            }
            Err(e) => {
                let n = format!("GUARD_CONFIG invalid ({}); keeping the previous settings", e.replace(['\r', '\n'], " "));
                if self.cfg_note.as_deref() != Some(&n) {
                    say(&n);
                    self.cfg_note = Some(n);
                }
            }
        }
    }

    /// Read the host now (PSI, load, the floors' disks).
    pub fn read(&self) -> Readings {
        let mut r = read_proc(self.spec.proc_root.as_deref().unwrap_or(Path::new("/proc")));
        for d in &self.settings.disk {
            let x = (self.free_of)(&d.path);
            r.disks.push((d.path.clone(), x.map(|x| x.0), x.and_then(|x| x.1)));
        }
        r
    }

    /// The hold reasons of these readings as (key, text); empty = admit. A new GUARD_HOLD line is printed when the KEYS change, not the numbers.
    pub fn reasons(&self, r: &Readings) -> Vec<(String, String)> {
        let s = &self.settings;
        let mut why = vec![];
        for (path, free, _) in &r.disks {
            if let (Some(f), Some(floor)) = (free, s.disk.iter().find(|d| &d.path == path)) {
                if *f < floor.min_free_gb {
                    why.push((format!("disk:{path}"), format!("disk {path} free {f:.1}GB < {}GB", floor.min_free_gb)));
                }
            }
        }
        for (name, v, max) in [("psi memory full avg10", r.mem_full10, s.max_psi_mem_full), ("psi io full avg10", r.io_full10, s.max_psi_io_full), ("psi cpu some avg10", r.cpu_some10, s.max_psi_cpu_some), ("load1", r.load1, s.max_load)] {
            if let (Some(v), Some(m)) = (v, max) {
                if v > m {
                    why.push((name.to_string(), format!("{name} {v} > {m}")));
                }
            }
        }
        for p in &s.probe {
            if let Some((n, last)) = self.probe_bad.get(&p.url) {
                if *n >= p.fails {
                    why.push((format!("probe:{}", p.url), format!("probe {} bad {n}x in a row (last {}, max {}ms)", p.url, last.short(), p.max_ms)));
                }
            }
        }
        why
    }

    fn probe_now(&mut self, now: Instant) {
        if self.settings.probe.is_empty() {
            return;
        }
        if self.last_probe.is_some_and(|t| now.saturating_duration_since(t).as_secs_f64() < self.settings.probe_every_s) {
            return;
        }
        self.last_probe = Some(now);
        self.probe_bad.retain(|u, _| self.settings.probe.iter().any(|p| &p.url == u));
        for p in self.settings.probe.clone() {
            let s = (self.probe_of)(&p.url, p.max_ms);
            let e = self.probe_bad.entry(p.url.clone()).or_insert((0, s.clone()));
            e.0 = if s.bad(p.max_ms) { e.0 + 1 } else if matches!(s, ProbeSample::Unknown(_)) { e.0 } else { 0 };
            e.1 = s;
        }
    }

    /// The monitor line: every reading, `?` for one that could not be taken.
    pub fn monitor_line(&self, r: &Readings, now: Instant) -> String {
        let o = |v: Option<f64>| v.map_or("?".to_string(), |x| format!("{x}"));
        let disks: Vec<String> = r
            .disks
            .iter()
            .map(|(p, f, t)| match (f, t) {
                (Some(f), Some(t)) if *t > 0.0 => format!("{p}={f:.1}GB({:.0}%used)", 100.0 * (1.0 - f / t)),
                (Some(f), _) => format!("{p}={f:.1}GB"),
                _ => format!("{p}=?"),
            })
            .collect();
        let probes: Vec<String> = self.settings.probe.iter().map(|p| format!("{}={}", p.url, self.probe_bad.get(&p.url).map_or("?".into(), |(n, s)| format!("{}{}", s.short(), if *n > 0 { format!(",bad{n}") } else { String::new() })))).collect();
        format!(
            "GUARD_MONITOR at={} disk=[{}] psi mem_full10={} io_full10={} cpu_some10={} load1={} probe=[{}]{}",
            crate::logfmt::Ts::now_utc().iso(),
            disks.join(" "),
            o(r.mem_full10),
            o(r.io_full10),
            o(r.cpu_some10),
            o(r.load1),
            probes.join(" "),
            self.hold_since.map_or(String::new(), |t| format!(" holding={:.0}s", now.saturating_duration_since(t).as_secs_f64()))
        )
    }

    /// The state a fresh process carries on (`guard wait --state`): the probe counts, when each service was last probed, when the hold began.
    pub fn state_json(&self, now: Instant) -> serde_json::Value {
        let unix = |t: Instant| unix_now() - now.saturating_duration_since(t).as_secs_f64();
        let probes: serde_json::Map<String, serde_json::Value> = self
            .probe_bad
            .iter()
            .map(|(u, (n, s))| {
                let sample = match s {
                    ProbeSample::Answer { status, ms } => serde_json::json!({"status": status, "ms": ms}),
                    ProbeSample::NoAnswer(e) => serde_json::json!({"no_answer": e}),
                    ProbeSample::Unknown(e) => serde_json::json!({"unknown": e}),
                };
                (u.clone(), serde_json::json!({"bad": n, "last": sample}))
            })
            .collect();
        serde_json::json!({"v": 1, "probes": probes, "last_probe": self.last_probe.map(unix), "hold_since": self.hold_since.map(unix), "hold_why": self.hold_why, "timed_out": self.timed_out})
    }

    /// Take over a state written by [`Guard::state_json`] (anything unreadable is ignored: a fresh state).
    pub fn load_state(&mut self, v: &serde_json::Value, now: Instant) {
        if v.get("v").and_then(|x| x.as_u64()) != Some(1) {
            return;
        }
        let back = |t: f64| now.checked_sub(Duration::from_secs_f64((unix_now() - t).max(0.0)));
        for (u, p) in v.get("probes").and_then(|x| x.as_object()).into_iter().flatten() {
            let n = p.get("bad").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let l = p.get("last");
            let s = if let (Some(st), Some(ms)) = (l.and_then(|x| x.get("status")).and_then(|x| x.as_u64()), l.and_then(|x| x.get("ms")).and_then(|x| x.as_u64())) {
                ProbeSample::Answer { status: st as u16, ms }
            } else if let Some(e) = l.and_then(|x| x.get("no_answer")).and_then(|x| x.as_str()) {
                ProbeSample::NoAnswer(e.to_string())
            } else {
                ProbeSample::Unknown("from state".into())
            };
            self.probe_bad.insert(u.clone(), (n, s));
        }
        self.last_probe = v.get("last_probe").and_then(|x| x.as_f64()).and_then(back);
        self.hold_since = v.get("hold_since").and_then(|x| x.as_f64()).and_then(back);
        self.hold_why = v.get("hold_why").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        self.timed_out = v.get("timed_out").and_then(|x| x.as_bool()).unwrap_or(false);
    }

    /// One check: re-read the config, read the host, probe when due, print the monitor line when due, and decide.
    pub fn check(&mut self, now: Instant, say: &mut dyn FnMut(&str)) -> Gate {
        self.reload(say);
        self.probe_now(now);
        let r = self.read();
        let why = self.reasons(&r);
        let gate = if why.is_empty() {
            if let Some(t) = self.hold_since.take() {
                say(&format!("GUARD_RELEASE held={:.1}s", now.saturating_duration_since(t).as_secs_f64()));
            }
            self.timed_out = false;
            self.hold_why.clear();
            Gate::Open
        } else {
            let keys = why.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(",");
            let since = *self.hold_since.get_or_insert(now);
            if keys != self.hold_why {
                say(&format!("GUARD_HOLD {}", why.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join("; ")));
                self.hold_why = keys;
            }
            if now.saturating_duration_since(since).as_secs_f64() >= self.settings.max_hold_s {
                if !self.timed_out {
                    say(&format!("GUARD_HOLD_TIMEOUT held={:.1}s >= max_hold_s {}; going on at par 1", now.saturating_duration_since(since).as_secs_f64(), self.settings.max_hold_s));
                    self.timed_out = true;
                }
                Gate::Par1
            } else {
                Gate::Hold
            }
        };
        let due = self.last_monitor.is_none_or(|t| now.saturating_duration_since(t).as_secs_f64() >= self.settings.monitor_every_s);
        if due && (self.monitor || self.hold_since.is_some()) {
            say(&self.monitor_line(&r, now));
            self.last_monitor = Some(now);
        }
        gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn fake_proc(dir: &Path, mem_full: f64, io_full: f64, cpu_some: f64, load: f64) {
        std::fs::create_dir_all(dir.join("pressure")).unwrap();
        let psi = |s: f64, f: f64| format!("some avg10={s:.2} avg60=0.00 avg300=0.00 total=1\nfull avg10={f:.2} avg60=0.00 avg300=0.00 total=1\n");
        std::fs::write(dir.join("pressure/memory"), psi(mem_full, mem_full)).unwrap();
        std::fs::write(dir.join("pressure/io"), psi(io_full, io_full)).unwrap();
        std::fs::write(dir.join("pressure/cpu"), format!("some avg10={cpu_some:.2} avg60=0.00 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")).unwrap();
        std::fs::write(dir.join("loadavg"), format!("{load:.2} 1.00 1.00 3/900 12345\n")).unwrap();
    }

    struct Rig {
        dir: tempfile::TempDir,
        free: Arc<Mutex<f64>>,
        probe: Arc<Mutex<ProbeSample>>,
    }

    fn rig(toml: &str, ov: Overrides) -> (Rig, Guard) {
        let dir = tempfile::tempdir().unwrap();
        fake_proc(&dir.path().join("proc"), 0.0, 0.0, 0.0, 0.5);
        let cfg = dir.path().join("hosts.toml");
        std::fs::write(&cfg, toml).unwrap();
        let free = Arc::new(Mutex::new(500.0));
        let probe = Arc::new(Mutex::new(ProbeSample::Answer { status: 200, ms: 50 }));
        let (f2, p2) = (free.clone(), probe.clone());
        let g = Guard::with_sources(
            Spec { config: Some(cfg), overrides: ov, proc_root: Some(dir.path().join("proc")) },
            Box::new(move |_| Some((*f2.lock().unwrap(), Some(1000.0)))),
            Box::new(move |_, _| p2.lock().unwrap().clone()),
        )
        .unwrap();
        (Rig { dir, free, probe }, g)
    }

    fn lines(g: &mut Guard, now: Instant) -> (Gate, Vec<String>) {
        let mut out = vec![];
        let gate = g.check(now, &mut |l| out.push(l.to_string()));
        (gate, out)
    }

    const CFG: &str = "profile = 'mixed'\n[guard]\nmax_hold_s = 60\nmax_psi_mem_full = 20\nprobe_every_s = 1\n[[guard.disk]]\npath = '/'\nmin_free_gb = 10\n[[guard.probe]]\nurl = 'https://protected.example/'\nmax_ms = 2000\nfails = 3\n";

    #[test]
    fn psi_lines_parse() {
        let t = "some avg10=1.50 avg60=0.10 avg300=0.38 total=5\nfull avg10=0.38 avg60=0.10 avg300=0.05 total=2\n";
        assert_eq!(psi_avg10(t, "some"), Some(1.5));
        assert_eq!(psi_avg10(t, "full"), Some(0.38));
        assert_eq!(psi_avg10("garbage", "full"), None);
    }

    #[test]
    fn a_quiet_host_is_admitted_and_monitored() {
        let (_r, mut g) = rig(CFG, Overrides::default());
        let (gate, out) = lines(&mut g, Instant::now());
        assert_eq!(gate, Gate::Open);
        assert!(out.iter().any(|l| l.starts_with("GUARD_MONITOR ") && l.contains("/=500.0GB(50%used)") && l.contains("mem_full10=0")), "{out:?}");
    }

    #[test]
    fn a_near_full_disk_holds_then_releases() {
        let (r, mut g) = rig(CFG, Overrides::default());
        *r.free.lock().unwrap() = 9.5;
        let t0 = Instant::now();
        let (gate, out) = lines(&mut g, t0);
        assert_eq!(gate, Gate::Hold);
        assert!(out.iter().any(|l| l == "GUARD_HOLD disk / free 9.5GB < 10GB"), "{out:?}");
        *r.free.lock().unwrap() = 11.0;
        let (gate, out) = lines(&mut g, t0 + Duration::from_secs(5));
        assert_eq!(gate, Gate::Open);
        assert!(out.iter().any(|l| l.starts_with("GUARD_RELEASE held=5.0s")), "{out:?}");
    }

    #[test]
    fn high_load_and_cpu_and_io_pressure_only_monitor_by_default() {
        let (r, mut g) = rig(CFG, Overrides::default());
        fake_proc(&r.dir.path().join("proc"), 5.0, 90.0, 95.0, 400.0);
        let (gate, out) = lines(&mut g, Instant::now());
        assert_eq!(gate, Gate::Open, "{out:?}");
        assert!(!out.iter().any(|l| l.starts_with("GUARD_HOLD")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("io_full10=90") && l.contains("cpu_some10=95") && l.contains("load1=400")), "{out:?}");
    }

    #[test]
    fn severe_memory_pressure_holds() {
        let (r, mut g) = rig(CFG, Overrides::default());
        fake_proc(&r.dir.path().join("proc"), 25.0, 0.0, 0.0, 1.0);
        let (gate, out) = lines(&mut g, Instant::now());
        assert_eq!(gate, Gate::Hold);
        assert!(out.iter().any(|l| l == "GUARD_HOLD psi memory full avg10 25 > 20"), "{out:?}");
    }

    #[test]
    fn flags_turn_load_and_io_into_holds() {
        let ov = Overrides { max_load: Some(100.0), max_psi_io_full: Some(50.0), ..Default::default() };
        let (r, mut g) = rig(CFG, ov);
        fake_proc(&r.dir.path().join("proc"), 0.0, 60.0, 0.0, 400.0);
        let (gate, out) = lines(&mut g, Instant::now());
        assert_eq!(gate, Gate::Hold);
        assert!(out.iter().any(|l| l.contains("psi io full avg10 60 > 50") && l.contains("load1 400 > 100")), "{out:?}");
    }

    #[test]
    fn a_slow_probe_holds_only_after_n_in_a_row() {
        let (r, mut g) = rig(CFG, Overrides::default());
        *r.probe.lock().unwrap() = ProbeSample::Answer { status: 200, ms: 2500 };
        let t0 = Instant::now();
        assert_eq!(lines(&mut g, t0).0, Gate::Open, "1 slow probe");
        assert_eq!(lines(&mut g, t0 + Duration::from_secs(1)).0, Gate::Open, "2 slow probes");
        let (gate, out) = lines(&mut g, t0 + Duration::from_secs(2));
        assert_eq!(gate, Gate::Hold, "3 slow probes in a row");
        assert!(out.iter().any(|l| l.contains("probe https://protected.example/ bad 3x in a row (last 2500ms/200")), "{out:?}");
        // a probe inside probe_every_s is not taken again: the count does not grow
        lines(&mut g, t0 + Duration::from_millis(2500));
        assert_eq!(g.probe_bad.values().next().unwrap().0, 3);
        *r.probe.lock().unwrap() = ProbeSample::Answer { status: 200, ms: 80 };
        assert_eq!(lines(&mut g, t0 + Duration::from_secs(3)).0, Gate::Open, "one good probe resets");
    }

    #[test]
    fn a_5xx_or_no_answer_is_bad_and_an_unknown_probe_never_is() {
        assert!(ProbeSample::Answer { status: 502, ms: 10 }.bad(2000));
        assert!(ProbeSample::NoAnswer("timeout".into()).bad(2000));
        assert!(!ProbeSample::Unknown("no curl".into()).bad(2000));
        assert!(!ProbeSample::Answer { status: 404, ms: 10 }.bad(2000));
    }

    #[test]
    fn a_hold_past_max_hold_goes_on_at_par_1() {
        let (r, mut g) = rig(CFG, Overrides::default());
        *r.free.lock().unwrap() = 1.0;
        let t0 = Instant::now();
        assert_eq!(lines(&mut g, t0).0, Gate::Hold);
        assert_eq!(lines(&mut g, t0 + Duration::from_secs(59)).0, Gate::Hold);
        let (gate, out) = lines(&mut g, t0 + Duration::from_secs(60));
        assert_eq!(gate, Gate::Par1);
        assert!(out.iter().any(|l| l.starts_with("GUARD_HOLD_TIMEOUT held=60.0s")), "{out:?}");
        let (_, out) = lines(&mut g, t0 + Duration::from_secs(61));
        assert!(!out.iter().any(|l| l.starts_with("GUARD_HOLD_TIMEOUT")), "said once: {out:?}");
    }

    #[test]
    fn an_edit_to_the_toml_changes_the_behaviour_at_the_next_check() {
        let (r, mut g) = rig(CFG, Overrides::default());
        *r.free.lock().unwrap() = 50.0;
        let t0 = Instant::now();
        assert_eq!(lines(&mut g, t0).0, Gate::Open, "50 GB free over a 10 GB floor");
        std::fs::write(r.dir.path().join("hosts.toml"), CFG.replace("min_free_gb = 10", "min_free_gb = 80")).unwrap();
        let (gate, out) = lines(&mut g, t0 + Duration::from_secs(1));
        assert_eq!(gate, Gate::Hold, "{out:?}");
        assert!(out.iter().any(|l| l == "GUARD_CONFIG changed: disk[/].min_free_gb: 10 -> 80"), "{out:?}");
        // an edit that does not parse keeps the 80 GB floor and says so once
        std::fs::write(r.dir.path().join("hosts.toml"), CFG.replace("min_free_gb = 10", "min_free_gb = 'lots'")).unwrap();
        let (gate, out) = lines(&mut g, t0 + Duration::from_secs(2));
        assert_eq!(gate, Gate::Hold);
        assert!(out.iter().any(|l| l.starts_with("GUARD_CONFIG invalid")), "{out:?}");
        let (_, out) = lines(&mut g, t0 + Duration::from_secs(3));
        assert!(!out.iter().any(|l| l.starts_with("GUARD_CONFIG")), "said once: {out:?}");
    }

    #[test]
    fn a_flag_overrides_the_file_and_survives_a_reload() {
        let ov = Overrides { min_free_disk: vec![DiskFloor { path: "/".into(), min_free_gb: 100.0 }], ..Default::default() };
        let (r, mut g) = rig(CFG, ov);
        *r.free.lock().unwrap() = 50.0;
        let t0 = Instant::now();
        assert_eq!(lines(&mut g, t0).0, Gate::Hold);
        std::fs::write(r.dir.path().join("hosts.toml"), CFG.replace("min_free_gb = 10", "min_free_gb = 5")).unwrap();
        assert_eq!(lines(&mut g, t0 + Duration::from_secs(1)).0, Gate::Hold, "the flag's 100 GB still wins");
    }

    #[test]
    fn a_typo_in_guard_is_refused_and_other_tables_are_ignored() {
        assert!(parse("[guard]\nmax_psi_mem = 20\n").unwrap_err().contains("unknown field"));
        assert!(parse("[tidy]\nwhatever = 1\n[[hosts]]\nname = 'x'\n").is_ok());
        assert!(parse("[guard.caps]\ncpu_quota = '24; rm -rf /'\n").is_err());
        assert!(parse("[[guard.probe]]\nurl = 'ftp://x'\n").is_err());
    }

    #[test]
    fn the_probe_count_and_the_hold_carry_over_to_a_fresh_process() {
        let (r, mut g) = rig(CFG, Overrides::default());
        *r.probe.lock().unwrap() = ProbeSample::NoAnswer("timeout".into());
        let t0 = Instant::now();
        lines(&mut g, t0);
        let st = g.state_json(t0);
        // a new process (a new Guard) with the saved state: its first probe is the third bad one in a row
        let (_r2, mut g2) = rig(CFG, Overrides::default());
        let mut st2 = st.clone();
        st2["probes"]["https://protected.example/"]["bad"] = serde_json::json!(2);
        st2["last_probe"] = serde_json::json!(0.0);
        g2.load_state(&st2, Instant::now());
        *_r2.probe.lock().unwrap() = ProbeSample::NoAnswer("timeout".into());
        assert_eq!(lines(&mut g2, Instant::now()).0, Gate::Hold);
        // a probe taken less than probe_every_s ago (by another process) is not repeated
        let (r3, mut g3) = rig(CFG, Overrides::default());
        g3.load_state(&st, Instant::now());
        *r3.probe.lock().unwrap() = ProbeSample::Answer { status: 200, ms: 1 };
        lines(&mut g3, Instant::now());
        assert_eq!(g3.probe_bad.values().next().unwrap().0, 1, "not re-probed inside probe_every_s");
        let _ = r;
    }

    #[test]
    fn the_hosts_file_tidy_reads_accepts_a_guard_table() {
        let t = "profile = 'mixed'
[guard]
max_psi_mem_full = 20
[[guard.disk]]
path = '/'
min_free_gb = 10
[[guard.probe]]
url = 'https://protected.example/'
[guard.caps]
scope = false
";
        crate::config::validate_text(t).unwrap();
        assert_eq!(parse(t).unwrap().disk[0].min_free_gb, 10.0);
    }

    #[test]
    fn caps_are_off_unless_set() {
        let s = parse("[guard]\n").unwrap();
        assert!(caps_env(&s).contains("GUARD_SCOPE=0\n") && caps_env(&s).contains("GUARD_CONTAINER_CPUS=\n"));
        let s = parse("[guard.caps]\nscope = true\ncpu_quota = '2400%'\ncontainer_memory = '2g'\n").unwrap();
        let e = caps_env(&s);
        assert!(e.contains("GUARD_SCOPE=1\n") && e.contains("GUARD_CPU_QUOTA=2400%\n") && e.contains("GUARD_CONTAINER_MEMORY=2g\n"), "{e}");
    }

    #[test]
    fn disk_floor_flags_parse() {
        assert_eq!(parse_disk_floor("/=80").unwrap(), DiskFloor { path: "/".into(), min_free_gb: 80.0 });
        assert_eq!(parse_disk_floor("C:\\x=1.5").unwrap().min_free_gb, 1.5);
        assert!(parse_disk_floor("/").is_err() && parse_disk_floor("/=-1").is_err());
    }
}
