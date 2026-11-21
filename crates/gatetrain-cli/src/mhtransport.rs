//! The real multi-host transport (docs/multi-host.md): ssh to each worker running `gatetrain agent`, attached, one session per step;
//! the coordinator's own slots run the same agent as a child process. `GATETRAIN_SSH` replaces the ssh program (a JSON list, or words
//! split on spaces): the tests stand a fixture in for ssh with it.
use gatetrain_core::multihost::{AgentResult, Hello, HostSpec, StepReq, Transport};
use gatetrain_core::pycompat::shlex_join;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How the coordinator reaches one worker.
#[derive(Debug, Clone)]
pub struct Worker {
    pub target: String,
    pub workdir: String,
    pub repo: Option<String>,
    pub push_url: Option<String>,
    /// push | fetch
    pub sync: String,
    pub bin: String,
    pub prepare: Option<String>,
    pub prepare_key: Vec<String>,
    pub prefix: Vec<String>,
}

pub struct Real {
    pub workers: HashMap<String, Worker>,
    /// the steps file relative to the repository top
    pub steps_rel: String,
    /// the coordinator's repository top
    pub top: PathBuf,
    /// the coordinator's `--out` (its own slots write there directly)
    pub out: PathBuf,
    pub exe: PathBuf,
    pub ssh: Vec<String>,
}

/// The ssh program and its fixed arguments: `GATETRAIN_SSH`, else `ssh`.
pub fn ssh_program() -> Vec<String> {
    match std::env::var("GATETRAIN_SSH") {
        Ok(v) if v.trim_start().starts_with('[') => serde_json::from_str(&v).unwrap_or_else(|_| vec!["ssh".into()]),
        Ok(v) if !v.trim().is_empty() => v.split_whitespace().map(String::from).collect(),
        _ => vec!["ssh".into()],
    }
}

/// `<ssh> <target> -o ... '<command line>'`: the options make a silent host a lost one within about a minute.
pub fn ssh_argv(ssh: &[String], target: &str, remote: &[String]) -> Vec<String> {
    let mut v: Vec<String> = ssh.to_vec();
    for o in ["BatchMode=yes", "ConnectTimeout=10", "ServerAliveInterval=15", "ServerAliveCountMax=4"] {
        v.push("-o".into());
        v.push(o.into());
    }
    // options first, then `--`: a target can never be read as an option
    v.push("--".into());
    v.push(target.to_string());
    v.push(shlex_join(remote));
    v
}

/// `git push --force <url> <sha>:refs/gatetrain/incoming`: the worker needs no credentials to any git host, only the coordinator's ssh.
pub fn push_argv(top: &Path, w: &Worker, sha: &str) -> Vec<String> {
    let url = w.push_url.clone().unwrap_or_else(|| format!("{}:{}", w.target, w.repo.as_deref().unwrap_or(&w.workdir)));
    vec!["git".into(), "-C".into(), top.to_string_lossy().into_owned(), "push".into(), "--quiet".into(), "--force".into(), url, format!("{sha}:refs/gatetrain/incoming")]
}

/// The agent's own arguments for `hello` on a worker with `slots` checkouts.
pub fn hello_args(w: &Worker, sha: &str, probes: &[String], slots: usize) -> Vec<String> {
    let mut a = vec![w.bin.clone(), "agent".into(), "hello".into(), "--workdir".into(), w.workdir.clone(), "--sha".into(), sha.into(), "--slots".into(), slots.to_string()];
    if let Some(r) = &w.repo {
        a.extend(["--repo".into(), r.clone()]);
    }
    if w.sync == "fetch" {
        a.push("--fetch".into());
    }
    if let Some(p) = &w.prepare {
        a.extend(["--prepare".into(), p.clone()]);
    }
    for k in &w.prepare_key {
        a.extend(["--prepare-key".into(), k.clone()]);
    }
    for p in probes {
        a.extend(["--probe".into(), p.clone()]);
    }
    a
}

fn run_args(bin: &str, workdir: &str, steps_rel: &str, req: &StepReq, out: Option<&Path>, slot: Option<usize>) -> Vec<String> {
    let mut a = vec![bin.into(), "agent".into(), "run".into(), "--workdir".into(), workdir.into(), "--sha".into(), req.sha.clone(), "--steps".into(), steps_rel.into(), "--step".into(), req.step.clone(), "--run-id".into(), req.run_id.clone(), "--par".into(), req.par.to_string()];
    if let Some(o) = out {
        a.extend(["--out".into(), o.to_string_lossy().into_owned()]);
    }
    if let Some(k) = slot {
        a.extend(["--slot".into(), k.to_string()]);
    }
    for p in &req.probes {
        a.extend(["--probe".into(), p.clone()]);
    }
    a
}

/// Run a command to its end (or `limit`), every stdout line to `line`; returns (exit code or None on timeout, the tagged JSON line, stderr tail).
fn stream(argv: &[String], tag: &str, limit: Option<f64>, line: &mut dyn FnMut(&str)) -> Result<(Option<i32>, Option<String>, String), String> {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("cannot start {}: {e}", argv[0]))?;
    let so = child.stdout.take().expect("piped");
    let mut se = child.stderr.take().expect("piped");
    let (tx, rx) = mpsc::channel::<String>();
    let rd = std::thread::spawn(move || {
        for l in BufReader::new(so).lines() {
            match l {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let er = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        let s = String::from_utf8_lossy(&b).into_owned();
        s.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect::<String>()
    });
    let deadline = limit.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let mut found = None;
    let prefix = format!("{tag} ");
    loop {
        let wait = deadline.map_or(Duration::from_secs(3600), |d| d.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(wait) {
            Ok(l) => {
                let l = l.trim_end_matches('\r').to_string();
                match l.strip_prefix(&prefix) {
                    Some(j) => found = Some(j.to_string()),
                    None => line(&l),
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    gatetrain_core::runner::kill_tree(&mut child);
                    let _ = child.wait();
                    let _ = rd.join();
                    return Ok((None, found, er.join().unwrap_or_default()));
                }
            }
        }
    }
    let st = child.wait().map_err(|e| e.to_string())?;
    let _ = rd.join();
    Ok((st.code(), found, er.join().unwrap_or_default()))
}

/// A file name a worker may send back for this step: only `<step>.full`, `<step>.check.full` and the step's own shard files
/// (`<step>.<i>of<n>.full`, as its result names them), in `[A-Za-z0-9._-]`. Anything else (another step's file, a stamp, `C:x`,
/// a path) is refused, so a worker can never write outside its own step's files in --out.
pub fn output_name_ok(step: &str, shards: &[String], f: &str) -> bool {
    let plain = !f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !f.starts_with('.');
    let shard = |x: &str| {
        x.strip_prefix(step).and_then(|r| r.strip_prefix('.')).and_then(|r| r.strip_suffix(".full")).and_then(|r| r.split_once("of")).is_some_and(|(i, n)| {
            matches!((i.parse::<u32>(), n.parse::<u32>()), (Ok(i), Ok(n)) if i >= 1 && i <= n && n <= 64)
        })
    };
    plain && (f == format!("{step}.full") || f == format!("{step}.check.full") || (shards.iter().any(|s| s == f) && shard(f)))
}

fn tail(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Real {
    fn worker(&self, h: &HostSpec) -> Result<&Worker, String> {
        self.workers.get(&h.name).ok_or_else(|| format!("no worker named {:?}", h.name))
    }
}

impl Transport for Real {
    fn hello(&self, host: &HostSpec, sha: &str, probes: &[String]) -> Result<Hello, String> {
        let w = self.worker(host)?;
        if w.sync == "push" {
            let argv = push_argv(&self.top, w, sha);
            let o = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).output().map_err(|e| format!("cannot run git push: {e}"))?;
            if !o.status.success() {
                return Err(format!("git push to the host failed: {}", tail(&String::from_utf8_lossy(&o.stderr))));
            }
        }
        let argv = ssh_argv(&self.ssh, &w.target, &hello_args(w, sha, probes, host.slots));
        let (rc, json, err) = stream(&argv, "AGENT_HELLO", Some(3700.0), &mut |_| {})?;
        match json {
            Some(j) => serde_json::from_str(&j).map_err(|e| format!("an AGENT_HELLO line that is not valid: {e}")),
            None => Err(format!("no AGENT_HELLO line (exit {}): {}", rc.map_or("none: timed out".into(), |c| c.to_string()), tail(&err))),
        }
    }

    fn run_step(&self, host: &HostSpec, req: &StepReq, line: &mut dyn FnMut(&str)) -> Result<AgentResult, String> {
        let argv = if host.local {
            // the coordinator's own slots: the same agent, as a child process in its own checkout, writing straight into --out
            run_args(&self.exe.to_string_lossy(), &self.top.to_string_lossy(), &self.steps_rel, req, Some(&self.out), None)
        } else {
            let w = self.worker(host)?;
            let mut remote = w.prefix.clone();
            remote.extend(run_args(&w.bin, &w.workdir, &self.steps_rel, req, None, Some(req.slot)));
            ssh_argv(&self.ssh, &w.target, &remote)
        };
        // the step's own timeout is enforced by the engine on the host; this one only catches a session that never ends
        let limit = req.timeout_s.map(|t| t + 600.0);
        let (rc, json, err) = stream(&argv, "AGENT_RESULT", limit, line)?;
        match json {
            Some(j) => serde_json::from_str(&j).map_err(|e| format!("an AGENT_RESULT line that is not valid: {e}")),
            None => Err(match rc {
                None => "the session ran past the step's timeout and was killed".to_string(),
                Some(c) => format!("the session ended without a result (exit {c}): {}", tail(&err)),
            }),
        }
    }

    fn fetch(&self, host: &HostSpec, res: &AgentResult, out: &Path) -> Result<(), String> {
        let w = self.worker(host)?;
        let mut missing = Vec::new();
        for f in &res.files {
            if !output_name_ok(&res.step, &res.shards.iter().map(|s| s.log_ref.clone()).collect::<Vec<_>>(), f) {
                missing.push(format!("{f} (not a plain file name)"));
                continue;
            }
            let argv = ssh_argv(&self.ssh, &w.target, &["cat".into(), "--".into(), format!("{}/{f}", res.out.trim_end_matches('/'))]);
            match Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).output() {
                Ok(o) if o.status.success() => {
                    if let Err(e) = std::fs::write(out.join(f), &o.stdout) {
                        missing.push(format!("{f} ({e})"));
                    }
                }
                Ok(o) => missing.push(format!("{f} ({})", tail(&String::from_utf8_lossy(&o.stderr)))),
                Err(e) => missing.push(format!("{f} ({e})")),
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing.join("; "))
        }
    }

    fn finish(&self, host: &HostSpec, run_id: &str) {
        if let Ok(w) = self.worker(host) {
            let argv = ssh_argv(&self.ssh, &w.target, &[w.bin.clone(), "agent".into(), "clean".into(), "--workdir".into(), w.workdir.clone(), "--run-id".into(), run_id.into(), "--slots".into(), host.slots.to_string()]);
            let _ = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w() -> Worker {
        Worker { target: "user@10.0.0.7".into(), workdir: "/srv/gt/work".into(), repo: Some("/srv/gt/repo.git".into()), push_url: None, sync: "push".into(), bin: "gatetrain".into(), prepare: Some("npm ci".into()), prepare_key: vec!["package-lock.json".into()], prefix: vec!["caffeinate".into(), "-i".into()] }
    }

    #[test]
    fn the_push_goes_to_the_bare_repo_over_the_hosts_ssh() {
        let a = push_argv(Path::new("/r"), &w(), "abc1234");
        assert_eq!(a[3..], ["push", "--quiet", "--force", "user@10.0.0.7:/srv/gt/repo.git", "abc1234:refs/gatetrain/incoming"]);
        let mut x = w();
        x.push_url = Some("/elsewhere.git".into());
        assert_eq!(push_argv(Path::new("/r"), &x, "s")[6], "/elsewhere.git");
    }

    #[test]
    fn hello_carries_the_repo_the_prepare_and_the_probes() {
        let a = hello_args(&w(), "abc1234", &["node -v".into()], 3);
        let j = a.join(" ");
        assert!(j.contains("--repo /srv/gt/repo.git") && j.contains("--prepare npm ci") && j.contains("--prepare-key package-lock.json") && j.contains("--probe node -v"), "{j}");
        assert!(!j.contains("--fetch"), "push mode does not fetch");
        assert!(j.contains("--slots 3"), "{j}");
    }

    #[test]
    fn ssh_is_batch_mode_with_keepalive_options_first_then_the_target_after_a_double_dash() {
        let v = ssh_argv(&["ssh".into()], "h", &["gatetrain".into(), "agent".into(), "--probe".into(), "node -v".into()]);
        let dd = v.iter().position(|a| a == "--").expect("a -- before the target");
        assert_eq!(v[dd + 1], "h");
        assert!(v[..dd].contains(&"BatchMode=yes".to_string()) && v[..dd].contains(&"ServerAliveInterval=15".to_string()));
        assert_eq!(v.last().unwrap(), "gatetrain agent --probe 'node -v'");
        assert_eq!(v.len(), dd + 3);
    }

    #[test]
    fn only_the_steps_own_output_files_are_copied_back() {
        let shards = ["unit.1of2.full".to_string(), "unit.2of2.full".to_string()];
        for ok in ["unit.full", "unit.check.full", "unit.1of2.full"] {
            assert!(output_name_ok("unit", &shards, ok), "{ok}");
        }
        for bad in ["other.full", "stamp-abc.json", "C:evil", "unit.full:x", "unit.3of2.full", "..", "unit.ful", "u nit.full"] {
            assert!(!output_name_ok("unit", &shards, bad), "{bad}");
        }
    }
}
