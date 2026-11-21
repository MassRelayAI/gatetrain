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
use std::time::Instant;

pub const END: &str = "__GT_END__";

/// Resolve a path with links followed up to its nearest existing folder (the part that does not exist is appended as written).
/// What `realpath -m` does where that flag exists; the fallback where it does not (BSD/macOS).
pub const RESOLVE_SH: &str = r#"p="$1"; rest=""; while [ -n "$p" ] && [ "$p" != "/" ] && [ ! -d "$p" ]; do rest="/${p##*/}$rest"; p="${p%/*}"; [ -z "$p" ] && p="/"; done; d=$(cd -- "$p" 2>/dev/null && pwd -P) || exit 1; out="${d%/}$rest"; printf '%s
' "${out:-/}""#;

/// A guard refused, or something else went wrong. A guard violation stops a tidy run (exit 3); the rest is an error (exit 2).
#[derive(Debug, Clone, PartialEq)]
pub enum Fatal {
    /// A guard refused: it stops the run.
    Guard(String),
    /// One item has a bad path or name and nothing was changed: it is reported and skipped, the run carries on.
    Item(String),
    Other(String),
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fatal::Guard(s) | Fatal::Item(s) | Fatal::Other(s) => f.write_str(s),
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

/// The folder inside the recycle bin that holds archived docker images.
pub const IMAGE_ARCHIVE: &str = "docker-images";

pub fn guard<T>(msg: String) -> R<T> {
    Err(Fatal::Guard(msg))
}

pub fn refuse<T>(msg: String) -> R<T> {
    Err(Fatal::Item(msg))
}

/// The recycle bin's memory of when tidy first saw each entry (a file inside the bin).
pub const SEEN: &str = ".gatetrain-seen.json";

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

/// Does the normalised path `n` fall inside, equal, or contain the normalised protect entry `pat`?
/// A plain entry is a folder: `n` is it, is below it, or has it below. A glob entry (`*`, `?`, `[`) matches `n` or anything below a match,
/// and `n` also counts when it is a folder above the entry's fixed leading part.
pub fn protect_match(n: &str, pat: &str) -> bool {
    use crate::pycompat::fnmatchcase;
    let pat = pat.trim_end_matches('/');
    if pat.is_empty() {
        return false;
    }
    match pat.find(['*', '?', '[']) {
        None => n == pat || n.starts_with(&format!("{pat}/")) || pat.starts_with(&format!("{n}/")),
        Some(i) => {
            if fnmatchcase(n, pat) || fnmatchcase(n, &format!("{pat}/*")) {
                return true;
            }
            let fixed = pat[..i].rsplit_once('/').map(|x| x.0).unwrap_or("");
            !fixed.is_empty() && (n == fixed || fixed.starts_with(&format!("{n}/")))
        }
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
        return refuse(format!("refusing to remove {}: empty", repr_str(p)));
    }
    if p.chars().any(control) {
        return refuse(format!("refusing {}: control character in the path", repr_str(p)));
    }
    let s = p.replace('\\', "/");
    if s.starts_with("//") {
        return refuse(format!("refusing {}: a network path", repr_str(p)));
    }
    let b = s.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/';
    if !(s.starts_with('/') || drive) {
        return refuse(format!("refusing {}: not an absolute path", repr_str(p)));
    }
    let comps: Vec<&str> = s.split('/').skip(1).filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return refuse(format!("refusing {}: a root", repr_str(p)));
    }
    if comps.iter().any(|c| *c == "." || *c == "..") {
        return refuse(format!("refusing {}: a . or .. component", repr_str(p)));
    }
    if let Some(under) = under {
        let u = norm(under);
        let nn = norm(p);
        let rest = nn.strip_prefix(&format!("{u}/")).map(String::from);
        if rest.as_deref().map(|r| r.is_empty() || r.contains('/')).unwrap_or(true) {
            return refuse(format!("{p} is not exactly one component below {under}"));
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
    /// `HEAD` of the worktree as `git worktree list` prints it, and whether it is detached (no branch)
    pub head: Option<String>,
    pub detached: bool,
    /// git says its registration is stale (its folder is missing): `git worktree prune` would remove it
    pub prunable: bool,
}

/// What tidy remembers about one recycle-bin entry: when it was first seen, and the entry's own folder time then (a different
/// folder time under the same name means a NEW entry under a reused name).
#[derive(Debug, Clone, PartialEq)]
pub struct SeenRec {
    pub t: f64,
    pub m: Option<i64>,
}

/// The state of one path: a link, a real item, nothing there, or the probe failed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkState {
    Link,
    Real,
    Missing,
    Unknown,
}

pub struct Ops<'a> {
    pub host: &'a Host,
    pub runner: &'a dyn Runner,
    pub cfg: &'a TidyCfg,
    pub win: bool,
    canary_ok: Cell<bool>,
    rp_cache: RefCell<HashMap<String, Option<String>>>,
    /// the end of the current item's time budget (`item_budget_s`); `None` outside an item
    deadline: Cell<Option<Instant>>,
    /// a probe of the current item was cut short or not run because the budget ran out
    over: Cell<bool>,
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
        Ops { host, runner, cfg, win: host_os(host) == "windows", canary_ok: Cell::new(false), rp_cache: RefCell::new(HashMap::new()), deadline: Cell::new(None), over: Cell::new(false) }
    }

    // -- the per-item time budget
    /// Start one item: its probes share `item_budget_s` from now.
    pub fn start_item(&self) {
        self.over.set(false);
        self.deadline.set((self.cfg.item_budget_s > 0.0).then(|| Instant::now() + std::time::Duration::from_secs_f64(self.cfg.item_budget_s)));
    }

    /// End the item; true when a probe of it was cut short or skipped by the budget (its findings are then not trusted).
    pub fn end_item(&self) -> bool {
        self.deadline.set(None);
        self.over.replace(false)
    }

    /// The time limit a probe gets inside the budget: `Some(t)` (the caller's own, or what is left when that is less), `None` when
    /// nothing is left (the probe is not run and reads as a timeout).
    fn budgeted(&self, timeout: Option<f64>) -> Option<Option<f64>> {
        let Some(d) = self.deadline.get() else { return Some(timeout) };
        let left = d.saturating_duration_since(Instant::now()).as_secs_f64();
        if left <= 0.0 {
            self.over.set(true);
            return None;
        }
        Some(if left < timeout.unwrap_or(60.0) { Some(left) } else { timeout })
    }

    fn timed(&self, r: RunOut, cut: bool) -> Cmd {
        if cut && r == RunOut::TimedOut {
            self.over.set(true);
        }
        self.cmd_of(r)
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
        refuse(format!("refusing to put {} on a Windows command line (contains {})", repr_str(v), repr_str(&bad.iter().collect::<String>())))
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
            let Some(t) = self.budgeted(timeout) else { return Ok(Cmd { rc: None, out: String::new() }) };
            return Ok(self.timed(self.runner.run_for(&cmd, t)?, t != timeout));
        }
        let Some(t) = self.budgeted(timeout) else { return Ok(Cmd { rc: None, out: String::new() }) };
        Ok(self.timed(self.runner.run_argv_to(argv, t)?, t != timeout))
    }

    /// (rc, out). A command that times out returns rc None instead of failing; callers treat it as a failure.
    pub fn run(&self, cmd: &str, timeout: Option<f64>) -> R<Cmd> {
        let low = cmd.to_lowercase();
        if low.contains("system prune") {
            return guard(format!("refusing a bulk command: {cmd}"));
        }
        if bulk_rm().is_match(&low) {
            return refuse(format!("refusing a command with a glob in it: {cmd}"));
        }
        let Some(t) = self.budgeted(timeout) else { return Ok(Cmd { rc: None, out: String::new() }) };
        Ok(self.timed(self.runner.run_for(cmd, t)?, t != timeout))
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
        Ok(match self.link_state(p)? {
            LinkState::Link => Some(true),
            LinkState::Real => Some(false),
            _ => None,
        })
    }

    /// `Link`, `Real`, `Missing` (the item is not there) or `Unknown` (the probe failed).
    pub fn link_state(&self, p: &str) -> R<LinkState> {
        let c = if self.win {
            let q = self.psq(p)?;
            self.run0(&format!("powershell -NoProfile -Command \"if (-not (Test-Path -LiteralPath {q})) {{'missing'}} elseif ((Get-Item -LiteralPath {q} -Force).Attributes -band 1024) {{'yes'}} else {{'no'}}\""))?
        } else {
            let q = self.q(p)?;
            self.run0(&format!("test -L {q} && echo yes || {{ test -e {q} && echo no || echo missing; }}"))?
        };
        let o = strip(&c.out);
        Ok(if !c.ok() {
            LinkState::Unknown
        } else if o.starts_with("yes") {
            LinkState::Link
        } else if o.starts_with("no") {
            LinkState::Real
        } else if o.starts_with("missing") {
            LinkState::Missing
        } else {
            LinkState::Unknown
        })
    }

    /// True when git, run in p, says p itself is the top of its worktree (a folder with no .git of its own would answer with the repository it
    /// sits in, and its status would be that repository's).
    pub fn toplevel_ok(&self, p: &str) -> R<bool> {
        let c = self.git(p, &["rev-parse", "--show-toplevel"])?;
        if !c.ok() || strip(&c.out).is_empty() {
            return Ok(false);
        }
        let (a, b) = (self.rp(strip(&c.out))?, self.rp(p)?);
        Ok(a.is_some() && a == b)
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
        if self.win && self.runner.is_local() {
            // this machine: answered in-process, no shell
            let v = crate::winprobe::local_realpath(p).map(|s| norm(&s));
            self.rp_cache.borrow_mut().insert(p.to_string(), v.clone());
            return Ok(v);
        }
        let argv: Vec<String> = if self.win {
            {
                ["powershell", "-NoProfile", "-Command", crate::winprobe::realpath_script(p).as_str()].iter().map(|s| s.to_string()).collect()
            }
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

    /// Bytes of ONE regular file, read with a stat whatever the disk's `measure` (a single stat is cheap even on an age-only disk). None when
    /// p is missing, is not a regular file, or the probe failed. Used to check the one tar `docker save` left; folders are sized by `size`.
    pub fn file_size(&self, p: &str) -> R<Option<i64>> {
        let c = if self.win {
            self.run0(&format!("powershell -NoProfile -Command \"(Get-Item -LiteralPath {} -Force | Where-Object {{ -not $_.PSIsContainer }}).Length\"", self.psq(p)?))?
        } else {
            let q = self.q(p)?;
            self.run0(&format!("test -f {q} && stat -c %s {q}"))?
        };
        let s = strip(&c.out);
        Ok(if c.ok() && is_digits(s) { s.parse().ok() } else { None })
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
            if !c.ok() && self.exists(p)? != Some(true) {
                return guard(format!("listing {p} failed and the folder cannot be shown to exist; a failed listing is never read as an empty folder"));
            }
            if c.ok() {
                splitlines(&c.out).iter().map(|l| strip(l).to_string()).filter(|l| !l.is_empty()).collect()
            } else {
                vec![]
            }
        } else {
            let c = self.run0(&format!("find {} -mindepth 1 -maxdepth 1 -print0", self.q(p)?))?;
            let pre = format!("{}/", p.replace('\\', "/").trim_end_matches('/'));
            let mut names = vec![];
            if !c.ok() {
                return guard(format!("listing {p} failed (rc {}); a failed listing is never read as an empty folder", c.rc.map(|r| r.to_string()).unwrap_or_else(|| "timeout".into())));
            }
            {
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

    /// True when a file inside p changed in the last `minutes` (a bounded walk, depth 6, `size_timeout_s`), False when none did, None
    /// when that cannot be told (a timeout or an error): unknown is never read as idle.
    pub fn recent(&self, p: &str, minutes: f64) -> R<Option<bool>> {
        let n = (minutes.ceil() as i64).max(1);
        let to = Some(self.cfg.size_timeout_s);
        let c = if self.win {
            self.run(
                &format!(
                    "powershell -NoProfile -Command \"Get-ChildItem -LiteralPath {} -Recurse -Depth {} -Force -File -ErrorAction SilentlyContinue | Where-Object {{ $_.LastWriteTime -gt (Get-Date).AddMinutes(-{n}) }} | Select-Object -First 1 -ExpandProperty FullName\"",
                    self.psq(p)?,
                    self.cfg.idle_walk_depth as i64
                ),
                to,
            )?
        } else {
            self.run(&format!("find {} -maxdepth {} -type f -mmin -{n} -print -quit", self.q(p)?, self.cfg.idle_walk_depth as i64), to)?
        };
        Ok(if !c.ok() { None } else { Some(!strip(&c.out).is_empty()) })
    }

    /// (normalised folder, run id) named by a running hold: the `targets` its lock sidecar (`<lock_file>.info`) lists while it is `held`.
    pub fn hold_named(&self) -> R<Vec<(String, String)>> {
        let Some(lf) = self.host.lock_file.as_deref().filter(|l| !l.is_empty()) else { return Ok(vec![]) };
        let c = self.run0(&format!("{}{}", if self.win { "type " } else { "cat " }, self.q(&format!("{lf}.info"))?))?;
        if !c.ok() || strip(&c.out).is_empty() {
            return Ok(vec![]);
        }
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&c.out) else { return Ok(vec![]) };
        if j.get("state").and_then(|s| s.as_str()) != Some("held") {
            return Ok(vec![]);
        }
        let run = j.get("run_id").and_then(|r| r.as_str()).unwrap_or("?").to_string();
        Ok(j.get("targets").and_then(|t| t.as_array()).into_iter().flatten().filter_map(|t| t.as_str()).map(|t| (norm(t), run.clone())).collect())
    }

    /// When tidy first saw each entry of the recycle bin (epoch seconds), from the file inside it.
    pub fn read_seen(&self, trash: &str) -> R<std::collections::BTreeMap<String, SeenRec>> {
        let c = self.run0(&format!("{}{}", if self.win { "type " } else { "cat " }, self.q(&self.join(trash, SEEN))?))?;
        let mut out = std::collections::BTreeMap::new();
        if c.ok() && !strip(&c.out).is_empty() {
            if let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(&c.out) {
                for (k, v) in m {
                    if v.as_f64().is_some() {
                        continue; // the older format (a bare time, no folder time to compare): first seen now, and upgraded
                    } else if let Some(t) = v.get("t").and_then(|t| t.as_f64()) {
                        out.insert(k, SeenRec { t, m: v.get("m").and_then(|m| m.as_i64()) });
                    }
                }
            }
        }
        Ok(out)
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
        // untracked files are listed whatever `status.showUntrackedFiles` says (BL4)
        let st = self.git(m, &["status", "--porcelain", "--untracked-files=all", "--ignore-submodules=none"])?;
        if !files.ok() || !st.ok() {
            return guard(format!("cannot read the main checkout {m}"));
        }
        let vol: Vec<String> = self.cfg.main_volatile_paths.iter().map(|v| v.replace('\\', "/").trim_end_matches('/').to_string()).filter(|v| !v.is_empty()).collect();
        let volatile = |path: &str| {
            let n = path.trim().replace('\\', "/");
            vol.iter().any(|v| n == *v || n.starts_with(&format!("{v}/")))
        };
        let tracked = splitlines(&files.out).iter().filter(|l| !strip(l).is_empty() && !volatile(l)).count();
        // tracked content and untracked-but-not-ignored files, outside the volatile paths
        let status: Vec<String> = splitlines(&st.out).into_iter().filter(|l| !strip(l).is_empty() && !l.get(3..).unwrap_or("").split(" -> ").all(&volatile)).collect();
        Ok(Some((tracked, strip(&status.join("\n")).to_string())))
    }

    /// The worktrees of the main checkout.
    pub fn worktrees(&self) -> R<Vec<Wt>> {
        let c = self.git(self.main()?, &["worktree", "list", "--porcelain"])?;
        if !c.ok() {
            return guard("git worktree list failed; an unreadable git is never read as 'no worktrees'".into());
        }
        let out = c.out;
        let mut res: Vec<Wt> = vec![];
        let mut cur: Option<Wt> = None;
        let mut lines = splitlines(&out);
        lines.push(String::new());
        for line in lines {
            if let Some(r) = line.strip_prefix("worktree ") {
                cur = Some(Wt { path: strip(r).to_string(), branch: None, locked: false, head: None, detached: false, prunable: false });
            } else if let (true, Some(c)) = (line == "locked" || line.starts_with("locked "), cur.as_mut()) {
                c.locked = true;
            } else if let (Some(r), Some(c)) = (line.strip_prefix("HEAD "), cur.as_mut()) {
                c.head = Some(strip(r).to_string());
            } else if let (true, Some(c)) = (line == "detached", cur.as_mut()) {
                c.detached = true;
            } else if let (true, Some(c)) = (line == "prunable" || line.starts_with("prunable "), cur.as_mut()) {
                c.prunable = true;
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
        if !c.ok() {
            return guard("git branch --list failed; an unreadable git is never read as 'no branches'".into());
        }
        Ok(splitlines(&c.out).iter().filter(|l| !strip(l).is_empty()).map(|l| strip(strip(l).trim_start_matches(['*', '+', ' '])).to_string()).collect())
    }

    /// True only when merging the branch into main would change nothing: an ancestor, or the merge result's tree equals main's tree
    /// (git merge-tree --write-tree, git 2.38+). A conflict, an older git or any failure is not merged; an unusable name is unknown.
    pub fn merged(&self, branch: &str) -> R<Option<bool>> {
        self.merged_ref(branch, false)
    }

    /// The same proof on a commit id (the very commit that is archived and deleted).
    pub fn merged_sha(&self, sha: &str) -> R<Option<bool>> {
        self.merged_ref(sha, true)
    }

    fn merged_ref(&self, branch: &str, sha: bool) -> R<Option<bool>> {
        let m = self.main()?;
        if !safe_branch(branch) {
            return Ok(None);
        }
        // a bare name could be a tag or a path: only `refs/heads/<name>` or a commit id is proved
        let r = if sha { branch.to_string() } else { format!("refs/heads/{branch}") };
        let mb = format!("refs/heads/{}", self.cfg.main_branch);
        if self.git(m, &["merge-base", "--is-ancestor", &r, &mb])?.ok() {
            return Ok(Some(true));
        }
        let t = self.git(m, &["rev-parse", "--verify", &format!("{mb}^{{tree}}")])?;
        if !t.ok() || strip(&t.out).is_empty() {
            return Ok(None);
        }
        let o = self.git(m, &["merge-tree", "--write-tree", &mb, &r])?;
        let lines = splitlines(&o.out);
        Ok(Some(o.ok() && !lines.is_empty() && strip(&lines[0]) == strip(&t.out)))
    }

    pub fn dirty(&self, path: &str) -> R<Option<bool>> {
        // `status.showUntrackedFiles = no` (BL4) or an ignored submodule (round-5 minor 6) must never make work look clean
        let c = self.git(path, &["status", "--porcelain", "--untracked-files=all", "--ignore-submodules=none"])?;
        if !c.ok() {
            return Ok(None);
        }
        if !strip(&c.out).is_empty() {
            return Ok(Some(true));
        }
        // an edit hidden by `update-index --skip-worktree` (tag `S`) or `--assume-unchanged` (a lower-case tag) is not in `status`:
        // such an entry is read as work (BL5); a sparse checkout is then always refused, which is the safe side
        let v = self.git(path, &["ls-files", "-v"])?;
        if !v.ok() {
            return Ok(None);
        }
        Ok(Some(splitlines(&v.out).iter().any(|l| l.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == 'S'))))
    }

    /// Does a running process hold this folder (its working directory is inside it, or on Windows its command line names it)?
    /// `None` = it could not be told, which the caller reads as held. Linux sees the processes this user may read; Windows has no
    /// working directory to read, so a command line or executable path under the folder is what counts.
    pub fn held_by_process(&self, path: &str) -> R<Option<bool>> {
        let c = if self.win {
            self.run0(&format!("powershell -NoProfile -Command \"Get-CimInstance Win32_Process | ForEach-Object {{ $_.CommandLine; $_.ExecutablePath }}\"; echo {END}"))?
        } else {
            self.run0(&format!("for p in /proc/[0-9]*; do readlink \"$p/cwd\" 2>/dev/null; done; echo {END}"))?
        };
        let lines: Vec<String> = splitlines(&c.out).iter().map(|l| strip(l).to_string()).collect();
        if !c.ok() || !lines.iter().any(|l| l == END) {
            return Ok(None);
        }
        let np = norm(path);
        let pre = format!("{np}/");
        let hit = |l: &String| {
            if self.win {
                let n = l.replace('\\', "/").to_lowercase();
                let np = np.to_lowercase();
                n.contains(&format!("{np}/")) || n.ends_with(&np)
            } else {
                *l == np || l.starts_with(&pre)
            }
        };
        Ok(Some(lines.iter().any(|l| l != END && hit(l))))
    }

    // -- links (L12, L44)
    pub fn list_links(&self, path: &str) -> R<Vec<String>> {
        let cmd = if self.win { format!("dir /AL /S /B {} & echo {END}", self.q(path)?) } else { format!("find {} -type l; echo {END}", self.q(path)?) };
        let c = self.run(&cmd, Some(self.cfg.link_list_timeout_s))?;
        if c.rc.is_none() {
            return refuse(format!("the link listing for {path} timed out after {}s (link_list_timeout_s); this item is left alone", crate::pycompat::fmt_g(self.cfg.link_list_timeout_s)));
        }
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
        // a host-wide proof, not a probe of the item that happens to call it first: never cut by that item's budget
        let d = self.deadline.take();
        let r = self.check_canary_now();
        self.deadline.set(d);
        r
    }

    fn check_canary_now(&self) -> R<()> {
        let c = self.cfg;
        let (Some(canary), Some(want)) = (&c.link_canary, c.link_canary_count) else {
            return guard("a Windows host needs link_canary and link_canary_count: without a listing that is known to find links, an empty result means nothing".into());
        };
        if self.canary_ok.get() {
            return Ok(());
        }
        self.canary_ok.set(true); // set first: the canary listing re-enters list_links
        let o = self.run(&format!("dir /AL /S /B {} & echo {END}", self.q(canary)?), Some(self.cfg.link_list_timeout_s))?;
        if o.rc.is_none() {
            self.canary_ok.set(false);
            return guard(format!("the link canary listing of {canary} timed out after {}s; the listing command is not trusted", crate::pycompat::fmt_g(self.cfg.link_list_timeout_s)));
        }
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

    /// The protect entry that covers `p`, if any: `p` is inside it, is it, or contains it (a glob entry matches by pattern),
    /// by its own spelling or by where it really leads (a link into a protected folder is protected).
    pub fn protect_hit(&self, p: &str) -> R<Option<String>> {
        if self.cfg.protect.is_empty() {
            return Ok(None);
        }
        let n = norm(p);
        for pat in &self.cfg.protect {
            if protect_match(&n, &norm(pat)) {
                return Ok(Some(pat.clone()));
            }
        }
        let Some(r) = self.rp(p)?.map(|r| norm(&r)) else { return Ok(None) };
        for pat in &self.cfg.protect {
            if protect_match(&r, &norm(pat)) {
                return Ok(Some(pat.clone()));
            }
            if !pat.contains(['*', '?', '[']) {
                if let Some(rk) = self.rp(pat)?.map(|k| norm(&k)) {
                    if protect_match(&r, &rk) {
                        return Ok(Some(pat.clone()));
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn check_path(&self, p: &str) -> R<()> {
        safe_path(p, None)?;
        if p.contains(['*', '?', '[']) {
            return refuse(format!("refusing to remove {}: a glob character in the name", repr_str(p)));
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
        // an archived image tar sits one level down, in `<trash_dir>/docker-images`
        let archive = format!("{}/{IMAGE_ARCHIVE}", norm(&td));
        let nested = norm(p).strip_prefix(&format!("{archive}/")).is_some();
        safe_path(p, Some(if nested { &archive } else { &td }))?;
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
        let want = if nested { format!("{rt}/{IMAGE_ARCHIVE}") } else { rt.clone() };
        if par.as_deref() != Some(want.as_str()) {
            return guard(format!("{p} does not sit directly in the recycle bin {td} (resolves to {})", par.map(|x| repr_str(&x)).unwrap_or_else(|| "None".into())));
        }
        Ok(())
    }

    /// (status, note). A link is removed as a link and never walked; a timeout is "stopped", never done.
    pub fn remove_dir(&self, p: &str) -> R<(&'static str, String)> {
        self.remove_dir_with(p, None)
    }

    /// `before_rm` runs after every check of the folder's links has passed and before the first change.
    pub fn remove_dir_with(&self, p: &str, before_rm: Option<&dyn Fn() -> R<()>>) -> R<(&'static str, String)> {
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
        let rpp = self.rp(p)?;
        let mut targets: Vec<(String, Option<String>)> = vec![]; // where every link inside points, before anything is removed
        for l in self.list_links(p)? {
            let t = self.rp(&l)?;
            targets.push((l, t));
        }
        if self.win {
            for (l, t) in &targets {
                // a name or a target that can never be put on a Windows command line refuses the item NOW, untouched
                self.win_ok(l)?;
                if let Some(t) = t {
                    self.win_ok(t)?;
                }
            }
        }
        // a link whose target was already gone (a dangling junction) has nothing to lose
        let mut existed: Vec<bool> = vec![];
        for (_, t) in &targets {
            existed.push(match t {
                Some(t) => self.exists(t)? == Some(true),
                None => false,
            });
        }
        if let Some(f) = before_rm {
            f()?;
        }
        self.unlink_all(p)?;
        let c = self.run(&if self.win { format!("rmdir /S /Q {}", self.q(p)?) } else { format!("rm -r -- {}", self.q(p)?) }, Some(self.cfg.remove_timeout_s))?;
        if c.rc.is_none() {
            let e = self.exists(p)?;
            self.assert_main(before)?;
            return Ok(("stopped", format!("removal did not finish in {}s; re-probe: {}; not counted as done", crate::pycompat::fmt_g(self.cfg.remove_timeout_s), probe_word(e))));
        }
        if !self.gone(p)? {
            return Ok(("failed", format!("PARTLY REMOVED: {p} still exists after the removal (a long path, a locked file or a permission); some of it may be gone. Left as it is for a person.")));
        }
        let inside: Vec<String> = std::iter::once(norm(p)).chain(rpp).collect();
        for (i, (l, t)) in targets.iter().enumerate() {
            if let Some(t) = t {
                if !existed[i] || *t == norm(l) || inside.iter().any(|x| t == x || t.starts_with(&format!("{x}/"))) {
                    continue; // a link inside the folder (an npm workspace junction, a relative symlink) goes with it
                }
                if self.exists(t)? != Some(true) {
                    return guard(format!("the target {t} of the link {l} is gone after removing {p}; stopping"));
                }
            }
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
        let (rc, out) = get(&crate::winprobe::free_bytes_cmd(path))?;
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
