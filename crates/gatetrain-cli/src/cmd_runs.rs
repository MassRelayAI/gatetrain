//! `ingest`, `status`, `next-tick`, `flaky`, `stop` and the ledger side of `report`, `trace` and `eta`.
use anyhow::{Context, Result};
use gatetrain_core::ledger::Ledger;
use gatetrain_core::logfmt::{self, ParsedLog, Ts};
use gatetrain_core::{analysis, doctor, history, predict, runner, runs, stop, trace};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub fn now_or(s: &Option<String>) -> Result<Ts> {
    Ok(match s {
        Some(t) => logfmt::parse_iso(t)?,
        None => Ts::now_utc(),
    })
}

pub fn read_log(p: &Path) -> Result<(String, ParsedLog)> {
    let bytes = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let parsed = logfmt::parse_log(&text)?;
    Ok((text, parsed))
}

pub fn run_id(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// A ledger ready for writes: created when missing, brought to the current version (forward only).
pub fn open_for_write(db: &str) -> Result<Ledger> {
    let mut l = if db == ":memory:" { Ledger::open_in_memory()? } else { Ledger::open(Path::new(db)).with_context(|| format!("opening {db}"))? };
    l.migrate()?;
    Ok(l)
}

pub struct IngestArgs {
    pub log: PathBuf,
    pub db: String,
    pub run_id: Option<String>,
    pub host: Option<String>,
    pub replace: bool,
    pub stale_min: f64,
    pub now: Option<String>,
}

pub fn ingest(a: IngestArgs) -> Result<ExitCode> {
    let (_, p) = read_log(&a.log)?;
    let rid = a.run_id.clone().unwrap_or_else(|| run_id(&a.log));
    let l = open_for_write(&a.db)?;
    let outcome = history::classify(&p, &now_or(&a.now)?, a.stale_min);
    match runs::ingest(&l, &a.db, &rid, &p, outcome, a.host.as_deref(), a.replace, &runs::now_iso())? {
        runs::Ingest::Refused(m) => {
            eprintln!("{m}");
            Ok(ExitCode::from(2))
        }
        runs::Ingest::Done => {
            // the sampler's file beside the log (`<log>.samples.jsonl`, written by `hold --log`): ingested with the log; absent = nothing
            let mut sf = a.log.clone().into_os_string();
            sf.push(".samples.jsonl");
            if let Ok(text) = std::fs::read_to_string(&sf) {
                let n = l.record_samples_for(&text, Some(&rid))?;
                if n > 0 {
                    println!("ingested {n} sample(s) of {rid}");
                }
            }
            let reds = p.steps.iter().filter(|s| matches!(s.exit_code, Some(x) if x != 0)).count();
            println!("ingested {rid}: {outcome}, {} steps, {reds} red", p.steps.len());
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub fn flaky(db: &str) -> Result<ExitCode> {
    let l = open_for_write(db)?;
    let rows = runs::flaky(&l)?;
    if rows.is_empty() {
        println!("no flaky steps");
        return Ok(ExitCode::SUCCESS);
    }
    println!("step  commit  green_runs  red_runs");
    for (n, c, g, r) in rows {
        println!("{n}  {}  {g}  {r}", c.unwrap_or_else(|| "None".into()));
    }
    Ok(ExitCode::SUCCESS)
}

/// (minutes, why, run id, gates done) for one log.
fn suggest(db: &Path, log: &Path, hist: Option<&history::History>, now_arg: &Option<String>, lo: i64, hi: i64) -> Result<(i64, String, String, bool)> {
    let (_, p) = read_log(log)?;
    let rid = run_id(log);
    let owned;
    let hist = match hist {
        Some(h) => h,
        None => {
            owned = history::history_durations(db, &rid).context("the ledger could not be read")?;
            &owned
        }
    };
    let now = now_or(now_arg)?;
    let reference = p.last_ts().unwrap_or(now);
    let rem = if !hist.is_empty() && !p.gates_done { Some(predict::predict(&p.steps, hist, reference).eta.secs_since(&now).max(0.0)) } else { None };
    let reds: Vec<Option<Ts>> = p.red_now.iter().map(|r| r.2).collect();
    let (m, why) = predict::next_tick(p.first_ts(), &reds, rem, now, lo, hi);
    Ok((m, why, rid, p.gates_done))
}

pub fn next_tick(logs: Vec<PathBuf>, db: &Path, now: Option<String>, lo: i64, hi: i64) -> Result<ExitCode> {
    if logs.is_empty() {
        eprintln!("--log is required (the running chain's log)");
        return Ok(ExitCode::from(1));
    }
    if logs.len() == 1 {
        let (m, why, _, _) = suggest(db, &logs[0], None, &now, lo, hi)?;
        println!("{m} min - {why}");
        return Ok(ExitCode::SUCCESS);
    }
    let got: Vec<_> = logs.iter().map(|l| suggest(db, l, None, &now, lo, hi)).collect::<Result<_>>()?;
    let live: Vec<_> = got.iter().filter(|g| !g.3).collect();
    let pool: Vec<_> = if live.is_empty() { got.iter().collect() } else { live };
    let best = pool.iter().fold(pool[0], |b, g| if g.0 < b.0 { g } else { b });
    println!("{} min - {} (decided by {})", best.0, best.1, best.2);
    Ok(ExitCode::SUCCESS)
}

pub struct StatusArgs {
    pub logs: Vec<PathBuf>,
    pub db: String,
    pub now: Option<String>,
    pub stale_min: f64,
    pub lo: i64,
    pub hi: i64,
}

fn hm(t: &Ts) -> String {
    t.hms()[..5].to_string()
}

pub fn status(a: StatusArgs) -> Result<ExitCode> {
    let l = open_for_write(&a.db)?;
    let now = now_or(&a.now)?;
    for lg in &a.logs {
        let (_, p) = read_log(lg)?;
        let rid = run_id(lg);
        let outcome = history::classify(&p, &now, a.stale_min);
        match runs::outcome_of(&l, &rid)? {
            None => {
                runs::ingest(&l, &a.db, &rid, &p, outcome, None, false, &runs::now_iso())?;
            }
            Some(o) if o == "running" || o == "stopped" => {
                // an older-format row cannot be replaced (nothing is deleted); the refusal is not an error here
                runs::ingest(&l, &a.db, &rid, &p, outcome, None, true, &runs::now_iso())?;
            }
            Some(_) => {}
        }
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut live: Vec<(i64, String, String)> = Vec::new();
    for lg in &a.logs {
        let (_, p) = read_log(lg)?;
        let rid = run_id(lg);
        let outcome = history::classify(&p, &now, a.stale_min);
        let hist = history::history_from(&l.conn, &rid).context("the ledger could not be read")?;
        let mut known: Vec<&str> = p.steps.iter().map(|s| s.name.as_str()).collect();
        for (n, _) in &hist {
            if !known.contains(&n.as_str()) {
                known.push(n);
            }
        }
        known.sort();
        known.dedup();
        let done = p.steps.iter().filter(|s| s.exit_code.is_some() || s.duration.is_some()).count();
        let mut red: Vec<&str> = p.steps.iter().filter(|s| matches!(s.exit_code, Some(x) if x != 0)).map(|s| s.name.as_str()).collect();
        red.extend(p.red_now.iter().map(|r| r.0.as_str()));
        red.sort();
        red.dedup();
        let reference = p.last_ts().unwrap_or(now);
        let (mut eta, mut band) = ("-".to_string(), "-".to_string());
        if !p.steps.is_empty() && outcome == "running" {
            let pr = predict::predict(&p.steps, &hist, reference);
            eta = hm(&pr.eta);
            band = format!("{}-{}", hm(&pr.lo), hm(&pr.hi));
        }
        let mut next = "-".to_string();
        if outcome == "running" {
            let (m, why, _, _) = suggest(Path::new(""), lg, Some(&hist), &a.now, a.lo, a.hi)?;
            next = m.to_string();
            live.push((m, rid.clone(), why));
        }
        rows.push(vec![rid, p.host.clone().unwrap_or_else(|| "-".into()), format!("{done}/{}", known.len()), red.len().to_string(), outcome.to_string(), eta, band, next]);
    }
    let head: Vec<String> = ["run", "host", "steps", "reds", "state", "eta", "band", "next look (min)"].iter().map(|s| s.to_string()).collect();
    let mut table = vec![head];
    table.extend(rows);
    let w: Vec<usize> = (0..8).map(|i| table.iter().map(|r| r[i].chars().count()).max().unwrap_or(0)).collect();
    for t in &table {
        let line: Vec<String> = (0..8).map(|i| format!("{:<width$}", t[i], width = w[i])).collect();
        println!("{}", line.join("  ").trim_end());
    }
    match live.iter().fold(None::<&(i64, String, String)>, |b, x| if b.is_none_or(|b| x.0 < b.0) { Some(x) } else { b }) {
        Some((m, rid, why)) => println!("next look: {m} min ({rid}: {why})"),
        None => println!("next look: none (no run is active)"),
    }
    Ok(ExitCode::SUCCESS)
}

pub fn stop_cmd(pattern: &str, followers: Vec<String>, yes: bool, config: Option<PathBuf>, host: Option<String>) -> Result<ExitCode> {
    let mut r: Box<dyn runner::Runner> = Box::new(runner::LocalRunner);
    let mut linux = cfg!(target_os = "linux");
    if let (Some(c), Some(h)) = (&config, &host) {
        let cfg = gatetrain_core::config::load(c).map_err(|e| anyhow::anyhow!("config error: {e}"))?;
        let Some(hh) = cfg.host(h) else {
            eprintln!("no host {h} in {}", c.display());
            return Ok(ExitCode::from(1));
        };
        linux = doctor::host_os(hh).starts_with("linux");
        match doctor::make_runner(hh) {
            Some(x) => r = x,
            None => {
                eprintln!("host {h} has reach {:?}; stop needs local or ssh", hh.reach);
                return Ok(ExitCode::from(1));
            }
        }
    }
    if !linux {
        println!("not supported: gatetrain stop works on Linux only");
        return Ok(ExitCode::from(2));
    }
    let mut out = std::io::stdout();
    match stop::stop(r.as_ref(), pattern, &followers, yes, &mut out, &|s| std::thread::sleep(std::time::Duration::from_secs(s)), &stop::own_pids()) {
        Ok(rc) => Ok(ExitCode::from(rc as u8)),
        Err(e) => {
            eprintln!("gatetrain stop: {e}");
            Ok(ExitCode::from(2))
        }
    }
}

/// What `report`, `trace` and `eta` read: a ledger run or a log.
pub struct Src {
    pub id: String,
    pub outcome: String,
    pub commit: Option<String>,
    pub host: Option<String>,
    pub steps: Vec<logfmt::Step>,
}

pub fn load_run(db: &Path, rid: &str) -> Result<Option<Src>> {
    if !db.exists() {
        return Ok(None);
    }
    let l = Ledger::open(db)?;
    Ok(runs::load(&l, rid)?.map(|x| Src { id: rid.to_string(), outcome: x.outcome, commit: x.commit, host: x.host, steps: x.steps }))
}

pub fn latest_run(db: &Path) -> Result<Option<String>> {
    if !db.exists() {
        return Ok(None);
    }
    Ok(runs::latest(&Ledger::open(db)?)?)
}

pub fn report_text(s: &Src) -> String {
    let meta = analysis::Meta { outcome: &s.outcome, commit: s.commit.as_deref(), host: s.host.as_deref() };
    analysis::report_text(&s.id, &meta, &s.steps)
}

pub fn trace_text(s: &Src) -> String {
    trace::trace_json(&s.steps)
}
