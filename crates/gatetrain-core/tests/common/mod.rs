//! Shared test support: `Sim`, a runner over a virtual file system, git state and docker state. A port of `SimHost` in
//! tests/test_tidy.py (the safety spec), with the same command vocabulary and the same fixtures.
#![allow(dead_code)]
use gatetrain_core::config::{Config, Disk, Host, Num};
use gatetrain_core::runner::{RunOut, Runner};
use regex::Regex;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub const NOW: i64 = 1_800_000_000;
pub const DAY: i64 = 86400;
pub const GB: i64 = 1024 * 1024 * 1024;
pub const MB: i64 = 1024 * 1024;

pub fn n(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub typ: &'static str, // dir | file | link
    pub size: i64,
    pub mtime: i64,
    pub target: Option<String>,
    /// for a Windows link: "file" when it links a file (default: a folder)
    pub ltype: Option<&'static str>,
}

pub type Pre = Box<dyn Fn(&mut SimState, &str) -> Option<RunOut>>;
pub type Post = Box<dyn Fn(&mut SimState, &str)>;

#[derive(Default)]
pub struct SimState {
    pub win: bool,
    pub free: f64,
    pub fs: Vec<(String, Entry)>, // insertion order, like a Python dict
    pub calls: Vec<String>,
    pub scripts: HashMap<String, String>,
    pub scratch: String,
    pub worktrees: Vec<(String, String)>,
    pub branches: Vec<String>,
    pub diffs: HashMap<String, String>,
    pub shown: HashMap<String, String>,
    pub ancestors: HashSet<String>,
    pub dirty: HashSet<String>,
    pub containers_exited: Vec<String>,
    pub containers_running: Vec<String>,
    pub images: Vec<String>,
    pub image_users: Vec<String>,
    pub cache_row: Option<String>,
    pub broken_links: bool,
    pub unlink_noop: bool,
    pub clean_needs_tag: bool,
    pub tags: Vec<String>,
    pub deleted_branches: Vec<String>,
    pub main: String,
    pub slow: Vec<String>,
    pub du_timeouts: Vec<Option<f64>>,
    pub argvs: Vec<Vec<String>>,
    pub shas: HashMap<String, String>,
    pub tag_refs: HashMap<String, String>,
    pub mtree: HashMap<String, String>,
    pub conflict: HashSet<String>,
    pub tag_fails: bool,
    pub mv_fails: bool,
    pub wt_timeout: bool,
    pub rm_timeout: bool,
    pub unknown_exists: Vec<String>,
    pub locked: HashSet<String>,
    pub tree_main: String,
    /// when set, a `git tag` moves this branch to the given sha right after (a commit lands between archive and delete)
    pub move_branch_on_tag: Option<(String, String)>,
    /// when set, a `git tag` records this sha instead of the one asked for
    pub wrong_tag_sha: Option<String>,
    /// when set, a command starting with this text makes the host "go away": the runner fails like a crashed process
    pub crash_on: Option<String>,
    /// every shell line with the timeout it was given
    pub call_timeouts: Vec<(String, Option<f64>)>,
    /// a recursive remove that says ok and removes nothing (a long path, a locked file)
    pub rm_noop: bool,
}

impl SimState {
    pub fn sha(&self, br: &str) -> String {
        self.shas.get(br).cloned().unwrap_or_else(|| format!("sha-{br}"))
    }
    pub fn get(&self, p: &str) -> Option<&Entry> {
        let p = n(p);
        self.fs.iter().find(|(k, _)| *k == p).map(|(_, e)| e)
    }
    pub fn get_mut(&mut self, p: &str) -> Option<&mut Entry> {
        let p = n(p);
        self.fs.iter_mut().find(|(k, _)| *k == p).map(|(_, e)| e)
    }
    pub fn has(&self, p: &str) -> bool {
        self.get(p).is_some()
    }
    pub fn add(&mut self, path: &str, typ: &'static str, size: i64, age_s: i64, target: Option<&str>) {
        let e = Entry { typ, size, mtime: NOW - age_s, target: target.map(n), ltype: None };
        let k = n(path);
        if let Some(slot) = self.fs.iter_mut().find(|(x, _)| *x == k) {
            slot.1 = e;
        } else {
            self.fs.push((k.clone(), e));
        }
        if let Some((parent, name)) = k.rsplit_once('/') {
            if ["/trash", "c:/trash"].contains(&parent.to_lowercase().as_str()) && name != ".gatetrain-seen.json" {
                // the recycle bin remembers when tidy first saw each entry; a fixture entry that is `age_s` old was first seen `age_s` ago
                let sp = format!("{parent}/.gatetrain-seen.json");
                let mut seen: serde_json::Map<String, Value> = self.scripts.get(&sp).and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
                seen.insert(name.to_string(), json!(NOW - age_s));
                self.scripts.insert(sp.clone(), serde_json::to_string(&seen).unwrap());
                let fe = Entry { typ: "file", size: 10, mtime: NOW, target: None, ltype: None };
                match self.fs.iter_mut().find(|(x, _)| *x == sp) {
                    Some(slot) => slot.1 = fe,
                    None => self.fs.push((sp, fe)),
                }
            }
        }
    }
    pub fn dir(&mut self, path: &str, age_s: i64) {
        self.add(path, "dir", 0, age_s, None)
    }
    pub fn file(&mut self, path: &str, size: i64, age_s: i64) {
        self.add(path, "file", size, age_s, None)
    }
    pub fn link(&mut self, path: &str, target: &str) {
        self.add(path, "link", 0, 10 * DAY, Some(target))
    }
    pub fn under(&self, p: &str) -> Vec<String> {
        let p = n(p);
        self.fs.iter().map(|(k, _)| k.clone()).filter(|k| *k == p || k.starts_with(&format!("{p}/"))).collect()
    }
    pub fn total(&self, p: &str) -> i64 {
        self.under(p).iter().map(|k| self.get(k).unwrap()).filter(|e| e.typ == "file").map(|e| e.size).sum()
    }
    pub fn tracked(&self) -> Vec<String> {
        let mut v: Vec<String> = self.under(&self.main.clone()).into_iter().filter(|k| self.get(k).unwrap().typ == "file").collect();
        v.sort();
        v
    }
    pub fn children(&self, p: &str) -> Vec<String> {
        let p = n(p);
        let mut s: Vec<String> = self
            .fs
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{p}/")))
            .map(|(k, _)| k[p.len() + 1..].split('/').next().unwrap().to_string())
            .collect();
        s.sort();
        s.dedup();
        s
    }
    pub fn pop(&mut self, p: &str) -> Option<Entry> {
        let p = n(p);
        let i = self.fs.iter().position(|(k, _)| *k == p)?;
        Some(self.fs.remove(i).1)
    }
    pub fn rm_tree(&mut self, p: &str, follow_links: bool) {
        for k in self.under(p) {
            if let Some(e) = self.get(&k).cloned() {
                if e.typ == "link" && follow_links {
                    for t in self.under(e.target.as_deref().unwrap()) {
                        self.pop(&t); // a recursive remove that follows a link empties its target
                    }
                }
            }
            self.pop(&k);
        }
    }
    pub fn snapshot(&self) -> Vec<(String, Entry)> {
        self.fs.clone()
    }
    pub fn fs_map(&self) -> HashMap<String, Entry> {
        self.fs.iter().cloned().collect()
    }
}

pub struct Sim {
    pub st: RefCell<SimState>,
    pub pre: RefCell<Vec<Pre>>,
    pub post: RefCell<Vec<Post>>,
}

/// A runner handle that shares one `Sim`.
#[derive(Clone)]
pub struct Shared(pub Rc<Sim>);

impl Runner for Shared {
    fn run_for(&self, cmd: &str, timeout: Option<f64>) -> Result<RunOut, String> {
        self.0.run_for(cmd, timeout)
    }
    fn put_file(&self, path: &str, text: &str) -> Result<(), String> {
        self.0.put_file(path, text)
    }
    fn scratch_dir(&self) -> Option<String> {
        self.0.scratch_dir()
    }
    fn run_argv_to(&self, argv: &[String], timeout: Option<f64>) -> Result<RunOut, String> {
        self.0.run_argv_to(argv, timeout)
    }
    fn shell_free(&self) -> bool {
        true
    }
}

impl Sim {
    pub fn new(win: bool) -> Sim {
        let st = SimState {
            win,
            free: 500.0,
            scratch: if win { "C:/tmp".into() } else { "/tmp".into() },
            clean_needs_tag: true,
            tree_main: "TREE_MAIN".into(),
            main: if win { "C:/repo/main".into() } else { "/repo/main".into() },
            ..Default::default()
        };
        Sim { st: RefCell::new(st), pre: RefCell::new(vec![]), post: RefCell::new(vec![]) }
    }
    pub fn shared(self) -> Shared {
        Shared(Rc::new(self))
    }
    pub fn s(&self) -> std::cell::RefMut<'_, SimState> {
        self.st.borrow_mut()
    }
}

impl Shared {
    pub fn s(&self) -> std::cell::RefMut<'_, SimState> {
        self.0.st.borrow_mut()
    }
    pub fn pre(&self, h: Pre) {
        self.0.pre.borrow_mut().push(h);
    }
    pub fn post(&self, h: Post) {
        self.0.post.borrow_mut().push(h);
    }
    pub fn calls(&self) -> Vec<String> {
        self.0.st.borrow().calls.clone()
    }
}

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

impl Runner for Sim {
    fn shell_free(&self) -> bool {
        true // models LocalRunner: run_argv never involves a shell
    }
    fn run_argv_to(&self, argv: &[String], timeout: Option<f64>) -> Result<RunOut, String> {
        self.st.borrow_mut().argvs.push(argv.to_vec());
        if argv[0] == "git" && argv.iter().skip(3).any(|a| a.contains('\\')) {
            return Ok(RunOut::done(128, "")); // a ref never contains a backslash
        }
        self.run_for(&argv.join(" "), timeout)
    }
    fn scratch_dir(&self) -> Option<String> {
        Some(self.st.borrow().scratch.clone())
    }
    fn put_file(&self, path: &str, text: &str) -> Result<(), String> {
        let mut st = self.st.borrow_mut();
        st.add(path, "file", text.chars().count() as i64, 0, None);
        st.scripts.insert(n(path), text.to_string());
        Ok(())
    }
    fn run_for(&self, cmd: &str, timeout: Option<f64>) -> Result<RunOut, String> {
        {
            let mut st = self.st.borrow_mut();
            st.call_timeouts.push((cmd.to_string(), timeout));
            if st.crash_on.as_deref().map(|p| cmd.starts_with(p)).unwrap_or(false) {
                return Err("host went away".into());
            }
        }
        if cmd.starts_with("du -sk") {
            self.st.borrow_mut().du_timeouts.push(timeout);
        }
        for h in self.pre.borrow().iter() {
            if let Some(r) = h(&mut self.st.borrow_mut(), cmd) {
                return Ok(r);
            }
        }
        let r = self.base(cmd);
        for h in self.post.borrow().iter() {
            h(&mut self.st.borrow_mut(), cmd);
        }
        Ok(r)
    }
}

impl Sim {
    fn base(&self, cmd: &str) -> RunOut {
        let mut st = self.st.borrow_mut();
        st.calls.push(cmd.to_string());
        if re(r"^(du -sk|powershell .*Measure-Object)").is_match(cmd) {
            let c = n(&cmd.replace(['"', '\''], ""));
            if st.slow.iter().any(|x| c.contains(x.as_str())) {
                return RunOut::TimedOut;
            }
        }
        let c = cmd.replace('\\', "/").replace(['"', '\''], "");
        let ok = |s: String| RunOut::done(0, s);
        if c.starts_with("df -Pk") {
            return ok(format!("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/a 9 9 {} 1% /m\n", (st.free * 1024.0 * 1024.0) as i64));
        }
        if c.starts_with("python -c") && c.contains("shutil") {
            return ok(format!("{}\n", (st.free * GB as f64) as i64));
        }
        if st.rm_timeout && re(r"^(rm -r --|rmdir /S /Q) ").is_match(&c) {
            return RunOut::TimedOut;
        }
        if st.unknown_exists.iter().any(|x| !x.is_empty() && c.contains(x.as_str())) && re(r"^(test -e|if exist)").is_match(&c) {
            return RunOut::done(1, "");
        }
        let m = re(r"^find (.+?) -maxdepth (\d+) -type f -mmin -(\d+) -print -quit$").captures(&c).or_else(|| re(r"Get-ChildItem -LiteralPath (.+?) -Recurse -Depth (\d+) .*AddMinutes\(-(\d+)\)").captures(&c));
        if let Some(m) = m {
            let limit = NOW - m[3].parse::<i64>().unwrap() * 60;
            for k in st.under(&m[1]) {
                let e = st.get(&k).unwrap();
                if e.typ == "file" && e.mtime > limit {
                    return ok(format!("{k}\n"));
                }
            }
            return ok(String::new());
        }
        if let Some(m) = re(r"^(?:cat|type) (.+)$").captures(&c) {
            return match st.scripts.get(&n(&m[1])) {
                Some(t) => ok(t.clone()),
                None => RunOut::done(1, ""),
            };
        }
        if let Some(m) = re(r"^du -sk (.+)$").captures(&c) {
            return ok(format!("{}\t{}\n", st.total(&m[1]) / 1024, &m[1]));
        }
        if c.starts_with("powershell") && c.contains("Measure-Object") {
            let m = re(r"-LiteralPath (.+?) -Recurse").captures(&c).unwrap();
            return ok(format!("{}\n", st.total(&m[1])));
        }
        let m = re(r"^stat -c %Y (.+)$").captures(&c).or_else(|| re(r"Get-Item -LiteralPath (.+?)\)\.LastWrite").captures(&c));
        if let Some(m) = m {
            return match st.get(&m[1]) {
                Some(e) => ok(format!("{}\n", e.mtime)),
                None => RunOut::done(1, ""),
            };
        }
        if let Some(m) = re(r"^find (.+?) -mindepth 1 -maxdepth 1 -print0$").captures(&c) {
            let pre = m[1].trim_end_matches('/').to_string();
            return ok(st.children(&m[1]).iter().map(|x| format!("{pre}/{x}\0")).collect::<String>()); // real find prints <path>/<name>
        }
        let m = re(r"^test -L (.+?) &&").captures(&c).or_else(|| re(r"Get-Item -LiteralPath (.+?) -Force\)\.Attributes").captures(&c));
        if let Some(m) = m {
            return ok(match st.get(&m[1]) {
                Some(e) if e.typ == "link" => "yes\n".into(),
                Some(_) => "no\n".into(),
                None => "missing\n".into(),
            });
        }
        if let Some(m) = re(r"Get-Item -LiteralPath (.+?) -Force\)\.PSIsContainer").captures(&c) {
            return match st.get(&m[1]) {
                Some(e) => ok(if e.ltype == Some("file") { "False\n".into() } else { "True\n".into() }),
                None => RunOut::done(1, ""),
            };
        }
        let m = re(r"^realpath -m -- (.+)$").captures(&c).or_else(|| re(r"^python -c .*os\.path\.realpath.*? (\S+)$").captures(&c));
        if let Some(m) = m {
            let p = n(&m[1]);
            let mut ks: Vec<&(String, Entry)> = st.fs.iter().collect();
            ks.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
            for (k, e) in ks {
                if e.typ == "link" && (p == *k || p.starts_with(&format!("{k}/"))) {
                    return ok(format!("{}{}\n", e.target.clone().unwrap(), &p[k.len()..]));
                }
            }
            return ok(format!("{p}\n"));
        }
        if let Some(m) = re(r"^(?:ls -1|dir /B) (.+)$").captures(&c) {
            return ok(st.children(&m[1]).join("\n") + "\n");
        }
        let m = re(r"^test -e (.+?) &&").captures(&c).or_else(|| re(r"^if exist (.+?) \(echo").captures(&c));
        if let Some(m) = m {
            let have = st.has(&m[1]) || (st.win && st.fs.iter().any(|(k, _)| k.to_lowercase() == n(&m[1]).to_lowercase())); // Windows paths compare without case
            return ok(if have { "yes\n".into() } else { "no\n".into() });
        }
        let m = re(r"^find (.+?) -type l; echo (\w+)").captures(&c).or_else(|| re(r"^dir /AL /S /B (.+?) & echo (\w+)").captures(&c));
        if let Some(m) = m {
            if st.broken_links {
                return RunOut::done(1, "");
            }
            let mut links: Vec<String> = st.under(&m[1]).into_iter().filter(|k| st.get(k).unwrap().typ == "link").collect();
            links.push(m[2].to_string());
            return ok(links.join("\n") + "\n");
        }
        if c.starts_with("unlink ") {
            if !st.unlink_noop {
                for p in c.split(" && ") {
                    for k in st.under(p.strip_prefix("unlink ").unwrap_or(p)) {
                        st.pop(&k); // removing a link takes what hangs below it with it
                    }
                }
            }
            return ok(String::new());
        }
        if let Some(rest) = c.strip_prefix("del ") {
            st.pop(rest);
            return ok(String::new());
        }
        if c.starts_with("rmdir ") && !c.starts_with("rmdir /S") {
            st.pop(&c[6..]); // a plain rmdir on a link removes only the link
            return ok(String::new());
        }
        if c.ends_with("gatetrain-unlink.cmd") {
            if !st.unlink_noop {
                let body = st.scripts.get(&n(&c)).cloned().unwrap();
                for l in body.lines() {
                    let l = l.replace('\\', "/").replace('"', "");
                    if let Some(m) = re(r"^(?:rmdir|del) (.+?)(?: 2>nul.*)?$").captures(&l) {
                        st.pop(&m[1]); // rmdir on a link removes only the link
                    }
                }
            }
            return ok(String::new());
        }
        if let Some(m) = re(r"^(?:rmdir /S /Q|rm -r --) (.+)$").captures(&c) {
            if st.rm_noop {
                return ok(String::new());
            }
            st.rm_tree(&m[1], true); // the dangerous kind: follows what is still linked
            return ok(String::new());
        }
        if let Some(m) = re(r"^(?:mv --|move) (.+?) (.+)$").captures(&c) {
            if st.mv_fails {
                return RunOut::done(1, "");
            }
            let (src, dst) = (n(&m[1]), n(&m[2]));
            for k in st.under(&src) {
                let e = st.pop(&k).unwrap();
                let nk = format!("{}{}", dst, &k[src.len()..]);
                st.fs.push((nk, e));
            }
            return ok(String::new());
        }
        if let Some(m) = re(r"cargo clean .*--target-dir (.+)$").captures(&c) {
            let p = n(&m[1]);
            if st.clean_needs_tag && !st.has(&format!("{p}/CACHEDIR.TAG")) {
                return ok(String::new()); // refuses, says nothing
            }
            for k in st.under(&p) {
                if k != p && !k.ends_with("CACHEDIR.TAG") {
                    st.pop(&k);
                }
            }
            return ok(String::new());
        }
        if c.starts_with("git -C") {
            return Self::git(&mut st, &c);
        }
        if c.starts_with("docker") {
            return Self::docker(&mut st, &c);
        }
        RunOut::done(127, "")
    }

    fn git(st: &mut SimState, c: &str) -> RunOut {
        let m = re(r"^git -C (\S+) (.+)$").captures(c).unwrap();
        let (repo, a) = (n(&m[1]), m[2].to_string());
        let ok = |s: String| RunOut::done(0, s);
        if a == "ls-files" {
            return ok(st.tracked().join("\n") + "\n");
        }
        if a == "status --porcelain" {
            return ok(if st.dirty.contains(&repo) { "M x\n".into() } else { String::new() });
        }
        if a.starts_with("worktree list") {
            return ok(st.worktrees.iter().map(|w| format!("worktree {}\nHEAD abc\nbranch refs/heads/{}\n{}{}", w.0, w.1, if st.locked.contains(&n(&w.0)) { "locked\n" } else { "" }, if !st.has(&w.0) && n(&w.0) != n(&st.main) { "prunable gitdir file points to non-existent location\n" } else { "" })).collect::<Vec<_>>().join("\n"));
        }
        if a == "branch --list" {
            return ok(st.branches.iter().map(|b| format!("  {b}")).collect::<Vec<_>>().join("\n") + "\n");
        }
        if let Some(m) = re(r"^merge-base --is-ancestor (\S+) (\S+)").captures(&a) {
            return RunOut::done(if st.ancestors.contains(&m[1].replacen("refs/heads/", "", 1).replacen("sha-", "", 1)) { 0 } else { 1 }, "");
        }
        if let Some(m) = re(r"^rev-parse --verify --quiet refs/heads/(\S+?)\^\{commit\}$").captures(&a) {
            return if st.branches.iter().any(|b| *b == m[1]) { ok(format!("{}\n", st.sha(&m[1]))) } else { RunOut::done(1, "") };
        }
        if let Some(m) = re(r"^rev-parse --verify --quiet refs/tags/(\S+?)\^\{commit\}$").captures(&a) {
            return match st.tag_refs.get(&m[1]) {
                Some(s) => ok(format!("{s}\n")),
                None => RunOut::done(1, ""),
            };
        }
        if re(r"^rev-parse --verify (\S+)\^\{tree\}$").is_match(&a) {
            return ok(format!("{}\n", st.tree_main));
        }
        if let Some(m) = re(r"^merge-tree --write-tree (\S+) (\S+)$").captures(&a) {
            let b2 = m[2].replacen("refs/heads/", "", 1).replacen("sha-", "", 1);
            if st.conflict.contains(&b2) {
                return RunOut::done(1, "TREE_C\n");
            }
            return ok(format!("{}\n", st.mtree.get(&b2).cloned().unwrap_or_else(|| "TREE_OTHER".into())));
        }
        if let Some(m) = re(r"^update-ref -d refs/heads/(\S+) (\S+)$").captures(&a) {
            if st.sha(&m[1]) != m[2] {
                return RunOut::done(1, "");
            }
            st.deleted_branches.push(m[1].to_string());
            st.branches.retain(|b| *b != m[1]);
            return ok(String::new());
        }
        if let Some(m) = re(r"^diff --unified=0 --no-color (\S+)\.\.\.(\S+)").captures(&a) {
            return ok(st.diffs.get(&m[2]).cloned().unwrap_or_default());
        }
        if let Some(m) = re(r"^show (\S+?):(\S+)$").captures(&a) {
            return match st.shown.get(&m[2]) {
                Some(t) => ok(t.clone()),
                None => RunOut::done(128, ""),
            };
        }
        if a == "rev-parse --show-toplevel" {
            return if st.has(&repo) { ok(format!("{repo}\n")) } else { RunOut::done(128, "") };
        }
        if a == "worktree prune" {
            let main = n(&st.main);
            let keep: Vec<(String, String)> = st.worktrees.iter().filter(|w| st.has(&w.0) || n(&w.0) == main).cloned().collect();
            st.worktrees = keep;
            return ok(String::new());
        }
        if let Some(m) = re(r"^worktree remove (\S+)").captures(&a) {
            if st.wt_timeout {
                return RunOut::TimedOut;
            }
            st.rm_tree(&m[1], true);
            let p = n(&m[1]);
            st.worktrees.retain(|w| n(&w.0) != p);
            return ok(String::new());
        }
        if a.starts_with("tag ") {
            if st.tag_fails {
                return RunOut::done(128, "");
            }
            let parts: Vec<&str> = a.split(' ').collect();
            if st.tag_refs.contains_key(parts[1]) {
                return RunOut::done(128, "");
            }
            st.tags.push(parts[1].to_string());
            let sha = st.wrong_tag_sha.clone().unwrap_or_else(|| parts.get(2).unwrap_or(&"").to_string());
            st.tag_refs.insert(parts[1].to_string(), sha);
            if let Some((b, s)) = st.move_branch_on_tag.clone() {
                st.shas.insert(b, s);
            }
            return ok(String::new());
        }
        if let Some(m) = re(r"^branch -D (\S+)").captures(&a) {
            st.deleted_branches.push(m[1].to_string());
            return ok(String::new());
        }
        RunOut::done(1, "")
    }

    fn docker(st: &mut SimState, c: &str) -> RunOut {
        let ok = |s: String| RunOut::done(0, s);
        if c.contains("ps -a --filter status=exited") {
            return ok(st.containers_exited.join("\n") + "\n");
        }
        if c.starts_with("docker ps --format") {
            return ok(st.containers_running.join("\n") + "\n");
        }
        if c.contains("ps -a --format") {
            return ok(st.image_users.join("\n") + "\n");
        }
        if c.starts_with("docker images") {
            return ok(st.images.join("\n") + "\n");
        }
        if c.starts_with("docker system df") {
            return ok(st.cache_row.clone().unwrap_or_default());
        }
        if c.starts_with("docker rm ") {
            let nm = c.split(' ').nth(2).unwrap().to_string();
            st.containers_exited.retain(|x| *x != nm);
            return ok(String::new());
        }
        if c.starts_with("docker rmi ") {
            let nm = c.split(' ').nth(2).unwrap().to_string();
            st.images.retain(|x| !x.starts_with(&format!("{nm}|")));
            return ok(String::new());
        }
        if c.starts_with("docker builder prune") {
            st.cache_row = None;
            return ok(String::new());
        }
        RunOut::done(127, "")
    }
}

// ---------------------------------------------------------------- fixtures

pub fn cargo_target(s: &mut SimState, path: &str, size: i64, tag: bool, age_s: i64) {
    s.dir(path, age_s);
    s.dir(&format!("{path}/debug"), age_s);
    s.dir(&format!("{path}/tmp"), age_s);
    s.file(&format!("{path}/debug/big.o"), size, age_s);
    if tag {
        s.file(&format!("{path}/CACHEDIR.TAG"), 100, age_s);
    }
}

pub fn linux_sim() -> Shared {
    let sim = Sim::new(false).shared();
    {
        let s = &mut *sim.s();
        s.dir("/repo/main", 0);
        for f in ["a.rs", "b.rs", "c.rs"] {
            s.file(&format!("/repo/main/{f}"), 10, 0);
        }
        s.dir("/repo/main/node_modules", 0);
        s.file("/repo/main/node_modules/pkg.js", 10, 0);
        s.dir("/repo/wt", 0);
        s.dir("/build", 0);
        s.dir("/build/targets", 0);
        cargo_target(s, "/build/targets/lane-a", 100 * MB, false, 10 * DAY); // merged by content, no CACHEDIR.TAG
        cargo_target(s, "/build/targets/lane-b", 100 * MB, true, 10 * DAY); // not merged
        cargo_target(s, "/build/targets/lane-c", 100 * MB, true, 10 * DAY); // merged but on the keep list
        cargo_target(s, "/build/targets/lane-new", 100 * MB, true, 60); // merged but used a minute ago
        cargo_target(s, "/build/chain", 3 * GB, true, 10 * DAY); // shared, over budget
        s.worktrees = [("/repo/main", "main"), ("/repo/wt/lane-a", "line/lane-a"), ("/repo/wt/lane-b", "line/lane-b"), ("/repo/wt/lane-d", "line/lane-d"), ("/repo/wt/lane-c", "line/lane-c"), ("/repo/wt/lane-new", "line/lane-new")]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        s.branches = ["main", "line/lane-a", "line/lane-b", "line/lane-c", "line/lane-d", "line/lane-new"].iter().map(|x| x.to_string()).collect();
        for l in ["a", "b", "c", "d", "new"] {
            s.dir(&format!("/repo/wt/lane-{l}"), DAY);
            s.file(&format!("/repo/wt/lane-{l}/src.rs"), 5, DAY);
        }
        s.link("/repo/wt/lane-a/node_modules", "/repo/main/node_modules");
        s.dirty.insert("/repo/wt/lane-d".into());
        s.shown.insert("f.rs".into(), "let x = 1;\nlet y = 2;\n".into());
        s.diffs.insert("line/lane-a".into(), "--- a/f.rs\n+++ b/f.rs\n@@ -0,0 +1,2 @@\n+let x = 1;\n+let y = 2;\n".into());
        s.diffs.insert("line/lane-b".into(), "--- a/f.rs\n+++ b/f.rs\n@@ -0,0 +1 @@\n+let z = 3;\n".into());
        s.ancestors = ["line/lane-c", "line/lane-d", "line/lane-new", "main"].iter().map(|x| x.to_string()).collect();
        s.mtree.insert("line/lane-a".into(), s.tree_main.clone()); // merging lane-a into main changes nothing
        s.dir("/trash", 0);
        s.dir("/trash/old", 10 * DAY);
        s.file("/trash/old/x", 50 * MB, 10 * DAY);
        s.dir("/trash/new", DAY);
        s.file("/trash/new/x", 5 * MB, DAY);
        s.containers_exited = vec!["gate-1".into(), "gate-9".into(), "other-1".into()];
        s.containers_running = vec!["gate-9".into()];
        s.images = ["gate-img:1|aaa111|2.0GB", "gate-img:2|bbb222|1.0GB", "keepme:1|ccc333|500MB", "gate-img:<none>|ddd444|1GB"].iter().map(|x| x.to_string()).collect();
        s.image_users = vec!["gate-img:2".into()];
        s.cache_row = Some("Build Cache|8GB|6GB\nImages|3GB|1GB\n".into());
        s.free = 5.0;
    }
    sim
}

pub fn win_sim() -> Shared {
    let sim = Sim::new(true).shared();
    {
        let s = &mut *sim.s();
        s.dir("C:/repo/main", 0);
        for f in ["a.rs", "b.rs"] {
            s.file(&format!("C:/repo/main/{f}"), 10, 0);
        }
        s.dir("C:/repo/main/node_modules", 0);
        s.file("C:/repo/main/node_modules/pkg.js", 10, 0);
        s.dir("C:/repo/wt/lane-a", DAY);
        s.file("C:/repo/wt/lane-a/src.rs", 5, DAY);
        s.link("C:/repo/wt/lane-a/node_modules", "C:/repo/main/node_modules");
        s.worktrees = vec![("C:/repo/main".into(), "main".into()), ("C:/repo/wt/lane-a".into(), "line/lane-a".into())];
        s.branches = vec!["main".into(), "line/lane-a".into()];
        s.ancestors = ["line/lane-a", "main"].iter().map(|x| x.to_string()).collect();
        s.mtree.insert("line/lane-a".into(), s.tree_main.clone());
        s.dir("C:/canary", 0);
        s.link("C:/canary/l1", "C:/repo/main/a.rs");
    }
    sim
}

pub fn base() -> Value {
    json!({"main_checkout": "/repo/main", "root_checkout": "/repo/root", "trash_dir": "/trash",
        "container_prefix": "gate-", "image_patterns": ["gate-img:*"], "cache_days": 3, "keep_lanes": ["lane-c"],
        "target_roots": ["/build/targets"], "targets": [{"path": "/build/chain", "budget_gb": 1}], "worktree_min_idle_days": 0})
}

pub fn win_extra() -> Value {
    json!({"main_checkout": "C:/repo/main", "root_checkout": "C:/repo/root", "scratch_dir": "C:/tmp", "target_roots": [], "targets": [],
        "trash_dir": "C:/trash", "container_prefix": "x-", "image_patterns": [], "cache_days": 0, "keep_lanes": [],
        "link_canary": "C:/canary", "link_canary_count": 1, "worktree_min_idle_days": 0})
}

pub fn merged(extra: Value) -> Value {
    let mut b = base();
    if let Value::Object(m) = extra {
        for (k, v) in m {
            b.as_object_mut().unwrap().insert(k, v);
        }
    }
    b
}

pub fn to_table(v: &Value) -> toml::Table {
    let mut t = toml::Table::new();
    for (k, x) in v.as_object().unwrap() {
        t.insert(k.clone(), toml::Value::try_from(x).unwrap());
    }
    t
}

pub fn hostcfg(extra: Value, win: bool, mins: f64) -> (Config, Host) {
    let h = Host {
        name: "h1".into(),
        reach: "local".into(),
        os: if win { "windows" } else { "linux" }.into(),
        disks: vec![Disk { path: if win { "C:/build" } else { "/build" }.into(), min_free_gb: Num::F(mins), measure: "du".into(), ..Default::default() }],
        tidy: to_table(&merged(extra)),
        ..Default::default()
    };
    let cfg = Config { hosts: vec![h.clone()], ..Default::default() };
    (cfg, h)
}

/// A hosts.toml on disk for the CLI tests (same shape as `write_conf` in test_tidy.py).
pub fn write_conf(dir: &std::path::Path, extra: Value, disk: bool) -> String {
    let mut host = toml::Table::new();
    host.insert("name".into(), "h1".into());
    host.insert("reach".into(), "local".into());
    host.insert("os".into(), "linux".into());
    if disk {
        let mut d = toml::Table::new();
        d.insert("path".into(), "/build".into());
        d.insert("min_free_gb".into(), 50.into());
        host.insert("disks".into(), toml::Value::Array(vec![toml::Value::Table(d)]));
    }
    let mut root = toml::Table::new();
    root.insert("hosts".into(), toml::Value::Array(vec![toml::Value::Table(host)]));
    root.insert("tidy".into(), toml::Value::Table(to_table(&merged(extra))));
    let p = dir.join("hosts.toml");
    std::fs::write(&p, toml::to_string(&root).unwrap()).unwrap();
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------- helpers shared by the test files

use gatetrain_core::tidy::config::{build_cfg, TidyCfg};
use gatetrain_core::tidy::inventory::{gather, Inventory, Item};
use gatetrain_core::tidy::{cmd_check, cmd_tidy, CliOut};

pub fn by_key<'a>(items: &'a [Item], key: &str) -> Vec<&'a Item> {
    items.iter().filter(|i| i.key == key).collect()
}

/// `gather` over a sim with the BASE config plus `extra`.
pub fn gather_sim(sim: &Shared, extra: Value, win: bool, mins: f64) -> (Host, TidyCfg, Inventory) {
    let (cfg, h) = hostcfg(extra, win, mins);
    let t = build_cfg(&cfg, &h).unwrap();
    let inv = gather(&h, sim, &t, Some(NOW as f64)).unwrap();
    (h, t, inv)
}

/// The command line over a sim: `check` / `tidy` with their flags, config at `conf`.
pub fn cli(sim: &Shared, args: &[&str], conf: &str) -> CliOut {
    let mk = |_h: &Host| -> Option<Box<dyn Runner>> { Some(Box::new(sim.clone())) };
    let now = Some(NOW as f64);
    let host = args.iter().position(|a| *a == "--host").map(|i| args[i + 1]);
    match args[0] {
        "check" => cmd_check(conf, host, args.contains(&"--json"), &mk, now),
        "tidy" => {
            let cls = args.iter().position(|a| *a == "--class").map(|i| args[i + 1]);
            cmd_tidy(conf, host, args.contains(&"--apply"), cls, args.contains(&"--auto"), &mk, now)
        }
        _ => unreachable!(),
    }
}

pub fn all_records(log: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

/// The action log without the `intent` lines written before each action.
pub fn records(log: &std::path::Path) -> Vec<Value> {
    all_records(log).into_iter().filter(|r| r["status"] != "intent").collect()
}
