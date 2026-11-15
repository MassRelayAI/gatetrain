//! `tidy --apply`: run the plan class by class, every action logged, the first guard violation stops everything (L50).
use super::config::{TidyCfg, CLASSES};
use crate::config::Host;
use super::inventory::{is_cargo, Extra, Inventory, Item};
use crate::pycompat::{dumps, round2, J};
use super::ops::{guard, norm, Fatal, Ops, R};
use crate::pycompat::{fmt_f, fmt_g, fnmatchcase, shlex_quote, split_ws};
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
            let fb = if ["clean", "move", "remove", "remove-worktree"].contains(&it.kind.as_str()) { ops.free_gb(&it.key)? } else { None };
            let (after, status, note) = match do_item(&ops, cfg, it) {
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

pub fn do_item(ops: &Ops, cfg: &TidyCfg, it: &Item) -> R<Done> {
    match it.kind.as_str() {
        "clean" => clean_target(ops, cfg, it),
        "move" => {
            let Some(trash) = cfg.trash_dir.as_deref().filter(|t| !t.is_empty()) else {
                return Ok((it.bytes, "skipped", "not a cargo target and no trash_dir configured; left alone".into()));
            };
            ops.check_path(&it.key)?;
            let dest = ops.join(trash, &format!("{}-{}", norm(&it.key).rsplit('/').next().unwrap(), local_fmt(super::inventory::now_secs(), "%Y%m%d%H%M%S")));
            ops.run0(&if ops.win { format!("move {} {}", ops.q(&it.key), ops.q(&dest)) } else { format!("mv -- {} {}", ops.q(&it.key), ops.q(&dest)) })?;
            Ok((Some(0), "ok", format!("moved to {dest}")))
        }
        "remove" => {
            let ok = cfg.trash_dir.as_deref().filter(|t| !t.is_empty()).map(|t| norm(&it.key).starts_with(&format!("{}/", norm(t)))).unwrap_or(false);
            if !ok {
                return guard(format!("{} is not inside the configured recycle bin", it.key));
            }
            ops.remove_dir(&it.key)?;
            Ok((Some(0), "ok", "removed from the recycle bin".into()))
        }
        "remove-worktree" => remove_worktree(ops, cfg, it),
        "rm-container" => {
            let o = ops.run0(r#"docker ps --format "{{.Names}}""#)?;
            if split_ws(&o.out).contains(&it.key.as_str()) {
                return guard(format!("container {} is running", it.key));
            }
            if !it.key.starts_with(cfg.container_prefix.as_deref().filter(|p| !p.is_empty()).unwrap_or("\0")) {
                return guard(format!("container {} does not match the prefix", it.key));
            }
            let r = ops.run0(&format!("docker rm {}", shlex_quote(&it.key)))?;
            Ok((Some(0), if r.ok() { "ok" } else { "failed" }, "removed by exact name".into()))
        }
        "rm-image" => {
            if !cfg.image_patterns.iter().any(|p| fnmatchcase(&it.key, p)) {
                return guard(format!("image {} matches no configured pattern", it.key));
            }
            let used = ops.run0(r#"docker ps -a --format "{{.Image}}""#)?;
            if split_ws(&used.out).contains(&it.key.as_str()) {
                return guard(format!("image {} is used by a container", it.key));
            }
            let r = ops.run0(&format!("docker rmi {}", shlex_quote(&it.key)))?;
            Ok((Some(0), if r.ok() { "ok" } else { "failed" }, "removed by exact name".into()))
        }
        "prune-cache" => {
            let r = ops.run0(&format!("docker builder prune -f --filter until={}h", (cfg.cache_days * 24.0) as i64))?;
            Ok((Some(0), if r.ok() { "ok" } else { "failed" }, format!("build cache older than {} days", fmt_g(cfg.cache_days))))
        }
        k => guard(format!("unknown action {k}")),
    }
}

pub fn clean_target(ops: &Ops, cfg: &TidyCfg, it: &Item) -> R<Done> {
    let p = it.key.as_str();
    ops.check_path(p)?;
    let before = ops.size(p)?;
    let fb = if before.is_none() { ops.free_gb(p)? } else { None }; // sizing failed: fall back to the disk's free space (L51)
    let mp = match &cfg.main_checkout {
        Some(m) => format!("--manifest-path {} ", ops.q(&ops.join(m, "Cargo.toml"))),
        None => String::new(),
    };
    let cmd = format!("cargo clean {mp}--target-dir {}", ops.q(p)); // never -q: a quiet clean can refuse and say nothing (L43)
    ops.run0(&cmd)?;
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
            ops.run0(&cmd)?;
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
        ops.run0(&cmd)?;
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

pub fn remove_worktree(ops: &Ops, cfg: &TidyCfg, it: &Item) -> R<Done> {
    let p = it.key.as_str();
    let Extra::Worktree { branch: br, .. } = &it.extra else { return guard(format!("{p} is not a worktree item")) };
    let Some(main) = cfg.main_checkout.as_deref() else { return guard("worktree removal needs main_checkout in the config".into()) };
    ops.check_path(p)?;
    if ops.dirty(p)? != Some(false) {
        return guard(format!("{p} has uncommitted work"));
    }
    if ops.merged(br)? != Some(true) {
        return guard(format!("branch {br} is not on {}", cfg.main_branch));
    }
    let before = ops.snapshot_main()?;
    ops.unlink_all(p)?;
    ops.git(main, &format!("tag archive/{br} {}", ops.q(br)))?;
    ops.git(main, &format!("worktree remove {}", ops.q(p)))?;
    if ops.exists(p)? {
        return guard(format!("{p} still exists after git worktree remove"));
    }
    ops.git(main, &format!("branch -D {}", ops.q(br)))?;
    ops.assert_main(before)?;
    Ok((Some(0), "ok", format!("links unlinked, tagged archive/{br}, worktree and branch removed, main checkout unchanged")))
}
