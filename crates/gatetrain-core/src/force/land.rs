//! `gatetrain land <branch>`: merge onto the remote main tip in a scratch worktree, run the cheap checks, push to main (fetch, re-merge,
//! retry; never force), record the landing in the decision log, clean the scratch worktree.
//!
//! Two modes, chosen by a path classifier in the config and never by the caller's wish (founder 2026-11-25T16:40):
//! FAST (cheap checks, post-fix on main) and SAFE (the configured full gates for the area, and a recorded independent-read verdict for
//! the branch tip must exist BEFORE it lands). A path that matches no rule is SAFE: the classifier fails closed.
use super::cfg::{LandCfg, Mode};
use super::exec::{git, Exec};
use crate::declog::{self, NewEntry};
use crate::ledger::Ledger;
use serde_json::json;

pub const NO_RULE: &str = "(no rule: fail closed)";

#[derive(Debug, Clone, PartialEq)]
pub struct FileClass {
    pub file: String,
    pub mode: Mode,
    /// the glob of the rule that decided it, or [`NO_RULE`]
    pub rule: String,
}

/// The mode of a change and of each of its files. A file's mode: SAFE when any rule that matches it says SAFE or none matches it, else
/// FAST. The change is SAFE when any file is.
pub fn classify(cfg: &LandCfg, files: &[String]) -> (Mode, Vec<FileClass>) {
    let mut rows = vec![];
    for f in files {
        let n = f.replace('\\', "/");
        let hits: Vec<&super::cfg::Rule> = cfg.rules.iter().filter(|r| r.re.is_match(&n)).collect();
        let row = match hits.iter().find(|r| r.mode == Mode::Safe).or(hits.first()) {
            Some(r) => FileClass { file: f.clone(), mode: r.mode, rule: r.glob.clone() },
            None => FileClass { file: f.clone(), mode: Mode::Safe, rule: NO_RULE.into() },
        };
        rows.push(row);
    }
    let mode = if rows.iter().any(|r| r.mode == Mode::Safe) { Mode::Safe } else { Mode::Fast };
    (mode, rows)
}

/// Record an independent read of one commit: `pass` or `fail`. The latest verdict of a commit counts.
pub fn record_verdict(l: &mut Ledger, sha: &str, by: &str, pass: bool, note: &str) -> crate::Result<()> {
    declog::append(
        l,
        &NewEntry {
            at: crate::runs::now_iso(),
            actor: by.into(),
            kind: "verdict".into(),
            subject: Some(sha.into()),
            outcome: if pass { "pass" } else { "fail" }.into(),
            reason: note.into(),
            ..Default::default()
        },
    )?;
    Ok(())
}

/// The reader of the latest verdict of `sha` when it is a pass.
pub fn verdict_for(l: &Ledger, sha: &str) -> Option<String> {
    let r: Option<(String, String)> = l
        .conn
        .query_row("SELECT outcome, actor FROM decisions WHERE kind='verdict' AND subject=? ORDER BY seq DESC LIMIT 1", [sha], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok();
    r.filter(|(o, _)| o == "pass").map(|(_, a)| a)
}

pub struct LandArgs {
    pub repo: String,
    pub branch: String,
    pub plan: bool,
    pub who: String,
}

pub struct LandOut {
    /// 0 landed or planned; 2 cannot read the repository or the branch; 4 refused (a missing item is named); 5 conflict; 6 a check or
    /// gate is red; 7 the push never went through
    pub rc: i32,
    pub text: String,
    pub merged: Option<String>,
}

fn out(rc: i32, text: String) -> LandOut {
    LandOut { rc, text, merged: None }
}

/// Plain path characters only: a name with a space, `$`, a quote, a backtick, `(`, `&`, `%` ... is not passed to a shell.
fn unsafe_name(n: &str) -> bool {
    n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || "_./@+=:,-".contains(c))
}

fn quote(f: &str) -> String {
    format!("\"{f}\"")
}

/// Remove the scratch worktree we made (one exact path, named `land-*`). The folder goes by the caller's `remover`, which must be link-safe (the
/// checks may have created a node_modules junction, and `git worktree remove` can follow one: it once emptied a main checkout); git is then told the
/// folder is gone. Never a prune, never a recursive delete of ours.
fn clean(e: &dyn Exec, repo: &str, path: &str, remover: &dyn Fn(&str) -> Result<(), String>) -> Result<(), String> {
    let leaf = path.replace('\\', "/").rsplit('/').next().unwrap_or("").to_string();
    if !leaf.starts_with("land-") {
        return Err(format!("not removing {path}: it is not a land scratch worktree"));
    }
    remover(path).map_err(|m| format!("scratch worktree {path} left in place (the link-safe removal said: {m}); remove it with `gatetrain tidy --force`"))?;
    let o = git(e, repo, &["worktree", "remove", "--force", path]);
    if o.ok() {
        Ok(())
    } else {
        Err(format!("scratch worktree {path} could not be removed: {}", o.text().trim()))
    }
}

/// `remover` removes a folder without ever following a link in it (tidy's link-safe removal; see `tidyforce::ops_remover`).
pub fn land(e: &dyn Exec, cfg: &LandCfg, a: &LandArgs, l: Option<&mut Ledger>, remover: &dyn Fn(&str) -> Result<(), String>) -> LandOut {
    let mut text = String::new();
    let remote = &cfg.remote;
    let f = git(e, &a.repo, &["fetch", "-q", remote]);
    if !f.ok() {
        return out(2, format!("cannot fetch {remote}: {}", f.text().trim()));
    }
    let mainref = format!("refs/remotes/{remote}/{}", cfg.main);
    let tip = ["refs/heads/", &format!("refs/remotes/{remote}/")]
        .iter()
        .map(|p| git(e, &a.repo, &["rev-parse", "--verify", "--quiet", &format!("{p}{}^{{commit}}", a.branch)]))
        .find(|o| o.ok() && !o.out.trim().is_empty())
        .map(|o| o.out.trim().to_string());
    let Some(tip) = tip else { return out(2, format!("branch {} not found locally or on {remote}", a.branch)) };
    if git(e, &a.repo, &["merge-base", "--is-ancestor", &tip, &mainref]).ok() {
        return out(0, format!("{} ({tip}) is already on {remote}/{}; nothing to land", a.branch, cfg.main));
    }
    let d = git(e, &a.repo, &["diff", "--name-only", "--no-renames", &format!("{mainref}...{tip}")]);
    if !d.ok() {
        return out(2, format!("cannot list the changed files: {}", d.text().trim()));
    }
    let files: Vec<String> = d.out.lines().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    let (mode, rows) = classify(cfg, &files);
    say(&mut text, format!("land {} ({tip}) onto {remote}/{}", a.branch, cfg.main));
    say(&mut text, format!("mode: {}", mode.word()));
    for r in &rows {
        say(&mut text, format!("  {:<4} {}  <- {}", r.mode.word(), r.file, r.rule));
    }
    if a.plan {
        return out(0, text);
    }
    let mut gates: Vec<String> = vec![];
    if mode == Mode::Safe {
        for r in rows.iter().filter(|r| r.mode == Mode::Safe) {
            for g in cfg.rules.iter().filter(|c| c.glob == r.rule).flat_map(|c| c.gates.iter()) {
                if !gates.contains(g) {
                    gates.push(g.clone());
                }
            }
        }
        for g in &cfg.safe_gates {
            if !gates.contains(g) {
                gates.push(g.clone());
            }
        }
        let mut missing = vec![];
        if !l.as_deref().and_then(|l| verdict_for(l, &tip)).is_some() {
            missing.push(format!("missing: independent-read verdict for {tip} (record it with `gatetrain verdict {tip} --by <reader> --pass`)"));
        }
        if gates.is_empty() {
            missing.push("missing: a full gate for this area (set `gates` on its [[land.rules]] or `safe_gates` under [land])".into());
        }
        if !missing.is_empty() {
            say(&mut text, format!("REFUSED: a SAFE change does not land without its proof\n{}", missing.join("\n")));
            return out(4, text);
        }
    }
    // a name that reaches a shell line is data, never syntax: a file or branch name with anything but plain path characters is refused when a
    // check uses it (`{files}`, `{branch}`); the checks are the only gate a FAST change gets, so nothing is built from an odd name
    let uses = |ph: &str| cfg.checks.iter().chain(gates.iter()).any(|c| c.contains(ph));
    let mut odd: Vec<String> = vec![];
    if uses("{files}") {
        odd.extend(files.iter().filter(|f| unsafe_name(f)).cloned());
    }
    if uses("{branch}") && unsafe_name(&a.branch) {
        odd.push(a.branch.clone());
    }
    if !odd.is_empty() {
        say(&mut text, format!("REFUSED: unsafe name(s) would reach a shell line: {}
rename them, or run the checks by hand and land with checks that do not use {{files}} or {{branch}}", odd.join(", ")));
        return out(4, text);
    }
    // the scratch worktree
    let base = cfg.scratch_dir.clone().unwrap_or_else(|| {
        let r = a.repo.replace('\\', "/");
        r.trim_end_matches('/').rsplit_once('/').map(|x| x.0.to_string()).unwrap_or(".".into())
    });
    let safe: String = a.branch.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let scratch = format!("{}/land-{safe}-{}", base.trim_end_matches('/'), chrono::Utc::now().timestamp());
    let add = git(e, &a.repo, &["worktree", "add", "--detach", &scratch, &mainref]);
    if !add.ok() {
        return out(2, format!("{text}cannot create the scratch worktree {scratch}: {}", add.text().trim()));
    }
    let mut result = (6, String::new(), None::<(String, Vec<(String, i32)>)>);
    'attempts: for attempt in 0..=cfg.retries {
        if attempt > 0 {
            say(&mut text, format!("push rejected: fetching {remote} and merging again (attempt {})", attempt + 1));
            if !git(e, &a.repo, &["fetch", "-q", remote]).ok() || !git(e, &scratch, &["checkout", "-q", "--detach", &mainref]).ok() {
                result = (7, "cannot refresh the scratch worktree onto the new main".into(), None);
                break;
            }
        }
        let m = git(e, &scratch, &["merge", "--no-ff", "--no-edit", "-m", &format!("land {}", a.branch), &tip]);
        if !m.ok() {
            let _ = git(e, &scratch, &["merge", "--abort"]);
            result = (5, format!("merging {} onto {remote}/{} conflicts: {}", a.branch, cfg.main, m.text().trim()), None);
            break;
        }
        let list = format!("-- {}", files.iter().map(|x| quote(x)).collect::<Vec<_>>().join(" "));
        let mut ran: Vec<(String, i32)> = vec![];
        for c in cfg.checks.iter().chain(gates.iter()) {
            let cmd = c.replace("{files}", &list).replace("{branch}", &a.branch).replace("{sha}", &tip);
            let o = e.sh(&scratch, &cmd);
            ran.push((c.clone(), o.rc));
            if !o.ok() {
                let full = o.text();
                let tail: Vec<&str> = full.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
                result = (6, format!("check red (rc {}): {c}\n{}", o.rc, tail.join("\n")), None);
                break 'attempts;
            }
        }
        let merged = git(e, &scratch, &["rev-parse", "HEAD"]).out.trim().to_string();
        let p = git(e, &scratch, &["push", remote, &format!("HEAD:refs/heads/{}", cfg.main)]);
        if p.ok() {
            result = (0, String::new(), Some((merged, ran)));
            break;
        }
        result = (7, format!("push to {remote}/{} failed: {}", cfg.main, p.text().trim()), None);
    }
    if let Err(m) = clean(e, &a.repo, &scratch, remover) {
        say(&mut text, m);
    }
    let (rc, msg, landed) = result;
    if !msg.is_empty() {
        say(&mut text, msg);
    }
    let Some((merged, ran)) = landed else { return out(rc, text) };
    say(&mut text, format!("landed {} as {merged} on {remote}/{}; post-fix on main: {}", a.branch, cfg.main, if mode == Mode::Safe { "none expected (proved first)" } else { "run the full chain on it (gatetrain main-watch)" }));
    if let Some(l) = l {
        let ev = json!({"branch": a.branch, "tip": tip, "merged": merged, "mode": mode.word(), "remote": remote, "main": cfg.main,
            "files": files.len(), "checks": ran.iter().map(|(c, rc)| json!({"cmd": c, "rc": rc})).collect::<Vec<_>>()});
        let r = declog::append(
            l,
            &NewEntry {
                at: crate::runs::now_iso(),
                actor: a.who.clone(),
                kind: "land".into(),
                subject: Some(a.branch.clone()),
                rule: Some(format!("land.{}", mode.word().to_lowercase())),
                evidence: Some(ev),
                outcome: "landed".into(),
                reason: format!("{} land of {}", mode.word(), a.branch),
                ..Default::default()
            },
        );
        if let Err(e) = r {
            say(&mut text, format!("WARNING: landed, but the ledger record failed: {e}"));
        }
    } else {
        say(&mut text, "note: no ledger given; the landing is not recorded".into());
    }
    LandOut { rc: 0, text, merged: Some(merged) }
}

fn say(t: &mut String, s: String) {
    t.push_str(&s);
    t.push('\n');
}
