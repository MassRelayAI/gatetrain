//! hosts.toml (port of gatetrain/config.py). The file is read as `toml::Value` and walked field by field, in the same order as the
//! Python, so the first error is the same error and names the same field (`hosts[1].disks[0].min_free_gb`). The wording of a
//! bad value uses Python's `repr` (`'lots'`, `True`, `[1, 'x']`). Known differences, outside the corpus: a TOML syntax error
//! carries the toml crate's own detail, floats outside 1e-4..1e16 print in Rust's exponent form, and a file that cannot be read
//! is a `ConfigError` here (Python raises the OS error).
use serde::Serialize;
use serde_json::Value as Json;
use toml::Value as T;

pub const PROFILES: [&str; 4] = ["zero-cost", "budget", "speed", "mixed"];
pub const KINDS: [&str; 3] = ["local", "server", "cloud"];
pub const OSES: [&str; 3] = ["auto", "linux", "windows"];
pub const USES: [&str; 5] = ["preflight", "lane-gates", "chain", "cache", "archive"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A field is wrong; the message names it.
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: not valid TOML: {detail}")]
    Syntax { path: String, detail: String },
}

type R<X> = std::result::Result<X, ConfigError>;

fn bad<X>(m: String) -> R<X> {
    Err(ConfigError::Invalid(m))
}

/// A number as the file wrote it: Python keeps `20` an int and `20.0` a float, and the dump shows the difference.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Num {
    I(i64),
    F(f64),
}

impl Num {
    /// The value as a float (the checks compare and print numbers, not their Python type).
    pub fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }
}

impl Default for Num {
    fn default() -> Self {
        Num::I(0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Disk {
    pub path: String,
    pub min_free_gb: Num,
    pub fast: bool,
    pub r#use: Vec<String>,
    pub removable: bool,
    pub measure: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Needs {
    pub preflight_gb: Num,
    pub lane_gates_gb: Num,
    pub chain_gb: Num,
    pub chain_needs_fast: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub name: String,
    pub build_dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Host {
    pub name: String,
    pub kind: String,
    pub roles: Vec<String>,
    pub reach: String,
    pub os: String,
    pub cores: Option<i64>,
    pub max_parallel_steps: Option<i64>,
    pub disks: Vec<Disk>,
    pub cost_per_hour_usd: Num,
    pub max_pollers: Option<i64>,
    pub awake_hours: Option<String>,
    pub enabled: bool,
    pub tools: Vec<String>,
    pub ports: Vec<i64>,
    pub reserved_port_ranges: Vec<String>,
    pub container_prefix: Option<String>,
    pub watcher_max_age_min: Option<Num>,
    pub worktrees: Vec<Worktree>,
    pub lock_file: Option<String>,
    pub lock_max_age_hours: Num,
    pub chain_pattern: Option<String>,
    pub cache_server_check: Option<String>,
    pub gate_scripts: Vec<String>,
    /// The host's own `[hosts.tidy]` table, in file order (the first bad key is the one reported).
    #[serde(serialize_with = "ser_tbl")]
    pub tidy: Tbl,
}

#[derive(Debug, Clone, Serialize)]
pub struct Model {
    pub name: String,
    pub kind: String,
    pub cost_per_mtok_usd: Num,
}

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub profile: String,
    pub budget: Json,
    pub needs: Needs,
    pub hosts: Vec<Host>,
    pub models: Vec<Model>,
    #[serde(serialize_with = "ser_tbl")]
    pub tidy: Tbl,
    pub stack: Json,
}

// ---- Python repr of a TOML value ------------------------------------------------------------------------------------

fn py_str(s: &str) -> String {
    crate::pycompat::repr_str(s)
}

/// Python `repr()` of a TOML value, for the "got ..." half of an error message.
pub fn repr(v: &T) -> String {
    match v {
        T::String(s) => py_str(s),
        T::Integer(i) => i.to_string(),
        T::Float(f) => {
            let s = format!("{f:?}");
            match s.split_once('e') {
                Some((m, e)) => {
                    let (sign, digits) = match e.strip_prefix('-') {
                        Some(d) => ('-', d),
                        None => ('+', e),
                    };
                    format!("{m}e{sign}{digits:0>2}")
                }
                None => s,
            }
        }
        T::Boolean(b) => (if *b { "True" } else { "False" }).to_string(),
        T::Datetime(d) => d.to_string(),
        T::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        T::Table(t) => format!("{{{}}}", t.iter().map(|(k, v)| format!("{}: {}", py_str(k), repr(v))).collect::<Vec<_>>().join(", ")),
    }
}

fn to_json(v: &T) -> Json {
    match v {
        T::String(s) => Json::String(s.clone()),
        T::Integer(i) => Json::from(*i),
        T::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        T::Boolean(b) => Json::Bool(*b),
        T::Datetime(d) => Json::String(d.to_string()),
        T::Array(a) => Json::Array(a.iter().map(to_json).collect()),
        T::Table(t) => Json::Object(t.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}

// ---- the field readers (config.py `_num`, `_bool`, `_str`, `_list`, `_opt_int`, `_table`) -----------------------------

type Tbl = toml::map::Map<String, T>;

fn ser_tbl<S: serde::Serializer>(t: &Tbl, s: S) -> Result<S::Ok, S::Error> {
    to_json(&T::Table(t.clone())).serialize(s)
}

impl Default for Disk {
    fn default() -> Self {
        Disk { path: String::new(), min_free_gb: Num::I(0), fast: false, r#use: vec![], removable: false, measure: "du".into() }
    }
}

impl Default for Needs {
    fn default() -> Self {
        Needs { preflight_gb: Num::I(20), lane_gates_gb: Num::I(40), chain_gb: Num::I(120), chain_needs_fast: true }
    }
}

impl Default for Host {
    fn default() -> Self {
        Host {
            name: String::new(),
            kind: "server".into(),
            roles: vec![],
            reach: "local".into(),
            os: "auto".into(),
            cores: None,
            max_parallel_steps: None,
            disks: vec![],
            cost_per_hour_usd: Num::I(0),
            max_pollers: None,
            awake_hours: None,
            enabled: true,
            tools: vec![],
            ports: vec![],
            reserved_port_ranges: vec![],
            container_prefix: None,
            watcher_max_age_min: None,
            worktrees: vec![],
            lock_file: None,
            lock_max_age_hours: Num::I(6),
            chain_pattern: None,
            cache_server_check: None,
            gate_scripts: vec![],
            tidy: Tbl::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config { profile: "mixed".into(), budget: Json::Object(Default::default()), needs: Needs::default(), hosts: vec![], models: vec![], tidy: Tbl::new(), stack: Json::Object(Default::default()) }
    }
}

impl Config {
    /// `[stack] generated_dirs` (validated as a list of strings by `parse`).
    pub fn generated_dirs(&self) -> Vec<String> {
        self.stack.get("generated_dirs").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
    }

    /// The host with this name.
    pub fn host(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.name == name)
    }
}

/// `doctor.host_os`: `auto` means this machine for a local host and Linux for a remote one.
pub fn host_os(h: &Host) -> &'static str {
    if h.os != "auto" {
        return if h.os == "windows" { "windows" } else { "linux" };
    }
    if h.reach == "local" {
        return if cfg!(windows) { "windows" } else { "linux" };
    }
    "linux"
}

fn tidy_table(d: &Tbl, w: &str) -> R<Tbl> {
    match d.get("tidy") {
        None => Ok(Tbl::new()),
        Some(T::Table(t)) => Ok(t.clone()),
        Some(_) => bad(format!("{w}.tidy: must be a table")),
    }
}

fn num(d: &Tbl, key: &str, w: &str, default: Num) -> R<Num> {
    match d.get(key) {
        None => Ok(default),
        Some(T::Integer(i)) => Ok(Num::I(*i)),
        Some(T::Float(f)) => Ok(Num::F(*f)),
        Some(v) => bad(format!("{w}.{key}: must be a number, got {}", repr(v))),
    }
}

fn boolean(d: &Tbl, key: &str, w: &str, default: bool) -> R<bool> {
    match d.get(key) {
        None => Ok(default),
        Some(T::Boolean(b)) => Ok(*b),
        Some(v) => bad(format!("{w}.{key}: must be true or false, got {}", repr(v))),
    }
}

fn string(d: &Tbl, key: &str, w: &str) -> R<Option<String>> {
    match d.get(key) {
        None => Ok(None),
        Some(T::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        Some(v) => bad(format!("{w}.{key}: must be a non-empty string, got {}", repr(v))),
    }
}

fn string_or(d: &Tbl, key: &str, w: &str, default: &str) -> R<String> {
    Ok(string(d, key, w)?.unwrap_or_else(|| default.to_string()))
}

fn required(d: &Tbl, key: &str, w: &str) -> R<String> {
    if !d.contains_key(key) {
        return bad(format!("{w}.{key}: required"));
    }
    Ok(string(d, key, w)?.unwrap_or_default())
}

fn strings(d: &Tbl, key: &str, w: &str, default: &[&str]) -> R<Vec<String>> {
    match d.get(key) {
        None => Ok(default.iter().map(|s| s.to_string()).collect()),
        Some(T::Array(a)) if a.iter().all(|x| matches!(x, T::String(_))) => Ok(a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
        Some(v) => bad(format!("{w}.{key}: must be a list of str, got {}", repr(v))),
    }
}

fn ints(d: &Tbl, key: &str, w: &str) -> R<Vec<i64>> {
    match d.get(key) {
        None => Ok(Vec::new()),
        Some(T::Array(a)) if a.iter().all(|x| matches!(x, T::Integer(_))) => Ok(a.iter().filter_map(|x| x.as_integer()).collect()),
        Some(v) => bad(format!("{w}.{key}: must be a list of int, got {}", repr(v))),
    }
}

fn opt_int(d: &Tbl, key: &str, w: &str) -> R<Option<i64>> {
    if !d.contains_key(key) {
        return Ok(None);
    }
    Ok(Some(match num(d, key, w, Num::I(0))? {
        Num::I(i) => i,
        Num::F(f) => f.trunc() as i64, // Python int() truncates toward zero
    }))
}

fn table(d: &Tbl, key: &str, w: &str) -> R<Json> {
    match d.get(key) {
        None => Ok(Json::Object(Default::default())),
        Some(v @ T::Table(_)) => Ok(to_json(v)),
        Some(_) => bad(format!("{w}.{key}: must be a table")),
    }
}

fn as_table<'a>(v: &'a T, w: &str) -> R<&'a Tbl> {
    v.as_table().ok_or_else(|| ConfigError::Invalid(format!("{w}: must be a table")))
}

fn py_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| py_str(s)).collect::<Vec<_>>().join(", "))
}

fn disk(v: &T, w: &str) -> R<Disk> {
    let d = as_table(v, w)?;
    let use_ = strings(d, "use", w, &USES)?;
    let unknown: Vec<String> = use_.iter().filter(|u| !USES.contains(&u.as_str())).cloned().collect();
    if !unknown.is_empty() {
        return bad(format!("{w}.use: unknown {}; allowed {}", py_list(&unknown), py_list(&USES.map(String::from))));
    }
    let measure = string_or(d, "measure", w, "du")?;
    if measure != "du" && measure != "age-only" {
        return bad(format!("{w}.measure: must be du or age-only, got {}", py_str(&measure)));
    }
    let path = required(d, "path", w)?;
    let min_free_gb = num(d, "min_free_gb", w, Num::I(0))?;
    let fast = boolean(d, "fast", w, false)?;
    let removable = boolean(d, "removable", w, false)?;
    Ok(Disk { path, min_free_gb, fast, r#use: use_, removable, measure })
}

const HOST_KEYS: [&str; 34] = ["awake_hours", "cache_server_check", "chain_pattern", "container_prefix", "cores", "cost_per_hour_usd", "disks", "enabled", "gate_scripts", "kind", "light_while_proving", "lock_file", "lock_max_age_hours", "max_parallel_steps", "max_pollers", "memory_check", "memory_gb", "memory_per_cargo_job_gb", "memory_per_step_gb", "memory_reserve_gb", "name", "os", "ports", "reach", "reserve_cores", "reserved_port_ranges", "roles", "step_default_gb", "tidy", "tools", "watcher_max_age_min", "worktrees", "wsl", "wsl_memory_gb"];

fn host(v: &T, i: usize) -> R<Host> {
    let w = format!("hosts[{i}]");
    let d = as_table(v, &w)?;
    let mut unknown: Vec<String> = d.keys().filter(|k| !HOST_KEYS.contains(&k.as_str())).cloned().collect();
    unknown.sort();
    if !unknown.is_empty() {
        // a misspelt key (memory_gb, step_default_gb ...) must be an error, never a setting that silently does nothing
        return bad(format!("{w}: unknown key(s) {}; allowed {}", py_list(&unknown), py_list(&HOST_KEYS.map(String::from))));
    }
    let kind = string_or(d, "kind", &w, "server")?;
    if !KINDS.contains(&kind.as_str()) {
        return bad(format!("{w}.kind: must be one of {}, got {}", py_list(&KINDS.map(String::from)), py_str(&kind)));
    }
    let os = string_or(d, "os", &w, "auto")?;
    if !OSES.contains(&os.as_str()) {
        return bad(format!("{w}.os: must be one of {}, got {}", py_list(&OSES.map(String::from)), py_str(&os)));
    }
    let mut disks = Vec::new();
    for (j, x) in d.get("disks").and_then(|x| x.as_array()).into_iter().flatten().enumerate() {
        disks.push(disk(x, &format!("{w}.disks[{j}]"))?);
    }
    let mut wts = Vec::new();
    for (j, x) in d.get("worktrees").and_then(|x| x.as_array()).into_iter().flatten().enumerate() {
        let ww = format!("{w}.worktrees[{j}]");
        let t = as_table(x, &ww)?;
        wts.push(Worktree { name: required(t, "name", &ww)?, build_dir: required(t, "build_dir", &ww)? });
    }
    // The remaining fields in the order config.py builds the Host, so the first error is the same one.
    let name = required(d, "name", &w)?;
    let roles = strings(d, "roles", &w, &[])?;
    let reach = string_or(d, "reach", &w, "local")?;
    let cores = opt_int(d, "cores", &w)?;
    let max_parallel_steps = opt_int(d, "max_parallel_steps", &w)?;
    let cost_per_hour_usd = num(d, "cost_per_hour_usd", &w, Num::I(0))?;
    let max_pollers = opt_int(d, "max_pollers", &w)?;
    let awake_hours = string(d, "awake_hours", &w)?;
    let enabled = boolean(d, "enabled", &w, true)?;
    let tools = strings(d, "tools", &w, &[])?;
    let ports = ints(d, "ports", &w)?;
    let reserved_port_ranges = strings(d, "reserved_port_ranges", &w, &[])?;
    let container_prefix = string(d, "container_prefix", &w)?;
    let watcher_max_age_min = if d.contains_key("watcher_max_age_min") { Some(num(d, "watcher_max_age_min", &w, Num::I(0))?) } else { None };
    let lock_file = string(d, "lock_file", &w)?;
    let lock_max_age_hours = num(d, "lock_max_age_hours", &w, Num::I(6))?;
    let chain_pattern = string(d, "chain_pattern", &w)?;
    let cache_server_check = string(d, "cache_server_check", &w)?;
    let gate_scripts = strings(d, "gate_scripts", &w, &[])?;
    let tidy = tidy_table(d, &w)?;
    Ok(Host {
        name, kind, roles, reach, os, cores, max_parallel_steps, disks, cost_per_hour_usd, max_pollers, awake_hours, enabled, tools, ports,
        reserved_port_ranges, container_prefix, watcher_max_age_min, worktrees: wts, lock_file, lock_max_age_hours, chain_pattern,
        cache_server_check, gate_scripts, tidy,
    })
}

/// The hosts.toml text, read by the same strict loader `doctor` uses: an unknown host key (a misspelt `memory_reserve_gbb`) is an error here too.
pub fn validate_text(text: &str) -> std::result::Result<(), String> {
    let t: Tbl = text.parse().map_err(|e: toml::de::Error| format!("hosts.toml: {e}"))?;
    parse(&t).map(|_| ()).map_err(|e| format!("hosts.toml: {e}"))
}

/// config.py `parse`.
pub fn parse(data: &Tbl) -> R<Config> {
    let profile = match data.get("profile") {
        None => "mixed".to_string(),
        Some(T::String(s)) if PROFILES.contains(&s.as_str()) => s.clone(),
        Some(v) => return bad(format!("profile: must be one of {}, got {}", py_list(&PROFILES.map(String::from)), repr(v))),
    };
    let empty = Tbl::new();
    let n = match data.get("needs") {
        None => &empty,
        Some(v) => as_table(v, "needs")?,
    };
    let needs = Needs {
        preflight_gb: num(n, "preflight_gb", "needs", Num::I(20))?,
        lane_gates_gb: num(n, "lane_gates_gb", "needs", Num::I(40))?,
        chain_gb: num(n, "chain_gb", "needs", Num::I(120))?,
        chain_needs_fast: boolean(n, "chain_needs_fast", "needs", true)?,
    };
    let mut hosts = Vec::new();
    for (i, h) in data.get("hosts").and_then(|x| x.as_array()).into_iter().flatten().enumerate() {
        hosts.push(host(h, i)?);
    }
    for (i, h) in hosts.iter().enumerate() {
        if hosts.iter().position(|x| x.name == h.name) != Some(i) {
            return bad(format!("hosts[{i}].name: duplicate {}", py_str(&h.name)));
        }
    }
    let mut models = Vec::new();
    for (i, m) in data.get("models").and_then(|x| x.as_array()).into_iter().flatten().enumerate() {
        let w = format!("models[{i}]");
        let t = as_table(m, &w)?;
        models.push(Model { name: required(t, "name", &w)?, kind: string_or(t, "kind", &w, "local")?, cost_per_mtok_usd: num(t, "cost_per_mtok_usd", &w, Num::I(0))? });
    }
    let budget = match data.get("budget") {
        None => Json::Object(Default::default()),
        Some(v @ T::Table(_)) => to_json(v),
        Some(_) => return bad("budget: must be a table".into()),
    };
    let stack = table(data, "stack", "")?;
    strings(data.get("stack").and_then(|x| x.as_table()).unwrap_or(&empty), "generated_dirs", "stack", &[])?;
    let tidy = tidy_table(data, "")?;
    Ok(Config { profile, budget, needs, hosts, models, tidy, stack })
}

/// `str(OSError)` of a missing file, the way Python prints it.
fn os_error(e: &std::io::Error, path: &str) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("[Errno 2] No such file or directory: {}", py_str(path))
    } else {
        format!("{path}: {e}")
    }
}

/// config.py `load`.
pub fn load(path: &std::path::Path) -> R<Config> {
    let text = std::fs::read(path).map_err(|e| ConfigError::Invalid(os_error(&e, &path.display().to_string())))?;
    let text = String::from_utf8_lossy(&text);
    let data: Tbl = text.parse::<toml::Table>().map_err(|e| ConfigError::Syntax { path: path.display().to_string(), detail: e.to_string() })?;
    parse(&data)
}

/// The corpus form: the Config as sorted-key JSON without whitespace. A TOML syntax error prints only `ERROR not valid TOML`.
pub fn dump_json(c: &Config) -> Result<String, serde_json::Error> {
    serde_json::to_string(&serde_json::to_value(c)?)
}

/// What `gatetrain config dump` prints for a file, as one line without the newline.
pub fn dump_line(path: &std::path::Path) -> String {
    match load(path) {
        Ok(c) => dump_json(&c).unwrap_or_else(|e| format!("ERROR {e}")),
        Err(ConfigError::Syntax { .. }) => "ERROR not valid TOML".into(),
        Err(e) => format!("ERROR {e}"),
    }
}
