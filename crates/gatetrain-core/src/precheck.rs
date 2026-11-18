//! `gatetrain batch precheck`: every cheap, parallel check a batch needs before stacking, all findings at once.
//! Read-only: git plumbing (`merge-tree --write-tree` writes only unreferenced objects; `archive` for lane checkouts)
//! plus the commands the operator configured under `[precheck]`. No repository, ref or worktree is changed.
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub enum Sev {
    Red,
    Warn,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub sev: Sev,
    pub check: String,
    pub line: String,
}

#[derive(Debug, Clone, Default)]
pub struct KnownRed {
    pub name: String,
    pub detect: String,
    pub hint: String,
}

#[derive(Debug, Clone)]
pub struct PrecheckConfig {
    pub lane_commands: Vec<String>,
    pub host_commands: Vec<String>,
    pub known_reds: Vec<KnownRed>,
    /// The remote whose base the local base must equal; empty disables the check.
    pub remote: String,
}

impl Default for PrecheckConfig {
    fn default() -> Self {
        PrecheckConfig { lane_commands: vec![], host_commands: vec![], known_reds: vec![], remote: "origin".into() }
    }
}

fn bad(m: String) -> Error {
    Error::Batch(m)
}

impl PrecheckConfig {
    /// A missing file is the empty config; a file that does not parse or has a wrong type is an error (exit 2).
    pub fn load(path: &Path) -> Result<PrecheckConfig> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let v: toml::Value = text.parse().map_err(|e| bad(format!("{}: {e}", path.display())))?;
        let mut c = Self::default();
        let Some(t) = v.get("precheck") else { return Ok(c) };
        let strs = |k: &str| -> Result<Vec<String>> {
            match t.get(k) {
                None => Ok(vec![]),
                Some(toml::Value::Array(a)) => a
                    .iter()
                    .map(|x| x.as_str().map(String::from).ok_or_else(|| bad(format!("[precheck] {k}: every entry must be a string"))))
                    .collect(),
                Some(_) => Err(bad(format!("[precheck] {k} must be a list of strings"))),
            }
        };
        c.lane_commands = strs("lane_commands")?;
        c.host_commands = strs("host_commands")?;
        match t.get("remote") {
            None => {}
            Some(toml::Value::String(s)) => c.remote = s.clone(),
            Some(_) => return Err(bad("[precheck] remote must be a string".into())),
        }
        match t.get("known_reds") {
            None => {}
            Some(toml::Value::Array(a)) => {
                for x in a {
                    let f = |k: &str| x.get(k).and_then(|s| s.as_str()).map(String::from);
                    let name = f("name").ok_or_else(|| bad("[precheck] known_reds: every entry needs a name".into()))?;
                    let detect = f("detect").ok_or_else(|| bad(format!("[precheck] known_reds {name}: needs detect")))?;
                    c.known_reds.push(KnownRed { name, detect, hint: f("hint").unwrap_or_default() });
                }
            }
            Some(_) => return Err(bad("[precheck] known_reds must be a list of tables".into())),
        }
        Ok(c)
    }
}

fn git(repo: &Path, args: &[&str]) -> (i32, String, String) {
    match Command::new("git").arg("-C").arg(repo).args(args).output() {
        Ok(o) => (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned()),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

fn shell(cmd: &str, dir: &Path, env: &[(&str, &str)]) -> (i32, String) {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    c.current_dir(dir).envs(env.iter().copied());
    match c.output() {
        Ok(o) => (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => (-1, e.to_string()),
    }
}

fn tail(out: &str) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(8);
    lines[from..].join(" | ")
}

/// `git merge-tree --write-tree --name-only a b`: Ok(empty) when clean, Ok(files) on conflict, Err when git itself failed.
pub fn merge_conflicts(repo: &Path, a: &str, b: &str) -> std::result::Result<Vec<String>, String> {
    let (code, out, err) = git(repo, &["merge-tree", "--write-tree", "--name-only", a, b]);
    match code {
        0 => Ok(vec![]),
        1 => {
            // first line is the tree id, then the conflicted names up to the first blank line
            let files: Vec<String> = out.lines().skip(1).take_while(|l| !l.is_empty()).map(String::from).collect();
            Ok(if files.is_empty() { vec!["(unnamed conflict)".into()] } else { files })
        }
        _ => Err(format!("git merge-tree failed ({code}): {}", err.trim())),
    }
}

struct Checkout(PathBuf);
impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A temporary checkout of `lane` made with `git archive | tar -x`; touches no worktree, index or ref of the repository.
fn checkout(repo: &Path, lane: &str, n: usize) -> std::result::Result<Checkout, String> {
    let dir = std::env::temp_dir().join(format!("gatetrain-precheck-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let c = Checkout(dir);
    let mut g = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", "--format=tar", lane])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let t = Command::new("tar").arg("-x").current_dir(&c.0).stdin(Stdio::from(g.stdout.take().unwrap())).output().map_err(|e| e.to_string())?;
    let gs = g.wait().map_err(|e| e.to_string())?;
    if !gs.success() || !t.status.success() {
        return Err(format!("cannot check out {lane}: {}", String::from_utf8_lossy(&t.stderr).trim()));
    }
    Ok(c)
}

type Task<'a> = Box<dyn FnOnce() -> Vec<Finding> + Send + 'a>;

fn red(check: &str, line: String) -> Finding {
    Finding { sev: Sev::Red, check: check.into(), line }
}
fn warn(check: &str, line: String) -> Finding {
    Finding { sev: Sev::Warn, check: check.into(), line }
}

fn run_cmd(check: &str, label: &str, cmd: &str, dir: &Path, env: &[(&str, &str)]) -> Vec<Finding> {
    let (code, out) = shell(cmd, dir, env);
    if code != 0 {
        vec![red(check, format!("{label}: `{cmd}` exited {code}: {}", tail(&out)))]
    } else if out.trim().is_empty() {
        vec![warn(check, format!("{label}: `{cmd}` exited 0 with no output (a check with no output may not have run)"))]
    } else {
        vec![]
    }
}

pub struct Report {
    pub findings: Vec<Finding>,
    pub checks_run: usize,
}

impl Report {
    pub fn reds(&self) -> usize {
        self.findings.iter().filter(|f| f.sev == Sev::Red).count()
    }
    pub fn warns(&self) -> usize {
        self.findings.iter().filter(|f| f.sev == Sev::Warn).count()
    }
    /// One line per finding, then the summary.
    pub fn render(&self) -> String {
        let mut s = String::new();
        for f in &self.findings {
            s.push_str(&format!("{} {}: {}\n", if f.sev == Sev::Red { "RED " } else { "WARN" }, f.check, f.line));
        }
        s.push_str(&format!(
            "precheck: {} checks, {} red, {} warning -> {}\n",
            self.checks_run,
            self.reds(),
            self.warns(),
            if self.reds() == 0 { "GREEN" } else { "RED" }
        ));
        s
    }
}

pub fn run(repo: &Path, cfg: &PrecheckConfig, base: &str, lanes: &[String], par: usize) -> Report {
    let mut tasks: Vec<Task> = Vec::new();
    // refs must resolve, else every later finding is noise
    let mut missing: Vec<Finding> = vec![];
    for r in std::iter::once(base).chain(lanes.iter().map(|s| s.as_str())) {
        if git(repo, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]).0 != 0 {
            missing.push(red("refs", format!("{r}: not a commit in this repository")));
        }
    }
    if !missing.is_empty() {
        return Report { checks_run: 1, findings: missing };
    }
    // 1. pairwise and lane-vs-base
    for (i, a) in lanes.iter().enumerate() {
        for b in &lanes[i + 1..] {
            tasks.push(Box::new(move || match merge_conflicts(repo, a, b) {
                Ok(f) if f.is_empty() => vec![],
                Ok(f) => vec![red("pair-conflict", format!("{a} x {b}: {}", f.join(", ")))],
                Err(e) => vec![red("pair-conflict", format!("{a} x {b}: {e}"))],
            }));
        }
    }
    for a in lanes {
        tasks.push(Box::new(move || match merge_conflicts(repo, base, a) {
            Ok(f) if f.is_empty() => vec![],
            Ok(f) => vec![red("base-conflict", format!("{a} x {base}: {}", f.join(", ")))],
            Err(e) => vec![red("base-conflict", format!("{a} x {base}: {e}"))],
        }));
    }
    // 2. per-lane commands, in a temporary checkout of the lane
    if !cfg.lane_commands.is_empty() {
        for (n, lane) in lanes.iter().enumerate() {
            tasks.push(Box::new(move || match checkout(repo, lane, n) {
                Err(e) => vec![red("lane-command", format!("{lane}: {e}"))],
                Ok(c) => cfg.lane_commands.iter().flat_map(|cmd| run_cmd("lane-command", lane, cmd, &c.0, &[("GATETRAIN_LANE", lane)])).collect(),
            }));
        }
    }
    // 3. host readiness
    if !cfg.remote.is_empty() {
        tasks.push(Box::new(move || {
            let local = git(repo, &["rev-parse", "--verify", &format!("{base}^{{commit}}")]).1.trim().to_string();
            let (code, out, err) = git(repo, &["ls-remote", &cfg.remote, &format!("refs/heads/{base}")]);
            if code != 0 {
                return vec![red("host-base", format!("cannot read remote {}: {}", cfg.remote, err.trim()))];
            }
            let Some(remote) = out.split_whitespace().next().map(String::from) else {
                return vec![warn("host-base", format!("remote {} has no branch {base} (a non-branch base is not compared)", cfg.remote))];
            };
            if remote == local {
                return vec![];
            }
            let known = git(repo, &["cat-file", "-e", &format!("{remote}^{{commit}}")]).0 == 0;
            let rel = if !known {
                "not at (the remote commit is not fetched here, so local is behind or diverged)"
            } else if git(repo, &["merge-base", "--is-ancestor", &local, &remote]).0 == 0 {
                "behind"
            } else if git(repo, &["merge-base", "--is-ancestor", &remote, &local]).0 == 0 {
                "ahead of"
            } else {
                "diverged from"
            };
            vec![red("host-base", format!("local {base} {} is {rel} {}/{base} {}", &local[..10.min(local.len())], cfg.remote, &remote[..10.min(remote.len())]))]
        }));
    }
    tasks.push(Box::new(move || {
        let (_, st, _) = git(repo, &["status", "--porcelain", "-z", "--untracked-files=all"]);
        let untracked: Vec<&str> = st.split('\0').filter_map(|e| e.strip_prefix("?? ")).collect();
        if untracked.is_empty() {
            return vec![];
        }
        let mut f = vec![];
        for lane in lanes {
            let (_, added, _) = git(repo, &["diff", "--name-only", "--diff-filter=A", "-z", base, lane]);
            for p in added.split('\0').filter(|p| !p.is_empty()) {
                if untracked.contains(&p) {
                    f.push(red("untracked-collision", format!("{lane} adds {p}, which sits untracked in the main checkout")));
                }
            }
        }
        f
    }));
    for cmd in &cfg.host_commands {
        tasks.push(Box::new(move || run_cmd("host-command", "host", cmd, repo, &[])));
    }
    // 4. known reds
    for k in &cfg.known_reds {
        tasks.push(Box::new(move || {
            let (code, out) = shell(&k.detect, repo, &[]);
            if code == 0 {
                vec![]
            } else {
                vec![red("known-red", format!("{}: {}{}", k.name, k.hint, if out.trim().is_empty() { String::new() } else { format!(" [{}]", tail(&out)) }))]
            }
        }));
    }

    let checks_run = tasks.len();
    let queue = Mutex::new(tasks.into_iter().enumerate().rev().collect::<Vec<_>>());
    let results: Mutex<Vec<(usize, Vec<Finding>)>> = Mutex::new(vec![]);
    std::thread::scope(|s| {
        for _ in 0..par.max(1) {
            s.spawn(|| loop {
                let job = queue.lock().unwrap().pop();
                let Some((i, t)) = job else { break };
                let r = t();
                results.lock().unwrap().push((i, r));
            });
        }
    });
    let mut all = results.into_inner().unwrap();
    all.sort_by_key(|(i, _)| *i);
    Report { checks_run, findings: all.into_iter().flat_map(|(_, f)| f).collect() }
}

static SELF_TEST_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The self-test: a fixture repository with one known conflicting pair; the check must report it.
pub fn self_test() -> std::result::Result<String, String> {
    self_test_with(&merge_conflicts)
}

/// The self-test with the conflict check injected, so a test can prove it fails when the check finds nothing.
pub fn self_test_with(check: &dyn Fn(&Path, &str, &str) -> std::result::Result<Vec<String>, String>) -> std::result::Result<String, String> {
    let dir = std::env::temp_dir().join(format!("gatetrain-precheck-selftest-{}-{}", std::process::id(), SELF_TEST_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _c = Checkout(dir.clone());
    let g = |a: &[&str]| -> std::result::Result<(), String> {
        let (c, _, e) = git(&dir, a);
        if c == 0 {
            Ok(())
        } else {
            Err(format!("{a:?}: {e}"))
        }
    };
    g(&["init", "-q", "-b", "main"])?;
    for (k, v) in [("user.email", "t@example.com"), ("user.name", "t"), ("commit.gpgsign", "false")] {
        g(&["config", k, v])?;
    }
    let w = |c: &str| std::fs::write(dir.join("f"), c).map_err(|e| e.to_string());
    w("1\n")?;
    g(&["add", "--", "f"])?;
    g(&["commit", "-q", "-m", "init"])?;
    for (b, c) in [("a", "2\n"), ("b", "3\n")] {
        g(&["switch", "-q", "-c", b, "main"])?;
        w(c)?;
        g(&["commit", "-q", "-am", b])?;
    }
    match check(&dir, "a", "b")? {
        f if f == ["f"] => Ok("self-test: known conflicting pair a x b reported (f)".into()),
        f => Err(format!("self-test FAILED: the known conflict a x b was not reported (got {f:?})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_test_passes_with_the_real_check() {
        assert!(self_test().is_ok());
    }

    #[test]
    fn self_test_fails_when_the_check_finds_nothing_on_a_known_conflict() {
        let e = self_test_with(&|_, _, _| Ok(vec![])).unwrap_err();
        assert!(e.contains("self-test FAILED"), "{e}");
    }
}
