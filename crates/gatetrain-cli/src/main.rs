//! The `gatetrain` binary: every command is Rust. See `gatetrain --help`.
mod cmd_portb;
mod cmd_runs;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gatetrain_core::batch::{system_now, Engine, Outcome};
use gatetrain_core::batchcfg::BatchConfig;
use gatetrain_core::hostlock::{MemoryAdmission, RealLock};
use gatetrain_core::{analysis, config, declog, diagnose, history, ledger::Ledger, logfmt, predict};
mod chaincmd;
mod holdcmd;
mod tidycmd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "gatetrain", version, about = "Gate train: the chain, the ledger, the decision log")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Parse a chain log (docs/log-format.md) and print what was read
    Parse {
        log: PathBuf,
        /// Print the parse as canonical-order JSON (the form the parity corpus compares)
        #[arg(long)]
        json: bool,
    },
    /// Classify every red step of a chain log: class, evidence, remedy, rerun needed
    Diagnose {
        log: PathBuf,
        /// A ledger file: step durations of earlier runs decide what counts as slow
        #[arg(long, default_value = "")]
        db: String,
        /// JSON list of extra rules (same keys as rules/red-classes.toml), tried first
        #[arg(long)]
        rules: Option<PathBuf>,
        /// The host's configured parallel steps
        #[arg(long, allow_negative_numbers = true)]
        parallel: Option<i64>,
        #[arg(long)]
        config: Option<String>,
        #[arg(long)]
        host: Option<String>,
        /// A `chain` run's --out folder: a red step with no output in the log is read from its <step>.full (or its red shards' files).
        /// Default: the folder the log's own CHAIN_OUT line names
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Text report of a chain log or of a run in the ledger
    Report(AnalysisArgs),
    /// Chrome trace events of a chain log, as one JSON line
    Trace(AnalysisArgs),
    /// Predict when a running chain ends, from the ledger's history
    Eta(AnalysisArgs),
    /// The hosts.toml file
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// The decision log
    Log {
        #[command(subcommand)]
        cmd: LogCmd,
    },
    /// The batch state machine (design 3)
    Batch {
        #[arg(long, default_value = "gatetrain.db", global = true)]
        db: PathBuf,
        #[arg(long, default_value = "gatetrain.toml", global = true)]
        config: PathBuf,
        /// The host lock file (default: `lock_file` of --host in --hosts, else <db dir>/.gatetrain/host.lock)
        #[arg(long, global = true)]
        lock: Option<PathBuf>,
        /// hosts.toml: the lock file and the memory admission come from it
        #[arg(long, global = true, requires = "host")]
        hosts: Option<PathBuf>,
        #[arg(long, global = true, requires = "hosts")]
        host: Option<String>,
        #[command(subcommand)]
        cmd: BatchCmd,
    },
    /// The ledger file
    Ledger {
        #[command(subcommand)]
        cmd: LedgerCmd,
    },
    /// Find leftovers: build targets, worktrees, containers, images, cache, recycle bin (read-only)
    Check(tidycmd::CheckArgs),
    /// Clean the leftovers `check` finds (dry run unless --apply or --auto)
    Tidy(tidycmd::TidyArgs),
    /// Run a steps file: fast then slow tier in parallel, full output per step, log-format lines on stdout, a stamp when green and clean
    Chain(chaincmd::ChainArgs),
    /// Fail when any file of a repository holds a word on its hashed deny-list `.privacy-denylist` (a hit names the file and line, never the word)
    PrivacyCheck {
        /// the repository to check (default: the current folder)
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// print the deny-list line (sha256) for this word instead of checking
        #[arg(long, value_name = "WORD")]
        hash: Option<String>,
    },
    /// Read a repository's lanes and say what to do next (read-only): live, merged awaiting tidy, stale, ready to stack, stackable, blocked, next assignments
    Watch {
        /// a TOML file with a [watch] table (repo = "...", lanes_csv, plans_index, ...); it may be hosts.toml
        #[arg(long, default_value = "hosts.toml")]
        config: PathBuf,
        /// the time to judge ages against, epoch seconds (default: now)
        #[arg(long)]
        now: Option<i64>,
    },
    /// Parse a chain log into the ledger (a run id already there is refused; --replace only for a running or stopped run)
    Ingest {
        log: PathBuf,
        #[arg(long, default_value = "gatetrain.db")]
        db: String,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        replace: bool,
        #[arg(long, default_value_t = 30.0)]
        stale_min: f64,
        /// the time to judge freshness against, ISO 8601 (default: now)
        #[arg(long)]
        now: Option<String>,
    },
    /// One table for every chain log given: state, eta and when to look next
    Status {
        #[arg(long, required = true)]
        log: Vec<PathBuf>,
        #[arg(long, default_value = ":memory:")]
        db: String,
        #[arg(long)]
        now: Option<String>,
        #[arg(long, default_value_t = 30.0)]
        stale_min: f64,
        #[arg(long = "min", default_value_t = 5)]
        min: i64,
        #[arg(long = "max", default_value_t = 60)]
        max: i64,
    },
    /// When to look at a running chain next
    NextTick {
        #[arg(long)]
        log: Vec<PathBuf>,
        #[arg(long, default_value = "gatetrain.db")]
        db: PathBuf,
        #[arg(long)]
        now: Option<String>,
        #[arg(long = "min", default_value_t = 5)]
        min: i64,
        #[arg(long = "max", default_value_t = 60)]
        max: i64,
    },
    /// Steps red in one finished run and green in another on the same commit
    Flaky {
        #[arg(long, default_value = "gatetrain.db")]
        db: String,
    },
    /// Stop a chain: queued followers first, then the processes (Linux only; a dry run unless --yes)
    Stop {
        pattern: String,
        #[arg(long)]
        follower: Vec<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        host: Option<String>,
    },
    /// Pre-run checks per host
    Doctor {
        #[arg(long, default_value = "hosts.toml")]
        config: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Pre-stack collision radar over lane refs
    Lanes {
        #[arg(long, default_value = ".")]
        repo: String,
        #[arg(long)]
        main: String,
        #[arg(required = true)]
        lane_ref: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        shared_files: Option<String>,
        #[arg(long)]
        resource_regex: Vec<String>,
        /// comma list of globs: the lists a new route or table must join (enables the baseline radar)
        #[arg(long)]
        baseline_files: Option<String>,
        /// regex for an added line that counts as a new route or table
        #[arg(long)]
        new_item_regex: Vec<String>,
    },
    /// List untracked files under the generated directories
    Generated {
        #[arg(long, default_value = ".")]
        repo: String,
        #[arg(long)]
        dir: Vec<String>,
        #[arg(long)]
        config: Option<String>,
    },
    /// Hold the host lock as the PARENT of a command (the chain); `--status` reads the lock instead
    Hold {
        /// The run id (not needed with --status)
        run_id: Option<String>,
        /// The lock file (default: `lock_file` of the host in --hosts)
        #[arg(long)]
        lock: Option<PathBuf>,
        #[arg(long, default_value = "gatetrain.db")]
        db: PathBuf,
        /// hosts.toml: parallelism and reserved cores come only from here
        #[arg(long, requires = "host")]
        hosts: Option<PathBuf>,
        #[arg(long, requires = "hosts")]
        host: Option<String>,
        /// Wait this long for a busy lock; 0 (default) refuses at once with exit 75
        #[arg(long, default_value_t = 0)]
        wait_secs: u64,
        /// Also append HOLDS / RELEASED lines to this holder log
        #[arg(long)]
        log: Option<PathBuf>,
        /// Comma-separated step names the chain would run: their predicted memory is admitted before the lock is taken
        #[arg(long, value_delimiter = ',')]
        steps: Vec<String>,
        /// The repository root the chain must run in: it is the child's working directory, and the hold is refused (exit 78) when it is
        /// not the top level of a git repository (a script that fell back to the wrong folder must not run a build there)
        #[arg(long)]
        repo_root: Option<PathBuf>,
        /// A build folder the chain writes to (repeatable): the hold is refused (exit 77) when the configured disk that holds it would
        /// drop under its `min_free_gb` by the size this folder reached on earlier runs; its size is recorded after the run
        #[arg(long)]
        target_dir: Vec<PathBuf>,
        /// Print FREE / HELD / STALE / STOPPED from the lock; exit 0 only when held
        #[arg(long)]
        status: bool,
        /// Sample the host (load, CPU, I/O wait, disk busy, free memory) every N seconds into <log>.samples.jsonl while the child runs, and
        /// ingest the samples into --db with the run; the child is told (GATETRAIN_SAMPLED) so `chain` does not sample twice. 0 = off; needs --log
        #[arg(long, default_value_t = 10)]
        samples: u64,
        /// The command to run as the child, after `--`
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Memory in the host model: record a step peak, or ask the admission rule
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Is a chain running? Reads the host lock, never the process table (exit 0 when held)
    Running {
        #[arg(long)]
        lock: Option<PathBuf>,
        #[arg(long, requires = "host")]
        hosts: Option<PathBuf>,
        #[arg(long, requires = "hosts")]
        host: Option<String>,
    },
}

/// The flags of `report`, `trace` and `eta`.
#[derive(clap::Args)]
struct AnalysisArgs {
    log: Option<PathBuf>,
    #[arg(long, default_value = "gatetrain.db")]
    db: PathBuf,
    #[arg(long)]
    run: Option<String>,
    #[arg(long)]
    now: Option<String>,
    #[arg(long)]
    parallel: Option<i64>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    host: Option<String>,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Load a hosts.toml and print it as sorted-key JSON (the form the parity corpus compares), or `ERROR <message>`
    Dump { file: PathBuf },
}

#[derive(Subcommand)]
enum LogCmd {
    /// Walk the hash chain; exit 1 and name the first bad entry when it is broken
    Verify {
        #[arg(long, default_value = "gatetrain.db")]
        db: PathBuf,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Store the peak memory of one step of a run (megabytes)
    Record {
        #[arg(long, default_value = "gatetrain.db")]
        db: PathBuf,
        #[arg(long)]
        run: String,
        #[arg(long)]
        step: String,
        #[arg(long)]
        peak_mb: f64,
        #[arg(long, default_value = "reported")]
        source: String,
    },
    /// Would these steps fit this host? Exit 0 admitted, 76 refused (prints the numbers, the rule and the cheap set)
    Admit {
        #[arg(long, default_value = "gatetrain.db")]
        db: PathBuf,
        #[arg(long)]
        hosts: PathBuf,
        #[arg(long)]
        host: String,
        /// Comma-separated step names
        #[arg(long, value_delimiter = ',', required = true)]
        steps: Vec<String>,
    },
}

#[derive(Subcommand)]
enum BatchCmd {
    /// Create a batch from lane refs
    New {
        id: String,
        #[arg(long)]
        base: String,
        /// The checkout the batch is stacked in
        #[arg(long, default_value = ".")]
        workdir: PathBuf,
        lanes: Vec<String>,
    },
    /// The derived state of one batch, or of all
    Status {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Take the next step (or resume a pending intent); exit 3 when a rule refuses
    Advance { id: String },
    /// A person's hold
    Hold {
        id: String,
        #[arg(long, default_value = "held by a person")]
        reason: String,
    },
    /// Return a held batch to the state it came from
    Release { id: String },
    /// Close without a merge; no branch is deleted
    Abandon {
        id: String,
        #[arg(long, default_value = "abandoned by a person")]
        reason: String,
    },
}

#[derive(Subcommand)]
enum LedgerCmd {
    /// Bring a ledger file to the schema version this binary writes (forward only)
    Migrate {
        #[arg(long, default_value = "gatetrain.db")]
        db: PathBuf,
    },
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Cli::try_parse().map(|c| run(c, &args)) {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            eprintln!("gatetrain: {e:#}");
            ExitCode::from(2)
        }
        Err(e) => {
            let _ = e.print();
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn run(cli: Cli, _raw: &[String]) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Report(a) => analysis_cmd("report", a),
        Cmd::Trace(a) => analysis_cmd("trace", a),
        Cmd::Eta(a) => analysis_cmd("eta", a),
        Cmd::Ingest { log, db, run_id, host, replace, stale_min, now } => cmd_runs::ingest(cmd_runs::IngestArgs { log, db, run_id, host, replace, stale_min, now }),
        Cmd::Status { log, db, now, stale_min, min, max } => cmd_runs::status(cmd_runs::StatusArgs { logs: log, db, now, stale_min, lo: min, hi: max }),
        Cmd::NextTick { log, db, now, min, max } => cmd_runs::next_tick(log, &db, now, min, max),
        Cmd::Flaky { db } => cmd_runs::flaky(&db),
        Cmd::Stop { pattern, follower, yes, config, host } => cmd_runs::stop_cmd(&pattern, follower, yes, config, host),
        Cmd::Config { cmd: ConfigCmd::Dump { file } } => {
            println!("{}", config::dump_line(&file));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Doctor { config, host, json } => Ok(cmd_portb::doctor_cmd(&config, host.as_deref(), json)),
        Cmd::Lanes { repo, main, lane_ref, json, shared_files, resource_regex, baseline_files, new_item_regex } => {
            Ok(cmd_portb::lanes_cmd(cmd_portb::LanesArgs { repo, main, refs: lane_ref, json, shared_files, resource_regex, baseline_files, new_item_regex }))
        }
        Cmd::Generated { repo, dir, config } => Ok(cmd_portb::generated_cmd(&repo, dir, config.as_deref())),
        Cmd::Parse { log, json } => {
            let bytes = std::fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
            // parse.py reads with errors="replace"
            let text = String::from_utf8_lossy(&bytes);
            let p = logfmt::parse_log(&text)?;
            if json {
                println!("{}", serde_json::to_string(&p.to_json())?);
            } else {
                println!("{} steps, {} red_now, gates_done={}", p.steps.len(), p.red_now.len(), p.gates_done);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Diagnose { log, db, rules, parallel, config, host, out } => {
            let parallel = configured_parallel(parallel, config.as_deref().map(Path::new), host.as_deref())?;
            let bytes = std::fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
            let text = String::from_utf8_lossy(&bytes);
            let p = logfmt::parse_log(&text)?;
            let mut rs: Vec<diagnose::Rule> = Vec::new();
            if let Some(path) = &rules {
                let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)
                    .with_context(|| format!("parsing {}", path.display()))?;
                rs = diagnose::rules_from_value(&v)?;
            }
            rs.extend(diagnose::builtin_rules().iter().cloned());
            let hist = if !db.is_empty() && db != ":memory:" && Path::new(&db).exists() {
                history_durations(Path::new(&db), log.file_stem().and_then(|s| s.to_str()).unwrap_or(""))?
            } else {
                diagnose::History::new()
            };
            println!("{}", diagnose::format_findings(&diagnose::diagnose_log_in(&text, &p, &hist, &rs, out.as_deref())));
            if let Some(note) = diagnose::parallelism_note(&p, parallel) {
                println!("{note}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Log { cmd: LogCmd::Verify { db } } => {
            let l = open_existing(&db)?;
            match declog::verify(&l)? {
                Ok(v) => {
                    println!("log verify: ok, {} entries, head {}", v.entries, v.head_hash);
                    Ok(ExitCode::SUCCESS)
                }
                Err(b) => {
                    println!("log verify: BROKEN at seq {}: {}", b.seq, b.reason);
                    Ok(ExitCode::from(1))
                }
            }
        }
        Cmd::Hold { status: true, lock, hosts, host, .. } => holdcmd::status(&lock, &hosts, &host),
        Cmd::Hold { run_id, lock, db, hosts, host, wait_secs, log, steps, repo_root, target_dir, samples, cmd, .. } => {
            holdcmd::run(holdcmd::HoldArgs { run_id, lock, db, hosts, host, wait_secs, log, steps, repo_root, target_dir, samples, cmd })
        }
        Cmd::Memory { cmd: MemoryCmd::Record { db, run, step, peak_mb, source } } => holdcmd::memory_record(&db, &run, &step, peak_mb, &source),
        Cmd::Memory { cmd: MemoryCmd::Admit { db, hosts, host, steps } } => holdcmd::memory_admit(&db, &hosts, &host, &steps),
        Cmd::Running { lock, hosts, host } => holdcmd::status(&lock, &hosts, &host),
        Cmd::Batch { db, config, lock, hosts, host, cmd } => run_batch(&db, &config, lock, hosts.zip(host), cmd),
        Cmd::Check(a) => Ok(tidycmd::run_check(a)),
        Cmd::Tidy(a) => Ok(tidycmd::run_tidy(a)),
        Cmd::Chain(a) => Ok(chaincmd::run(a)),
        Cmd::PrivacyCheck { root, hash } => Ok(ExitCode::from(gatetrain_core::privacy::run(&root, hash.as_deref()) as u8)),
        Cmd::Watch { config, now } => {
            let cfg = match gatetrain_core::watch::load_cfg(&config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("config error: {e}");
                    return Ok(ExitCode::from(1));
                }
            };
            let now = now.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
            match gatetrain_core::watch::gather(&cfg, now) {
                Ok(r) => {
                    println!("{}", gatetrain_core::watch::format(&r, &cfg));
                    Ok(ExitCode::SUCCESS)
                }
                Err(e) => {
                    eprintln!("gatetrain watch: {e}");
                    Ok(ExitCode::from(2))
                }
            }
        }
        Cmd::Ledger { cmd: LedgerCmd::Migrate { db } } => {
            let mut l = Ledger::open(&db).with_context(|| format!("opening {}", db.display()))?;
            let (from, to) = l.migrate()?;
            if from == to {
                println!("ledger migrate: {} is already at version {to}", db.display());
            } else {
                println!("ledger migrate: {} version {from} -> {to}", db.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// db.history_durations: successful, uncached step durations of completed chain runs, minus the run being diagnosed.
fn history_durations(db: &Path, exclude_run: &str) -> Result<diagnose::History> {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db.display()))?;
    let mut st = c.prepare(
        "SELECT s.name, s.duration_sec FROM steps s JOIN runs r ON r.id=s.run_id          WHERE r.kind='chain' AND r.outcome IN ('green','red') AND s.exit_code=0 AND s.cached=0          AND s.duration_sec IS NOT NULL AND r.id<>?1",
    )?;
    let mut h = diagnose::History::new();
    for row in st.query_map([exclude_run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))? {
        let (n, d) = row?;
        h.entry(n).or_default().push(d);
    }
    Ok(h)
}

fn run_batch(db: &Path, config: &Path, lock: Option<PathBuf>, hosts: Option<(PathBuf, String)>, cmd: BatchCmd) -> Result<ExitCode> {
    let mut ledger = open_existing(db)?;
    ledger.require_current().context("run `gatetrain ledger migrate` first")?;
    let cfg = if config.exists() { BatchConfig::load(config)? } else { BatchConfig::default() };
    let state_dir = db.parent().map(|p| if p.as_os_str().is_empty() { PathBuf::from(".") } else { p.to_path_buf() }).unwrap_or_else(|| PathBuf::from(".")).join(".gatetrain");
    let lock_path = match (lock, &hosts) {
        (Some(l), _) => l,
        (None, Some((h, n))) => match gatetrain_core::hold::host_capacity(&std::fs::read_to_string(h)?, n)?.lock_file {
            Some(l) => PathBuf::from(l),
            None => state_dir.join("host.lock"),
        },
        (None, None) => state_dir.join("host.lock"),
    };
    let exe = std::env::current_exe().context("locating the gatetrain binary")?.to_string_lossy().into_owned();
    let real = RealLock { lock: lock_path.clone(), db: db.to_path_buf(), hosts: hosts.clone(), holder: vec![exe, "hold".into()] };
    let mut adm = MemoryAdmission::new(lock_path, db.to_path_buf(), hosts.clone(), cfg.chain_steps.clone());
    adm.target_dirs = cfg.chain_target_dirs.iter().map(PathBuf::from).collect();
    adm.repo_root = cfg.chain_repo_root.as_ref().map(PathBuf::from);
    adm.doctor = cfg.doctor && hosts.is_some();
    let actor = std::env::var("GATETRAIN_ACTOR").unwrap_or_else(|_| "engine".into());
    let mut e = Engine { ledger: &mut ledger, cfg: &cfg, lock: &real, adm: &adm, now: &system_now, state_dir, actor, crash_after_effect: None };
    let report = |o: Outcome| -> ExitCode {
        match &o {
            Outcome::Advanced { from, to, note } => println!("{from} -> {to} {note}"),
            Outcome::Waiting(m) | Outcome::Idle(m) => println!("{m}"),
            Outcome::Failed(m) => eprintln!("FAILED {m}"),
            Outcome::Refused { rule, because, decision } => {
                println!("REFUSED
  rule    {rule}
  because {because}
  decision #{decision}")
            }
        }
        ExitCode::from(o.exit_code())
    };
    Ok(match cmd {
        BatchCmd::New { id, base, workdir, lanes } => {
            e.new_batch(&id, &base, &workdir, &lanes)?;
            println!("batch {id} opened on {base} with {} lanes", lanes.len());
            ExitCode::SUCCESS
        }
        BatchCmd::Status { id, json } => {
            let ids = match id {
                Some(i) => vec![i],
                None => e.batch_ids()?,
            };
            for i in ids {
                let s = e.status(&i)?;
                if json {
                    println!("{}", serde_json::to_string(&s)?);
                } else {
                    let pend = if s["pending_intent"].is_null() { String::new() } else { format!(" (pending intent {})", s["pending_intent"]["transition"].as_str().unwrap_or("?")) };
                    println!("{i}: {}{pend}", s["state"].as_str().unwrap_or("?"));
                }
            }
            ExitCode::SUCCESS
        }
        BatchCmd::Advance { id } => report(e.advance(&id)?),
        BatchCmd::Hold { id, reason } => report(e.hold(&id, &reason)?),
        BatchCmd::Release { id } => report(e.release(&id)?),
        BatchCmd::Abandon { id, reason } => report(e.abandon(&id, &reason)?),
    })
}


/// The configured parallelism: --parallel, else max_parallel_steps of --host in --config.
fn configured_parallel(parallel: Option<i64>, config: Option<&Path>, host: Option<&str>) -> Result<Option<i64>> {
    if let Some(p) = parallel.filter(|p| *p != 0) {
        return Ok(Some(p));
    }
    match (config, host) {
        (Some(c), Some(h)) => {
            let cfg = config::load(c).map_err(|e| anyhow::anyhow!("config error: {e}"))?;
            Ok(cfg.hosts.iter().find(|x| x.name == h).and_then(|x| x.max_parallel_steps).filter(|p| *p != 0))
        }
        _ => Ok(None),
    }
}

fn fail(msg: &str) -> Result<ExitCode> {
    eprintln!("{msg}");
    Ok(ExitCode::from(1))
}

/// `report`, `trace` and `eta`, over a log file or (report, trace) a run in the ledger.
fn analysis_cmd(which: &str, mut a: AnalysisArgs) -> Result<ExitCode> {
    let configured = configured_parallel(a.parallel, a.config.as_deref(), a.host.as_deref())?;
    if which == "eta" {
        let Some(log) = a.log.clone() else {
            eprintln!("error: the following arguments are required: <LOG>");
            return Ok(ExitCode::from(2));
        };
        let (_, p) = cmd_runs::read_log(&log)?;
        let run_id = cmd_runs::run_id(&log);
        let Some(hist) = history::history_durations(&a.db, &run_id) else { return fail("the ledger could not be read") };
        let reference = match p.last_ts() {
            Some(t) => t,
            None => cmd_runs::now_or(&a.now)?,
        };
        if p.steps.is_empty() {
            return fail("no steps in log");
        }
        let pred = predict::predict(&p.steps, &hist, reference);
        let finished = p.steps.iter().filter(|s| s.exit_code.is_some() || s.duration.is_some()).count();
        println!("{}", predict::eta_text(&pred, &reference, finished));
        if let Some(note) = analysis::parallelism_note(&p, configured) {
            println!("{note}");
        }
        if p.gates_done {
            println!("note: this run already printed GATES_DONE");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if which == "report" && a.run.is_none() && a.log.is_none() {
        match cmd_runs::latest_run(&a.db)? {
            Some(r) => a.run = Some(r),
            None => return fail("ledger is empty"),
        }
    }
    if which == "trace" && a.run.is_none() && a.log.is_none() {
        return fail("give a log path or --run ID");
    }
    let (src, log_text) = if let Some(rid) = &a.run {
        match cmd_runs::load_run(&a.db, rid)? {
            Some(s) => (s, None),
            None => return fail(&format!("no run {rid} in {}", a.db.display())),
        }
    } else {
        let log = a.log.clone().unwrap();
        let (text, p) = cmd_runs::read_log(&log)?;
        let outcome = history::classify(&p, &cmd_runs::now_or(&a.now)?, 30.0);
        (cmd_runs::Src { id: cmd_runs::run_id(&log), outcome: outcome.to_string(), commit: p.commit.clone(), host: p.host.clone(), steps: p.steps }, Some(text))
    };
    if which == "trace" {
        println!("{}", cmd_runs::trace_text(&src));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", cmd_runs::report_text(&src));
    if let Some(text) = log_text {
        let reds = src.steps.iter().any(|s| matches!(s.exit_code, Some(x) if x != 0));
        if reds || configured.is_some() {
            let p = logfmt::parse_log(&text)?;
            let findings = diagnose::diagnose_log(&text, &p, &diagnose::History::new(), diagnose::builtin_rules());
            let body: Vec<String> = diagnose::format_findings(&findings).lines().map(|l| format!("  {l}")).collect();
            let mut block = format!("diagnosis:\n{}", body.join("\n"));
            if let Some(note) = diagnose::parallelism_note(&p, configured) {
                block.push('\n');
                block.push_str(&note);
            }
            println!("{block}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Verifying must not create an empty ledger by accident.
fn open_existing(db: &Path) -> Result<Ledger> {
    anyhow::ensure!(db.exists(), "no ledger at {}", db.display());
    let l = Ledger::open(db).with_context(|| format!("opening {}", db.display()))?;
    anyhow::ensure!(
        l.version()? >= 1,
        "{} is at schema version {}; run `gatetrain ledger migrate` first",
        db.display(),
        l.version()?
    );
    Ok(l)
}
