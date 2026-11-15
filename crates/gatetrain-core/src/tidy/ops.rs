//! Host operations and the guarded removal of `tidy.py` (`Ops`). Every command goes through the `Runner`; every removal names one
//! exact path. The guards (L12, L43-L51): bulk commands refused, kept paths refused, links unlinked before a folder is removed,
//! the link listing proven to have run, the main checkout compared before and after.
use super::config::TidyCfg;
use crate::config::{host_os, Host};
use crate::pycompat::{is_digits, shlex_quote, split_ws, splitlines, strip};
use crate::runner::{RunOut, Runner};
use regex::Regex;
use std::cell::Cell;

pub const END: &str = "__GT_END__";

/// A guard refused, or something else went wrong. A guard violation stops a tidy run (exit 3); the rest is an error (exit 2).
#[derive(Debug, Clone, PartialEq)]
pub enum Fatal {
    Guard(String),
    Other(String),
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fatal::Guard(s) | Fatal::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Fatal {}

impl From<String> for Fatal {
    fn from(s: String) -> Fatal {
        Fatal::Other(s)
    }
}

pub type R<T> = Result<T, Fatal>;

pub fn guard<T>(msg: String) -> R<T> {
    Err(Fatal::Guard(msg))
}

/// What a command returned: `rc` is `None` when it did not finish in time (Python's TIMEOUT), and then never equals 0.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub rc: Option<i64>,
    pub out: String,
}

impl Cmd {
    pub fn ok(&self) -> bool {
        self.rc == Some(0)
    }
}

pub fn norm(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

pub fn lane_of(path: &str, cfg: &TidyCfg) -> String {
    let n = norm(path);
    let b = n.rsplit('/').next().unwrap_or("").to_string();
    let p = cfg.worktree_prefix.to_lowercase();
    if !p.is_empty() && b.starts_with(&p) {
        b.chars().skip(p.chars().count()).collect()
    } else {
        b
    }
}

pub struct Ops<'a> {
    pub host: &'a Host,
    pub runner: &'a dyn Runner,
    pub cfg: &'a TidyCfg,
    pub win: bool,
    canary_ok: Cell<bool>,
}

/// `(tracked file count, status text)` of the main checkout.
pub type Snapshot = (usize, String);

impl<'a> Ops<'a> {
    pub fn new(host: &'a Host, runner: &'a dyn Runner, cfg: &'a TidyCfg) -> Ops<'a> {
        Ops { host, runner, cfg, win: host_os(host) == "windows", canary_ok: Cell::new(false) }
    }

    // -- primitives
    pub fn q(&self, p: &str) -> String {
        if self.win {
            format!("\"{}\"", p.replace('/', "\\"))
        } else {
            shlex_quote(p)
        }
    }

    pub fn psq(&self, p: &str) -> String {
        format!("'{}'", p.replace('/', "\\").replace('\'', "''"))
    }

    /// (rc, out). A command that times out returns rc None instead of failing; callers treat it as a failure.
    pub fn run(&self, cmd: &str, timeout: Option<f64>) -> R<Cmd> {
        let low = cmd.to_lowercase();
        if low.contains("system prune") || bulk_rm().is_match(&low) {
            return guard(format!("refusing a bulk command: {cmd}"));
        }
        Ok(match self.runner.run_for(cmd, timeout)? {
            RunOut::Done { rc, out } => Cmd { rc: Some(rc), out },
            RunOut::TimedOut => Cmd { rc: None, out: String::new() },
        })
    }

    pub fn run0(&self, cmd: &str) -> R<Cmd> {
        self.run(cmd, None)
    }

    /// True when p sits on a disk configured measure = age-only: never sized there (L51).
    pub fn age_only(&self, p: &str) -> bool {
        let n = norm(p);
        let mut best: Option<&crate::config::Disk> = None;
        for d in &self.host.disks {
            let dn = norm(&d.path);
            if (n == dn || n.starts_with(&format!("{dn}/"))) && best.map(|b| dn.chars().count() > norm(&b.path).chars().count()).unwrap_or(true) {
                best = Some(d);
            }
        }
        best.map(|b| b.measure == "age-only").unwrap_or(false)
    }

    pub fn join(&self, a: &str, b: &str) -> String {
        format!("{}{}{}", a.trim_end_matches(['/', '\\']), if self.win { "\\" } else { "/" }, b)
    }

    pub fn exists(&self, p: &str) -> R<bool> {
        let cmd = if self.win { format!("if exist {} (echo yes) else (echo no)", self.q(p)) } else { format!("test -e {} && echo yes || echo no", self.q(p)) };
        Ok(strip(&self.run0(&cmd)?.out).starts_with("yes"))
    }

    /// Bytes under p, or None when unknown (timeout, error, age-only disk). Never a fake 0 (L51).
    pub fn size(&self, p: &str) -> R<Option<i64>> {
        if self.age_only(p) {
            return Ok(None);
        }
        let to = Some(self.cfg.size_timeout_s);
        if self.win {
            let c = self.run(
                &format!("powershell -NoProfile -Command \"(Get-ChildItem -LiteralPath {} -Recurse -Force -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum\"", self.psq(p)),
                to,
            )?;
            let s = strip(&c.out);
            if !c.ok() {
                return Ok(None);
            }
            return Ok(if is_digits(s) { s.parse().ok() } else if s.is_empty() { Some(0) } else { None });
        }
        let c = self.run(&format!("du -sk {}", self.q(p)), to)?;
        let w = split_ws(&c.out);
        Ok(if c.ok() && !w.is_empty() && is_digits(w[0]) { w[0].parse::<i64>().ok().map(|k| k * 1024) } else { None })
    }

    pub fn mtime(&self, p: &str) -> R<Option<i64>> {
        let c = if self.win {
            self.run0(&format!("powershell -NoProfile -Command \"[DateTimeOffset]::new((Get-Item -LiteralPath {}).LastWriteTimeUtc).ToUnixTimeSeconds()\"", self.psq(p)))?
        } else {
            self.run0(&format!("stat -c %Y {}", self.q(p)))?
        };
        let s = strip(&c.out);
        Ok(if is_digits(s) { s.parse().ok() } else { None })
    }

    pub fn listdir(&self, p: &str) -> R<Vec<String>> {
        let c = self.run0(&if self.win { format!("dir /B {}", self.q(p)) } else { format!("ls -1 {}", self.q(p)) })?;
        Ok(if c.ok() { splitlines(&c.out).iter().map(|l| strip(l).to_string()).filter(|l| !l.is_empty()).collect() } else { vec![] })
    }

    pub fn free_gb(&self, p: &str) -> R<Option<f64>> {
        free_gb(self.host, self.runner, p)
    }

    // -- git
    pub fn git(&self, repo: &str, args: &str) -> R<Cmd> {
        self.run0(&format!("git -C {} {}", self.q(repo), args))
    }

    fn main(&self) -> R<&str> {
        self.cfg.main_checkout.as_deref().ok_or_else(|| Fatal::Other("main_checkout is not configured".into()))
    }

    pub fn snapshot_main(&self) -> R<Option<Snapshot>> {
        let Some(m) = &self.cfg.main_checkout else { return Ok(None) };
        let files = self.git(m, "ls-files")?;
        let st = self.git(m, "status --porcelain")?;
        if !files.ok() || !st.ok() {
            return guard(format!("cannot read the main checkout {m}"));
        }
        Ok(Some((splitlines(&files.out).iter().filter(|l| !strip(l).is_empty()).count(), strip(&st.out).to_string())))
    }

    /// (path, branch) for each worktree of the main checkout.
    pub fn worktrees(&self) -> R<Vec<(String, Option<String>)>> {
        let out = self.git(self.main()?, "worktree list --porcelain")?.out;
        let mut res: Vec<(String, Option<String>)> = vec![];
        let mut cur: Option<(String, Option<String>)> = None;
        let mut lines = splitlines(&out);
        lines.push(String::new());
        for line in lines {
            if let Some(r) = line.strip_prefix("worktree ") {
                cur = Some((strip(r).to_string(), None));
            } else if let (Some(r), Some(c)) = (line.strip_prefix("branch "), cur.as_mut()) {
                c.1 = Some(strip(r).replacen("refs/heads/", "", 1));
            } else if strip(&line).is_empty() {
                if let Some(c) = cur.take() {
                    res.push(c);
                }
            }
        }
        Ok(res)
    }

    pub fn branches(&self) -> R<Vec<String>> {
        let c = self.git(self.main()?, "branch --list")?;
        Ok(if c.ok() {
            splitlines(&c.out).iter().filter(|l| !strip(l).is_empty()).map(|l| strip(strip(l).trim_start_matches(['*', '+', ' '])).to_string()).collect()
        } else {
            vec![]
        })
    }

    /// True when the branch content is on main: an ancestor, or every added line is present in main's version of the file (L49).
    pub fn merged(&self, branch: &str) -> R<Option<bool>> {
        let m = self.main()?;
        let mb = &self.cfg.main_branch;
        if self.git(m, &format!("merge-base --is-ancestor {} {}", self.q(branch), self.q(mb)))?.ok() {
            return Ok(Some(true));
        }
        let d = self.git(m, &format!("diff --unified=0 --no-color {}", self.q(&format!("{mb}...{branch}"))))?;
        if !d.ok() {
            return Ok(None);
        }
        if strip(&d.out).is_empty() {
            return Ok(Some(true));
        }
        let mut added: Vec<(String, Vec<String>)> = vec![];
        let mut f: Option<String> = None;
        for line in splitlines(&d.out) {
            if let Some(r) = line.strip_prefix("+++ ") {
                let r = strip(r);
                f = if let Some(x) = r.strip_prefix("b/") {
                    Some(x.to_string())
                } else if r == "/dev/null" {
                    None
                } else {
                    Some(r.to_string())
                };
                // Python: f = f[2:] if f.startswith("b/") else None if f == "/dev/null" else f
                if let Some(name) = &f {
                    if !name.is_empty() && !added.iter().any(|(k, _)| k == name) {
                        added.push((name.clone(), vec![]));
                    }
                }
                if f.as_deref() == Some("") {
                    f = None;
                }
            } else if let (Some(r), Some(name)) = (line.strip_prefix('+'), f.as_ref()) {
                if let Some(slot) = added.iter_mut().find(|(k, _)| k == name) {
                    slot.1.push(strip(r).to_string());
                }
            }
        }
        let todo: Vec<(String, Vec<String>)> = added.into_iter().map(|(f, ls)| (f, ls.into_iter().filter(|x| !x.is_empty()).collect())).collect();
        if !todo.iter().any(|(_, ls)| !ls.is_empty()) {
            return Ok(Some(false)); // only deletions or renames: content cannot be proven present
        }
        for (f, ls) in &todo {
            let t = self.git(m, &format!("show {}", self.q(&format!("{mb}:{f}"))))?;
            if !t.ok() {
                return Ok(Some(false));
            }
            let have: std::collections::HashSet<String> = splitlines(&t.out).iter().map(|l| strip(l).to_string()).collect();
            if ls.iter().any(|x| !have.contains(x)) {
                return Ok(Some(false));
            }
        }
        Ok(Some(true))
    }

    pub fn dirty(&self, path: &str) -> R<Option<bool>> {
        let c = self.git(path, "status --porcelain")?;
        Ok(if !c.ok() { None } else { Some(!strip(&c.out).is_empty()) })
    }

    // -- links (L12, L44)
    pub fn list_links(&self, path: &str) -> R<Vec<String>> {
        let cmd = if self.win { format!("dir /AL /S /B {} & echo {END}", self.q(path)) } else { format!("find {} -type l; echo {END}", self.q(path)) };
        let c = self.run0(&cmd)?;
        let lines: Vec<String> = splitlines(&c.out).iter().map(|l| strip(l).to_string()).filter(|l| !l.is_empty()).collect();
        if !lines.iter().any(|l| l == END) {
            return guard(format!("the link listing for {path} did not run (no end marker); refusing to continue"));
        }
        self.check_canary()?;
        Ok(lines.into_iter().filter(|l| l != END).collect())
    }

    pub fn check_canary(&self) -> R<()> {
        let c = self.cfg;
        let Some(canary) = &c.link_canary else { return Ok(()) };
        if self.canary_ok.get() || !self.win {
            return Ok(());
        }
        self.canary_ok.set(true); // set first: the canary listing re-enters list_links
        let o = self.run0(&format!("dir /AL /S /B {} & echo {END}", self.q(canary)))?;
        let n = splitlines(&o.out).iter().filter(|l| !strip(l).is_empty() && strip(l) != END).count() as i64;
        if let Some(want) = c.link_canary_count {
            if n != want {
                self.canary_ok.set(false);
                return guard(format!("link listing check: {canary} returned {n} links, expected {want}; the listing command is not trusted"));
            }
        }
        Ok(())
    }

    pub fn unlink_all(&self, path: &str) -> R<usize> {
        let links = self.list_links(path)?;
        if !links.is_empty() {
            if self.win {
                let sd = self.cfg.scratch_dir.clone().or_else(|| self.runner.scratch_dir());
                let Some(sd) = sd else { return guard("scratch_dir is needed to write the unlink script on this host".into()) };
                let script = self.join(&sd, "gatetrain-unlink.cmd");
                let mut body = String::from("@echo off\r\n");
                for w in links.iter().map(|x| x.replace('/', "\\")) {
                    body.push_str(&format!("rmdir \"{w}\" 2>nul || del \"{w}\"\r\n"));
                }
                self.runner.put_file(&script, &body)?;
                self.run0(&self.q(&script))?;
                self.run0(&format!("del {}", self.q(&script)))?;
            } else {
                self.run0(&links.iter().map(|l| format!("unlink {}", self.q(l))).collect::<Vec<_>>().join(" && "))?;
            }
        }
        let left = self.list_links(path)?;
        if !left.is_empty() {
            return guard(format!("{} link(s) remain in {path} after unlinking; not removing", left.len()));
        }
        Ok(links.len())
    }

    // -- guarded removal
    pub fn check_path(&self, p: &str) -> R<()> {
        let n = norm(p);
        if p.is_empty() || p.contains(['*', '?', '[']) || n.is_empty() || n == "/" || drive_only().is_match(&n) {
            return guard(format!("refusing to remove {}", crate::pycompat::repr_str(p)));
        }
        let kept = [self.cfg.main_checkout.clone(), self.cfg.root_checkout.clone()].into_iter().chain(self.cfg.keep_paths.iter().cloned().map(Some));
        for k in kept.flatten() {
            if !k.is_empty() && (n == norm(&k) || norm(&k).starts_with(&format!("{n}/"))) {
                return guard(format!("{p} is on the keep list or contains a kept path ({k})"));
            }
        }
        Ok(())
    }

    pub fn remove_dir(&self, p: &str) -> R<()> {
        self.check_path(p)?;
        let before = self.snapshot_main()?;
        self.unlink_all(p)?;
        self.run0(&if self.win { format!("rmdir /S /Q {}", self.q(p)) } else { format!("rm -r -- {}", self.q(p)) })?;
        if self.exists(p)? {
            return guard(format!("{p} still exists after removal"));
        }
        self.assert_main(before)
    }

    pub fn assert_main(&self, before: Option<Snapshot>) -> R<()> {
        let Some(before) = before else { return Ok(()) };
        let after = self.snapshot_main()?.unwrap_or((0, String::new()));
        if after != before {
            return guard(format!(
                "the main checkout changed during removal: {} tracked files before, {} after; status changed={}",
                before.0,
                after.0,
                if before.1 != after.1 { "True" } else { "False" }
            ));
        }
        Ok(())
    }
}

fn bulk_rm() -> Regex {
    Regex::new(r"rm\s+-\w*r\w*f?\s.*[*?]").unwrap()
}

fn drive_only() -> Regex {
    Regex::new(r"^[a-z]:$").unwrap()
}

/// `doctor.free_gb`: free space in GB at `path`, or None when the path cannot be read.
pub fn free_gb(h: &Host, runner: &dyn Runner, path: &str) -> R<Option<f64>> {
    let get = |cmd: &str| -> R<(Option<i64>, String)> {
        Ok(match runner.run_for(cmd, None)? {
            RunOut::Done { rc, out } => (Some(rc), out),
            RunOut::TimedOut => (None, String::new()),
        })
    };
    if host_os(h) == "windows" {
        let (rc, out) = get(&format!("python -c \"import shutil,sys;print(shutil.disk_usage(sys.argv[1]).free)\" \"{path}\""))?;
        let s = strip(&out);
        return Ok(if rc == Some(0) && is_digits(s) { s.parse::<f64>().ok().map(|v| v / (1024.0 * 1024.0 * 1024.0)) } else { None });
    }
    let (rc, out) = get(&format!("df -Pk {}", shlex_quote(path)))?;
    let lines: Vec<String> = splitlines(&out).into_iter().filter(|l| !strip(l).is_empty()).collect();
    if rc != Some(0) || lines.len() < 2 {
        return Ok(None);
    }
    let cols = split_ws(lines.last().unwrap());
    Ok(if cols.len() >= 4 && is_digits(cols[3]) { cols[3].parse::<f64>().ok().map(|v| v / (1024.0 * 1024.0)) } else { None })
}
