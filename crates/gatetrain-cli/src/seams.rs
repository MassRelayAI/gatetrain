//! The real seams of the batch engine (the holder's lock, the memory, disk and doctor admission, the close environment), built once
//! from `gatetrain.toml` and `hosts.toml`. `batch` and `tick` both use this, so a tick advances a batch exactly as `batch advance` does.
use anyhow::{Context, Result};
use gatetrain_core::batchcfg::BatchConfig;
use gatetrain_core::config;
use gatetrain_core::hostlock::{MemoryAdmission, RealLock};
use std::path::{Path, PathBuf};

/// `<db dir>/.gatetrain`: stamps, chain logs and the default host lock live here.
pub fn state_dir(db: &Path) -> PathBuf {
    db.parent().map(|p| if p.as_os_str().is_empty() { PathBuf::from(".") } else { p.to_path_buf() }).unwrap_or_else(|| PathBuf::from(".")).join(".gatetrain")
}

/// The lock, the admission and the state folder for one host (`hosts` = the hosts.toml file and the host name), as `batch` uses them.
pub fn engine_seams(db: &Path, cfg: &BatchConfig, lock: Option<PathBuf>, hosts: Option<(PathBuf, String)>) -> Result<(RealLock, MemoryAdmission, PathBuf)> {
    let state_dir = state_dir(db);
    let lock_path = match (lock, &hosts) {
        (Some(l), _) => l,
        (None, Some((h, n))) => match gatetrain_core::hold::host_capacity(&std::fs::read_to_string(h)?, n)?.lock_file {
            Some(l) => PathBuf::from(l),
            None => state_dir.join("host.lock"),
        },
        (None, None) => state_dir.join("host.lock"),
    };
    let exe = std::env::current_exe().context("locating the gatetrain binary")?.to_string_lossy().into_owned();
    let real = RealLock { lock: lock_path.clone(), db: db.to_path_buf(), hosts: hosts.clone(), holder: vec![exe, "hold".into()] };
    let mut adm = MemoryAdmission::new(lock_path, db.to_path_buf(), hosts.clone(), cfg.chain_steps.clone());
    adm.target_dirs = cfg.chain_target_dirs.iter().map(PathBuf::from).collect();
    adm.repo_root = cfg.chain_repo_root.as_ref().map(PathBuf::from);
    adm.doctor = cfg.doctor && hosts.is_some();
    adm.preflight_steps = cfg.preflight_steps.clone();
    if let Some((h, n)) = &hosts {
        let text = std::fs::read_to_string(h)?;
        adm.light_while_proving = gatetrain_core::hostlock::host_flag(&text, n, "light_while_proving");
        adm.disk_floor = true;
        match config::load(h) {
            Err(e) => adm.floor_error = Some(format!("{e}")),
            Ok(hc) => {
                adm.needs_preflight_gb = Some(hc.needs.preflight_gb.f());
                adm.needs_chain_gb = Some(hc.needs.chain_gb.f());
                if let Some(host) = hc.hosts.iter().find(|x| x.name == *n) {
                    if let (Some(r), Ok(t)) = (gatetrain_core::runner::for_reach(&host.reach), gatetrain_core::tidy::config::build_cfg(&hc, host)) {
                        adm.close = Some(Box::new(gatetrain_core::tidy::close::OwnedEnv { host: host.clone(), runner: r, cfg: t, reference_prefixes: cfg.close_protected_container_prefixes.clone() }));
                    }
                }
            }
        }
        if cfg.tidy_auto {
            // the unattended classes only (`tidy --auto`); never a bulk prune
            let (hp, hn) = (h.to_string_lossy().into_owned(), n.clone());
            adm.tidy = Some(Box::new(move || {
                let out = gatetrain_core::tidy::cmd_tidy(&hp, Some(&hn), false, None, true, &gatetrain_core::tidy::make_runner, None);
                gatetrain_core::tidy::tidy_auto_note(&out, &hn)
            }));
        }
    }
    Ok((real, adm, state_dir))
}
