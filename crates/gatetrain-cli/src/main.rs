//! The `gatetrain` binary. Ported here: `parse --json`, `log verify`, `ledger migrate`, `diagnose`, `hold`, `running`, `memory`, `batch`, `report`, `trace`, `eta`, `config dump`, `doctor`, `lanes`, `generated`, `tidy` (dry run only; `--apply` and `--auto` go to Python), `check` (Linux only).
//! Every other subcommand is forwarded to `python -m gatetrain` with the same arguments and exit code (design 9.3).
mod cmd_portb;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gatetrain_core::batch::{system_now, Engine, Outcome};
use gatetrain_core::batchcfg::BatchConfig;
use gatetrain_core::hostlock::{MemoryAdmission, RealLock};
use gatetrain_core::{analysis, config, declog, diagnose, history, ledger::Ledger, logfmt, predict, trace};
mod holdcmd;
mod tidycmd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
    },
    /// Text report of a chain log (a run read from the ledger, or one that needs the diagnosis block, is answered by Python)
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
        /// Print FREE / HELD / STALE / STOPPED from the lock; exit 0 only when held
        #[arg(long)]
        status: bool,
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

/// The flags of the Python `report`, `trace` and `eta`. What this binary cannot answer itself is forwarded unchanged.
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

/// `check` and `tidy` (every mode, the dry run included) are NOT ported past the hardening of c46717c on main (argv commands, a
/// verified archive before branch delete, one safe_path, merge proof by tree, link-first removal, verified moves, timeouts):
/// the Rust copy predates it, so both forward to Python until a follow-up lane ports it with tests/test_tidy_harden.py.
fn not_ported_here(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("check" | "tidy"))
}

const PORTED: [&str; 17] = ["parse", "log", "ledger", "diagnose", "hold", "running", "memory", "batch", "report", "trace", "eta", "config", "doctor", "lanes", "generated", "check", "tidy"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Forward anything not ported (and not a flag of ours such as --help/--version) before clap sees it.
    if let Some(first) = args.first() {
        if !first.starts_with('-') && (!PORTED.contains(&first.as_str()) || not_ported_here(&args)) {
            return forward(&args);
        }
    }
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

fn run(cli: Cli, raw: &[String]) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Report(a) => analysis_cmd("report", a, raw),
        Cmd::Trace(a) => analysis_cmd("trace", a, raw),
        Cmd::Eta(a) => analysis_cmd("eta", a, raw),
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
        Cmd::Diagnose { log, db, rules, parallel, config, host } => {
            // The host's parallelism from --config/--host lives in the Python config reader until it is ported.
            if parallel.unwrap_or(0) == 0 && (config.is_some() || host.is_some()) {
                let args: Vec<String> = std::env::args().skip(1).collect();
                return Ok(forward(&args));
            }
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
            println!("{}", diagnose::format_findings(&diagnose::diagnose_log(&text, &p, &hist, &rs)));
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
        Cmd::Hold { run_id, lock, db, hosts, host, wait_secs, log, steps, cmd, .. } => {
            holdcmd::run(holdcmd::HoldArgs { run_id, lock, db, hosts, host, wait_secs, log, steps, cmd })
        }
        Cmd::Memory { cmd: MemoryCmd::Record { db, run, step, peak_mb, source } } => holdcmd::memory_record(&db, &run, &step, peak_mb, &source),
        Cmd::Memory { cmd: MemoryCmd::Admit { db, hosts, host, steps } } => holdcmd::memory_admit(&db, &hosts, &host, &steps),
        Cmd::Running { lock, hosts, host } => holdcmd::status(&lock, &hosts, &host),
        Cmd::Batch { db, config, lock, hosts, host, cmd } => run_batch(&db, &config, lock, hosts.zip(host), cmd),
        Cmd::Check(a) => Ok(tidycmd::run_check(a)),
        Cmd::Tidy(a) => Ok(tidycmd::run_tidy(a)),
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
    let adm = MemoryAdmission { lock: lock_path, db: db.to_path_buf(), hosts, chain_steps: cfg.chain_steps.clone() };
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

fn now_or(s: &Option<String>) -> Result<logfmt::Ts> {
    Ok(match s {
        Some(t) => logfmt::parse_iso(t)?,
        None => logfmt::Ts::now_utc(),
    })
}

/// The configured parallelism: --parallel, else max_parallel_steps of --host in --config. `Err` = Python must answer
/// (a config that does not load raises there).
fn configured_parallel(a: &AnalysisArgs) -> std::result::Result<Option<i64>, ()> {
    if let Some(p) = a.parallel.filter(|p| *p != 0) {
        return Ok(Some(p));
    }
    match (&a.config, &a.host) {
        (Some(c), Some(h)) => {
            let cfg = config::load(c).map_err(|_| ())?;
            Ok(cfg.hosts.iter().find(|x| &x.name == h).and_then(|x| x.max_parallel_steps).filter(|p| *p != 0))
        }
        _ => Ok(None),
    }
}

/// `report`, `trace` and `eta` over a log file. Anything else (a ledger run, the diagnosis block, a config that does not load,
/// a ledger this reader does not understand) is forwarded to Python so the answer is the same.
fn analysis_cmd(which: &str, a: AnalysisArgs, raw: &[String]) -> Result<ExitCode> {
    let Some(log) = a.log.clone() else { return Ok(forward(raw)) };
    if a.run.is_some() {
        return Ok(forward(raw));
    }
    let Ok(configured) = configured_parallel(&a) else { return Ok(forward(raw)) };
    let Ok(bytes) = std::fs::read(&log) else { return Ok(forward(raw)) };
    let p = logfmt::parse_log(&String::from_utf8_lossy(&bytes))?;
    let steps = &p.steps;
    let run_id = log.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match which {
        "report" => {
            let reds = steps.iter().any(|s| matches!(s.exit_code, Some(x) if x != 0));
            if reds || configured.is_some() {
                return Ok(forward(raw)); // the diagnosis block is not ported
            }
            let outcome = history::classify(&p, &now_or(&a.now)?, 30.0);
            let meta = analysis::Meta { outcome, commit: p.commit.as_deref(), host: p.host.as_deref() };
            println!("{}", analysis::report_text(&run_id, &meta, steps));
        }
        "trace" => println!("{}", trace::trace_json(steps)),
        _ => {
            let Some(hist) = history::history_durations(&a.db, &run_id) else { return Ok(forward(raw)) };
            let reference = match p.last_ts() {
                Some(t) => t,
                None => now_or(&a.now)?,
            };
            if steps.is_empty() {
                eprintln!("no steps in log");
                return Ok(ExitCode::from(1));
            }
            let pred = predict::predict(steps, &hist, reference);
            let finished = steps.iter().filter(|s| s.exit_code.is_some() || s.duration.is_some()).count();
            println!("{}", predict::eta_text(&pred, &reference, finished));
            if let Some(note) = analysis::parallelism_note(&p, configured) {
                println!("{note}");
            }
            if p.gates_done {
                println!("note: this run already printed GATES_DONE");
            }
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

/// Find a checkout of the Python package: GATETRAIN_PYROOT, else walk up from the binary and the working directory.
fn python_root() -> Option<PathBuf> {
    if let Some(r) = std::env::var_os("GATETRAIN_PYROOT") {
        return Some(PathBuf::from(r));
    }
    let starts = [std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)), std::env::current_dir().ok()];
    for s in starts.into_iter().flatten() {
        for d in s.ancestors() {
            if d.join("gatetrain").join("__main__.py").is_file() {
                return Some(d.to_path_buf());
            }
        }
    }
    None
}

fn forward(args: &[String]) -> ExitCode {
    if std::env::var_os("GATETRAIN_NO_FORWARD").is_some() {
        // test switch: proves a path is ported by refusing the Python fallback
        eprintln!("gatetrain: `{}` would be forwarded to Python (GATETRAIN_NO_FORWARD is set)", args[0]);
        return ExitCode::from(126);
    }
    let mut last_err = None;
    let order: [&str; 2] = if cfg!(windows) { ["python", "python3"] } else { ["python3", "python"] };
    for py in order {
        let mut c = Command::new(py);
        c.arg("-m").arg("gatetrain").args(args);
        if let Some(root) = python_root() {
            let mut paths = vec![root];
            paths.extend(std::env::var_os("PYTHONPATH").iter().flat_map(std::env::split_paths));
            if let Ok(joined) = std::env::join_paths(paths) {
                c.env("PYTHONPATH", joined);
            }
        }
        match c.status() {
            Ok(st) => return ExitCode::from(st.code().unwrap_or(1).clamp(0, 255) as u8),
            Err(e) => last_err = Some(e),
        }
    }
    eprintln!(
        "gatetrain: `{}` is not ported to the Rust binary yet and Python was not found ({}); install Python 3 to run it",
        args[0],
        last_err.map(|e| e.to_string()).unwrap_or_default()
    );
    ExitCode::from(127)
}
