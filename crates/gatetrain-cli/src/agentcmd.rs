//! `gatetrain agent`: the worker side of a multi-host chain (docs/multi-host.md). Started by the coordinator over ssh, attached (no
//! daemon). `hello` gets the commit into the work dir, runs the host's `prepare` and answers the fingerprint commands; `run` runs ONE
//! step through the ordinary chain engine and reports it; `clean` removes a run's output folder. Each prints exactly one
//! `AGENT_HELLO {json}` / `AGENT_RESULT {json}` line; everything else on stdout is progress.
use clap::{Args, Subcommand};
use gatetrain_core::chain::{digest_of, run as run_plan, tree_status, Opts};
use gatetrain_core::multihost::{fingerprint, platform_tag, AgentResult, Hello, ShardOut, PROTOCOL};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Get the commit into the work dir (a checkout, detached), run `--prepare` once per key, answer the fingerprint commands
    Hello(HelloArgs),
    /// Run one step of the steps file at the commit and report it as one AGENT_RESULT line
    Run(RunArgs),
    /// Remove the output folder of a run
    Clean(CleanArgs),
}

#[derive(Args)]
pub struct HelloArgs {
    #[arg(long)]
    workdir: PathBuf,
    #[arg(long)]
    sha: String,
    /// a bare repository the coordinator pushed into: a missing work dir is created as its worktree
    #[arg(long)]
    repo: Option<PathBuf>,
    /// run `git fetch` in the work dir first (sync = "fetch")
    #[arg(long)]
    fetch: bool,
    /// a shell command run in the work dir after the checkout (for example `npm ci`); a failure refuses the host
    #[arg(long)]
    prepare: Option<String>,
    /// tracked files whose content keys `--prepare` (unchanged since its last success = not run again)
    #[arg(long)]
    prepare_key: Vec<String>,
    /// a fingerprint command (repeat)
    #[arg(long)]
    probe: Vec<String>,
    /// how many checkouts to keep ready: slot 0 is the work dir, slot k is `<work dir>-slot<k>`, a worktree of the same repository
    #[arg(long, default_value_t = 1)]
    slots: usize,
}

#[derive(Args)]
pub struct RunArgs {
    #[arg(long)]
    workdir: PathBuf,
    #[arg(long)]
    sha: String,
    /// the steps file, relative to the repository top
    #[arg(long)]
    steps: String,
    #[arg(long)]
    step: String,
    #[arg(long)]
    run_id: String,
    #[arg(long, default_value_t = 1)]
    par: usize,
    /// the output folder (default: <git dir>/gatetrain/out/<run id>, never part of the tree)
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    probe: Vec<String>,
    /// which checkout runs the step (see `hello --slots`)
    #[arg(long, default_value_t = 0)]
    slot: usize,
}

#[derive(Args)]
pub struct CleanArgs {
    #[arg(long)]
    workdir: PathBuf,
    #[arg(long)]
    run_id: String,
    #[arg(long, default_value_t = 1)]
    slots: usize,
}

/// The checkout of one slot: the work dir itself, or `<work dir>-slot<k>`.
pub fn slot_dir(workdir: &Path, k: usize) -> PathBuf {
    if k == 0 {
        workdir.to_path_buf()
    } else {
        PathBuf::from(format!("{}-slot{k}", workdir.to_string_lossy().trim_end_matches(['/', '\\'])))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| format!("cannot run git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(format!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// `<git dir>/gatetrain`: private to gatetrain, never part of the tree (a worktree has its own git dir).
fn private_dir(workdir: &Path) -> Result<PathBuf, String> {
    Ok(PathBuf::from(git(workdir, &["rev-parse", "--absolute-git-dir"])?).join("gatetrain"))
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !s.starts_with('.')
}

fn valid_sha(s: &str) -> bool {
    s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn print_line(tag: &str, v: serde_json::Value) {
    let mut so = std::io::stdout().lock();
    let _ = writeln!(so, "{tag} {v}");
    let _ = so.flush();
}

fn clean(workdir: &Path, out: &Path) -> Result<(), String> {
    match tree_status(workdir, out) {
        Some(s) if s.is_empty() => Ok(()),
        Some(s) => Err(format!("the work dir has changes ({} path(s) in git status --porcelain); gatetrain never runs or overwrites a dirty tree", s.lines().count())),
        None => Err("git status failed in the work dir".into()),
    }
}

fn hello_inner(a: &HelloArgs) -> Result<Hello, String> {
    if !valid_sha(&a.sha) {
        return Err("--sha must be a commit id".into());
    }
    if !(1..=256).contains(&a.slots) {
        return Err("--slots must be 1 to 256".into());
    }
    if !a.workdir.exists() {
        let repo = a.repo.as_ref().ok_or_else(|| format!("the work dir {} does not exist (give the host a repo to make it from)", a.workdir.display()))?;
        git(repo, &["worktree", "prune"])?;
        git(repo, &["worktree", "add", "--detach", &a.workdir.to_string_lossy(), &a.sha])?;
    }
    clean(&a.workdir, &a.workdir.join(".git"))?;
    if a.fetch {
        git(&a.workdir, &["fetch", "--quiet"])?;
    }
    git(&a.workdir, &["cat-file", "-e", &format!("{}^{{commit}}", a.sha)]).map_err(|_| format!("commit {} is not in the work dir (push it, or fetch it)", a.sha))?;
    let mut head = String::new();
    // every slot has its own checkout, so a step can only ever see (and dirty) its own tree
    for k in 0..a.slots {
        let dir = slot_dir(&a.workdir, k);
        if !dir.exists() {
            let src = a.repo.clone().unwrap_or_else(|| a.workdir.clone());
            git(&src, &["worktree", "prune"])?;
            git(&src, &["worktree", "add", "--detach", &dir.to_string_lossy(), &a.sha]).map_err(|e| format!("slot {k}: {e}"))?;
        }
        head = ready_checkout(&dir, a).map_err(|e| if k == 0 { e } else { format!("slot {k} ({}): {e}", dir.display()) })?;
    }
    let fp = fingerprint(&a.workdir, &a.probe)?;
    Ok(Hello { protocol: PROTOCOL, sha: head, fingerprint: Some(fp), platform: platform_tag(), error: None })
}

/// One checkout at the commit, clean, prepared. Returns its HEAD.
fn ready_checkout(dir: &Path, a: &HelloArgs) -> Result<String, String> {
    let nowhere = dir.join(".git");
    clean(dir, &nowhere)?;
    git(dir, &["checkout", "--detach", "--quiet", &a.sha])?;
    let head = git(dir, &["rev-parse", "HEAD"])?;
    if let Some(cmd) = &a.prepare {
        let mut h = Sha256::new();
        h.update(cmd.as_bytes());
        for f in &a.prepare_key {
            h.update(b"\0");
            h.update(f.as_bytes());
            h.update(git(dir, &["rev-parse", &format!("{head}:{f}")]).unwrap_or_else(|_| "missing".into()).as_bytes());
        }
        let key: String = h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect();
        let pdir = private_dir(dir)?;
        std::fs::create_dir_all(&pdir).map_err(|e| e.to_string())?;
        let marker = pdir.join(format!("prepared-{key}"));
        // no key files: run every time (nothing says the last result still holds)
        if a.prepare_key.is_empty() || !marker.exists() {
            let log = pdir.join("prepare.log");
            let mut c = gatetrain_core::runner::shell_command(cmd);
            c.current_dir(dir);
            match gatetrain_core::runner::run_to_file(c, &log, 3600.0)? {
                Some(0) => {
                    let _ = std::fs::write(&marker, &head);
                }
                Some(rc) => return Err(format!("prepare `{cmd}` exited {rc} (output: {})", log.display())),
                None => return Err(format!("prepare `{cmd}` timed out after 3600 s (output: {})", log.display())),
            }
        }
        clean(dir, &nowhere).map_err(|e| format!("after prepare: {e}"))?;
    }
    Ok(head)
}

fn run_inner(a: &RunArgs) -> Result<AgentResult, String> {
    if !valid_sha(&a.sha) || !valid_id(&a.run_id) {
        return Err("--sha must be a commit id and --run-id letters, digits, . _ -".into());
    }
    let wd = slot_dir(&a.workdir, a.slot);
    let head = git(&wd, &["rev-parse", "HEAD"])?;
    if head != a.sha {
        return Err(format!("the work dir is at {head}, not {}", a.sha));
    }
    let out = match &a.out {
        Some(o) => o.clone(),
        None => private_dir(&wd)?.join("out").join(&a.run_id),
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    // not clean before the step: the step is LOST (it may pass on another checkout), never run on this tree
    if let Err(e) = clean(&wd, &out) {
        return Ok(AgentResult { protocol: PROTOCOL, sha: head, step: a.step.clone(), lost: Some(e), ..Default::default() });
    }
    let fp = fingerprint(&wd, &a.probe)?;
    let steps = wd.join(&a.steps);
    let (mut plan, text) = crate::chaincmd::load_plan(&steps)?;
    // no `cwd`: the repository top, on every host (the coordinator does the same)
    plan.cwd.get_or_insert_with(|| wd.to_string_lossy().into_owned());
    let o = Opts {
        par: a.par.max(1),
        out: out.clone(),
        keep_going: false,
        only: vec![a.step.clone()],
        skip: vec![],
        cache: None,
        no_cache_for: vec![],
        mode: "preflight".into(),
        host: None,
        samples: 0,
        expected: None,
        cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
        retry_flakes: false,
        quarantine_ref: None,
        stamp: false,
    };
    let mut emit = |l: &str| {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{l}");
        let _ = so.flush();
    };
    let r = run_plan(&plan, &digest_of(&text), &o, &mut emit)?;
    let rec = r.record.steps.iter().find(|s| s.name == a.step).ok_or_else(|| format!("step {:?} did not run", a.step))?;
    let mut files = vec![format!("{}.full", a.step)];
    for sh in r.record.shards.iter().filter(|s| s.step == a.step) {
        files.push(sh.log_ref.clone());
    }
    if out.join(format!("{}.check.full", a.step)).exists() {
        files.push(format!("{}.check.full", a.step));
    }
    // the tree after the step: a step that changed it did not run on the commit alone, so it is red
    let dirty = clean(&wd, &out).err();
    Ok(AgentResult {
        protocol: PROTOCOL,
        sha: head,
        fingerprint: fp.hash,
        step: a.step.clone(),
        rc: Some(rec.rc),
        secs: rec.secs,
        started_at: rec.started_at.clone(),
        ended_at: rec.ended_at.clone(),
        shards: r.record.shards.iter().filter(|s| s.step == a.step).map(|s| ShardOut { shard: s.shard, of: s.of, started_at: s.started_at.clone(), secs: s.secs, rc: s.rc, log_ref: s.log_ref.clone() }).collect(),
        files,
        out: out.to_string_lossy().into_owned(),
        error: None,
        lost: None,
        dirty,
    })
}

pub fn run(cmd: AgentCmd) -> ExitCode {
    match cmd {
        AgentCmd::Hello(a) => match hello_inner(&a) {
            Ok(h) => {
                print_line("AGENT_HELLO", serde_json::to_value(&h).unwrap_or_default());
                ExitCode::SUCCESS
            }
            Err(e) => {
                print_line("AGENT_HELLO", serde_json::to_value(&Hello { protocol: PROTOCOL, platform: platform_tag(), error: Some(e), ..Default::default() }).unwrap_or_default());
                ExitCode::from(3)
            }
        },
        AgentCmd::Run(a) => match run_inner(&a) {
            Ok(r) => {
                print_line("AGENT_RESULT", serde_json::to_value(&r).unwrap_or_default());
                ExitCode::SUCCESS
            }
            Err(e) => {
                print_line("AGENT_RESULT", serde_json::to_value(&AgentResult { protocol: PROTOCOL, step: a.step.clone(), error: Some(e), ..Default::default() }).unwrap_or_default());
                ExitCode::from(3)
            }
        },
        AgentCmd::Clean(a) => {
            if !valid_id(&a.run_id) {
                eprintln!("agent clean: --run-id must be letters, digits, . _ -");
                return ExitCode::from(2);
            }
            let mut rc = 0u8;
            for k in 0..a.slots.clamp(1, 256) {
                let dir = slot_dir(&a.workdir, k);
                if !dir.exists() {
                    continue;
                }
                match private_dir(&dir) {
                    Ok(d) => {
                        let p = d.join("out").join(&a.run_id);
                        if p.exists() {
                            if let Err(e) = std::fs::remove_dir_all(&p) {
                                eprintln!("agent clean: {}: {e}", p.display());
                                rc = 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("agent clean: {e}");
                        rc = 1;
                    }
                }
            }
            ExitCode::from(rc)
        }
    }
}
