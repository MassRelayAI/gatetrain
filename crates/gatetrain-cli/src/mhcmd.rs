//! `gatetrain chain --hosts`: read the workers from hosts.toml and run the plan over them (docs/multi-host.md).
use crate::chaincmd::{finish, ChainArgs};
use crate::mhtransport::{ssh_program, Real, Worker};
use gatetrain_core::chain::{digest_of, Plan};
use gatetrain_core::multihost::{fingerprint, mh_config, platform_tag, HostSpec, MhConfig, MhOpts};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

pub type Workers = (Vec<HostSpec>, HashMap<String, Worker>, MhConfig);

/// The enabled hosts whose roles include "worker" (only those named in `only`, when given). None = no worker: the run is local.
pub fn workers_of(file: &Path, only: &[String]) -> Result<Option<Workers>, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let cfg = gatetrain_core::config::load(file).map_err(|e| e.to_string())?;
    let mhc = mh_config(&text)?;
    for n in only {
        if !cfg.hosts.iter().any(|h| &h.name == n && h.roles.iter().any(|r| r == "worker")) {
            return Err(format!("--workers names {n:?}, which is no worker host of {}", file.display()));
        }
    }
    let mut specs = Vec::new();
    let mut map = HashMap::new();
    for h in cfg.hosts.iter().filter(|h| h.enabled && h.roles.iter().any(|r| r == "worker") && (only.is_empty() || only.contains(&h.name))) {
        let target = h.reach.strip_prefix("ssh ").map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| format!("host {:?}: a worker needs reach = \"ssh <target>\", got {:?}", h.name, h.reach))?;
        if target.starts_with('-') || target.contains(char::is_whitespace) {
            return Err(format!("host {:?}: reach must be \"ssh <user@address>\" with one target and no ssh options, got {:?}", h.name, h.reach));
        }
        let w = &h.worker;
        let workdir = w.workdir.clone().ok_or_else(|| format!("host {:?}: a worker needs a workdir", h.name))?;
        let slots = w.slots.or(h.max_parallel_steps).unwrap_or(1).max(1) as usize;
        specs.push(HostSpec { name: h.name.clone(), tags: w.tags.clone(), slots, cores: h.cores.map(|c| c.max(1) as usize), local: false });
        map.insert(
            h.name.clone(),
            Worker {
                target: target.to_string(),
                workdir,
                repo: w.repo.clone(),
                push_url: w.push_url.clone(),
                sync: w.sync.clone().unwrap_or_else(|| "push".into()),
                bin: w.gatetrain_bin.clone().unwrap_or_else(|| "gatetrain".into()),
                prepare: w.prepare.clone(),
                prepare_key: w.prepare_key.clone(),
                prefix: w.command_prefix.clone(),
            },
        );
    }
    Ok((!specs.is_empty()).then_some((specs, map, mhc)))
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn plain(p: &Path) -> PathBuf {
    let c = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    PathBuf::from(c.to_string_lossy().trim_start_matches(r"\\?\"))
}

fn fail(msg: String) -> ExitCode {
    eprintln!("chain: {msg}");
    ExitCode::from(2)
}

pub fn run(a: ChainArgs, mut plan: Plan, text: &str, w: Workers, expected: Option<HashMap<String, f64>>) -> ExitCode {
    let (specs, workers, mut mhc) = w;
    if a.cache.is_some() || a.retry_flakes {
        return fail("--cache and --retry-flakes are refused with --hosts: a multi-host run is a proof-like run".into());
    }
    let steps_dir = a.steps.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Some(top) = git(steps_dir, &["rev-parse", "--show-toplevel"]).map(|t| plain(Path::new(&t))) else {
        return fail("--hosts needs the steps file inside a git repository".into());
    };
    let steps_abs = plain(&a.steps);
    let Ok(rel) = steps_abs.strip_prefix(&top).map(|r| r.to_string_lossy().replace('\\', "/")) else {
        return fail(format!("the steps file {} is not inside {}", steps_abs.display(), top.display()));
    };
    if git(&top, &["ls-files", "--error-unmatch", "--", &rel]).is_none() {
        return fail(format!("the steps file {rel} is not tracked: every host reads it from the commit"));
    }
    let Some(sha) = git(&top, &["rev-parse", "HEAD"]) else { return fail("no commit to run".into()) };
    // no `cwd`: the repository top, on every host (the agent does the same)
    plan.cwd.get_or_insert_with(|| top.to_string_lossy().into_owned());
    mhc.local_tags.extend(a.tags.iter().cloned());
    let fp = match fingerprint(&top, &mhc.all_probes()) {
        Ok(f) => f,
        Err(e) => return fail(format!("the coordinator's own fingerprint: {e}")),
    };
    if let Err(e) = std::fs::create_dir_all(&a.out) {
        return fail(format!("cannot create {}: {e}", a.out.display()));
    }
    let out = plain(&a.out);
    let run_id = a.run_id.clone().unwrap_or_else(|| {
        let t: String = gatetrain_core::logfmt::Ts::now_utc().iso().chars().take_while(|c| *c != '+' && *c != 'Z').filter(char::is_ascii_digit).collect();
        format!("chain-{t}-{}", &sha[..7.min(sha.len())])
    });
    let o = MhOpts {
        out: out.clone(),
        keep_going: a.keep_going,
        only: a.only.clone(),
        skip: a.skip.clone(),
        mode: a.mode.clone(),
        host: a.host.clone(),
        run_id: run_id.clone(),
        weights: expected.unwrap_or_default(),
        local_slots: a.local_slots.unwrap_or(a.par),
        local_cores: a.cores,
        workers: specs,
        cfg: mhc,
        fingerprint: fp,
        platform: platform_tag(),
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return fail(format!("cannot find this binary: {e}")),
    };
    let t = Arc::new(Real { workers, steps_rel: rel, top, out: out.clone(), exe, ssh: ssh_program() });
    let mut emit = |l: &str| {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{l}");
        let _ = so.flush();
    };
    let res = gatetrain_core::multihost::run(&plan, &digest_of(text), &o, t, &mut emit);
    finish(res, a.db, Some(run_id), &out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(body: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("hosts.toml");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }

    #[test]
    fn a_reach_that_looks_like_an_ssh_option_is_refused() {
        let (_d, p) = file("[[hosts]]\nname = 'w'\nroles = ['worker']\nreach = 'ssh -oProxyCommand=touch-x'\nworkdir = '/w'\n");
        let e = workers_of(&p, &[]).expect_err("refused");
        assert!(e.contains("reach"), "{e}");
        let (_d2, p2) = file("[[hosts]]\nname = 'w'\nroles = ['worker']\nreach = 'ssh user@192.0.2.1'\nworkdir = '/w'\n");
        assert!(workers_of(&p2, &[]).unwrap().is_some());
    }
}
