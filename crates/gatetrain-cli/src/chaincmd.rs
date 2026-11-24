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
    pub(crate) steps: PathBuf,
    /// how many steps of a tier may run at once
    #[arg(long, default_value_t = 4)]
    pub(crate) par: usize,
    /// where each step's full output (<step>.full) and the stamp go
    #[arg(long)]
    pub(crate) out: PathBuf,
    /// run the slow tier even when a fast step is red
    #[arg(long)]
    pub(crate) keep_going: bool,
    /// run only these steps (comma list)
    #[arg(long, value_delimiter = ',')]
    pub(crate) only: Vec<String>,
    /// run every step of these tiers (`fast`, `slow`; repeatable or a comma list) as a COMPLETE run of them: not partial, so `--since` can reuse from an earlier run of the same tiers. The stamp names the
    /// tiers it covers and, for a subset of the plan's tiers, is `stamp-<sha>-<tiers>.json`; a pre-flight gate accepts the union of tier stamps on one sha. The steps a tier needs from another tier run in the same run (its closure) and are listed as ran_for_closure. Not with --only/--skip/--hosts/--mode full
    #[arg(long, value_delimiter = ',', conflicts_with_all = ["only", "skip", "hosts"])]
    pub(crate) tier: Vec<String>,
    /// leave these steps out (comma list)
    #[arg(long, value_delimiter = ',')]
    pub(crate) skip: Vec<String>,
    /// the step cache folder (per host, your choice): a step with `inputs` whose tracked inputs are unchanged since it last passed is not re-run
    /// (printed as STEP_CACHED). For pre-flight only: a proof run must not pass this flag
    #[arg(long)]
    pub(crate) cache: Option<PathBuf>,
    /// run these steps even when the cache has them (comma list)
    #[arg(long, value_delimiter = ',')]
    pub(crate) no_cache_for: Vec<String>,
    /// what this run claims to be: a partial run can never call itself `full` (refused with --only or --skip)
    #[arg(long, default_value = "preflight", value_parser = ["preflight", "lane", "full"])]
    pub(crate) mode: String,
    /// the host's name (the one in hosts.toml); default: $GATETRAIN_HOST (set by `hold`), then the environment, then the OS host name
    #[arg(long)]
    pub(crate) host: Option<String>,
    /// at the end, write this run (its steps, shards, skips and stamp) into this ledger file; it is created or migrated forward
    #[arg(long)]
    pub(crate) db: Option<PathBuf>,
    /// sample the host (load, CPU, I/O wait, disk busy, free memory) every N seconds into <out>/samples.jsonl while the run is alive; 0 = off.
    /// Off when a `hold` already samples (GATETRAIN_SAMPLED). With --db the samples are ingested with the run
    #[arg(long, default_value_t = 10)]
    pub(crate) samples: u64,
    /// the run's id in the ledger (default: chain-<UTC time>-<commit>)
    #[arg(long)]
    pub(crate) run_id: Option<String>,
    /// which ready step starts first: `longest` (the longest median in the --db ledger first; the default when there is history) or `file` (the steps file's order; the default otherwise)
    #[arg(long, value_parser = ["file", "longest"])]
    pub(crate) order: Option<String>,
    /// the core budget: a step with `cores = N` starts only when N of these are free (a step without `cores` counts as 1). Default: this host's core count
    #[arg(long)]
    pub(crate) cores: Option<usize>,
    /// re-run a red step ONCE on the same sha, ONLY when a live row of --quarantine-ref names every test that failed: the step declares its `runner`, ANSI is stripped, the runner's own failed count equals the failing ids parsed, every id is the exact `test` of a live row, and the exit code is the runner's test-failure code (1; cargo 101). No row, no retry; there is no infra retry. Logged as STEP_RETRY, CHAIN_RETRIES and `RED_SUMMARY: none; retried: ...`; listed in the stamp. At most 2 steps per run; sharded steps never. Off by default; needs --quarantine-ref
    #[arg(long, requires = "quarantine_ref")]
    pub(crate) retry_flakes: bool,
    /// the quarantine file for --retry-flakes as `<ref>:<path>` (for example `origin/main:ci/flake-quarantine.csv`), read with `git show`. Must be a remote-tracking ref (refs/remotes/...); refused: a local branch, a sha, HEAD~1, the index form `:path` and any ref that resolves to the commit under test (HEAD). Pass a remote-tracking ref pinned to the trusted main. CSV `test,step,finding,added,expires`, header required, `added <= today <= expires`, at most 14 days
    #[arg(long, requires = "retry_flakes")]
    pub(crate) quarantine_ref: Option<String>,
    /// hosts.toml: spread the steps over every enabled host whose roles include "worker" (docs/multi-host.md). Without a worker the run is local, as before
    #[arg(long)]
    pub(crate) hosts: Option<PathBuf>,
    /// with --hosts: only these workers (comma list)
    #[arg(long, value_delimiter = ',', requires = "hosts")]
    pub(crate) workers: Vec<String>,
    /// with --hosts: how many steps the coordinator itself runs at once (default: --par; 0 = it only dispatches)
    #[arg(long, requires = "hosts")]
    pub(crate) local_slots: Option<usize>,
    /// tags of this host besides its platform tag (comma list); a step whose `requires` this host lacks is refused in a local run
    #[arg(long, value_delimiter = ',')]
    pub(crate) tags: Vec<String>,
    /// incremental re-run: start from the previous run at this commit (or a result file from <out>): a step runs again when it was not green there, its declared `inputs`
    /// or definition changed, it declares no `inputs`, a lockfile or toolchain pin changed, or a step it needs runs; any other step is REUSED (STEP_REUSED, `reused-from <run> <sha>`
    /// in the ledger, `freshness: "reused"` in the stamp). Reuse needs equal tiers, mode, --toolchain-probe and --env-key fingerprints. Not with --hosts (docs/since.md)
    #[arg(long, conflicts_with = "hosts")]
    pub(crate) since: Option<String>,
    /// with --since: a result carried through more than this many runs is run again
    #[arg(long, default_value_t = 5, requires = "since")]
    pub(crate) since_max_age: u32,
    /// with --since: run every step (nothing reused)
    #[arg(long, requires = "since")]
    pub(crate) full: bool,
    /// a shell line whose output is part of the toolchain fingerprint (repeatable; default: rustc -Vv, cargo -V, node -v, git --version); reuse only crosses runs with an equal fingerprint. Example: --toolchain-probe "rustc -V"
    #[arg(long = "toolchain-probe")]
    pub(crate) toolchain_probe: Vec<String>,
    /// the admission guard (docs/admission.md): hold new steps while the host or a protected service is in trouble; a hold is never a red
    #[command(flatten)]
    pub(crate) guard: crate::guardcmd::GuardFlags,
    /// an environment variable whose value is part of the environment fingerprint (comma list; default: RUSTFLAGS, RUSTUP_TOOLCHAIN, CARGO_BUILD_TARGET, NODE_ENV, NODE_OPTIONS, CC, CXX, CI; the value is hashed, never printed)
    #[arg(long, value_delimiter = ',')]
    pub(crate) env_key: Vec<String>,
}

/// Probes used when no `--toolchain-probe` is given: the compilers and runtimes most steps depend on (a missing tool prints a fixed word, so the probe never fails).
const DEFAULT_PROBES: &[&str] = &["rustc -Vv 2>&1 || echo no-rustc", "cargo -V 2>&1 || echo no-cargo", "node -v 2>&1 || echo no-node", "git --version 2>&1 || echo no-git"];
/// Variables hashed into the environment fingerprint when no `--env-key` is given: the ones that change what a build or a test does.
const DEFAULT_ENV: &[&str] = &["RUSTFLAGS", "RUSTUP_TOOLCHAIN", "CARGO_BUILD_TARGET", "CARGO_ENCODED_RUSTFLAGS", "NODE_ENV", "NODE_OPTIONS", "CC", "CXX", "CI"];

/// (toolchain, environment) fingerprints: sha-256 over gatetrain's version, the platform and the probes' output; over the listed variables' values.
fn fingerprints(a: &ChainArgs, cwd: &std::path::Path) -> Result<(String, String), String> {
    use sha2::{Digest, Sha256};
    let probes: Vec<String> = if a.toolchain_probe.is_empty() { DEFAULT_PROBES.iter().map(|s| s.to_string()).collect() } else { a.toolchain_probe.clone() };
    let fp = gatetrain_core::multihost::fingerprint(cwd, &probes)?;
    let tc = Sha256::digest(format!("{}|{}-{}", fp.hash, std::env::consts::ARCH, std::env::consts::OS).as_bytes());
    let mut keys: Vec<String> = if a.env_key.is_empty() { DEFAULT_ENV.iter().map(|s| s.to_string()).collect() } else { a.env_key.clone() };
    keys.sort();
    let env: String = keys.iter().map(|k| format!("{k}={}\n", std::env::var(k).unwrap_or_default())).collect();
    let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
    Ok((hex(&tc), hex(&Sha256::digest(env.as_bytes()))))
}

/// Read a steps file and resolve its `cwd` against the file's own folder (a relative `cwd` means the same from wherever it is run).
pub fn load_plan(steps: &std::path::Path) -> Result<(gatetrain_core::chain::Plan, String), String> {
    let text = std::fs::read_to_string(steps).map_err(|e| format!("cannot read {}: {e}", steps.display()))?;
    let mut plan = load(&text)?;
    if let Some(c) = plan.cwd.as_ref().filter(|c| std::path::Path::new(c).is_relative()) {
        let base = steps.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
        plan.cwd = Some(base.join(c).to_string_lossy().into_owned());
    }
    Ok((plan, text))
}

/// Median seconds per step of the ledger's history; a step with none gets the median of the known ones. None = no history.
pub fn expected_from(db: &std::path::Path, plan: &gatetrain_core::chain::Plan) -> Option<std::collections::HashMap<String, f64>> {
    use gatetrain_core::stats::median;
    let hist = gatetrain_core::history::history_durations(db, "")?;
    let known: Vec<f64> = hist.iter().filter(|(_, v)| !v.is_empty()).map(|(_, v)| median(v)).collect();
    if known.is_empty() {
        return None;
    }
    let fallback = median(&known);
    Some(plan.steps.iter().map(|s| (s.name.clone(), hist.iter().find(|(n, v)| *n == s.name && !v.is_empty()).map_or(fallback, |(_, v)| median(v)))).collect())
}

pub fn run(a: ChainArgs) -> ExitCode {
    let (plan, text) = match load_plan(&a.steps) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("chain: {e}");
            return ExitCode::from(2);
        }
    };
    if a.par == 0 {
        eprintln!("chain: --par must be at least 1");
        return ExitCode::from(2);
    }
    if a.mode == "full" && (!a.only.is_empty() || !a.skip.is_empty() || !a.tier.is_empty()) {
        eprintln!("chain: --mode full cannot be combined with --only, --skip or --tier: a partial or single-tier run is not a full one");
        return ExitCode::from(2);
    }
    if a.cores == Some(0) {
        eprintln!("chain: --cores must be at least 1");
        return ExitCode::from(2);
    }
    let cores = a.cores.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let expected = match (a.order.as_deref(), &a.db) {
        (Some("file"), _) => None,
        (_, Some(db)) => expected_from(db, &plan),
        _ => None,
    };
    if a.hosts.is_some() && a.guard.spec().enabled() {
        eprintln!("chain: the admission guard (--guard-config, --min-free-disk, --probe, --max-*) guards a local run; with --hosts run `gatetrain guard wait` on each host");
        return ExitCode::from(2);
    }
    if let Some(hf) = a.hosts.clone() {
        match crate::mhcmd::workers_of(&hf, &a.workers) {
            Err(e) => {
                eprintln!("chain: {e}");
                return ExitCode::from(2);
            }
            Ok(Some(w)) => return crate::mhcmd::run(a, plan, &text, w, expected),
            Ok(None) => eprintln!("chain: {} names no enabled worker host; the run is local", hf.display()),
        }
    }
    // a local run claims every step it runs: a step whose `requires` this host lacks is refused, never skipped
    let mut local_tags = vec![gatetrain_core::multihost::platform_tag()];
    local_tags.extend(a.tags.iter().cloned());
    for s in plan.steps.iter().filter(|s| (a.only.is_empty() || a.only.contains(&s.name)) && !a.skip.contains(&s.name)) {
        if let Some(t) = s.requires.iter().find(|t| !local_tags.contains(t)) {
            eprintln!("chain: step {:?} requires the tag {t:?}, which this host lacks (it has {}); run it with --hosts, or pass --tags", s.name, local_tags.join(","));
            return ExitCode::from(2);
        }
    }
    let plan_cwd = std::path::PathBuf::from(plan.cwd.clone().unwrap_or_else(|| ".".into()));
    let (toolchain, env_fp) = match fingerprints(&a, &plan_cwd) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("chain: --toolchain-probe: {e}");
            return ExitCode::from(2);
        }
    };
    let guard_spec = a.guard.spec();
    let since = a.since.clone().map(|prev| gatetrain_core::since::SinceOpts { prev, max_age: a.since_max_age, full: a.full, db: a.db.clone() });
    let (db, run_id) = (a.db, a.run_id);
    let out_dir = a.out.clone();
    let o = Opts { tiers: a.tier, run_id: run_id.clone(), toolchain, env_fp, since, par: a.par, out: a.out, keep_going: a.keep_going, only: a.only, skip: a.skip, cache: a.cache, no_cache_for: a.no_cache_for, mode: a.mode, host: a.host, samples: if std::env::var_os("GATETRAIN_SAMPLED").is_some() { 0 } else { a.samples }, expected, cores, retry_flakes: a.retry_flakes, quarantine_ref: a.quarantine_ref, stamp: true, guard: Some(guard_spec) };
    let mut emit = |l: &str| {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{l}");
        let _ = so.flush();
    };
    finish(run_plan(&plan, &digest_of(&text), &o, &mut emit), db, run_id, &out_dir)
}

/// The end of a run, local or multi-host: the summary line, the ledger, the exit code.
pub fn finish(res: Result<gatetrain_core::chain::Outcome, String>, db: Option<PathBuf>, run_id: Option<String>, out_dir: &std::path::Path) -> ExitCode {
    match res {
        Ok(r) => {
            match &r.stamp {
                Ok(p) => eprintln!("chain: green, stamp {}", p.display()),
                Err(why) if r.reds.is_empty() => eprintln!("chain: {why}"),
                Err(_) => {}
            }
            if let Some(db) = &db {
                let rid = run_id.unwrap_or_else(|| default_run_id(&r.record));
                match crate::cmd_runs::open_for_write(&db.to_string_lossy()).and_then(|l| Ok(l.record_chain_run(&rid, &r.record)?)) {
                    Ok(()) => {
                        eprintln!("chain: ledger {}: run {rid}", db.display());
                        // the samples file of this run, if the sampler ran: rows that name no run take this run's id
                        if let Ok(text) = std::fs::read_to_string(out_dir.join("samples.jsonl")) {
                            if let Err(e) = crate::cmd_runs::open_for_write(&db.to_string_lossy()).and_then(|l| Ok(l.record_samples_for(&text, Some(&rid))?)) {
                                eprintln!("chain: ledger {}: the samples were not recorded: {e:#}", db.display());
                            }
                        }
                    }
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
pub fn default_run_id(r: &gatetrain_core::chain::RunRec) -> String {
    gatetrain_core::chain::default_run_id(&r.started_at, r.sha.as_deref())
}
