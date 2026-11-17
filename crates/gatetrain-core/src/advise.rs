//! `gatetrain advise` (engine v2 E8, section 10.3): rules over what the ledger measured. It only reads and prints; nothing here
//! changes a config file, a lock or a run (V6). Unknown is a value: a rule that needs a figure the platform did not give does not fire,
//! and an advice with any unknown input is at most `low`.
//!
//! The rules are a pure function of [`Inputs`], so each has a fixture that fires it and one that does not; [`gather`] fills `Inputs`
//! from the ledger and the host's config.
use crate::analysis;
use crate::ledger::Ledger;
use crate::logfmt;
use crate::runs;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct StepInfo {
    pub name: String,
    /// seconds from the first step's start
    pub start_s: f64,
    pub dur_s: f64,
    pub shards: i64,
}

#[derive(Debug, Clone)]
pub struct RunInfo {
    pub id: String,
    pub wall_s: f64,
    pub busy_s: f64,
    /// average parallelism (shard slots counted)
    pub avg_par: f64,
    pub steps: Vec<StepInfo>,
}

#[derive(Debug, Clone)]
pub struct SampleInfo {
    pub run_id: Option<String>,
    pub t: f64,
    pub cpu_busy: Option<f64>,
    pub iowait: Option<f64>,
    pub stall_io_some: Option<f64>,
    pub stall_io_full: Option<f64>,
    pub stall_cpu_some: Option<f64>,
    pub mem_free_mb: Option<f64>,
    pub disks: Vec<(String, Option<f64>)>,
}

#[derive(Debug, Clone)]
pub struct DiskCfg {
    pub path: String,
    pub min_free_gb: f64,
    pub fast: bool,
    pub uses: Vec<String>,
}

/// The newest probe (and free space) of a configured disk. `None` = unknown; `rotational_reason` says why.
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub path: String,
    pub rotational: Option<bool>,
    pub rotational_reason: Option<String>,
    pub fsync_p50_ms: Option<f64>,
    pub free_gb: Option<f64>,
    pub total_gb: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ReclaimInfo {
    pub class: String,
    /// NaN = not measured (docker absent, a probe that timed out): never counted as zero
    pub gb: f64,
    pub how: String,
}

#[derive(Debug, Clone)]
pub struct Inputs {
    pub host: String,
    pub allowed_par: Option<i64>,
    pub cores: Option<i64>,
    pub memory_reserve_gb: Option<f64>,
    pub disks: Vec<DiskCfg>,
    pub disk_info: Vec<DiskInfo>,
    /// newest first is not required; the rules use medians
    pub runs: Vec<RunInfo>,
    pub samples: Vec<SampleInfo>,
    /// median size a build folder reached in earlier runs (the `target-size` rows), MB
    pub growth_mb: Option<f64>,
    pub reclaim: Vec<ReclaimInfo>,
    /// steps whose `run` string holds `&&` or `;`
    pub split_steps: Vec<String>,
    pub config_hash: String,
    /// age of the newest probe, days; None = never probed
    pub probe_age_days: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Minutes {
    /// minutes saved per chain, lowest and highest
    Range(f64, f64),
    Unknown(String),
    /// the advice prevents a problem rather than saving time
    None,
}

#[derive(Debug, Clone)]
pub struct Advice {
    pub rule: &'static str,
    pub subject: String,
    /// the config line to change (or the command to run)
    pub move_line: String,
    pub evidence: Vec<String>,
    pub minutes: Minutes,
    pub confidence: &'static str,
}

#[allow(clippy::ptr_arg)] // callers pass a temporary built by `collect()`, which needs the Vec type to be named
fn median(v: &mut Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

#[allow(clippy::ptr_arg)]
fn pctile(v: &mut Vec<f64>, p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[(((v.len() - 1) as f64) * p / 100.0).floor() as usize])
}

fn col(s: &[SampleInfo], f: impl Fn(&SampleInfo) -> Option<f64>) -> Vec<f64> {
    s.iter().filter_map(f).collect()
}

fn confidence(i: &Inputs, m: &Minutes) -> &'static str {
    if matches!(m, Minutes::Unknown(_)) {
        return "low"; // an advice with any unknown input is at most low
    }
    if i.runs.len() >= 3 && i.probe_age_days.is_some_and(|d| d < 30.0) {
        "medium"
    } else {
        "low"
    }
}

fn mk(i: &Inputs, rule: &'static str, subject: String, move_line: String, evidence: Vec<String>, minutes: Minutes) -> Advice {
    let confidence = confidence(i, &minutes);
    Advice { rule, subject, move_line, evidence, minutes, confidence }
}

fn mins(s: f64) -> f64 {
    s / 60.0
}

/// Every rule, in a fixed order.
pub fn advise(i: &Inputs) -> Vec<Advice> {
    let mut out = Vec::new();
    let allowed = i.allowed_par.filter(|p| *p > 0);
    let cpu = median(&mut col(&i.samples, |s| s.cpu_busy));
    let iow = median(&mut col(&i.samples, |s| s.iowait));
    let stall_cpu = median(&mut col(&i.samples, |s| s.stall_cpu_some));
    let mem_p05 = pctile(&mut col(&i.samples, |s| s.mem_free_mb), 5.0);
    let disk_busy_p50 = median(&mut i.samples.iter().filter_map(|s| s.disks.iter().filter_map(|d| d.1).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.max(x))))).collect());
    let avg_par = median(&mut i.runs.iter().map(|r| r.avg_par).collect());
    let wall = median(&mut i.runs.iter().map(|r| r.wall_s).collect());
    let busy = median(&mut i.runs.iter().map(|r| r.busy_s).collect());
    let reserve_mb = i.memory_reserve_gb.map(|g| g * 1024.0);

    // advise.mem: free memory under the reserve
    if let (Some(p05), Some(res)) = (mem_p05, reserve_mb) {
        if p05 < res {
            let new = allowed.map(|a| (a * 3 / 4).max(1));
            out.push(mk(
                i,
                "advise.mem",
                i.host.clone(),
                match new {
                    Some(n) => format!("lower parallelism so peak memory fits: max_parallel_steps = {n} (or CARGO_BUILD_JOBS)"),
                    None => "lower max_parallel_steps or CARGO_BUILD_JOBS".into(),
                },
                vec![format!("free memory p05 {p05:.0} MB is under the reserve {res:.0} MB ({} samples)", i.samples.len())],
                Minutes::Unknown("no measurement of time against memory pressure".into()),
            ));
        }
    }
    // advise.par-down: the host is full (CPU or I/O wait)
    let full = cpu.is_some_and(|c| c >= 95.0) || stall_cpu.is_some_and(|c| c >= 20.0) || iow.is_some_and(|w| w >= 20.0);
    if let (true, Some(a)) = (full, allowed) {
        let new = (a - (a / 4).max(1)).max(1);
        let mut ev = Vec::new();
        if let Some(c) = cpu {
            ev.push(format!("cpu busy p50 {c:.0}%"));
        }
        if let Some(w) = iow {
            ev.push(format!("iowait p50 {w:.0}%"));
        }
        if let Some(s) = stall_cpu {
            ev.push(format!("cpu stall p50 {s:.0}%"));
        }
        ev.push(format!("allowed parallelism {a} over {} samples", i.samples.len()));
        out.push(mk(
            i,
            "advise.par-down",
            i.host.clone(),
            format!("max_parallel_steps = {new} (now {a})"),
            ev,
            Minutes::Unknown("needs a regression of step time on parallelism: at least 3 runs at different values".into()),
        ));
    }
    // advise.plan-shape: the host has room and the plan is serial
    if let (Some(a), Some(avg), Some(c), Some(w), Some(b)) = (allowed, avg_par, cpu, wall, busy) {
        let mem_ok = match (mem_p05, reserve_mb) {
            (Some(m), Some(r)) => m >= r,
            (Some(_), None) => true,
            _ => false,
        };
        if avg < 0.75 * a as f64 && c < 60.0 && disk_busy_p50.is_none_or(|d| d <= 70.0) && mem_ok {
            let longest = median(&mut i.runs.iter().map(|r| r.steps.iter().map(|s| s.dur_s).fold(0.0, f64::max)).collect()).unwrap_or(0.0);
            let bound = longest.max(b / a as f64);
            let saved = mins((w - bound).max(0.0));
            let mut ev = vec![format!("average parallelism {avg:.1} of {a} allowed"), format!("cpu busy p50 {c:.0}%"), format!("wall {} s, longest step {longest:.0} s, busy {b:.0} s", w.round())];
            if let Some(r) = i.runs.last() {
                if let Some(alone) = alone_stretch(r) {
                    ev.push(alone);
                }
            }
            out.push(mk(i, "advise.plan-shape", i.host.clone(), "the host has room: look at the needs, tier barriers or exclusive steps behind the stretches with few steps running".into(), ev, Minutes::Range(saved, saved)));
        }
    }
    // advise.shard-step: the longest step, when the host has free slots
    if let (Some(a), Some(avg), Some(w)) = (allowed, avg_par, wall) {
        if let Some((name, l, next)) = longest_step(&i.runs) {
            let sharded = i.runs.iter().any(|r| r.steps.iter().any(|s| s.name == name && s.shards > 1));
            let free = a - avg.ceil() as i64;
            if !sharded && l >= 0.15 * w && avg < a as f64 && free >= 2 {
                let n = free.min(4);
                let saved_hi = (l * (1.0 - 1.0 / n as f64)).min(l - next);
                if saved_hi > 0.0 {
                    out.push(mk(
                        i,
                        "advise.shard-step",
                        name.clone(),
                        format!("[[step]] {name}: shards = {n}"),
                        vec![
                            format!("{name} is the longest step: {l:.0} s of a {w:.0} s wall ({:.0}%)", 100.0 * l / w),
                            format!("average parallelism {avg:.1} of {a}: {free} free slots"),
                            "the range assumes an even split; the low end halves the saving for contention".into(),
                        ],
                        Minutes::Range(mins(saved_hi * 0.5), mins(saved_hi)),
                    ));
                }
            }
        }
    }
    disk_rules(i, &mut out, wall);
    // advise.reclaim: space that tidy could give back, by class
    let known: Vec<&ReclaimInfo> = i.reclaim.iter().filter(|r| r.gb.is_finite()).collect();
    let total: f64 = known.iter().map(|r| r.gb).sum();
    if total >= 5.0 {
        let mut ev: Vec<String> = known.iter().map(|r| format!("{}: {:.1} GB reclaimable ({})", r.class, r.gb, r.how)).collect();
        ev.extend(i.reclaim.iter().filter(|r| !r.gb.is_finite()).map(|r| format!("{}: not measured ({})", r.class, r.how)));
        let mut classes: Vec<&str> = Vec::new();
        for r in &known {
            let c = if r.class.contains("image") {
                "images"
            } else if r.class.contains("cache") {
                "cache"
            } else if r.class.contains("target") {
                "targets"
            } else {
                continue;
            };
            if !classes.contains(&c) {
                classes.push(c);
            }
        }
        out.push(mk(i, "advise.reclaim", i.host.clone(), format!("gatetrain tidy --class {} (a dry run; add --apply to remove)", classes.join(",")), ev, Minutes::None));
    }
    // advise.flag-contradicts-probe: `fast = true` on a disk the probe measured slower than a disk without the flag
    for d in i.disks.iter().filter(|d| d.fast) {
        let mine = i.disk_info.iter().find(|x| x.path == d.path).and_then(|x| x.fsync_p50_ms);
        let best_slow = i.disks.iter().filter(|o| !o.fast && o.path != d.path).filter_map(|o| i.disk_info.iter().find(|x| x.path == o.path).and_then(|x| x.fsync_p50_ms)).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.min(x))));
        if let (Some(m), Some(b)) = (mine, best_slow) {
            if m > b {
                out.push(mk(
                    i,
                    "advise.flag-contradicts-probe",
                    d.path.clone(),
                    format!("[[hosts.disks]] path = {:?}: fast = false (or fix the disk)", d.path),
                    vec![format!("{} is flagged fast but its durable write p50 is {m:.1} ms; a disk without the flag measured {b:.1} ms", d.path)],
                    Minutes::None,
                ));
            }
        }
    }
    // advise.split-step: a `run` string with `&&` or `;` can hide a red
    for s in &i.split_steps {
        out.push(mk(i, "advise.split-step", s.clone(), "split it into steps joined by `needs`: one red must not hide behind another".into(), vec![format!("the run string of {s} holds `&&` or `;`")], Minutes::None));
    }
    out
}

fn alone_stretch(r: &RunInfo) -> Option<String> {
    let mut ev: Vec<(f64, i64, &str)> = Vec::new();
    for s in &r.steps {
        ev.push((s.start_s, 1, &s.name));
        ev.push((s.start_s + s.dur_s, -1, &s.name));
    }
    ev.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    let (mut cur, mut best, mut at): (i64, f64, f64) = (0, 0.0, 0.0);
    let mut best_names = String::new();
    let mut active: Vec<&str> = Vec::new();
    for (t, d, n) in ev {
        if cur == 1 && t - at > best {
            best = t - at;
            best_names = active.join(", ");
        }
        if d == 1 {
            active.push(n);
        } else {
            active.retain(|x| *x != n);
        }
        cur += d;
        at = t;
    }
    (best > 0.0).then(|| format!("the longest stretch with one step running is {best:.0} s ({best_names})"))
}

/// (name, median duration, next longest median) of the longest step over the runs.
fn longest_step(runs: &[RunInfo]) -> Option<(String, f64, f64)> {
    let mut names: Vec<String> = Vec::new();
    for r in runs {
        for s in &r.steps {
            if !names.contains(&s.name) {
                names.push(s.name.clone());
            }
        }
    }
    let mut meds: Vec<(String, f64)> = names.into_iter().filter_map(|n| median(&mut runs.iter().filter_map(|r| r.steps.iter().find(|s| s.name == n).map(|s| s.dur_s)).collect()).map(|m| (n, m))).collect();
    meds.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let first = meds.first()?.clone();
    Some((first.0, first.1, meds.get(1).map_or(0.0, |x| x.1)))
}

fn disk_rules(i: &Inputs, out: &mut Vec<Advice>, wall: Option<f64>) {
    let chain_disk = i.disks.iter().find(|d| d.uses.iter().any(|u| u == "chain")).or_else(|| i.disks.first());
    // advise.disk-headroom: free space against the floor, and chains of headroom when the folder's growth is measured
    for d in &i.disks {
        let Some(info) = i.disk_info.iter().find(|x| x.path == d.path) else { continue };
        let Some(free) = info.free_gb else { continue };
        let growth_gb = i.growth_mb.map(|m| m / 1024.0).filter(|g| *g > 0.0);
        let chains = growth_gb.map(|g| ((free - d.min_free_gb) / g).max(0.0));
        let low = free < d.min_free_gb || chains.is_some_and(|c| c < 3.0);
        if low {
            let mut ev = vec![format!("{}: {free:.1} GB free, the floor is {:.1} GB", d.path, d.min_free_gb)];
            ev.push(match chains {
                Some(c) => format!("{c:.1} chains of headroom: ({free:.1} - {:.1}) GB / {:.1} GB median folder growth", d.min_free_gb, growth_gb.unwrap_or(0.0)),
                None => "chains of headroom: unknown (no build-folder growth recorded yet; it is stated only when measured)".into(),
            });
            out.push(mk(i, "advise.disk-headroom", d.path.clone(), "free space: gatetrain tidy, or move the build folder to a larger disk".into(), ev, Minutes::None));
        }
    }
    // the chain disk is busy: I/O bound
    let Some(cd) = chain_disk else { return };
    let n = i.samples.len();
    if n == 0 {
        return;
    }
    let busy_hi = i.samples.iter().filter(|s| s.disks.iter().any(|d| d.1.is_some_and(|b| b >= 80.0))).count();
    let stall_hi = i.samples.iter().filter(|s| s.stall_io_some.is_some_and(|x| x >= 10.0)).count();
    let share = busy_hi.max(stall_hi) as f64 / n as f64;
    if share < 0.2 {
        return;
    }
    let Some(info) = i.disk_info.iter().find(|x| x.path == cd.path) else {
        out.push(mk(
            i,
            "advise.disk-bound-unknown",
            cd.path.clone(),
            "gatetrain probe --config hosts.toml --host NAME".into(),
            vec![format!("I/O busy or stalled in {:.0}% of {n} samples, but {} has never been probed", 100.0 * share, cd.path)],
            Minutes::Unknown("no probe of this disk".into()),
        ));
        return;
    };
    let mut ev = vec![format!("disk busy at least 80% or I/O stall at least 10% in {:.0}% of {n} samples", 100.0 * share)];
    match info.rotational {
        None => {
            ev.push(format!("rotational of {}: unknown ({})", cd.path, info.rotational_reason.clone().unwrap_or_else(|| "no reason recorded".into())));
            out.push(mk(i, "advise.disk-bound-unknown", cd.path.clone(), "ask the host that owns the disk (for the Linux subsystem on Windows: the Windows side) or probe it".into(), ev, Minutes::Unknown("rotational is unknown, so no move is chosen".into())));
        }
        Some(false) => {}
        Some(true) => {
            ev.push(format!("{} is rotational (read from the OS), durable write p50 {}", cd.path, info.fsync_p50_ms.map_or("unknown".into(), |x| format!("{x:.1} ms"))));
            let need_gb = i.growth_mb.map(|m| m / 1024.0);
            // another disk of the host: not rotational (or at most a third of the latency), with room for the folder plus its floor
            let target = i.disks.iter().filter(|d| d.path != cd.path).find_map(|d| {
                let x = i.disk_info.iter().find(|x| x.path == d.path)?;
                let faster = x.rotational == Some(false) || matches!((x.fsync_p50_ms, info.fsync_p50_ms), (Some(n), Some(o)) if n * 3.0 <= o);
                let room = match (x.free_gb, need_gb) {
                    (Some(f), Some(g)) => f >= g + d.min_free_gb,
                    (Some(_), None) => true, // the folder size is unknown: stated below, not guessed
                    _ => false,
                };
                (faster && room).then(|| (d.clone(), x.clone()))
            });
            // the stall formula: saved = W x share x (1 - L_new / L_old), with `full` for the low end and `some` for the high end
            let frac = |f: fn(&SampleInfo) -> Option<f64>| median(&mut col(&i.samples, f)).map(|p| p / 100.0);
            let (full, some) = (frac(|s| s.stall_io_full), frac(|s| s.stall_io_some));
            match target {
                Some((d, x)) => {
                    ev.push(format!("{} has {} GB free and a durable write p50 of {}", d.path, x.free_gb.map_or("unknown".into(), |f| format!("{f:.0}")), x.fsync_p50_ms.map_or("unknown".into(), |m| format!("{m:.1} ms"))));
                    if need_gb.is_none() {
                        ev.push("the build folder's size is not recorded, so room on the target is not checked".into());
                    }
                    let m = match (wall, full, some, x.fsync_p50_ms, info.fsync_p50_ms) {
                        (Some(w), Some(f), Some(s), Some(ln), Some(lo)) if lo > 0.0 => Minutes::Range(mins(w * f * (1.0 - ln / lo)), mins(w * s * (1.0 - ln / lo))),
                        _ => Minutes::Unknown("this platform reports no I/O stall, or a probe latency is missing".into()),
                    };
                    let mut a = mk(i, "advise.target-to-fast-disk", cd.path.clone(), format!("point the build folder at {} (target_dirs)", d.path), ev, m);
                    if need_gb.is_none() {
                        a.confidence = "low";
                    }
                    out.push(a);
                }
                None => {
                    let need = need_gb.map_or("unknown".into(), |g| format!("{:.0} GB", g + cd.min_free_gb));
                    ev.push(format!("no other disk of the host qualifies (needs room for the folder plus its floor: {need})"));
                    out.push(mk(i, "advise.add-device", cd.path.clone(), format!("add a solid-state disk of at least {need}, use = [\"chain\"]"), ev, Minutes::Unknown("no probe of a faster disk to compare against".into())));
                }
            }
        }
    }
}

/// An id stable for the same advice under the same config: a repeated `advise` writes nothing new.
pub fn advice_id(host: &str, a: &Advice, config_hash: &str) -> String {
    let mut h = Sha256::new();
    for p in [host, a.rule, &a.subject, config_hash] {
        h.update(p.len().to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Store the advice printed (insert-only; the same advice under the same config is stored once). Returns the ids.
pub fn record(l: &Ledger, host: &str, advice: &[Advice], config_hash: &str, at: &str) -> crate::Result<Vec<String>> {
    l.require_current()?;
    let mut ids = Vec::new();
    for a in advice {
        let id = advice_id(host, a, config_hash);
        let (lo, hi) = match a.minutes {
            Minutes::Range(lo, hi) => (Some(lo), Some(hi)),
            _ => (None, None),
        };
        l.conn.execute(
            "INSERT OR IGNORE INTO advice(advice_id,at,rule,host,subject,move_json,minutes_low,minutes_high,confidence,evidence,config_hash) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            rusqlite::params![id, at, a.rule, host, a.subject, serde_json::json!({ "move": a.move_line }).to_string(), lo, hi, a.confidence, serde_json::json!(a.evidence).to_string(), config_hash],
        )?;
        ids.push(id);
    }
    Ok(ids)
}

/// Close the loop: for an earlier advice on this host whose config hash differs from the current one, compare the wall time of each run
/// that finished after it with the wall time before it. The check row is the only way an advice reaches `high`. Returns how many
/// rows were written.
pub fn close_loop(l: &Ledger, host: &str, current_hash: &str, now_at: &str) -> crate::Result<usize> {
    l.require_current()?;
    let mut st = l.conn.prepare("SELECT advice_id, at, minutes_low, minutes_high FROM advice WHERE host=? AND config_hash<>? ORDER BY at")?;
    let adv: Vec<(String, String, Option<f64>, Option<f64>)> = st.query_map(rusqlite::params![host, current_hash], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut wrote = 0;
    for (id, at, lo, hi) in adv {
        let walls = |after: bool| -> crate::Result<Vec<(String, f64)>> {
            let mut st = l.conn.prepare(&format!("SELECT id, started_at, ended_at FROM runs WHERE host=? AND kind='chain' AND outcome='green' AND started_at {} ? ORDER BY started_at", if after { ">" } else { "<=" }))?;
            let rows: Vec<(String, String, Option<String>)> = st.query_map(rusqlite::params![host, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|(rid, s, e)| {
                    let (s, e) = (logfmt::parse_iso(&s).ok()?, logfmt::parse_iso(&e?).ok()?);
                    Some((rid, e.secs_since(&s)))
                })
                .collect())
        };
        let before = median(&mut walls(false)?.into_iter().map(|x| x.1).collect());
        for (rid, w) in walls(true)? {
            let measured = before.map(|b| mins(b - w));
            let verdict = match (measured, lo, hi) {
                (Some(m), Some(lo), Some(hi)) if m >= lo * 0.9 && m <= hi * 1.1 => "within the predicted range",
                (Some(_), Some(_), Some(_)) => "outside the predicted range",
                (Some(_), _, _) => "no prediction to compare",
                _ => "no earlier run to compare",
            };
            wrote += l.conn.execute("INSERT OR IGNORE INTO advice_checks(advice_id,run_id,at,measured_minutes,verdict) VALUES(?,?,?,?,?)", rusqlite::params![id, rid, now_at, measured, verdict])?;
        }
    }
    Ok(wrote)
}

/// Fill [`Inputs`] from the ledger: the newest `n_runs` finished chain runs of `host`, the samples of their window, the newest probe of
/// every configured disk, and the median build-folder growth. `cfg` and `reclaim` come from the caller (the config file, `docker`).
#[allow(clippy::too_many_arguments)]
pub fn gather(l: &Ledger, host: &str, cfg: Option<&crate::config::Host>, n_runs: usize, now_epoch_us: i64, reclaim: Vec<ReclaimInfo>, split_steps: Vec<String>, config_hash: String) -> crate::Result<Inputs> {
    let ids: Vec<String> = {
        let mut st = l.conn.prepare("SELECT id FROM runs WHERE host=? AND kind='chain' AND outcome IN ('green','red') ORDER BY started_at DESC LIMIT ?")?;
        let v = st.query_map(rusqlite::params![host, n_runs as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        v
    };
    let mut run_infos = Vec::new();
    let mut windows: Vec<(String, i64, i64)> = Vec::new();
    for id in &ids {
        let Some(loaded) = runs::load(l, id)? else { continue };
        let t0 = loaded.steps.iter().filter_map(|s| s.start).map(|t| t.instant_us()).min();
        let t1 = loaded.steps.iter().filter_map(|s| s.end()).map(|t| t.instant_us()).max();
        let (Some(t0), Some(t1)) = (t0, t1) else { continue };
        let c = analysis::concurrency(&loaded.steps);
        let shard_n = |name: &str| l.conn.query_row("SELECT COALESCE(shards,1) FROM steps WHERE run_id=? AND name=?", rusqlite::params![id, name], |r| r.get::<_, i64>(0)).unwrap_or(1);
        let steps = loaded.steps.iter().filter_map(|s| Some(StepInfo { name: s.name.clone(), start_s: (s.start?.instant_us() - t0) as f64 / 1e6, dur_s: s.duration?, shards: shard_n(&s.name) })).collect::<Vec<_>>();
        run_infos.push(RunInfo { id: id.clone(), wall_s: (t1 - t0) as f64 / 1e6, busy_s: loaded.steps.iter().map(|s| s.duration.unwrap_or(0.0)).sum(), avg_par: c.avg, steps });
        windows.push((id.clone(), t0, t1));
    }
    // samples: rows of these runs, or rows of this host inside a run's window
    let mut samples = Vec::new();
    {
        let mut st = l.conn.prepare("SELECT at, run_id, cpu_busy_pct, iowait_pct, stall_io_some, stall_io_full, stall_cpu_some, mem_free_mb FROM host_samples WHERE host=? ORDER BY at")?;
        let rows = st.query_map([host], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Option<f64>>(4)?, r.get::<_, Option<f64>>(5)?, r.get::<_, Option<f64>>(6)?, r.get::<_, Option<f64>>(7)?))
        })?;
        for r in rows {
            let (at, rid, cpu, iow, sio, sif, scpu, mem) = r?;
            let t = logfmt::parse_iso(&at).map(|t| t.instant_us()).unwrap_or(0);
            let inside = windows.iter().any(|(id, a, b)| rid.as_deref() == Some(id.as_str()) || (rid.is_none() && t >= *a && t <= *b));
            if !inside {
                continue;
            }
            let mut ds = Vec::new();
            let mut dst = l.conn.prepare("SELECT device, busy_pct FROM disk_samples WHERE host=? AND at=?")?;
            for d in dst.query_map(rusqlite::params![host, at], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<f64>>(1)?)))? {
                ds.push(d?);
            }
            samples.push(SampleInfo { run_id: rid, t: t as f64 / 1e6, cpu_busy: cpu, iowait: iow, stall_io_some: sio, stall_io_full: sif, stall_cpu_some: scpu, mem_free_mb: mem, disks: ds });
        }
    }
    // the newest probe of every configured disk
    let mut disks = Vec::new();
    let mut disk_info = Vec::new();
    let mut newest_probe: Option<String> = None;
    for d in cfg.map(|c| c.disks.as_slice()).unwrap_or(&[]) {
        disks.push(DiskCfg { path: d.path.clone(), min_free_gb: d.min_free_gb.f(), fast: d.fast, uses: d.r#use.clone() });
        let row = l
            .conn
            .query_row(
                "SELECT at, rotational, rotational_source, fsync_p50_ms, free_gb, total_gb FROM disk_probes WHERE host=? AND path=? ORDER BY at DESC LIMIT 1",
                rusqlite::params![host, d.path],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Option<f64>>(4)?, r.get::<_, Option<f64>>(5)?)),
            )
            .ok();
        if let Some((at, rot, why, p50, free, total)) = row {
            if newest_probe.as_ref().is_none_or(|n| at > *n) {
                newest_probe = Some(at);
            }
            disk_info.push(DiskInfo { path: d.path.clone(), rotational: rot.map(|r| r == 1), rotational_reason: why, fsync_p50_ms: p50, free_gb: free, total_gb: total });
        }
    }
    let probe_age_days = newest_probe.and_then(|at| logfmt::parse_iso(&at).ok()).map(|t| (now_epoch_us - t.instant_us()) as f64 / 86_400_000_000.0);
    let growth_mb = {
        let mut v: Vec<f64> = Vec::new();
        let mut st = l.conn.prepare("SELECT peak_mb FROM step_memory WHERE source='target-size'")?;
        for r in st.query_map([], |r| r.get::<_, f64>(0))? {
            v.push(r?);
        }
        median(&mut v)
    };
    Ok(Inputs {
        host: host.to_string(),
        allowed_par: cfg.and_then(|c| c.max_parallel_steps),
        cores: cfg.and_then(|c| c.cores),
        memory_reserve_gb: None,
        disks,
        disk_info,
        runs: run_infos,
        samples,
        growth_mb,
        reclaim,
        split_steps,
        config_hash,
        probe_age_days,
    })
}
