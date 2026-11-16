//! Pre-run checks per host. Port of gatetrain/doctor.py, byte for byte in text and `--json`.
//! Each check takes `(host, runner, needs)` and returns results; the runner is injected ([`crate::runner::Runner`]).
//!
//! SEAM for lane rs2-hold (memory checks): add a `Check { name, run }` line to [`CHECKS`] and put the function in its own
//! file. Nothing else in this file needs to change; `run_checks` reports an `Err` from any check as a FAIL named after it.
use crate::config::{Config, Host, Needs};
use crate::portscan::{scan_random_ports, uncovered};
use crate::pycompat::{fmt_g, is_digits, normpath, repr_list, repr_str, shlex_quote, splitlines, J};
use crate::runner::Runner;
use regex::Regex;

pub const BUILD_USES: [&str; 3] = ["preflight", "lane-gates", "chain"];
const WIN_PS: &str = "powershell -NoProfile -Command \"Get-CimInstance Win32_Process | ForEach-Object { $_.ProcessId.ToString() + ' ' + $_.CommandLine }\"";

#[derive(Debug, Clone, PartialEq)]
pub struct DResult {
    pub host: String,
    pub check: String,
    pub status: String, // PASS | WARN | FAIL
    pub lessons: Vec<String>,
    pub message: String,
    pub fix: String,
}

impl DResult {
    pub fn new(host: &str, check: &str, status: &str, lessons: &[&str], message: String, fix: &str) -> DResult {
        DResult { host: host.into(), check: check.into(), status: status.into(), lessons: lessons.iter().map(|s| s.to_string()).collect(), message, fix: fix.into() }
    }
    pub fn as_json(&self) -> J {
        J::Obj(vec![
            ("host".into(), J::s(&self.host)),
            ("check".into(), J::s(&self.check)),
            ("status".into(), J::s(&self.status)),
            ("lessons".into(), J::strs(&self.lessons)),
            ("message".into(), J::s(&self.message)),
            ("fix".into(), J::s(&self.fix)),
        ])
    }
}

type Out = Result<Vec<DResult>, String>;

pub fn host_os(h: &Host) -> String {
    crate::config::host_os(h).to_string()
}

fn win(h: &Host) -> bool {
    host_os(h) == "windows"
}

fn q(s: &str, os: &str) -> String {
    if os == "windows" { format!("\"{s}\"") } else { shlex_quote(s) }
}

fn skip(h: &Host, check: &str, lessons: &[&str], why: &str) -> Out {
    Ok(vec![DResult::new(&h.name, check, "WARN", lessons, format!("skipped on {}: {}", host_os(h), why), "run this check on a host where it is supported")])
}

/// `s.split(None, maxsplit)`: whitespace-separated, the rest kept as is after its leading whitespace.
fn split_none(s: &str, maxsplit: usize) -> Vec<String> {
    let mut out = vec![];
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        if out.len() == maxsplit {
            out.push(rest.to_string());
            return out;
        }
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push(rest[..end].to_string());
        rest = rest[end..].trim_start();
    }
    out
}

/// Free space in GB at `path`, or None when the path cannot be read (missing, not mounted).
pub fn free_gb(h: &Host, runner: &dyn Runner, path: &str) -> Result<Option<f64>, String> {
    if win(h) {
        let (rc, out) = runner.run(&crate::winprobe::free_bytes_cmd(path))?;
        let s = out.trim();
        return Ok(if rc == 0 && is_digits(s) { s.parse::<f64>().ok().map(|v| v / 1024f64.powi(3)) } else { None });
    }
    let (rc, out) = runner.run(&format!("df -Pk {}", q(path, "linux")))?;
    let lines: Vec<String> = splitlines(&out).into_iter().filter(|l| !l.trim().is_empty()).collect();
    if rc != 0 || lines.len() < 2 {
        return Ok(None);
    }
    let cols: Vec<&str> = lines.last().unwrap().split_whitespace().collect();
    Ok(if cols.len() >= 4 && is_digits(cols[3]) { cols[3].parse::<f64>().ok().map(|v| v / 1024f64.powi(2)) } else { None })
}

fn check_disks(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let mut out = vec![];
    for d in &h.disks {
        match free_gb(h, runner, &d.path)? {
            None => {
                if d.removable {
                    out.push(DResult::new(&h.name, "disk", "WARN", &["L18"], format!("{}: removable disk is not mounted", d.path), &format!("attach and mount {}; what it enables is dropped meanwhile", d.path)));
                } else {
                    out.push(DResult::new(&h.name, "disk", "FAIL", &["L18", "L30"], format!("{}: cannot read free space (missing or not mounted)", d.path), &format!("mount {} or correct the path in the config", d.path)));
                }
                continue;
            }
            Some(f) if f < d.min_free_gb.f() => out.push(DResult::new(
                &h.name,
                "disk",
                "FAIL",
                &["L18", "L30"],
                format!("{}: {:.0} GB free, need {}", d.path, f, fmt_g(d.min_free_gb.f())),
                &format!("free space on {} (prune build outputs of merged lanes) before starting a run", d.path),
            )),
            Some(f) => out.push(DResult::new(&h.name, "disk", "PASS", &["L18", "L30"], format!("{}: {:.0} GB free >= {}", d.path, f, fmt_g(d.min_free_gb.f())), "")),
        }
        let build_uses: Vec<String> = d.r#use.iter().filter(|u| BUILD_USES.contains(&u.as_str())).cloned().collect();
        if !d.fast && !build_uses.is_empty() {
            out.push(DResult::new(
                &h.name,
                "disk-speed",
                "WARN",
                &["L19"],
                format!("{}: allowed for {} but not marked fast", d.path, repr_list(&build_uses)),
                &format!("put build outputs on a fast disk, or limit {} to use = [\"archive\"]", d.path),
            ));
        }
    }
    Ok(out)
}

fn check_capacity(h: &Host, runner: &dyn Runner, needs: &Needs) -> Out {
    let kinds: Vec<&str> = BUILD_USES.iter().copied().filter(|k| h.roles.iter().any(|r| r == k)).collect();
    if kinds.is_empty() {
        return Ok(vec![]);
    }
    let need = |k: &str| match k {
        "preflight" => needs.preflight_gb.f(),
        "lane-gates" => needs.lane_gates_gb.f(),
        _ => needs.chain_gb.f(),
    };
    let mut free: Vec<(String, Option<f64>)> = vec![];
    for d in &h.disks {
        let f = free_gb(h, runner, &d.path)?;
        match free.iter_mut().find(|(p, _)| *p == d.path) {
            Some(e) => e.1 = f,
            None => free.push((d.path.clone(), f)),
        }
    }
    let free_of = |p: &str| free.iter().find(|(x, _)| x == p).and_then(|(_, f)| *f);
    let fits = |d: &crate::config::Disk, k: &str| {
        if !d.r#use.iter().any(|u| u == k) || (k == "chain" && needs.chain_needs_fast && !d.fast) {
            return false;
        }
        free_of(&d.path).map(|f| f >= need(k).max(d.min_free_gb.f())).unwrap_or(false)
    };
    let (mut ok, mut no, mut hint) = (vec![], vec![], vec![]);
    for k in &kinds {
        if h.disks.iter().any(|d| fits(d, k)) {
            ok.push(*k);
            continue;
        }
        no.push(*k);
        for d in &h.disks {
            if d.removable && free_of(&d.path).is_none() && d.r#use.iter().any(|u| u == k) && (*k != "chain" || d.fast || !needs.chain_needs_fast) {
                hint.push(format!("{} (not mounted) would enable {}", d.path, k));
            }
        }
    }
    let mut msg = format!("can run now: {}", if ok.is_empty() { "nothing".to_string() } else { ok.join(", ") });
    if !no.is_empty() {
        let parts: Vec<String> = no
            .iter()
            .map(|k| format!("{} (needs {} GB free on an allowed{} disk)", k, fmt_g(need(k)), if *k == "chain" && needs.chain_needs_fast { " fast" } else { "" }))
            .collect();
        msg += &format!("; cannot: {}", parts.join(", "));
    }
    if !hint.is_empty() {
        msg += &format!("; {}", hint.join("; "));
    }
    let chain_lost = no.contains(&"chain");
    let status = if chain_lost && !hint.iter().any(|x| x.contains("would enable chain")) {
        "FAIL"
    } else if !no.is_empty() {
        "WARN"
    } else {
        "PASS"
    };
    let fix = if status == "PASS" { "" } else { "add or mount a disk with use including the missing kind, enough free space and fast = true for chains" };
    Ok(vec![DResult::new(&h.name, "capacity", status, &["L18", "L19"], msg, fix)])
}

fn check_containers(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let Some(prefix) = &h.container_prefix else { return Ok(vec![]) };
    let (rc, out) = runner.run("docker ps -a --filter status=exited --format \"{{.Names}}\"")?;
    if rc != 0 {
        return Ok(vec![DResult::new(&h.name, "containers", "WARN", &["L31"], "docker not available; leftover containers not checked".into(), "install or start docker, or drop container_prefix")]);
    }
    let names: Vec<String> = splitlines(&out).iter().map(|n| n.trim().to_string()).filter(|n| n.starts_with(prefix.as_str())).collect();
    if names.is_empty() {
        return Ok(vec![DResult::new(&h.name, "containers", "PASS", &["L31"], format!("no exited container with prefix {prefix}"), "")]);
    }
    Ok(vec![DResult::new(
        &h.name,
        "containers",
        "FAIL",
        &["L31"],
        format!("{} leftover exited container(s): {}", names.len(), names.join(", ")),
        &format!("docker rm {}  (doctor never removes; exact names only)", names.join(" ")),
    )])
}

fn check_watchers(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let Some(max_age) = h.watcher_max_age_min.map(|n| n.f()) else { return Ok(vec![]) };
    if win(h) {
        return skip(h, "watchers", &["L24"], "needs ps with elapsed time");
    }
    let (rc, out) = runner.run("ps -eo pid=,etimes=,args=")?;
    if rc != 0 {
        return Ok(vec![DResult::new(&h.name, "watchers", "WARN", &["L24"], "cannot list processes".into(), "check ps on the host")]);
    }
    let mut stray: Vec<(String, i64, String)> = vec![];
    for line in splitlines(&out) {
        let p = split_none(&line, 2);
        if p.len() < 3 || !is_digits(&p[0]) || !is_digits(&p[1]) {
            continue;
        }
        let words: Vec<&str> = p[2].split_whitespace().collect();
        let etimes: f64 = p[1].parse().unwrap_or(f64::MAX);
        if let Some(w0) = words.first() {
            if w0.rsplit('/').next() == Some("tail") && (words.contains(&"-F") || words.contains(&"--follow=name")) && etimes > max_age * 60.0 {
                stray.push((p[0].clone(), p[1].parse::<i64>().unwrap_or(i64::MAX) / 60, p[2].clone()));
            }
        }
    }
    if stray.is_empty() {
        return Ok(vec![DResult::new(&h.name, "watchers", "PASS", &["L24"], format!("no tail -F older than {} min", fmt_g(max_age)), "")]);
    }
    let msg = stray.iter().map(|(a, m, c)| format!("pid {a} up {m} min: {c}")).collect::<Vec<_>>().join("; ");
    Ok(vec![DResult::new(&h.name, "watchers", "FAIL", &["L24"], msg, &format!("kill {}  (by exact pid)", stray.iter().map(|s| s.0.as_str()).collect::<Vec<_>>().join(" ")))])
}

fn check_tools(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let mut out = vec![];
    for t in &h.tools {
        let (rc, o) = runner.run(&format!("{t} --version"))?;
        let first = splitlines(o.trim()).into_iter().next().unwrap_or_default();
        if rc == 0 && !first.is_empty() {
            out.push(DResult::new(&h.name, "toolchain", "PASS", &["L29"], format!("{t}: {first}"), ""));
        } else {
            out.push(DResult::new(&h.name, "toolchain", "FAIL", &["L29"], format!("{t}: does not resolve or prints no version"), &format!("install {t} or fix the PATH or version file the chain uses")));
        }
    }
    Ok(out)
}

type Range3 = (i64, i64, &'static str);

fn digits(s: &str) -> Vec<i64> {
    Regex::new(r"[0-9]+").unwrap().find_iter(s).filter_map(|m| m.as_str().parse().ok()).collect()
}

fn ranges_linux(runner: &dyn Runner) -> Result<Option<Vec<Range3>>, String> {
    let (rc, o) = runner.run("cat /proc/sys/net/ipv4/ip_local_port_range")?;
    let n = digits(&o);
    Ok(if rc == 0 && n.len() >= 2 { Some(vec![(n[0], n[1], "ephemeral range")]) } else { None })
}

/// Ports the kernel keeps out of the ephemeral pool (`ip_local_reserved_ports`, e.g. "55400-58399,6000"): a fixed test port
/// inside the ephemeral range is safe when it is reserved here (L21).
fn kernel_reserved_linux(runner: &dyn Runner) -> Result<Vec<(i64, i64)>, String> {
    let (rc, o) = runner.run("cat /proc/sys/net/ipv4/ip_local_reserved_ports")?;
    if rc != 0 {
        return Ok(vec![]);
    }
    let rx = Regex::new(r"^\s*([0-9]+)(?:\s*-\s*([0-9]+))?\s*$").unwrap();
    let mut out = vec![];
    for part in o.trim().split(',') {
        if let Some(m) = rx.captures(part) {
            let a: i64 = m[1].parse().unwrap_or(0);
            out.push((a, m.get(2).and_then(|b| b.as_str().parse().ok()).unwrap_or(a)));
        }
    }
    Ok(out)
}

fn ranges_windows(runner: &dyn Runner) -> Result<Option<Vec<Range3>>, String> {
    let (rc, o) = runner.run("netsh int ipv4 show dynamicport tcp")?;
    let s = Regex::new(r"Start Port\s*:\s*([0-9]+)").unwrap().captures(&o).map(|c| c[1].parse::<i64>().unwrap_or(0));
    let c = Regex::new(r"Number of Ports\s*:\s*([0-9]+)").unwrap().captures(&o).map(|c| c[1].parse::<i64>().unwrap_or(0));
    let (Some(s), Some(c)) = (s, c) else { return Ok(None) };
    if rc != 0 {
        return Ok(None);
    }
    let mut out = vec![(s, s + c - 1, "dynamic range")];
    let (_rc, o) = runner.run("netsh int ipv4 show excludedportrange protocol=tcp")?;
    for m in Regex::new(r"(?m)^\s*([0-9]+)\s+([0-9]+)").unwrap().captures_iter(&o) {
        out.push((m[1].parse().unwrap_or(0), m[2].parse().unwrap_or(0), "OS-excluded range"));
    }
    Ok(Some(out))
}

fn reserved_ranges(h: &Host) -> Vec<(i64, i64)> {
    let rx = Regex::new(r"^\s*([0-9]+)\s*-\s*([0-9]+)\s*$").unwrap();
    h.reserved_port_ranges.iter().filter_map(|r| rx.captures(r).map(|m| (m[1].parse().unwrap_or(0), m[2].parse().unwrap_or(0)))).collect()
}

fn check_ports(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    if h.ports.is_empty() {
        return Ok(vec![]);
    }
    let mut ranges = if win(h) { ranges_windows(runner)? } else { ranges_linux(runner)? };
    let extra: Vec<Range3> = reserved_ranges(h).into_iter().map(|(a, b)| (a, b, "reserved range")).collect();
    let mut out = vec![];
    if ranges.is_none() {
        out.push(DResult::new(&h.name, "ports", "WARN", &["L21"], "cannot read the OS port ranges; only reserved ranges and duplicates checked".into(), "check the ephemeral and excluded ranges by hand"));
        ranges = Some(vec![]);
    }
    let kernel_reserved = if win(h) { vec![] } else { kernel_reserved_linux(runner)? };
    let all: Vec<Range3> = ranges.unwrap().into_iter().chain(extra).collect();
    for &p in &h.ports {
        let hit = all.iter().find(|(a, b, _)| *a <= p && p <= *b);
        if let Some((_, _, "ephemeral range")) = hit {
            if kernel_reserved.iter().any(|(a, b)| *a <= p && p <= *b) {
                out.push(DResult::new(&h.name, "ports", "PASS", &["L21"], format!("port {p} is inside the ephemeral range but reserved by the kernel (ip_local_reserved_ports)"), ""));
                continue;
            }
        }
        match hit {
            Some((a, b, w)) => out.push(DResult::new(&h.name, "ports", "FAIL", &["L21"], format!("port {p} is inside the {w} {a}-{b}"), &format!("move port {p} outside {a}-{b}"))),
            None => out.push(DResult::new(&h.name, "ports", "PASS", &["L21"], format!("port {p} is outside every ephemeral or reserved range"), "")),
        }
    }
    let mut dup: Vec<i64> = h.ports.iter().copied().filter(|p| h.ports.iter().filter(|x| *x == p).count() > 1).collect();
    dup.sort();
    dup.dedup();
    if !dup.is_empty() {
        out.push(DResult::new(&h.name, "ports", "FAIL", &["L22"], format!("duplicate port(s): {}", dup.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")), "give every gate its own port"));
    }
    Ok(out)
}

/// Random port defaults in gate scripts whose range reaches the host's ephemeral range collide with outgoing connections (L52).
fn check_random_ports(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    if h.gate_scripts.is_empty() {
        return Ok(vec![]);
    }
    let w = win(h);
    let os = host_os(h);
    let names = h.gate_scripts.iter().map(|p| q(p, &os)).collect::<Vec<_>>().join(" ");
    let cmd = if w {
        format!("for %f in ({names}) do @(echo @@FILE %f & type \"%f\")")
    } else {
        format!("for f in {names}; do echo \"@@FILE $f\"; cat \"$f\"; done")
    };
    let (rc, out) = runner.run(&cmd)?;
    if rc != 0 && out.is_empty() {
        return Ok(vec![DResult::new(&h.name, "random-ports", "WARN", &["L52"], "cannot read the gate scripts".into(), "check the gate_scripts paths")]);
    }
    let ranges = if w { ranges_windows(runner)? } else { ranges_linux(runner)? };
    let eph = ranges
        .unwrap_or_default()
        .iter()
        .find(|(_, _, k)| *k == "ephemeral range" || *k == "dynamic range")
        .map(|(a, b, _)| (*a, *b))
        .unwrap_or(if w { (49152, 65535) } else { (32768, 60999) });
    let mut reserved = if w { vec![] } else { kernel_reserved_linux(runner)? };
    reserved.extend(reserved_ranges(h));
    let mut body: Vec<(String, Vec<String>)> = vec![];
    let mut cur: Option<usize> = None;
    for ln in splitlines(&out) {
        if let Some(rest) = ln.strip_prefix("@@FILE ") {
            let name = rest.trim().to_string();
            match body.iter().position(|(n, _)| *n == name) {
                Some(i) => {
                    body[i].1.clear();
                    cur = Some(i)
                }
                None => {
                    body.push((name, vec![]));
                    cur = Some(body.len() - 1)
                }
            }
        } else if let Some(i) = cur {
            body[i].1.push(ln);
        }
    }
    let mut res = vec![];
    for (name, lines) in &body {
        for (n, lo, hi) in scan_random_ports(&lines.join("\n")) {
            if !uncovered(lo, hi, eph, &reserved).is_empty() {
                res.push(DResult::new(
                    &h.name,
                    "random-ports",
                    "FAIL",
                    &["L52"],
                    format!("{name}:{n} picks a random port in {lo}-{hi}, which reaches the ephemeral range {}-{}", eph.0, eph.1),
                    "keep random port ranges below the ephemeral range, for example 20000-31999",
                ));
            } else {
                res.push(DResult::new(&h.name, "random-ports", "PASS", &["L52"], format!("{name}:{n} random range {lo}-{hi} stays clear of the ephemeral range"), ""));
            }
        }
    }
    Ok(res)
}

fn norm(path: &str, os: &str) -> String {
    let p = normpath(&path.replace('\\', "/"));
    if os == "windows" { p.to_lowercase() } else { p }
}

fn check_shared_build(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    if h.worktrees.is_empty() {
        return Ok(vec![]);
    }
    let os = host_os(h);
    let mut groups: Vec<(String, Vec<String>)> = vec![];
    for w in &h.worktrees {
        let mut key = norm(&w.build_dir, &os);
        if os != "windows" {
            let (rc, o) = runner.run(&format!("realpath {}", shlex_quote(&w.build_dir)))?;
            if rc == 0 && !o.trim().is_empty() {
                key = o.trim().to_string();
            }
        }
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some(g) => g.1.push(w.name.clone()),
            None => groups.push((key, vec![w.name.clone()])),
        }
    }
    let shared: Vec<&(String, Vec<String>)> = groups.iter().filter(|(_, v)| v.len() > 1).collect();
    if shared.is_empty() {
        return Ok(vec![DResult::new(&h.name, "shared-build", "PASS", &["L17"], format!("{} worktree(s), each with its own build output folder", h.worktrees.len()), "")]);
    }
    let msg = shared.iter().map(|(k, v)| format!("{} share {}", v.join(", "), k)).collect::<Vec<_>>().join("; ");
    Ok(vec![DResult::new(&h.name, "shared-build", "FAIL", &["L17"], msg, "point each worktree at its own build output folder")])
}

/// L68: a build folder that is a dangling link (or gone) kills every cargo step within seconds; each must resolve to a directory.
fn check_build_folders(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    if h.worktrees.is_empty() {
        return Ok(vec![]);
    }
    let os = host_os(h);
    let mut bad: Vec<String> = vec![];
    for w in &h.worktrees {
        let cmd = if os == "windows" {
            format!("if exist \"{}\\\" (echo dir) else (echo missing)", w.build_dir.replace('/', "\\").trim_end_matches('\\'))
        } else {
            let q = shlex_quote(&w.build_dir);
            format!("test -d {q} && echo dir || {{ test -L {q} && echo dangling || echo missing; }}")
        };
        let (rc, o) = runner.run(&cmd)?;
        let kind = if rc == 0 && !o.trim().is_empty() { o.trim().lines().last().unwrap_or("").trim().to_string() } else { "unknown".to_string() };
        if kind != "dir" {
            bad.push(format!("{}: {} is {}", w.name, w.build_dir, kind));
        }
    }
    if bad.is_empty() {
        return Ok(vec![DResult::new(&h.name, "build-folders", "PASS", &["L68"], format!("{} build folder(s), each resolves to a directory", h.worktrees.len()), "")]);
    }
    Ok(vec![DResult::new(&h.name, "build-folders", "FAIL", &["L68"], bad.join("; "), "re-create the folder each build link points to (or fix the link) before any build step")])
}

fn check_lock(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let Some(lock) = &h.lock_file else { return Ok(vec![]) };
    if win(h) {
        return skip(h, "lock", &["L32"], "needs stat and date");
    }
    let qq = shlex_quote(lock);
    let (rc, m) = runner.run(&format!("stat -c %Y {qq}"))?;
    if rc != 0 {
        return Ok(vec![DResult::new(&h.name, "lock", "PASS", &["L32"], format!("{lock}: no lock file"), "")]);
    }
    let (_, n) = runner.run("date +%s")?;
    let age_h = match (n.trim().parse::<i64>(), m.trim().parse::<i64>()) {
        (Ok(n), Ok(m)) => (n - m) as f64 / 3600.0,
        _ => return Ok(vec![DResult::new(&h.name, "lock", "WARN", &["L32"], "cannot read the lock age".into(), "check stat and date on the host")]),
    };
    if age_h <= h.lock_max_age_hours.f() {
        return Ok(vec![DResult::new(&h.name, "lock", "PASS", &["L32"], format!("{lock}: {:.1} h old, within {} h", age_h, fmt_g(h.lock_max_age_hours.f())), "")]);
    }
    let (_, c) = runner.run(&format!("grep -c released {qq}"))?;
    if is_digits(c.trim()) && c.trim().parse::<i64>().unwrap_or(0) > 0 {
        return Ok(vec![DResult::new(&h.name, "lock", "PASS", &["L32"], format!("{lock}: {:.1} h old but has a released line", age_h), "")]);
    }
    Ok(vec![DResult::new(
        &h.name,
        "lock",
        "FAIL",
        &["L32"],
        format!("{lock}: {:.1} h old with no released line (holder died?)", age_h),
        "find the holder, then remove the lock by hand after checking no chain is running",
    )])
}

fn check_chain(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let Some(pat) = &h.chain_pattern else { return Ok(vec![]) };
    let (rc, o) = runner.run(if win(h) { WIN_PS } else { "ps -eo pid=,args=" })?;
    if rc != 0 {
        return Ok(vec![DResult::new(&h.name, "chain", "WARN", &["L26"], "cannot list processes".into(), "check ps on the host")]);
    }
    let rx = Regex::new(pat).map_err(|e| e.to_string())?;
    let mut hits = vec![];
    for line in splitlines(&o) {
        let p = split_none(line.trim(), 1);
        if p.len() == 2 && is_digits(&p[0]) && rx.is_match(&p[1]) && !p[1].contains("ps -eo") && !p[1].contains("Get-CimInstance") {
            hits.push(p[0].clone());
        }
    }
    if !hits.is_empty() {
        return Ok(vec![DResult::new(&h.name, "chain", "WARN", &["L26"], format!("a chain is running (pid {}): logs and light reads only", hits.join(", ")), "wait for the chain to finish before heavy work on this host")]);
    }
    Ok(vec![DResult::new(&h.name, "chain", "PASS", &["L26"], "no chain running".into(), "")])
}

fn check_cache(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let Some(c) = &h.cache_server_check else { return Ok(vec![]) };
    let (rc, _) = runner.run(c)?;
    if rc == 0 {
        return Ok(vec![DResult::new(&h.name, "cache", "PASS", &["L20"], "shared cache server answers".into(), "")]);
    }
    Ok(vec![DResult::new(&h.name, "cache", "FAIL", &["L20"], format!("cache server check failed (rc={rc})"), "restart the server with its own environment and no idle timeout")])
}

fn check_pollers(h: &Host, runner: &dyn Runner, _n: &Needs) -> Out {
    let Some(max) = h.max_pollers else { return Ok(vec![]) };
    if win(h) {
        return skip(h, "pollers", &["L25"], "counts sshd sessions with ps");
    }
    let (_, o) = runner.run("ps -eo args=")?;
    let rx = Regex::new(r"^sshd: \S+@").unwrap();
    let n = splitlines(&o).iter().filter(|l| rx.is_match(l.trim())).count() as i64;
    if n > max {
        return Ok(vec![DResult::new(&h.name, "pollers", "FAIL", &["L25"], format!("{n} ssh sessions > max_pollers {max}"), "stop extra pollers; use one watcher per log")]);
    }
    Ok(vec![DResult::new(&h.name, "pollers", "PASS", &["L25"], format!("{n} ssh sessions <= max_pollers {max}"), "")])
}

pub type CheckFn = fn(&Host, &dyn Runner, &Needs) -> Out;

pub struct Check {
    /// the name a crashed check is reported under (the Python function name without `check_`)
    pub name: &'static str,
    pub run: CheckFn,
}

/// The checks, in the order they run and print. Lane rs2-hold appends its memory check here.
pub const CHECKS: &[Check] = &[
    Check { name: "disks", run: check_disks },
    Check { name: "capacity", run: check_capacity },
    Check { name: "containers", run: check_containers },
    Check { name: "watchers", run: check_watchers },
    Check { name: "tools", run: check_tools },
    Check { name: "ports", run: check_ports },
    Check { name: "random_ports", run: check_random_ports },
    Check { name: "shared_build", run: check_shared_build },
    Check { name: "build_folders", run: check_build_folders },
    Check { name: "lock", run: check_lock },
    Check { name: "chain", run: check_chain },
    Check { name: "cache", run: check_cache },
    Check { name: "pollers", run: check_pollers },
];

pub fn run_checks(h: &Host, runner: &dyn Runner, needs: &Needs) -> Vec<DResult> {
    let mut out = vec![];
    for c in CHECKS {
        match (c.run)(h, runner, needs) {
            Ok(mut r) => out.append(&mut r),
            // a check that cannot run is a failure, never a silent pass
            Err(e) => out.push(DResult::new(&h.name, c.name, "FAIL", &[], format!("check could not run: {e}"), "fix the connection or the command")),
        }
    }
    out
}

/// What `reach` means: `local` or `ssh <name>`; anything else has no runner.
pub fn make_runner(h: &Host) -> Option<Box<dyn Runner>> {
    crate::runner::for_reach(&h.reach)
}

/// Run every enabled host (or only `only`). `Err` is the message of the Python `SystemExit`.
pub fn run_doctor(cfg: &Config, only: Option<&str>, make: &dyn Fn(&Host) -> Option<Box<dyn Runner>>) -> Result<Vec<DResult>, String> {
    let hosts: Vec<&Host> = cfg.hosts.iter().filter(|h| only.map(|o| h.name == o).unwrap_or(true)).collect();
    if let Some(o) = only {
        if hosts.is_empty() {
            return Err(format!("no host {o} in the config"));
        }
    }
    let mut out = vec![];
    for h in hosts {
        if !h.enabled && only.is_none() {
            continue;
        }
        match make(h) {
            None => out.push(DResult::new(&h.name, "reach", "WARN", &[], format!("reach {} is not local or ssh; nothing checked", repr_str(&h.reach)), "use reach = \"local\" or \"ssh <name>\" to check this host")),
            Some(r) => out.extend(run_checks(h, r.as_ref(), &cfg.needs)),
        }
    }
    Ok(out)
}

pub fn format_text(results: &[DResult]) -> String {
    let mut lines = vec![];
    for r in results {
        let les = if r.lessons.is_empty() { "-".to_string() } else { r.lessons.join(",") };
        let mut line = format!("{:<4}  {:<7}  {} {}: {}", r.status, les, r.host, r.check, r.message);
        if r.status != "PASS" && !r.fix.is_empty() {
            line += &format!("  -> fix: {}", r.fix);
        }
        lines.push(line);
    }
    lines.join("\n")
}

pub fn exit_code(results: &[DResult]) -> i32 {
    if results.iter().any(|r| r.status == "FAIL") { 1 } else { 0 }
}

pub fn to_json(results: &[DResult]) -> J {
    J::Arr(results.iter().map(|r| r.as_json()).collect())
}
