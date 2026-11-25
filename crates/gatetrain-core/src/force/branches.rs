//! `gatetrain branches`: every branch and worktree of a host and of the remote in one table, with its state, and the limits that turn a
//! pile-up into a red line (founder 2026-11-25T16:45: "full monitoring on branches"). Read-only: git reads and one size probe, nothing else.
//!
//! State: `archived` (its tip is on an `archive/*` ref), `landed` (its CONTENT is on main: an ancestor, or `git merge-tree --write-tree
//! main <b>` gives main's own tree, so a squash or a cherry-pick counts), `stale` (not landed and idle past `max_idle_days`), else `live`.
use super::cfg::Limits;
use super::exec::{git, Exec};
use crate::ledger::Ledger;
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BranchRow {
    pub host: String,
    pub name: String,
    /// worktree | branch | remote
    pub kind: String,
    pub path: Option<String>,
    pub branch: Option<String>,
    pub tip: String,
    pub size_bytes: Option<i64>,
    pub ahead: i64,
    pub behind: i64,
    pub on_main: bool,
    pub dirty: i64,
    pub age_hours: f64,
    /// hours since the oldest commit that is not on main (None when the content is on main)
    pub unlanded_hours: Option<f64>,
    pub owner: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    pub host: String,
    pub item: String,
    pub rule: String,
    pub detail: String,
    pub fix: String,
}

pub struct HostScan<'a> {
    pub host: &'a str,
    pub e: &'a dyn Exec,
    /// the host's main checkout
    pub repo: &'a str,
    pub main: &'a str,
    pub remote: &'a str,
    /// the worktree folder prefix that is not part of a lane's name
    pub prefix: &'a str,
    pub win: bool,
    pub stale_days: f64,
    /// size every worktree (a walk of the folder, links not followed)
    pub sizes: bool,
}

struct Base {
    mainref: String,
    tree: String,
    archived: HashSet<String>,
}

fn base(h: &HostScan) -> Result<Base, String> {
    let mut mainref = None;
    for r in [format!("refs/remotes/{}/{}", h.remote, h.main), format!("refs/heads/{}", h.main)] {
        if git(h.e, h.repo, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]).ok() {
            mainref = Some(r);
            break;
        }
    }
    let mainref = mainref.ok_or(format!("{}: neither {}/{} nor {} exists in {}", h.host, h.remote, h.main, h.main, h.repo))?;
    let t = git(h.e, h.repo, &["rev-parse", &format!("{mainref}^{{tree}}")]);
    if !t.ok() {
        return Err(format!("{}: cannot read the tree of {mainref}", h.host));
    }
    let a = git(h.e, h.repo, &["for-each-ref", "--format=%(objectname)", &format!("refs/remotes/{}/archive", h.remote), "refs/heads/archive"]);
    Ok(Base { mainref, tree: t.out.trim().to_string(), archived: a.out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect() })
}

fn leaf(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn stackdump(l: &str) -> bool {
    l.trim_end().ends_with(".stackdump")
}

#[allow(clippy::too_many_arguments)]
fn describe(h: &HostScan, b: &Base, now: i64, host: &str, name: &str, kind: &str, path: Option<&str>, branch: Option<&str>, tip: &str, owner: &dyn Fn(&str) -> Option<String>) -> BranchRow {
    let mut r = BranchRow { host: host.into(), name: name.into(), kind: kind.into(), path: path.map(String::from), branch: branch.map(String::from), tip: tip.into(), ..Default::default() };
    let c = git(h.e, h.repo, &["rev-list", "--left-right", "--count", &format!("{}...{tip}", b.mainref)]);
    let n: Vec<i64> = c.out.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if n.len() == 2 {
        (r.behind, r.ahead) = (n[0], n[1]);
    }
    r.on_main = git(h.e, h.repo, &["merge-base", "--is-ancestor", tip, &b.mainref]).ok() || {
        let m = git(h.e, h.repo, &["merge-tree", "--write-tree", &b.mainref, tip]);
        m.ok() && m.out.lines().next().map(str::trim) == Some(b.tree.as_str())
    };
    let last = git(h.e, h.repo, &["log", "-1", "--format=%ct%x09%an", tip]);
    let (ct, author) = last.out.trim().split_once('\t').map(|(a, b)| (a.parse::<i64>().unwrap_or(now), b.to_string())).unwrap_or((now, String::new()));
    r.age_hours = ((now - ct).max(0) as f64) / 3600.0;
    if !r.on_main {
        let l = git(h.e, h.repo, &["log", "--format=%ct", &format!("{}..{tip}", b.mainref)]);
        r.unlanded_hours = l.out.lines().filter_map(|x| x.trim().parse::<i64>().ok()).min().map(|t| ((now - t).max(0) as f64) / 3600.0);
    }
    if let Some(p) = path {
        let s = git(h.e, p, &["status", "--porcelain", "--untracked-files=normal"]);
        r.dirty = s.out.lines().filter(|l| !l.trim().is_empty() && !stackdump(l)).count() as i64;
        if h.sizes {
            r.size_bytes = size_of(h, p);
        }
    }
    r.owner = branch
        .and_then(owner)
        .or_else(|| path.map(leaf).map(|d| d.strip_prefix(h.prefix).unwrap_or(&d).to_string()).filter(|d| !d.is_empty()))
        .unwrap_or(author);
    let archived = branch.map(|x| x.starts_with("archive/")).unwrap_or(false) || b.archived.contains(tip);
    r.state = if archived {
        "archived"
    } else if r.on_main {
        "landed"
    } else if r.age_hours > h.stale_days * 24.0 {
        "stale"
    } else {
        "live"
    }
    .into();
    r
}

/// Bytes of a folder, links not followed: `robocopy /L /XJ /BYTES` on Windows, `du -sk` elsewhere. None when it cannot be read.
fn size_of(h: &HostScan, p: &str) -> Option<i64> {
    if h.win {
        // list-only (`/L`): nothing is copied or created; the destination is a name that does not exist (NUL makes robocopy wait and retry)
        let dest = format!("{}-size-probe-never-created", p.trim_end_matches(['/', '\\']));
        let o = h.e.run(&["robocopy".into(), p.into(), dest, "/L".into(), "/E".into(), "/XJ".into(), "/NJH".into(), "/NFL".into(), "/NDL".into(), "/NP".into(), "/BYTES".into(), "/R:0".into(), "/W:0".into()]);
        return parse_robocopy_bytes(&o.text());
    }
    let o = h.e.run(&["du".into(), "-sk".into(), p.into()]);
    o.out.split_whitespace().next().and_then(|x| x.parse::<i64>().ok()).filter(|_| o.ok()).map(|k| k * 1024)
}

/// The total of the `Bytes :` line of a robocopy summary (the first number after the colon).
pub fn parse_robocopy_bytes(out: &str) -> Option<i64> {
    out.lines().find_map(|l| {
        let t = l.trim_start();
        let rest = t.strip_prefix("Bytes :")?;
        rest.split_whitespace().next()?.parse::<i64>().ok()
    })
}

/// The worktrees (but not the main checkout) and the local branches of one host.
pub fn scan(h: &HostScan, now: i64, owner: &dyn Fn(&str) -> Option<String>) -> Result<Vec<BranchRow>, String> {
    let b = base(h)?;
    let wl = git(h.e, h.repo, &["worktree", "list", "--porcelain"]);
    if !wl.ok() {
        return Err(format!("{}: git worktree list failed: {}", h.host, wl.text().trim()));
    }
    let mut rows = vec![];
    let mut held: HashSet<String> = HashSet::new();
    let mut first = true;
    for block in wl.out.split("\n\n") {
        let (mut path, mut head, mut branch) = (None, None, None);
        for l in block.lines() {
            if let Some(x) = l.strip_prefix("worktree ") {
                path = Some(x.trim().to_string());
            } else if let Some(x) = l.strip_prefix("HEAD ") {
                head = Some(x.trim().to_string());
            } else if let Some(x) = l.strip_prefix("branch ") {
                branch = Some(x.trim().trim_start_matches("refs/heads/").to_string());
            }
        }
        let (Some(path), Some(head)) = (path, head) else { continue };
        if let Some(bn) = &branch {
            held.insert(bn.clone());
        }
        if std::mem::take(&mut first) {
            continue; // the main checkout
        }
        rows.push(describe(h, &b, now, h.host, &leaf(&path), "worktree", Some(&path), branch.as_deref(), &head, owner));
    }
    let lb = git(h.e, h.repo, &["for-each-ref", "--format=%(refname:short)%09%(objectname)", "refs/heads"]);
    for l in lb.out.lines() {
        let Some((n, sha)) = l.trim().split_once('\t') else { continue };
        if n == h.main || held.contains(n) {
            continue;
        }
        rows.push(describe(h, &b, now, h.host, n, "branch", None, Some(n), sha, owner));
    }
    Ok(rows)
}

/// The remote's branches (host `remote`), minus main, HEAD and the names in `skip` (a branch that is also local is listed once).
pub fn scan_remote(h: &HostScan, now: i64, skip: &[String], owner: &dyn Fn(&str) -> Option<String>) -> Result<Vec<BranchRow>, String> {
    let b = base(h)?;
    let lb = git(h.e, h.repo, &["for-each-ref", "--format=%(refname:short)%09%(objectname)", &format!("refs/remotes/{}", h.remote)]);
    let mut rows = vec![];
    let pre = format!("{}/", h.remote);
    for l in lb.out.lines() {
        let Some((n, sha)) = l.trim().split_once('\t') else { continue };
        let Some(br) = n.strip_prefix(&pre) else { continue };
        if br == "HEAD" || br == h.main || skip.iter().any(|s| s == br) {
            continue;
        }
        rows.push(describe(h, &b, now, "remote", n, "remote", None, Some(br), sha, owner));
    }
    Ok(rows)
}

pub fn ledger_owner(l: &Ledger, branch: &str) -> Option<String> {
    l.conn.query_row("SELECT actor FROM decisions WHERE kind='land' AND subject=? ORDER BY seq DESC LIMIT 1", [branch], |r| r.get::<_, String>(0)).ok()
}

fn fix_land(r: &BranchRow) -> String {
    match &r.branch {
        Some(b) => format!("gatetrain land {b}  (or gatetrain tidy --force)"),
        None => "gatetrain tidy --force".into(),
    }
}

/// The limits, checked over the rows of every host. Work that is landed or archived is never "idle" or "unlanded".
pub fn breaches(rows: &[BranchRow], lim: &Limits) -> Vec<Breach> {
    let mut out = vec![];
    let mut hosts: Vec<&str> = rows.iter().filter(|r| r.kind == "worktree").map(|r| r.host.as_str()).collect();
    hosts.sort();
    hosts.dedup();
    for h in hosts {
        let n = rows.iter().filter(|r| r.kind == "worktree" && r.host == h).count();
        if n > lim.max_worktrees {
            out.push(Breach { host: h.into(), item: h.into(), rule: "max_worktrees".into(), detail: format!("{n} worktrees (limit {})", lim.max_worktrees), fix: "gatetrain tidy --force".into() });
        }
    }
    for r in rows {
        let open = r.state != "landed" && r.state != "archived";
        if open && r.age_hours > lim.max_idle_days * 24.0 {
            out.push(Breach { host: r.host.clone(), item: r.name.clone(), rule: "max_idle_days".into(), detail: format!("idle {:.1} days (limit {})", r.age_hours / 24.0, lim.max_idle_days), fix: fix_land(r) });
        }
        if let (true, Some(u)) = (open, r.unlanded_hours) {
            if u > lim.max_unlanded_hours {
                out.push(Breach { host: r.host.clone(), item: r.name.clone(), rule: "max_unlanded_hours".into(), detail: format!("work not on main for {u:.1} h (limit {})", lim.max_unlanded_hours), fix: fix_land(r) });
            }
        }
        if let (true, Some(s)) = (r.kind == "worktree", r.size_bytes) {
            let gb = s as f64 / (1024.0 * 1024.0 * 1024.0);
            if gb > lim.max_worktree_gb {
                out.push(Breach { host: r.host.clone(), item: r.name.clone(), rule: "max_worktree_gb".into(), detail: format!("{gb:.1} GB (limit {})", lim.max_worktree_gb), fix: "gatetrain tidy --force".into() });
            }
        }
    }
    out
}

fn gb(b: Option<i64>) -> String {
    b.map(|x| format!("{:.1}G", x as f64 / 1_073_741_824.0)).unwrap_or("-".into())
}

pub fn table(rows: &[BranchRow], b: &[Breach]) -> String {
    let mut s = format!("{:<8} {:<34} {:<9} {:<9} {:<14} {:>5} {:>6} {:>5} {:>7} {:>8} {:>10}\n", "host", "name", "kind", "state", "owner", "ahead", "behind", "dirty", "size", "age_h", "unlanded_h");
    for r in rows {
        s.push_str(&format!(
            "{:<8} {:<34} {:<9} {:<9} {:<14} {:>5} {:>6} {:>5} {:>7} {:>8.1} {:>10}\n",
            r.host,
            r.name,
            r.kind,
            r.state,
            r.owner,
            r.ahead,
            r.behind,
            r.dirty,
            gb(r.size_bytes),
            r.age_hours,
            r.unlanded_hours.map(|u| format!("{u:.1}")).unwrap_or("-".into())
        ));
    }
    s.push_str("size: folder bytes, links not followed (robocopy /XJ on Windows, du -sk elsewhere); ahead/behind: against main; state: archived > landed (content on main by tree test) > stale > live\n");
    for x in b {
        s.push_str(&format!("BREACH {} {} {}: {} -> {}\n", x.host, x.rule, x.item, x.detail, x.fix));
    }
    if b.is_empty() {
        s.push_str("limits: all within bounds\n");
    }
    s
}

pub fn to_json(rows: &[BranchRow], b: &[Breach]) -> Value {
    json!({
        "rows": rows.iter().map(|r| json!({"host": r.host, "name": r.name, "kind": r.kind, "path": r.path, "branch": r.branch, "tip": r.tip, "size_bytes": r.size_bytes,
            "size_method": "folder bytes, links not followed (robocopy /XJ or du -sk)", "ahead": r.ahead, "behind": r.behind, "on_main": r.on_main, "dirty": r.dirty,
            "age_hours": r.age_hours, "unlanded_hours": r.unlanded_hours, "owner": r.owner, "state": r.state})).collect::<Vec<_>>(),
        "breaches": b.iter().map(|x| json!({"host": x.host, "item": x.item, "rule": x.rule, "detail": x.detail, "fix": x.fix})).collect::<Vec<_>>(),
    })
}
