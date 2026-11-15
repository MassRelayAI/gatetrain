//! `tidy --apply`: run the plan class by class, every action logged, the first guard violation stops everything (L50).
use super::config::{TidyCfg, CLASSES};
use crate::config::Host;
use super::inventory::{is_cargo, Extra, Inventory, Item};
use crate::pycompat::{dumps, round2, J};
use super::ops::{guard, safe_branch, safe_container, safe_image, safe_path, Fatal, Ops, R};
use crate::pycompat::{fmt_f, fmt_g, fnmatchcase, repr_str, split_ws};
use crate::runner::Runner;
use chrono::TimeZone;
use std::io::Write;

pub const CARGO_TAG_SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55";

pub fn cargo_tag() -> String {
    format!("{CARGO_TAG_SIG}\n# This file is a cache directory tag created by cargo.\n# For information about cache directory tags see https://bford.info/cachedir/\n")
}

fn local_fmt(secs: f64, fmt: &str) -> String {
    match chrono::Local.timestamp_opt(secs as i64, 0).single() {
        Some(t) => t.format(fmt).to_string(),
        None => String::new(),
    }
}

/// Append one record to the action log (append-only; keys sorted, Python `json.dumps` separators).
pub fn log_action(path: &str, rec: &J) -> R<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| Fatal::Other(format!("cannot open the action log {path}: {e}")))?;
    f.write_all(format!("{}\n", dumps(rec, None, true)).as_bytes()).map_err(|e| Fatal::Other(format!("cannot write the action log {path}: {e}")))?;
    Ok(())
}

/// Run the plan class by class. Returns (done, failed); a guard violation is logged, then returned, stopping everything.
#[allow(clippy::too_many_arguments)]
pub fn apply_plan(host: &Host, runner: &dyn Runner, cfg: &TidyCfg, inv: &Inventory, classes: &[String], now: Option<f64>, log: Option<&str>, attended: bool) -> R<(usize, usize)> {
    let ops = Ops::new(host, runner, cfg);
    let log = log.unwrap_or(&cfg.log);
    let (mut done, mut failed) = (0, 0);
    for cls in CLASSES {
        if !classes.iter().any(|c| c == cls) {
            continue;
        }
        for it in inv.items.iter().filter(|i| i.cls == cls && (attended || !i.attended_only)) {
            let ts = local_fmt(now.unwrap_or_else(super::inventory::now_secs), "%Y-%m-%dT%H:%M:%S%z");
            let mut rec: Vec<(String, J)> = vec![
                ("ts".into(), J::s(ts)),
                ("host".into(), J::s(&host.name)),
                ("class".into(), J::s(cls)),
                ("action".into(), J::s(&it.kind)),
                ("item".into(), J::s(&it.key)),
                ("before_bytes".into(), J::opt_int(it.bytes)),
                ("after_bytes".into(), J::Null),
            ];
            let mut intent = rec.clone();
            intent.push(("status".into(), J::s("intent")));
            intent.push(("note".into(), J::s("about to run")));
            log_action(log, &J::Obj(intent))?; // written BEFORE the action: a crash mid-action leaves a trace
            let fb = if ["clean", "move", "remove", "remove-worktree"].contains(&it.kind.as_str()) { ops.free_gb(&it.key)? } else { None };
            let (after, status, note) = match do_item(&ops, cfg, it, now) {
                Ok(x) => x,
                Err(Fatal::Guard(msg)) => {
                    set(&mut rec, "status", J::s("guard-violation"));
                    set(&mut rec, "note", J::s(&msg));
                    log_action(log, &J::Obj(rec))?;
                    return Err(Fatal::Guard(msg));
                }
                Err(e) => return Err(e),
            };
            set(&mut rec, "after_bytes", J::opt_int(after));
            set(&mut rec, "status", J::s(status));
            set(&mut rec, "note", J::s(&note));
            if let Some(fb) = fb {
                set(&mut rec, "free_gb_before", J::Float(round2(fb)));
                set(&mut rec, "free_gb_after", ops.free_gb(&it.key)?.map(J::Float).unwrap_or(J::Null));
            }
            log_action(log, &J::Obj(rec))?;
            if status == "ok" {
                done += 1;
            } else {
                failed += 1;
            }
        }
    }
    Ok((done, failed))
}

fn set(rec: &mut Vec<(String, J)>, k: &str, v: J) {
    if let Some(slot) = rec.iter_mut().find(|(x, _)| x == k) {
        slot.1 = v;
    } else {
        rec.push((k.into(), v));
    }
}

type Done = (Option<i64>, &'static str, String);

pub fn cache_hours(cfg: &TidyCfg) -> i64 {
    ((cfg.cache_days * 24.0).ceil() as i64).max(1) // a window under an hour is an hour, never `until=0h` (= everything)
}

pub fn do_item(ops: &Ops, cfg: &TidyCfg, it: &Item, now: Option<f64>) -> R<Done> {
    match it.kind.as_str() {
        "clean" => clean_target(ops, cfg, it),
        "move" => {
            let Some(trash) = cfg.trash_dir.as_deref().filter(|t| !t.is_empty()) else {
                return Ok((it.bytes, "skipped", "not a cargo target and no trash_dir configured; left alone".into()));
            };
            ops.check_path(&it.key)?;
            let name = it.key.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
            let dest = ops.join(trash, &format!("{}-{}", name, local_fmt(now.unwrap_or_else(super::inventory::now_secs), "%Y%m%d%H%M%S")));
            safe_path(&dest, Some(trash))?;
            if ops.exists(&dest)? != Some(false) {
                return Ok((it.bytes, "failed", format!("{dest} exists or cannot be probed; not moving onto it")));
            }
            let c = ops.run(
                &if ops.win { format!("move {} {}", ops.q(&it.key)?, ops.q(&dest)?) } else { format!("mv -- {} {}", ops.q(&it.key)?, ops.q(&dest)?) },
                Some(cfg.remove_timeout_s),
            )?;
            let Some(rc) = c.rc else {
                return Ok((None, "stopped", format!("move did not finish in {}s; not counted as done", fmt_g(cfg.remove_timeout_s))));
            };
            let (src, dst) = (ops.exists(&it.key)?, ops.exists(&dest)?);
            if rc != 0 || src != Some(false) || dst != Some(true) {
                return Ok((it.bytes, "failed", format!("move did not complete (rc {rc}; source gone: {}; destination exists: {})", py_opt_bool(src), py_opt_bool(dst))));
            }
            Ok((Some(0), "ok", format!("moved to {dest}")))
        }
        "remove" => {
            if cfg.trash_dir.as_deref().filter(|t| !t.is_empty()).is_none() {
                return guard(format!("{} is not inside a configured recycle bin", it.key));
            }
            ops.check_trash_entry(&it.key)?;
            let (status, note) = ops.remove_dir(&it.key)?;
            Ok((if status == "ok" { Some(0) } else { None }, status, note))
        }
        "remove-worktree" => remove_worktree(ops, cfg, it),
        "rm-container" => {
            let o = ops.run0(r#"docker ps --format "{{.Names}}""#)?;
            if split_ws(&o.out).contains(&it.key.as_str()) {
                return guard(format!("container {} is running", it.key));
            }
            if !safe_container(&it.key) || !it.key.starts_with(cfg.container_prefix.as_deref().filter(|p| !p.is_empty()).unwrap_or("\0")) {
                return guard(format!("container {} does not match the prefix", it.key));
            }
            let r = ops.run_argv(&["docker".into(), "rm".into(), it.key.clone()], None)?;
            Ok((Some(0), if r.ok() { "ok" } else { "failed" }, "removed by exact name".into()))
        }
        "rm-image" => {
            if !safe_image(&it.key) || !cfg.image_patterns.iter().any(|p| fnmatchcase(&it.key, p)) {
                return guard(format!("image {} matches no configured pattern", it.key));
            }
            let used = ops.run0(r#"docker ps -a --format "{{.Image}}""#)?;
            if split_ws(&used.out).contains(&it.key.as_str()) {
                return guard(format!("image {} is used by a container", it.key));
            }
            let r = ops.run_argv(&["docker".into(), "rmi".into(), it.key.clone()], None)?;
            Ok((Some(0), if r.ok() { "ok" } else { "failed" }, "removed by exact name".into()))
        }
        "prune-cache" => {
            let argv: Vec<String> = ["docker", "builder", "prune", "-f", "--filter"].iter().map(|s| s.to_string()).chain([format!("until={}h", cache_hours(cfg))]).collect();
            let r = ops.run_argv(&argv, None)?;
            Ok((Some(0), if r.ok() { "ok" } else { "failed" }, format!("build cache older than {} days", fmt_g(cfg.cache_days))))
        }
        k => guard(format!("unknown action {k}")),
    }
}

fn py_opt_bool(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "True",
        Some(false) => "False",
        None => "None",
    }
}

pub fn clean_target(ops: &Ops, cfg: &TidyCfg, it: &Item) -> R<Done> {
    let p = it.key.as_str();
    ops.check_path(p)?;
    if ops.is_link(p)? != Some(false) {
        return Ok((it.bytes, "failed", "is a link (or link state unknown); cargo clean not run".into()));
    }
    if !is_cargo(&ops.listdir(p)?) {
        // re-check at the moment of action: the plan may be minutes old and sizing may have failed
        return Ok((it.bytes, "failed", "no longer looks like a cargo target (no debug/release with tmp); cargo clean not run".into()));
    }
    let before = ops.size(p)?;
    let fb = if before.is_none() { ops.free_gb(p)? } else { None }; // sizing failed: fall back to the disk's free space (L51)
    let mut cmd: Vec<String> = vec!["cargo".into(), "clean".into()];
    if let Some(m) = &cfg.main_checkout {
        cmd.push("--manifest-path".into());
        cmd.push(ops.join(m, "Cargo.toml"));
    }
    cmd.push("--target-dir".into());
    cmd.push(p.to_string()); // never -q: a quiet clean can refuse and say nothing (L43)
    let c = ops.run_argv(&cmd, Some(cfg.remove_timeout_s))?;
    if c.rc.is_none() {
        return Ok((None, "stopped", format!("cargo clean did not finish in {}s; not counted as done", fmt_g(cfg.remove_timeout_s))));
    }
    if !c.ok() {
        return Ok((ops.size(p)?, "failed", format!("cargo clean exited {}", c.rc.unwrap_or(-1))));
    }
    let mut after = ops.size(p)?;
    let mut note = "cargo clean".to_string();
    let tag = || cargo_tag();
    if before.is_none() {
        let mut fa = ops.free_gb(p)?;
        let (Some(fb), true) = (fb, fa.is_some()) else {
            return Ok((after, "ok", "cargo clean ran; freed: unknown (size and free space could not be measured)".into()));
        };
        let mut d = Some(fa.unwrap() - fb);
        note = String::new();
        if d.unwrap() <= 0.001 && !ops.listdir(p)?.iter().any(|n| n == "CACHEDIR.TAG") {
            ops.runner.put_file(&ops.join(p, "CACHEDIR.TAG"), &tag())?;
            let c2 = ops.run_argv(&cmd, Some(cfg.remove_timeout_s))?;
            if c2.rc.is_none() {
                return Ok((None, "stopped", format!("the cargo clean rerun did not finish in {}s; not counted as done", fmt_g(cfg.remove_timeout_s))));
            }
            if !c2.ok() {
                return Ok((after, "failed", format!("the cargo clean rerun exited {}", c2.rc.unwrap_or(-1))));
            }
            fa = ops.free_gb(p)?;
            d = fa.map(|fa| fa - fb);
            note = "cargo refused silently (no CACHEDIR.TAG); wrote cargo's standard tag and re-ran; ".into();
        }
        let Some(d) = d else { return Ok((after, "ok", format!("{note}cargo clean ran; freed: unknown"))) };
        if d <= 0.001 {
            return Ok((after, "failed", format!("{note}free space did not grow after cargo clean (df)")));
        }
        return Ok((after, "ok", format!("{note}cargo clean; free space grew by about {} GB (df)", fmt_f(d, 1))));
    }
    let before_v = before.unwrap();
    if before_v != 0 && after.is_none() {
        return Ok((after, "ok", "cargo clean ran; freed: unknown (size after could not be measured)".into()));
    }
    let names = if after.map(|a| a != 0).unwrap_or(false) { ops.listdir(p)? } else { vec![] };
    if before_v != 0 && after.unwrap_or(0) as f64 >= before_v as f64 * 0.99 && is_cargo(&names) && !names.iter().any(|n| n == "CACHEDIR.TAG") {
        ops.runner.put_file(&ops.join(p, "CACHEDIR.TAG"), &tag())?;
        let c2 = ops.run_argv(&cmd, Some(cfg.remove_timeout_s))?;
        if c2.rc.is_none() {
            return Ok((None, "stopped", format!("the cargo clean rerun did not finish in {}s; not counted as done", fmt_g(cfg.remove_timeout_s))));
        }
        if !c2.ok() {
            return Ok((after, "failed", format!("the cargo clean rerun exited {}", c2.rc.unwrap_or(-1))));
        }
        after = ops.size(p)?;
        note = "cargo refused silently (no CACHEDIR.TAG); wrote cargo's standard tag and re-ran".into();
        if after.is_none() {
            return Ok((after, "ok", format!("{note}; freed: unknown")));
        }
    }
    if before_v != 0 && after.unwrap_or(0) as f64 >= before_v as f64 * 0.99 {
        return Ok((after, "failed", "cargo clean freed nothing".into()));
    }
    Ok((after, "ok", note))
}

/// Order: resolve the branch to one sha, archive THAT sha under a tag and verify it, unlink links, remove the worktree, then delete
/// the branch only if it still points at that sha. Any step that cannot be proven stops here with the branch intact.
pub fn remove_worktree(ops: &Ops, cfg: &TidyCfg, it: &Item) -> R<Done> {
    let p = it.key.as_str();
    let Extra::Worktree { branch: br, .. } = &it.extra else { return guard(format!("{p} is not a worktree item")) };
    let Some(m) = cfg.main_checkout.as_deref() else { return guard("worktree removal needs main_checkout in the config".into()) };
    if !safe_branch(br) {
        return guard(format!("branch {} has an unsafe name; not removing", repr_str(br)));
    }
    ops.check_path(p)?;
    if ops.is_link(p)? != Some(false) {
        return guard(format!("{p} is a link (or cannot be probed); not removing a worktree through it"));
    }
    if ops.dirty(p)? != Some(false) {
        return guard(format!("{p} has uncommitted work"));
    }
    let r = ops.git(m, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{br}^{{commit}}")])?;
    let sha = crate::pycompat::strip(&r.out).to_string();
    if !r.ok() || sha.is_empty() || sha.contains(char::is_whitespace) {
        return guard(format!("cannot resolve branch {br} to one commit"));
    }
    if ops.merged_sha(&sha)? != Some(true) {
        // proved on the very commit that is archived and deleted
        return guard(format!("branch {br} ({sha}) is not on {}", cfg.main_branch));
    }
    let before = ops.snapshot_main()?;
    let mut tag = format!("archive/{br}");
    let mut ex = ops.git(m, &["rev-parse", "--verify", "--quiet", &format!("refs/tags/{tag}^{{commit}}")])?;
    if ex.ok() && crate::pycompat::strip(&ex.out) != sha {
        tag = format!("{tag}-{}", &sha[..sha.len().min(8)]); // that name belongs to another commit: never overwrite or reuse it
        ex = ops.git(m, &["rev-parse", "--verify", "--quiet", &format!("refs/tags/{tag}^{{commit}}")])?;
    }
    if !(ex.ok() && crate::pycompat::strip(&ex.out) == sha) {
        let t = ops.git(m, &["tag", &tag, &sha])?;
        if !t.ok() {
            return guard(format!("could not create the archive tag {tag} for {br}; branch and worktree kept"));
        }
    }
    let got = ops.git(m, &["rev-parse", "--verify", "--quiet", &format!("refs/tags/{tag}^{{commit}}")])?;
    if !got.ok() || crate::pycompat::strip(&got.out) != sha {
        return guard(format!("archive tag {tag} does not point at {sha}; branch and worktree kept"));
    }
    // the folder goes by a removal that never follows a link (`git worktree remove` may), links first and their targets checked after;
    // git is then told the folder is gone
    let (status, note) = ops.remove_dir(p)?;
    if status != "ok" {
        return Ok((None, "stopped", format!("{note}; branch {br} kept, archived as {tag}")));
    }
    let pr = ops.git_to(m, &["worktree", "prune"], Some(cfg.remove_timeout_s))?;
    if !pr.ok() {
        return guard(format!("git worktree prune failed after removing {p}; branch {br} kept, archived as {tag}"));
    }
    let d = ops.git(m, &["update-ref", "-d", &format!("refs/heads/{br}"), &sha])?;
    if !d.ok() {
        return guard(format!("branch {br} no longer points at {sha} (or could not be deleted); it is kept, archived as {tag}"));
    }
    ops.assert_main(before)?;
    Ok((Some(0), "ok", format!("links unlinked, tagged {tag}, worktree and branch removed, main checkout unchanged")))
}
