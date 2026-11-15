//! Host operations and the guarded removal of `tidy.py` (`Ops`). Every command goes through the `Runner`; every removal names one
//! exact path. The guards (L12, L43-L51, L60-L66): bulk commands refused, kept paths refused (also after resolving links), links
//! unlinked before a folder is removed, the link listing proven to have run, the main checkout compared before and after, git and
//! docker as argument lists (never a shell string), branch names and Windows metacharacters refused, unknown states never read as gone.
use super::config::TidyCfg;
use crate::config::{host_os, Host};
use crate::pycompat::{is_digits, repr_str, shlex_quote, split_ws, splitlines, strip};
use crate::runner::{RunOut, Runner};
use regex::Regex;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub const END: &str = "__GT_END__";

/// Resolve a path with links followed up to its nearest existing folder (the part that does not exist is appended as written).
/// What `realpath -m` does where that flag exists; the fallback where it does not (BSD/macOS).
pub const RESOLVE_SH: &str = r#"p="$1"; rest=""; while [ -n "$p" ] && [ "$p" != "/" ] && [ ! -d "$p" ]; do rest="/${p##*/}$rest"; p="${p%/*}"; [ -z "$p" ] && p="/"; done; d=$(cd -- "$p" 2>/dev/null && pwd -P) || exit 1; out="${d%/}$rest"; printf '%s
' "${out:-/}""#;

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

/// Compare key. Windows (drive-letter) paths are case-insensitive; POSIX paths keep their case.
pub fn norm(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.trim_end_matches('/');
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'/') {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

pub fn lane_of(path: &str, cfg: &TidyCfg) -> String {
    let n = norm(path);
    let b = n.rsplit('/').next().unwrap_or("").to_string();
    let p = cfg.worktree_prefix.to_lowercase();
    if !p.is_empty() && b.to_lowercase().starts_with(&p) {
        b.chars().skip(p.chars().count()).collect()
    } else {
        b
    }
}

pub fn safe_branch(b: &str) -> bool {
    Regex::new(r"^[A-Za-z0-9._][A-Za-z0-9._/-]*$").unwrap().is_match(b)
}
pub fn safe_container(b: &str) -> bool {
    Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$").unwrap().is_match(b)
}
pub fn safe_image(b: &str) -> bool {
    Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.:/@-]*$").unwrap().is_match(b)
}
/// Characters never interpolated into a Windows command line.
pub fn win_bad_char(c: char) -> bool {
    "\"%^!&|<>".contains(c) || ('\u{2018}'..='\u{201f}').contains(&c) || (c as u32) < 32
}
fn control(c: char) -> bool {
    (c as u32) < 32 || c as u32 == 127
}

/// One check for every path that may be removed: absolute, no `.` or `..` component, no control character, not a root, not a share.
/// With `under`, the path must be exactly one component below that folder (a recycle-bin entry).
pub fn safe_path(p: &str, under: Option<&str>) -> R<()> {
    if p.is_empty() {
        return guard(format!("refusing to remove {}: empty", repr_str(p)));
    }
    if p.chars().any(control) {
        return guard(format!("refusing {}: control character in the path", repr_str(p)));
    }
    let s = p.replace('\\', "/");
    if s.starts_with("//") {
        return guard(format!("refusing {}: a network path", repr_str(p)));
    }
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/';
    if !(s.starts_with('/') || drive) {
        return guard(format!("refusing {}: not an absolute path", repr_str(p)));
    }
    let comps: Vec<&str> = s.split('/').skip(1).filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return guard(format!("refusing {}: a root", repr_str(p)));
    }
    if comps.iter().any(|c| *c == "." || *c == "..") {
        return guard(format!("refusing {}: a . or .. component", repr_str(p)));
    }
    if let Some(under) = under {
        let u = norm(under);
        let nn = norm(p);
        let rest = nn.strip_prefix(&format!("{u}/")).map(String::from);
        if rest.as_deref().map(|r| r.is_empty() || r.contains('/')).unwrap_or(true) {
            return guard(format!("{p} is not exactly one component below {under}"));
        }
    }
    Ok(())
}

/// A worktree of the main checkout, as `git worktree list --porcelain` reports it.
#[derive(Debug, Clone)]
pub struct Wt {
    pub path: String,
    pub branch: Option<String>,
    pub locked: bool,
}

pub struct Ops<'a> {
    pub host: &'a Host,
    pub runner: &'a dyn Runner,
    pub cfg: &'a TidyCfg,
    pub win: bool,
    canary_ok: Cell<bool>,
    rp_cache: RefCell<HashMap<String, Option<String>>>,
}

/// `(tracked file count, status text)` of the main checkout.
pub type Snapshot = (usize, String);

fn yes_no(c: &Cmd) -> Option<bool> {
    let o = strip(&c.out);
    if !c.ok() {
        None
    } else if o.starts_with("yes") {
        Some(true)
    } else if o.starts_with("no") {
        Some(false)
    } else {
        None
    }
}

impl<'a> Ops<'a> {
    pub fn new(host: &'a Host, runner: &'a dyn Runner, cfg: &'a TidyCfg) -> Ops<'a> {
        Ops { host, runner, cfg, win: host_os(host) == "windows", canary_ok: Cell::new(false), rp_cache: RefCell::new(HashMap::new()) }
    }

    // -- primitives
    pub fn win_unsafe(&self, v: &str) -> bool {
        v.chars().any(win_bad_char)
    }

    fn win_ok(&self, v: &str) -> R<()> {
        let mut bad: Vec<char> = v.chars().filter(|c| win_bad_char(*c)).collect();
        if bad.is_empty() {
            return Ok(());
        }
        bad.sort();
        bad.dedup();
        guard(format!("refusing to put {} on a Windows command line (contains {})", repr_str(v), repr_str(&bad.iter().collect::<String>())))
    }

    /// Quote a PATH for the host's shell. Windows has no safe escape for its metacharacters, so a path holding one is refused.
    pub fn q(&self, p: &str) -> R<String> {
        if self.win {
            self.win_ok(p)?;
            Ok(format!("\"{}\"", p.replace('/', "\\")))
        } else {
            Ok(shlex_quote(p))
        }
    }

    pub fn psq(&self, p: &str) -> R<String> {
        self.win_ok(p)?;
        Ok(format!("'{}'", p.replace('/', "\\").replace('\'', "''")))
    }

    fn cmd_of(&self, r: RunOut) -> Cmd {
        match r {
            RunOut::Done { rc, out } => Cmd { rc: Some(rc), out },
            RunOut::TimedOut => Cmd { rc: None, out: String::new() },
        }
    }

    /// Run a program with an argument list. A local runner uses no shell at all; ssh gets POSIX single-quote escaping.
    pub fn run_argv(&self, argv: &[String], timeout: Option<f64>) -> R<Cmd> {
        if argv.join(" ").to_lowercase().contains("system prune") {
            return guard(format!("refusing a bulk command: {argv:?}"));
        }
        if self.win && !self.runner.shell_free() {
            for a in argv {
                self.win_ok(a)?;
            }
            let cmd = argv.iter().map(|a| if a.chars().any(|c| c.is_whitespace()) { format!("\"{a}\"") } else { a.clone() }).collect::<Vec<_>>().join(" ");
            return Ok(self.cmd_of(self.runner.run_for(&cmd, timeout)?));
        }
        Ok(self.cmd_of(self.runner.run_argv_to(argv, timeout)?))
    }

    /// (rc, out). A command that times out returns rc None instead of failing; callers treat it as a failure.
    pub fn run(&self, cmd: &str, timeout: Option<f64>) -> R<Cmd> {
        let low = cmd.to_lowercase();
        if low.contains("system prune") || bulk_rm().is_match(&low) {
            return guard(format!("refusing a bulk command: {cmd}"));
        }
        Ok(self.cmd_of(self.runner.run_for(cmd, timeout)?))
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

    /// True, False, or None when the probe itself failed (unknown is never read as gone).
    pub fn exists(&self, p: &str) -> R<Option<bool>> {
        let cmd = if self.win { format!("if exist {} (echo yes) else (echo no)", self.q(p)?) } else { format!("test -e {} && echo yes || echo no", self.q(p)?) };
        Ok(yes_no(&self.run0(&cmd)?))
    }

    pub fn gone(&self, p: &str) -> R<bool> {
        match self.exists(p)? {
            None => guard(format!("cannot tell whether {p} still exists; not assuming it is gone")),
            Some(e) => Ok(!e),
        }
    }

    /// True for a symlink or Windows reparse point (junction), False for a real item, None when the probe failed.
    pub fn is_link(&self, p: &str) -> R<Option<bool>> {
        let c = if self.win {
            self.run0(&format!("powershell -NoProfile -Command \"if ((Get-Item -LiteralPath {} -Force).Attributes -band 1024) {{'yes'}} else {{'no'}}\"", self.psq(p)?))?
        } else {
            self.run0(&format!("test -L {} && echo yes || echo no", self.q(p)?))?
        };
        Ok(yes_no(&c))
    }

    /// "dir" or "file" for a Windows link; anything unclear is a violation (the script must pick rmdir or del).
    pub fn link_type(&self, p: &str) -> R<&'static str> {
        let c = self.run0(&format!("powershell -NoProfile -Command \"(Get-Item -LiteralPath {} -Force).PSIsContainer\"", self.psq(p)?))?;
        let o = strip(&c.out).to_lowercase();
        if !c.ok() || (o != "true" && o != "false") {
            return guard(format!("cannot tell whether the link {p} is a folder or a file"));
        }
        Ok(if o == "true" { "dir" } else { "file" })
    }

    /// The resolved location of p (links followed), normalised; None when it cannot be resolved.
    pub fn rp(&self, p: &str) -> R<Option<String>> {
        if let Some(v) = self.rp_cache.borrow().get(p) {
            return Ok(v.clone());
        }
        let argv: Vec<String> = if self.win {
            ["python", "-c", "import os,sys;print(os.path.realpath(sys.argv[1]))", p].iter().map(|s| s.to_string()).collect()
        } else {
            ["realpath", "-m", "--", p].iter().map(|s| s.to_string()).collect()
        };
        let mut c = self.run_argv(&argv, None)?;
        if !self.win && !c.ok() {
            // BSD/macOS realpath has no -m: resolve the nearest existing folder with the shell instead
            let sh: Vec<String> = ["sh", "-c", RESOLVE_SH, "sh", p].iter().map(|s| s.to_string()).collect();
            c = self.run_argv(&sh, None)?;
        }
        let o = strip(&c.out);
        let v = if c.ok() && !o.is_empty() && !o.contains('\n') { Some(norm(o)) } else { None };
        self.rp_cache.borrow_mut().insert(p.to_string(), v.clone());
        Ok(v)
    }

    /// Where the item itself sits: the parent resolved, the last component NOT followed (an item that is a link is judged as the link).
    pub fn rpl(&self, p: &str) -> R<Option<String>> {
        let s = p.replace('\\', "/");
        let s = s.trim_end_matches('/');
        let (parent, base) = match s.rfind('/') {
            Some(i) => (&s[..i], &s[i + 1..]),
            None => ("", s),
        };
        let rpar = self.rp(if parent.is_empty() { "/" } else { parent })?;
        Ok(rpar.map(|r| norm(&format!("{}/{}", r.trim_end_matches('/'), base))))
    }

    /// Bytes under p, or None when unknown (timeout, error, age-only disk). Never a fake 0 (L51).
    pub fn size(&self, p: &str) -> R<Option<i64>> {
        if self.age_only(p) {
            return Ok(None);
        }
        let to = Some(self.cfg.size_timeout_s);
        if self.win {
            let c = self.run(
                &format!("powershell -NoProfile -Command \"(Get-ChildItem -LiteralPath {} -Recurse -Force -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum\"", self.psq(p)?),
                to,
            )?;
            let s = strip(&c.out);
            if !c.ok() {
                return Ok(None);
            }
            return Ok(if is_digits(s) { s.parse().ok() } else if s.is_empty() { Some(0) } else { None });
        }
        let c = self.run(&format!("du -sk {}", self.q(p)?), to)?;
        let w = split_ws(&c.out);
        Ok(if c.ok() && !w.is_empty() && is_digits(w[0]) { w[0].parse::<i64>().ok().map(|k| k * 1024) } else { None })
    }

    pub fn mtime(&self, p: &str) -> R<Option<i64>> {
        let c = if self.win {
            self.run0(&format!("powershell -NoProfile -Command \"[DateTimeOffset]::new((Get-Item -LiteralPath {}).LastWriteTimeUtc).ToUnixTimeSeconds()\"", self.psq(p)?))?
        } else {
            self.run0(&format!("stat -c %Y {}", self.q(p)?))?
        };
        let s = strip(&c.out);
        Ok(if is_digits(s) { s.parse().ok() } else { None })
    }

    /// Names directly in p. A name with a control character (a newline) is refused, never split into two names.
    pub fn listdir(&self, p: &str) -> R<Vec<String>> {
        let names: Vec<String> = if self.win {
            let c = self.run0(&format!("dir /B {}", self.q(p)?))?;
            if c.ok() {
                splitlines(&c.out).iter().map(|l| strip(l).to_string()).filter(|l| !l.is_empty()).collect()
            } else {
                vec![]
            }
        } else {
            let c = self.run0(&format!("find {} -mindepth 1 -maxdepth 1 -print0", self.q(p)?))?;
            let pre = format!("{}/", p.replace('\\', "/").trim_end_matches('/'));
            let mut names = vec![];
            if c.ok() {
                for r in c.out.split('\0').filter(|x| !x.is_empty()) {
                    if !r.starts_with(&pre) || r.len() == pre.len() {
                        return guard(format!("find returned {}, which is not directly inside {p}; not trusted", repr_str(r)));
                    }
                    names.push(r[pre.len()..].to_string()); // find prints <path>/<name>
                }
            }
            names
        };
        for nm in &names {
            if nm.chars().any(control) {
                return guard(format!("{p} holds a name with a control character ({}); refusing to guess at it", repr_str(nm)));
            }
        }
        Ok(names)
    }

    pub fn free_gb(&self, p: &str) -> R<Option<f64>> {
        free_gb(self.host, self.runner, p)
    }

    // -- git
    /// git as an argument list: a branch, a path or a file name is data, never part of a command line.
    pub fn git(&self, repo: &str, args: &[&str]) -> R<Cmd> {
        self.git_to(repo, args, None)
    }

    pub fn git_to(&self, repo: &str, args: &[&str], timeout: Option<f64>) -> R<Cmd> {
        let mut argv: Vec<String> = vec!["git".into(), "-C".into(), repo.into()];
        argv.extend(args.iter().map(|a| a.to_string()));
        self.run_argv(&argv, timeout)
    }

    fn main(&self) -> R<&str> {
        self.cfg.main_checkout.as_deref().ok_or_else(|| Fatal::Other("main_checkout is not configured".into()))
    }

    pub fn snapshot_main(&self) -> R<Option<Snapshot>> {
        let Some(m) = &self.cfg.main_checkout else { return Ok(None) };
        let files = self.git(m, &["ls-files"])?;
        let st = self.git(m, &["status", "--porcelain"])?;
        if !files.ok() || !st.ok() {
            return guard(format!("cannot read the main checkout {m}"));
        }
        Ok(Some((splitlines(&files.out).iter().filter(|l| !strip(l).is_empty()).count(), strip(&st.out).to_string())))
    }

    /// The worktrees of the main checkout.
    pub fn worktrees(&self) -> R<Vec<Wt>> {
        let out = self.git(self.main()?, &["worktree", "list", "--porcelain"])?.out;
        let mut res: Vec<Wt> = vec![];
        let mut cur: Option<Wt> = None;
        let mut lines = splitlines(&out);
        lines.push(String::new());
        for line in lines {
            if let Some(r) = line.strip_prefix("worktree ") {
                cur = Some(Wt { path: strip(r).to_string(), branch: None, locked: false });
            } else if let (true, Some(c)) = (line == "locked" || line.starts_with("locked "), cur.as_mut()) {
                c.locked = true;
            } else if let (Some(r), Some(c)) = (line.strip_prefix("branch "), cur.as_mut()) {
                c.branch = Some(strip(r).replacen("refs/heads/", "", 1));
            } else if strip(&line).is_empty() {
                if let Some(c) = cur.take() {
                    res.push(c);
                }
            }
        }
        Ok(res)
    }

    pub fn branches(&self) -> R<Vec<String>> {
        let c = self.git(self.main()?, &["branch", "--list"])?;
        Ok(if c.ok() {
            splitlines(&c.out).iter().filter(|l| !strip(l).is_empty()).map(|l| strip(strip(l).trim_start_matches(['*', '+', ' '])).to_string()).collect()
        } else {
            vec![]
        })
    }

    /// True only when merging the branch into main would change nothing: an ancestor, or the merge result's tree equals main's tree
    /// (git merge-tree --write-tree, git 2.38+). A conflict, an older git or any failure is not merged; an unusable name is unknown.
    pub fn merged(&self, branch: &str) -> R<Option<bool>> {
        let m = self.main()?;
        let mb = self.cfg.main_branch.as_str();
        if !safe_branch(branch) {
            return Ok(None);
        }
        if self.git(m, &["merge-base", "--is-ancestor", branch, mb])?.ok() {
            return Ok(Some(true));
        }
        let t = self.git(m, &["rev-parse", "--verify", &format!("{mb}^{{tree}}")])?;
        if !t.ok() || strip(&t.out).is_empty() {
            return Ok(None);
        }
        let o = self.git(m, &["merge-tree", "--write-tree", mb, branch])?;
        let lines = splitlines(&o.out);
        Ok(Some(o.ok() && !lines.is_empty() && strip(&lines[0]) == strip(&t.out)))
    }

    pub fn dirty(&self, path: &str) -> R<Option<bool>> {
        let c = self.git(path, &["status", "--porcelain"])?;
        Ok(if !c.ok() { None } else { Some(!strip(&c.out).is_empty()) })
    }

    // -- links (L12, L44)
    pub fn list_links(&self, path: &str) -> R<Vec<String>> {
        let cmd = if self.win { format!("dir /AL /S /B {} & echo {END}", self.q(path)?) } else { format!("find {} -type l; echo {END}", self.q(path)?) };
        let c = self.run0(&cmd)?;
        let lines: Vec<String> = splitlines(&c.out).iter().map(|l| strip(l).to_string()).filter(|l| !l.is_empty()).collect();
        if !lines.iter().any(|l| l == END) {
            return guard(format!("the link listing for {path} did not run (no end marker); refusing to continue"));
        }
        self.check_canary()?;
        let res: Vec<String> = lines.into_iter().filter(|l| l != END).collect();
        let pre = format!("{}/", norm(path));
        for l in &res {
            if !norm(l).starts_with(&pre) {
                return guard(format!("the link listing for {path} returned {}, which is not inside it; not trusted", repr_str(l)));
            }
        }
        let ns: Vec<String> = res.iter().map(|l| norm(l)).collect();
        // removing a link removes what hangs below it
        Ok(res.into_iter().zip(ns.iter()).filter(|(_, nl)| !ns.iter().any(|o| nl.starts_with(&format!("{o}/")))).map(|(l, _)| l).collect())
    }

    pub fn check_canary(&self) -> R<()> {
        if !self.win {
            return Ok(());
        }
        let c = self.cfg;
        let (Some(canary), Some(want)) = (&c.link_canary, c.link_canary_count) else {
            return guard("a Windows host needs link_canary and link_canary_count: without a listing that is known to find links, an empty result means nothing".into());
        };
        if self.canary_ok.get() {
            return Ok(());
        }
        self.canary_ok.set(true); // set first: the canary listing re-enters list_links
        let o = self.run0(&format!("dir /AL /S /B {} & echo {END}", self.q(canary)?))?;
        let n = splitlines(&o.out).iter().filter(|l| !strip(l).is_empty() && strip(l) != END).count() as i64;
        if n != want {
            self.canary_ok.set(false);
            return guard(format!("link listing check: {canary} returned {n} links, expected {want}; the listing command is not trusted"));
        }
        Ok(())
    }

    /// Remove ONE link, never anything inside it. Windows: rmdir for a folder link, del for a file link, chosen by type.
    pub fn remove_link(&self, p: &str) -> R<()> {
        if self.win {
            let verb = if self.link_type(p)? == "dir" { "rmdir " } else { "del " };
            self.run0(&format!("{verb}{}", self.q(p)?))?;
        } else {
            self.run0(&format!("unlink {}", self.q(p)?))?;
        }
        Ok(())
    }

    pub fn unlink_all(&self, path: &str) -> R<usize> {
        let links = self.list_links(path)?;
        if !links.is_empty() {
            for l in &links {
                if self.is_link(l)? != Some(true) {
                    return guard(format!("{l} was listed as a link but is not one; refusing to unlink it"));
                }
            }
            if self.win {
                let sd = self.cfg.scratch_dir.clone().or_else(|| self.runner.scratch_dir());
                let Some(sd) = sd else { return guard("scratch_dir is needed to write the unlink script on this host".into()) };
                let script = self.join(&sd, "gatetrain-unlink.cmd");
                let mut body = String::from("@echo off\r\n");
                for l in &links {
                    let verb = if self.link_type(l)? == "dir" { "rmdir " } else { "del " };
                    body.push_str(&format!("{verb}{}\r\n", self.q(l)?));
                }
                self.runner.put_file(&script, &body)?;
                self.run0(&self.q(&script)?)?;
                self.run0(&format!("del {}", self.q(&script)?))?;
            } else {
                let mut parts = vec![];
                for l in &links {
                    parts.push(format!("unlink {}", self.q(l)?));
                }
                self.run0(&parts.join(" && "))?;
            }
        }
        let left = self.list_links(path)?;
        if !left.is_empty() {
            return guard(format!("{} link(s) remain in {path} after unlinking; not removing", left.len()));
        }
        Ok(links.len())
    }

    // -- guarded removal
    pub fn kept(&self) -> Vec<String> {
        [self.cfg.main_checkout.clone(), self.cfg.root_checkout.clone()].into_iter().chain(self.cfg.keep_paths.iter().cloned().map(Some)).flatten().filter(|k| !k.is_empty()).collect()
    }

    pub fn check_path(&self, p: &str) -> R<()> {
        safe_path(p, None)?;
        if p.contains(['*', '?', '[']) {
            return guard(format!("refusing to remove {}", repr_str(p)));
        }
        let n = norm(p);
        for k in self.kept() {
            if n == norm(&k) || norm(&k).starts_with(&format!("{n}/")) {
                return guard(format!("{p} is on the keep list or contains a kept path ({k})"));
            }
        }
        let Some(r) = self.rpl(p)? else { return guard(format!("cannot resolve {p}; not removing what cannot be located")) };
        for k in self.kept() {
            let Some(rk) = self.rp(&k)? else { return guard(format!("cannot resolve the kept path {k}")) };
            if r == rk || rk.starts_with(&format!("{r}/")) {
                return guard(format!("{p} resolves to {r}, which is or contains the kept path {k}"));
            }
        }
        Ok(())
    }

    /// A recycle-bin entry is exactly one component below a bin that resolves to a place of its own.
    pub fn check_trash_entry(&self, p: &str) -> R<()> {
        let td = self.cfg.trash_dir.clone().unwrap_or_default();
        safe_path(p, Some(&td))?;
        let Some(rt) = self.rp(&td)? else { return guard(format!("cannot resolve the recycle bin {td}")) };
        for k in self.kept() {
            let bad = match self.rp(&k)? {
                None => true,
                Some(rk) => rt == rk || rk.starts_with(&format!("{rt}/")) || rt.starts_with(&format!("{rk}/")),
            };
            if bad {
                return guard(format!("the recycle bin {td} resolves to {rt}, which overlaps the kept path {k}"));
            }
        }
        let s = p.replace('\\', "/");
        let parent = s.trim_end_matches('/').rsplit_once('/').map(|x| x.0.to_string()).unwrap_or_default();
        let par = self.rp(&parent)?;
        if par.as_deref() != Some(rt.as_str()) {
            return guard(format!("{p} does not sit directly in the recycle bin {td} (resolves to {})", par.map(|x| repr_str(&x)).unwrap_or_else(|| "None".into())));
        }
        Ok(())
    }

    /// (status, note). A link is removed as a link and never walked; a timeout is "stopped", never done.
    pub fn remove_dir(&self, p: &str) -> R<(&'static str, String)> {
        self.check_path(p)?;
        let before = self.snapshot_main()?;
        let Some(lk) = self.is_link(p)? else { return guard(format!("cannot tell whether {p} is a link; not removing")) };
        if lk {
            self.remove_link(p)?;
            if !self.gone(p)? {
                return guard(format!("{p} (a link) still exists after removal"));
            }
            self.assert_main(before)?;
            return Ok(("ok", "link removed; its target was not touched".into()));
        }
        self.unlink_all(p)?;
        let c = self.run(&if self.win { format!("rmdir /S /Q {}", self.q(p)?) } else { format!("rm -r -- {}", self.q(p)?) }, Some(self.cfg.remove_timeout_s))?;
        if c.rc.is_none() {
            let e = self.exists(p)?;
            self.assert_main(before)?;
            return Ok(("stopped", format!("removal did not finish in {}s; re-probe: {}; not counted as done", crate::pycompat::fmt_g(self.cfg.remove_timeout_s), probe_word(e))));
        }
        if !self.gone(p)? {
            return guard(format!("{p} still exists after removal"));
        }
        self.assert_main(before)?;
        Ok(("ok", "removed".into()))
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

pub fn probe_word(e: Option<bool>) -> &'static str {
    match e {
        Some(true) => "still there",
        Some(false) => "gone",
        None => "unknown",
    }
}

fn bulk_rm() -> Regex {
    Regex::new(r"rm\s+-\w*r\w*f?\s.*[*?]").unwrap()
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
