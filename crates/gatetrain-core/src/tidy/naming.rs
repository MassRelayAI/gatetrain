//! The naming report of tidy: flags folders whose names break the convention. It only reports; nothing is renamed or removed, and
//! the exit code of tidy is not affected. Local hosts only (it reads the folders and runs `git` itself).
//!
//! For every direct child folder of a configured root that is not on the `exempt` list (compared by folder name, case-insensitive):
//! (a) a git worktree must be named `<repo>-wt-<lane>`, `<repo>` being the folder name of its main clone;
//! (b) no folder may start with a bare `wt-`, `kk-`, `tmp-` or `tmp_` (git or not);
//! (c) a main clone's folder name must equal basename(remote url without `.git`), the remote being `origin`, else the first one.
use super::config::TidyCfg;
use std::path::Path;
use std::process::Command;

/// One folder that breaks the convention and why (a folder can break more than one rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub reasons: Vec<String>,
}

const BARE: [&str; 4] = ["wt-", "kk-", "tmp-", "tmp_"];

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn folder_of(p: &str) -> Option<String> {
    Path::new(p.trim_end_matches(['/', '\\'])).file_name().map(|n| n.to_string_lossy().into_owned())
}

/// basename of a remote url without a trailing `/` and `.git` (`https://h/o/app.git`, `git@h:o/app.git`, `/srv/app.git`).
fn remote_basename(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches(['/', '\\']);
    let u = u.strip_suffix(".git").unwrap_or(u);
    let b = u.rsplit(['/', '\\', ':']).next()?;
    (!b.is_empty()).then(|| b.to_string())
}

fn remote_url(dir: &Path) -> Option<String> {
    let remotes = git_out(dir, &["remote"])?;
    let names: Vec<&str> = remotes.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let name = if names.contains(&"origin") { "origin" } else { *names.first()? };
    git_out(dir, &["remote", "get-url", name])
}

pub fn scan(roots: &[String], exempt: &[String]) -> Vec<Finding> {
    let exempt: Vec<String> = exempt.iter().map(|e| e.to_lowercase()).collect();
    let mut out = vec![];
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        let mut kids: Vec<_> = rd.filter_map(|e| e.ok()).filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).collect();
        kids.sort_by_key(|e| e.file_name());
        for e in kids {
            let name = e.file_name().to_string_lossy().into_owned();
            if exempt.contains(&name.to_lowercase()) {
                continue;
            }
            let dir = e.path();
            let mut reasons: Vec<String> = vec![];
            let lower = name.to_lowercase();
            if let Some(p) = BARE.iter().find(|p| lower.starts_with(**p)) {
                reasons.push(format!("bare prefix `{p}`"));
            }
            let dotgit = dir.join(".git");
            if dotgit.is_file() {
                // a linked worktree: its main clone's folder gives the repo name
                if let Some(common) = git_out(&dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
                    let main = Path::new(&common).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    if let Some(repo) = folder_of(&main) {
                        let want = format!("{}-wt-", repo.to_lowercase());
                        if !(lower.starts_with(&want) && lower.len() > want.len()) {
                            reasons.push(format!("worktree of `{repo}` must be named `{repo}-wt-<lane>`"));
                        }
                    }
                }
            } else if dotgit.is_dir() {
                if let Some(base) = remote_url(&dir).as_deref().and_then(remote_basename) {
                    if !base.eq_ignore_ascii_case(&name) {
                        reasons.push(format!("clone folder differs from its remote name `{base}`"));
                    }
                }
            }
            if !reasons.is_empty() {
                out.push(Finding { path: dir.to_string_lossy().replace('\\', "/"), reasons });
            }
        }
    }
    out
}

/// The text block for a dry run or an apply; empty when no `[tidy.naming]` roots are configured.
pub fn report_block(t: &TidyCfg) -> String {
    if t.naming_roots.is_empty() {
        return String::new();
    }
    let f = scan(&t.naming_roots, &t.naming_exempt);
    let mut l = vec![format!("naming report ({} flagged under {}; flag only, nothing is renamed)", f.len(), t.naming_roots.join(", "))];
    l.extend(f.iter().map(|x| format!("  {}: {}", x.path, x.reasons.join("; "))));
    l.join("\n")
}
