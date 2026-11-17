//! `gatetrain chain`: run a steps file (see `gatetrain_core::chain`). Exit 0 green, 1 red, 2 config error.
use clap::Args;
use gatetrain_core::chain::{digest_of, load, run as run_plan, Opts};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args)]
pub struct ChainArgs {
    /// the steps file: [[step]] name, run (shell string) or argv, tier = "fast"|"slow", needs, timeout_s, inputs, cache_salt; optional top-level cwd
    #[arg(long)]
    steps: PathBuf,
    /// how many steps of a tier may run at once
    #[arg(long, default_value_t = 4)]
    par: usize,
    /// where each step's full output (<step>.full) and the stamp go
    #[arg(long)]
    out: PathBuf,
    /// run the slow tier even when a fast step is red
    #[arg(long)]
    keep_going: bool,
    /// run only these steps (comma list)
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// leave these steps out (comma list)
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
    /// the step cache folder (per host, your choice): a step with `inputs` whose tracked inputs are unchanged since it last passed is not re-run
    /// (printed as STEP_CACHED). For pre-flight only: a proof run must not pass this flag
    #[arg(long)]
    cache: Option<PathBuf>,
    /// run these steps even when the cache has them (comma list)
    #[arg(long, value_delimiter = ',')]
    no_cache_for: Vec<String>,
    /// what this run claims to be: a partial run can never call itself `full` (refused with --only or --skip)
    #[arg(long, default_value = "preflight", value_parser = ["preflight", "lane", "full"])]
    mode: String,
    /// the host's name (the one in hosts.toml); default: $GATETRAIN_HOST (set by `hold`), then the environment, then the OS host name
    #[arg(long)]
    host: Option<String>,
    /// at the end, write this run (its steps, shards, skips and stamp) into this ledger file; it is created or migrated forward
    #[arg(long)]
    db: Option<PathBuf>,
    /// the run's id in the ledger (default: chain-<UTC time>-<commit>)
    #[arg(long)]
    run_id: Option<String>,
}

pub fn run(a: ChainArgs) -> ExitCode {
    let text = match std::fs::read_to_string(&a.steps) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("chain: cannot read {}: {e}", a.steps.display());
            return ExitCode::from(2);
        }
    };
    let mut plan = match load(&text) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("chain: {e}");
            return ExitCode::from(2);
        }
    };
    // a relative `cwd` is the steps file's own, so the file means the same thing from wherever it is run (no cwd = the caller's)
    if let Some(c) = plan.cwd.as_ref().filter(|c| std::path::Path::new(c).is_relative()) {
        let base = a.steps.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
        plan.cwd = Some(base.join(c).to_string_lossy().into_owned());
    }
    if a.par == 0 {
        eprintln!("chain: --par must be at least 1");
        return ExitCode::from(2);
    }
    if a.mode == "full" && (!a.only.is_empty() || !a.skip.is_empty()) {
        eprintln!("chain: --mode full cannot be combined with --only or --skip: a partial run is not a full one");
        return ExitCode::from(2);
    }
    let (db, run_id) = (a.db, a.run_id);
    let o = Opts { par: a.par, out: a.out, keep_going: a.keep_going, only: a.only, skip: a.skip, cache: a.cache, no_cache_for: a.no_cache_for, mode: a.mode, host: a.host };
    let mut emit = |l: &str| {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{l}");
        let _ = so.flush();
    };
    match run_plan(&plan, &digest_of(&text), &o, &mut emit) {
        Ok(r) => {
            match &r.stamp {
                Ok(p) => eprintln!("chain: green, stamp {}", p.display()),
                Err(why) if r.reds.is_empty() => eprintln!("chain: {why}"),
                Err(_) => {}
            }
            if let Some(db) = &db {
                let rid = run_id.unwrap_or_else(|| default_run_id(&r.record));
                match crate::cmd_runs::open_for_write(&db.to_string_lossy()).and_then(|l| Ok(l.record_chain_run(&rid, &r.record)?)) {
                    Ok(()) => eprintln!("chain: ledger {}: run {rid}", db.display()),
                    Err(e) => eprintln!("chain: ledger {}: the run was not recorded: {e:#}", db.display()),
                }
            }
            ExitCode::from(u8::from(!r.reds.is_empty()))
        }
        Err(e) => {
            eprintln!("chain: {e}");
            ExitCode::from(2)
        }
    }
}

/// `chain-<UTC time to the microsecond>-<commit, 7 chars>`: two runs of one commit never share an id unless they start in the same microsecond.
fn default_run_id(r: &gatetrain_core::chain::RunRec) -> String {
    let t: String = r.started_at.chars().take_while(|c| *c != '+' && *c != 'Z').filter(char::is_ascii_digit).collect();
    format!("chain-{t}-{}", r.sha.as_deref().map_or("nosha", |s| &s[..s.len().min(7)]))
}
