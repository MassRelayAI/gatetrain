//! `gatetrain tick` (engine v2 E10): one bounded pass that moves every open batch forward through `batch advance`'s own code path,
//! with doctor and `tidy --auto` per host, one advice line and the cause-storm line. Nothing is armed and nothing is left running:
//! the operator schedules the tick (cron or a timer). The pass itself is `gatetrain_core::tick`.
use crate::seams;
use anyhow::Result;
use clap::Args;
use gatetrain_core::batch::{system_now, Engine};
use gatetrain_core::batchcfg::BatchConfig;
use gatetrain_core::config::Host;
use gatetrain_core::doctor::{self, DResult};
use gatetrain_core::ledger::Ledger;
use gatetrain_core::tick::{self, HostSeams, TickOpts};
use gatetrain_core::{config, diagnose};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args)]
pub struct TickArgs {
    /// The ledger
    #[arg(long, default_value = "gatetrain.db")]
    db: PathBuf,
    /// gatetrain.toml: the batch plug-ins (`[chain] storm_reds` included)
    #[arg(long, default_value = "gatetrain.toml")]
    config: PathBuf,
    /// hosts.toml: doctor, `tidy --auto` and advice run for every enabled host in it
    #[arg(long)]
    hosts: PathBuf,
    /// The host whose lock and admission the state machine uses, as `batch --hosts --host` (default: `[proof] host` of gatetrain.toml)
    #[arg(long)]
    host: Option<String>,
    /// The host lock file (default: `lock_file` of --host in --hosts, else <db dir>/.gatetrain/host.lock), as for `batch`
    #[arg(long)]
    lock: Option<PathBuf>,
    /// Print the report as JSON
    #[arg(long)]
    json: bool,
}

/// Doctor and `tidy --auto` through the existing runners (`local` or `ssh <name>`); a host with any other reach has none.
struct RealHosts {
    hosts_path: String,
    needs: config::Needs,
}

impl HostSeams for RealHosts {
    fn doctor(&self, h: &Host) -> Option<Vec<DResult>> {
        let r = doctor::make_runner(h)?;
        Some(doctor::run_checks(h, r.as_ref(), &self.needs))
    }
    fn tidy_auto(&self, h: &Host) -> String {
        if doctor::make_runner(h).is_none() {
            return "skipped".into();
        }
        let out = gatetrain_core::tidy::cmd_tidy(&self.hosts_path, Some(&h.name), false, None, true, &gatetrain_core::tidy::make_runner, None);
        match gatetrain_core::tidy::tidy_auto_note(&out, &h.name) {
            Ok(note) => tick::short_tidy_note(&note, &h.name),
            Err(e) => format!("not run ({})", e.to_string().trim_start_matches("batch: ")),
        }
    }
}

fn config_error(msg: String) -> Result<ExitCode> {
    eprintln!("tick: config error: {msg}");
    Ok(ExitCode::from(2))
}

pub fn run(a: TickArgs) -> Result<ExitCode> {
    let cfg = if a.config.exists() {
        match BatchConfig::load(&a.config) {
            Ok(c) => c,
            Err(e) => return config_error(e.to_string()),
        }
    } else {
        BatchConfig::default()
    };
    if !cfg.tick_enabled {
        // off by default: an unattended pass merges and deploys, so a person turns it on once the deploy targets are ready for it
        return config_error(format!(
            "tick is off: set [tick] enabled = true in {} once every deploy target has timeout_min and a deploy command that is safe to run twice",
            a.config.display()
        ));
    }
    let hosts = match config::load(&a.hosts) {
        Ok(h) => h,
        Err(e) => return config_error(e.to_string()),
    };
    let Some(engine_host) = a.host.clone().or_else(|| cfg.proof_host.clone()) else {
        return config_error("the tick needs the host whose lock and admission it uses: --host NAME, or [proof] host in gatetrain.toml".into());
    };
    if hosts.host(&engine_host).is_none() {
        return config_error(format!("no host {engine_host} in {}", a.hosts.display()));
    }
    // the ledger lock before anything is written: a second tick (or a `batch` command holding it) means this tick does nothing
    let lock = tick::lock_path(&a.db);
    let held = match tick::take(&lock, "tick")? {
        tick::Taken::Got(l) => l,
        tick::Taken::Held(info) => {
            let Some(i) = info else {
                // no live holder recorded: the lock file's age (touched when it was taken) is the only clock
                if let Some(m) = tick::lock_age_min(&lock).filter(|m| *m > cfg.stuck_minutes) {
                    println!("tick: the ledger lock is held by an unknown holder since {m:.0} min ago, longer than [tick] stuck_minutes = {}; look at what holds {}", cfg.stuck_minutes, lock.display());
                    return Ok(ExitCode::from(1));
                }
                println!("tick: another tick holds the lock");
                return Ok(ExitCode::SUCCESS);
            };
            if let (Some(over), Some(step)) = (i.overdue_min(), i.effect.as_deref()) {
                // beating, but the step it runs is past its own deadline: the heartbeat proves the process is alive, not that it moves
                if over > 1.0 {
                    println!("tick: the ledger lock is held by pid {} ({}) running {step}, {over:.0} min past its deadline; look at that process", i.pid, i.by);
                    return Ok(ExitCode::from(1));
                }
            }
            if i.age_min().is_some_and(|m| m > cfg.stuck_minutes) {
                println!("tick: the ledger lock is held by pid {} ({}) since {}, longer than [tick] stuck_minutes = {}; look at that process", i.pid, i.by, i.started, cfg.stuck_minutes);
                return Ok(ExitCode::from(1));
            }
            if i.held_min().is_some_and(|m| m > cfg.max_hold_minutes) {
                println!("tick: the ledger lock is held by pid {} ({}) since {}, longer than [tick] max_hold_minutes = {}, however recently it beat; look at that process", i.pid, i.by, i.started, cfg.max_hold_minutes);
                return Ok(ExitCode::from(1));
            }
            println!("tick: another tick holds the lock (pid {}, started {})", i.pid, i.started);
            return Ok(ExitCode::SUCCESS);
        }
    };
    if !a.db.exists() {
        return config_error(format!("no ledger at {} (run `gatetrain ledger migrate`)", a.db.display()));
    }
    let mut ledger = Ledger::open(&a.db)?;
    if let Err(e) = ledger.require_current() {
        return config_error(format!("{e}"));
    }
    let (real, adm, state_dir) = seams::engine_seams(&a.db, &cfg, a.lock.clone(), Some((a.hosts.clone(), engine_host)))?;
    let actor = std::env::var("GATETRAIN_ACTOR").unwrap_or_else(|_| "tick".into());
    let mut e = Engine { ledger: &mut ledger, cfg: &cfg, lock: &real, adm: &adm, now: &system_now, state_dir, actor, progress: Some(held.beat()) };
    let rules: Vec<diagnose::Rule> = diagnose::builtin_rules().to_vec();
    let seams = RealHosts { hosts_path: a.hosts.to_string_lossy().into_owned(), needs: hosts.needs.clone() };
    let now_us = gatetrain_core::logfmt::Ts::now_utc().instant_us();
    let opts = TickOpts { hosts: &hosts, seams: &seams, rules: &rules, storm_reds: cfg.storm_reds, now_us, max_steps: tick::MAX_STEPS, max_wait_minutes: cfg.max_wait_minutes, max_run_minutes: cfg.max_run_minutes };
    let report = tick::tick(&mut e, &opts)?;
    if a.json {
        println!("{}", report.to_json());
    } else {
        for l in &report.lines {
            println!("{l}");
        }
    }
    Ok(ExitCode::from(report.exit_code()))
}
