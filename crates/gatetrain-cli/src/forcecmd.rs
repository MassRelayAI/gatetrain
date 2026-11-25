//! The forced, measured and landed commands: `measure`, `tidy --force`, `land`, `verdict`, `main-watch`, `branches`.
//! The logic is `gatetrain_core::force`; this file reads the arguments, the config and the hosts, and prints.
use clap::Args;
use gatetrain_core::config::{self, host_os, Config, Host};
use gatetrain_core::force::cfg::{self, ForceCfg};
use gatetrain_core::force::exec::{LocalExec, RunnerExec};
use gatetrain_core::force::land::{self, LandArgs};
use gatetrain_core::force::measure::{self, Measure};
use gatetrain_core::force::tidyforce::{self, OpsHost};
use gatetrain_core::ledger::Ledger;
use gatetrain_core::tidy::config::{build_cfg, TidyCfg};
use gatetrain_core::tidy::inventory::gather;
use gatetrain_core::tidy::make_runner;
use std::path::Path;
use std::process::ExitCode;

fn who() -> String {
    std::env::var("GATETRAIN_WHO").or_else(|_| std::env::var("USERNAME")).or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "unknown".into())
}

fn force_cfg(path: &str) -> Result<ForceCfg, String> {
    if !Path::new(path).exists() {
        eprintln!("note: {path} not found; the defaults apply (no path rules: every change is SAFE)");
        return Ok(ForceCfg::default());
    }
    cfg::load(path)
}

fn ledger_rw(db: &str) -> Result<Ledger, String> {
    let mut l = Ledger::open(Path::new(db)).map_err(|e| format!("opening {db}: {e}"))?;
    l.migrate().map_err(|e| format!("{db}: {e}"))?;
    Ok(l)
}

fn ledger_ro(db: &str) -> Option<Ledger> {
    if !Path::new(db).exists() {
        return None;
    }
    ledger_rw(db).ok()
}

struct HostCtx<'a> {
    host: &'a Host,
    tidy: TidyCfg,
}

fn hosts<'a>(c: &'a Config, only: Option<&str>) -> Result<Vec<HostCtx<'a>>, String> {
    let only = only.filter(|h| !h.is_empty());
    let hs: Vec<&Host> = c.hosts.iter().filter(|h| if let Some(n) = only { h.name == n } else { h.enabled }).collect();
    if let (Some(n), true) = (only, hs.is_empty()) {
        return Err(format!("no host {n} in the config"));
    }
    hs.into_iter().map(|h| build_cfg(c, h).map(|t| HostCtx { host: h, tidy: t }).map_err(|e| format!("config error: {e}"))).collect()
}

// ---------------------------------------------------------------- measure

#[derive(Args)]
pub struct MeasureArgs {
    #[arg(long, default_value = "hosts.toml")]
    config: String,
    #[arg(long)]
    host: Option<String>,
    /// JSON instead of a table
    #[arg(long)]
    json: bool,
}

pub fn measure_cmd(a: MeasureArgs) -> ExitCode {
    let run = || -> Result<(), String> {
        let c = config::load(Path::new(&a.config)).map_err(|e| format!("config error: {e}"))?;
        let fc = force_cfg(&a.config)?;
        let mut all = vec![];
        for hc in hosts(&c, a.host.as_deref())? {
            let Some(r) = make_runner(hc.host) else {
                eprintln!("{}: skipped, reach {:?} is not local or ssh", hc.host.name, hc.host.reach);
                continue;
            };
            let win = host_os(hc.host) == "windows";
            let inv = gather(hc.host, r.as_ref(), &hc.tidy, None).map_err(|e| format!("{}: {e}", hc.host.name))?;
            let ex = RunnerExec(r.as_ref());
            let mut rows = measure::from_inventory(&inv, win);
            rows.extend(measure::docker_rows(&ex));
            let vm: Vec<String> = fc
                .vm_disks
                .iter()
                .filter_map(|v| match v.split_once('=') {
                    Some((h, p)) if !p.starts_with('=') && !h.contains(['/', '\\', ':']) => (h == hc.host.name).then(|| p.to_string()),
                    _ => Some(v.clone()),
                })
                .collect();
            rows.extend(measure::vm_rows(&ex, win, &vm));
            all.push(Measure { host: hc.host.name.clone(), rows, free: inv.disks.iter().map(|(p, f, _)| (p.clone(), *f)).collect() });
        }
        if a.json {
            println!("{}", serde_json::to_string_pretty(&serde_json::Value::Array(all.iter().map(measure::to_json).collect())).unwrap());
        } else {
            for m in &all {
                print!("{}", measure::table(m));
            }
        }
        Ok(())
    };
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gatetrain: {e}");
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------- tidy --force

pub fn tidy_force(config_path: &str, host: Option<&str>, keep: Option<&str>, apply: bool) -> ExitCode {
    let run = || -> Result<i32, String> {
        let c = config::load(Path::new(config_path)).map_err(|e| format!("config error: {e}"))?;
        let fc = force_cfg(config_path)?;
        let keep = keep.map(regex::Regex::new).transpose().map_err(|e| format!("--keep: {e}"))?;
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let mut rc = 0;
        for hc in hosts(&c, host)? {
            let Some(r) = make_runner(hc.host) else {
                println!("== {}\nskipped: reach {:?} is not local or ssh", hc.host.name, hc.host.reach);
                continue;
            };
            println!("== {}", hc.host.name);
            let oh = OpsHost::new(hc.host, r.as_ref(), &hc.tidy, &fc.land.remote);
            let o = tidyforce::run(&oh, keep.as_ref(), apply, &date);
            print!("{}", o.text);
            rc = rc.max(o.rc);
            if o.rc == 6 {
                break;
            }
        }
        Ok(rc)
    };
    match run() {
        Ok(rc) => ExitCode::from(rc as u8),
        Err(e) => {
            eprintln!("gatetrain: {e}");
            ExitCode::from(2)
        }
    }
}

// ---------------------------------------------------------------- land, verdict

#[derive(Args)]
pub struct LandCmdArgs {
    /// the branch to land (local, or on the remote)
    branch: String,
    /// the repository (default: the current folder)
    #[arg(long, default_value = ".")]
    repo: String,
    #[arg(long, default_value = "hosts.toml")]
    config: String,
    /// the remote's main branch (default: [land] main, else main)
    #[arg(long = "main")]
    main_branch: Option<String>,
    /// the remote (default: [land] remote, else origin)
    #[arg(long)]
    remote: Option<String>,
    /// the ledger that records the landing and holds the verdicts
    #[arg(long, default_value = "gatetrain.db")]
    db: String,
    /// print the mode and the matched rule per file, change nothing
    #[arg(long)]
    plan: bool,
    /// who lands it (default: $GATETRAIN_WHO, $USERNAME, $USER)
    #[arg(long)]
    by: Option<String>,
}

pub fn land_cmd(a: LandCmdArgs) -> ExitCode {
    let run = || -> Result<i32, String> {
        let mut fc = force_cfg(&a.config)?;
        if let Some(m) = &a.main_branch {
            fc.land.main = m.clone();
        }
        if let Some(r) = &a.remote {
            fc.land.remote = r.clone();
        }
        let mut l = if a.plan { ledger_ro(&a.db) } else { Some(ledger_rw(&a.db)?) };
        let args = LandArgs { repo: a.repo.clone(), branch: a.branch.clone(), plan: a.plan, who: a.by.clone().unwrap_or_else(who) };
        let o = land::land(&LocalExec, &fc.land, &args, l.as_mut());
        print!("{}", o.text);
        Ok(o.rc)
    };
    match run() {
        Ok(rc) => ExitCode::from(rc as u8),
        Err(e) => {
            eprintln!("gatetrain: {e}");
            ExitCode::from(2)
        }
    }
}

#[derive(Args)]
pub struct VerdictArgs {
    /// the commit the independent read covered (the branch tip)
    sha: String,
    /// who read it
    #[arg(long)]
    by: String,
    /// the read found nothing blocking
    #[arg(long, conflicts_with = "fail")]
    pass: bool,
    /// the read found a blocker
    #[arg(long)]
    fail: bool,
    #[arg(long, default_value = "")]
    note: String,
    #[arg(long, default_value = "gatetrain.db")]
    db: String,
}

pub fn verdict_cmd(a: VerdictArgs) -> ExitCode {
    if a.pass == a.fail {
        eprintln!("gatetrain: say --pass or --fail");
        return ExitCode::from(2);
    }
    match ledger_rw(&a.db).and_then(|mut l| land::record_verdict(&mut l, &a.sha, &a.by, a.pass, &a.note).map_err(|e| e.to_string())) {
        Ok(()) => {
            println!("verdict recorded: {} {} by {}", a.sha, if a.pass { "pass" } else { "fail" }, a.by);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gatetrain: {e}");
            ExitCode::from(2)
        }
    }
}

