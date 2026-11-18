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
    // the ledger lock first: a second tick does nothing at all
    let Some(_held) = tick::try_lock(&tick::lock_path(&a.db))? else {
        println!("tick: another tick holds the lock");
        return Ok(ExitCode::SUCCESS);
    };
    let cfg = if a.config.exists() {
        match BatchConfig::load(&a.config) {
            Ok(c) => c,
            Err(e) => return config_error(e.to_string()),
        }
    } else {
        BatchConfig::default()
    };
    let hosts = match config::load(&a.hosts) {
        Ok(h) => h,
        Err(e) => return config_error(e.to_string()),
    };
    let engine_host = a.host.clone().or_else(|| cfg.proof_host.clone());
    if let Some(n) = &engine_host {
        if hosts.host(n).is_none() {
            return config_error(format!("no host {n} in {}", a.hosts.display()));
        }
    }
    if !a.db.exists() {
        return config_error(format!("no ledger at {} (run `gatetrain ledger migrate`)", a.db.display()));
    }
    let mut ledger = Ledger::open(&a.db)?;
    if let Err(e) = ledger.require_current() {
        return config_error(format!("{e}"));
    }
    let (real, adm, state_dir) = seams::engine_seams(&a.db, &cfg, None, engine_host.map(|n| (a.hosts.clone(), n)))?;
    let actor = std::env::var("GATETRAIN_ACTOR").unwrap_or_else(|_| "tick".into());
    let mut e = Engine { ledger: &mut ledger, cfg: &cfg, lock: &real, adm: &adm, now: &system_now, state_dir, actor, crash_after_effect: None };
    let rules: Vec<diagnose::Rule> = diagnose::builtin_rules().to_vec();
    let seams = RealHosts { hosts_path: a.hosts.to_string_lossy().into_owned(), needs: hosts.needs.clone() };
    let now_us = gatetrain_core::logfmt::Ts::now_utc().instant_us();
    let opts = TickOpts { hosts: &hosts, seams: &seams, rules: &rules, storm_reds: cfg.storm_reds, now_us };
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
