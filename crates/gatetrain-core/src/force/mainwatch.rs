//! `gatetrain main-watch`: when main has a commit with no full chain yet, start the configured full-chain command for it (through a pushed
//! NON-main branch), ingest its steps into the ledger, list each red as a post-fix item with an owner, and run `--deploy-cmd` only for a
//! sha whose full chain is green (founder 2026-11-25: if a merge waits too long, force to main and post-fix after).
//! One pass per call; the CLI loops. State lives in the ledger: `chain-start`, `post-fix` and `deploy` rows of the decision log, and the
//! ingested run itself.
use super::cfg::{LandCfg, WatchCfg};
use super::exec::{git, Exec};
use crate::declog::{self, NewEntry};
use crate::history;
use crate::ledger::Ledger;
use crate::logfmt::{self, Ts};
use crate::runs;
use serde_json::json;

pub struct WatchArgs {
    pub repo: String,
    pub deploy_cmd: Option<String>,
    pub who: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostFix {
    pub sha: String,
    pub step: String,
    pub rc: i64,
    pub owner: String,
}

pub struct WatchOut {
    /// 0 fine; 1 the chain has reds (post-fix items); 2 cannot read main or nothing is configured to start a chain; 3 the chain command failed
    pub rc: i32,
    pub text: String,
    pub sha: String,
    pub started: bool,
    pub post_fix: Vec<PostFix>,
    pub deployed: bool,
}

fn has(l: &Ledger, kind: &str, subject: &str) -> bool {
    l.conn.query_row("SELECT 1 FROM decisions WHERE kind=? AND subject=? LIMIT 1", [kind, subject], |_| Ok(())).is_ok()
}

fn record(l: &mut Ledger, who: &str, kind: &str, subject: &str, outcome: &str, reason: &str, ev: serde_json::Value) {
    let _ = declog::append(l, &NewEntry { at: runs::now_iso(), actor: who.into(), kind: kind.into(), subject: Some(subject.into()), outcome: outcome.into(), reason: reason.into(), evidence: Some(ev), ..Default::default() });
}

/// The finished chain run of a commit: (run id, outcome).
fn finished(l: &Ledger, sha: &str) -> Option<(String, String)> {
    l.conn
        .query_row(
            "SELECT id, outcome FROM runs WHERE kind='chain' AND outcome IN ('green','red') AND length(commit_sha) >= 7 AND substr(?1,1,length(commit_sha)) = commit_sha ORDER BY started_at DESC LIMIT 1",
            [sha],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

fn owner_of(w: &WatchCfg, step: &str) -> String {
    w.owners.iter().find(|(re, _)| re.is_match(step)).map(|(_, o)| o.clone()).unwrap_or("unassigned".into())
}

fn fill(t: &str, branch: &str, sha: &str) -> String {
    t.replace("{branch}", branch).replace("{sha}", sha).replace("{short}", &sha[..sha.len().min(12)])
}

pub fn watch_once(e: &dyn Exec, land: &LandCfg, w: &WatchCfg, a: &WatchArgs, l: &mut Ledger, read_log: &dyn Fn(&str) -> Option<String>) -> WatchOut {
    let mut t = String::new();
    let mut o = WatchOut { rc: 0, text: String::new(), sha: String::new(), started: false, post_fix: vec![], deployed: false };
    macro_rules! say {
        ($($x:tt)*) => {{ t.push_str(&format!($($x)*)); t.push('\n'); }};
    }
    let remote = &land.remote;
    let f = git(e, &a.repo, &["fetch", "-q", remote]);
    if !f.ok() {
        say!("cannot fetch {remote}: {}", f.text().trim());
        o.rc = 2;
        o.text = t;
        return o;
    }
    let sha = git(e, &a.repo, &["rev-parse", "--verify", &format!("refs/remotes/{remote}/{}^{{commit}}", land.main)]).out.trim().to_string();
    if sha.is_empty() {
        say!("cannot read {remote}/{}", land.main);
        o.rc = 2;
        o.text = t;
        return o;
    }
    let short = sha[..sha.len().min(12)].to_string();
    o.sha = sha.clone();
    say!("main {short} ({remote}/{})", land.main);
    let mut run = finished(l, &sha);
    if run.is_some() {
        say!("  main already has a full chain");
    } else {
        let started = has(l, "chain-start", &sha);
        if !started {
            let Some(cmd) = &w.chain_cmd else {
                say!("  no chain: [mainwatch] chain_cmd is not configured, so none can be started");
                o.rc = 2;
                o.text = t;
                return o;
            };
            let branch = format!("{}{short}", w.branch_prefix);
            if branch == land.main || branch.is_empty() || w.branch_prefix.is_empty() {
                say!("  refusing to start: the chain branch {branch:?} could be main; set branch_prefix");
                o.rc = 2;
                o.text = t;
                return o;
            }
            let p = git(e, &a.repo, &["push", "-q", remote, &format!("{sha}:refs/heads/{branch}")]);
            if !p.ok() {
                say!("  cannot push {branch} to {remote}: {}", p.text().trim());
                o.rc = 3;
                o.text = t;
                return o;
            }
            let c = e.sh(&a.repo, &fill(cmd, &branch, &sha));
            if !c.ok() {
                say!("  the chain command failed (rc {}): {}", c.rc, c.text().lines().last().unwrap_or(""));
                o.rc = 3;
                o.text = t;
                return o;
            }
            record(l, &a.who, "chain-start", &sha, "started", &format!("full chain of {short} started on {branch}"), json!({"branch": branch, "cmd": cmd}));
            say!("  no full chain yet: started one on {branch} ({})", fill(cmd, &branch, &sha));
            o.started = true;
        } else if let Some(tpl) = &w.log_template {
            let path = fill(tpl, "", &sha);
            match read_log(&path) {
                None => say!("  chain started, log {path} not there yet: still running"),
                Some(text) => match logfmt::parse_log(&text) {
                    Err(er) => say!("  chain log {path} is unreadable: {er}"),
                    Ok(p) => {
                        let full = matches!(p.mode.as_deref(), None | Some("full"));
                        let outcome = history::classify(&p, &Ts::now_utc(), 30.0);
                        if !full {
                            say!("  chain log {path} is mode {}: not a full chain, not counted", p.mode.as_deref().unwrap_or("?"));
                        } else if outcome == "running" {
                            say!("  chain still running (log {path})");
                        } else if outcome == "stopped" {
                            say!("  chain log {path} went quiet before GATES_DONE: stopped; not counted");
                        } else {
                            let rid = format!("main-{short}");
                            match runs::ingest(l, "ledger", &rid, &p, outcome, Some("main-watch"), false, &runs::now_iso()) {
                                Ok(runs::Ingest::Done) => say!("  chain {outcome}: {} steps ingested as run {rid}", p.steps.len()),
                                Ok(runs::Ingest::Refused(m)) => say!("  ingest refused: {m}"),
                                Err(er) => say!("  ingest failed: {er}"),
                            }
                            run = finished(l, &sha);
                        }
                    }
                },
            }
        } else {
            say!("  chain started; no log_template configured, so it cannot be ingested here: still running");
        }
    }
    if let Some((rid, outcome)) = run.clone() {
        let reds: Vec<(String, i64)> = l
            .conn
            .prepare("SELECT name, exit_code FROM steps WHERE run_id=? AND exit_code IS NOT NULL AND exit_code != 0 ORDER BY name")
            .and_then(|mut s| s.query_map([&rid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map(|x| x.filter_map(|y| y.ok()).collect()))
            .unwrap_or_default();
        for (step, rc) in &reds {
            let owner = owner_of(w, step);
            let subj = format!("{short}:{step}");
            if !has(l, "post-fix", &subj) {
                record(l, &a.who, "post-fix", &subj, "open", &format!("{step} red on {short} (rc {rc}); fix after the force"), json!({"sha": sha, "step": step, "rc": rc, "owner": owner, "run": rid}));
            }
            o.post_fix.push(PostFix { sha: sha.clone(), step: step.clone(), rc: *rc, owner });
        }
        if outcome == "green" && reds.is_empty() {
            match &a.deploy_cmd {
                None => {}
                Some(_) if has(l, "deploy", &sha) => say!("  deploy: already run for {short}"),
                Some(cmd) => {
                    let c = e.sh(&a.repo, &fill(cmd, "", &sha));
                    let ok = c.ok();
                    record(l, &a.who, "deploy", &sha, if ok { "ok" } else { "failed" }, &format!("deploy of {short}"), json!({"cmd": cmd, "rc": c.rc}));
                    say!("  deploy {}: {}", if ok { "ran" } else { "FAILED" }, fill(cmd, "", &sha));
                    o.deployed = ok;
                    if !ok {
                        o.rc = 3;
                    }
                }
            }
        } else {
            say!("  {} red step(s): post-fix items (the deploy command does not run for a red sha)", reds.len());
            for p in &o.post_fix {
                say!("    post-fix {} step {} rc {} owner {}", short, p.step, p.rc, p.owner);
            }
            if !reds.is_empty() {
                o.rc = o.rc.max(1);
            }
        }
    } else if a.deploy_cmd.is_some() {
        say!("  deploy: waits for a green full chain of {short}");
    }
    o.text = t;
    o
}
