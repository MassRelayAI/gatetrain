//! Disk in the host model (the twin of `memory`): a run that would fill a configured disk below its `min_free_gb` is refused before it
//! starts. The prediction is what the same build folder grew to on earlier runs: each run records the size of its target folder in the
//! ledger (`step_memory`, step `target:<folder>`, source `target-size`, in MB) and the median of those sizes is the prediction.
//! No size is invented: a folder with no history predicts 0 (the rule then only protects a disk that is already under its minimum), and a
//! disk whose free space cannot be read is reported as unknown, never as "enough".
use crate::{Error, Result};
use std::path::Path;

/// One `[[hosts.disks]]` entry: where, and how much must stay free.
#[derive(Debug, Clone, PartialEq)]
pub struct DiskRule {
    pub path: String,
    pub min_free_gb: f64,
}

/// The disks of a host in `hosts.toml` that have a `min_free_gb`.
pub fn disks_from_hosts_toml(text: &str, host: &str) -> Result<Vec<DiskRule>> {
    let v: toml::Value = text.parse().map_err(|e: toml::de::Error| Error::Canon(format!("hosts.toml: {e}")))?;
    let h = v
        .get("hosts")
        .and_then(|h| h.as_array())
        .and_then(|a| a.iter().find(|h| h.get("name").and_then(|n| n.as_str()) == Some(host)))
        .ok_or_else(|| Error::Canon(format!("hosts.toml: no host named {host:?}")))?;
    let mut out = vec![];
    for d in h.get("disks").and_then(|d| d.as_array()).into_iter().flatten() {
        let (Some(path), Some(min)) = (d.get("path").and_then(|p| p.as_str()), d.get("min_free_gb").and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))) else { continue };
        out.push(DiskRule { path: path.to_string(), min_free_gb: min });
    }
    Ok(out)
}

/// The ledger step name under which a target folder's size is recorded.
pub fn step_key(dir: &str) -> String {
    format!("target:{}", norm(dir))
}

/// Compare key of a path: forward slashes, no trailing slash, lower case when it starts with a drive letter.
pub fn norm(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        s.to_lowercase()
    } else {
        s
    }
}

/// The disk rule whose path holds `dir` (the longest such path), if any.
pub fn disk_of<'a>(disks: &'a [DiskRule], dir: &str) -> Option<&'a DiskRule> {
    let d = norm(dir);
    disks.iter().filter(|r| {
        let p = norm(&r.path);
        d == p || d.starts_with(&format!("{p}/")) || p.is_empty()
    }).max_by_key(|r| norm(&r.path).len())
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiskAdmission {
    pub admitted: bool,
    /// The disk rule that decided (none when no configured disk holds the folder).
    pub disk: Option<String>,
    pub free_gb: Option<f64>,
    pub min_free_gb: f64,
    pub predicted_gb: f64,
    /// The rule and the numbers, ready to print and to put in the decision log.
    pub rule: String,
}

/// `free - predicted >= min_free`. `free_of` reads the free space of a path in GB. A folder no configured disk holds is admitted
/// (nothing is configured for it); an unreadable free space is admitted but says so.
pub fn admit(disks: &[DiskRule], dir: &str, predicted_mb: f64, free_of: &dyn Fn(&str) -> Option<f64>) -> DiskAdmission {
    let predicted_gb = predicted_mb / 1024.0;
    let Some(rule) = disk_of(disks, dir) else {
        return DiskAdmission { admitted: true, disk: None, free_gb: None, min_free_gb: 0.0, predicted_gb, rule: format!("no configured disk holds {dir}; nothing to protect") };
    };
    let Some(free) = free_of(&rule.path) else {
        return DiskAdmission {
            admitted: true,
            disk: Some(rule.path.clone()),
            free_gb: None,
            min_free_gb: rule.min_free_gb,
            predicted_gb,
            rule: format!("free space of {} is unknown (could not be read); admitted without a disk check", rule.path),
        };
    };
    let left = free - predicted_gb;
    let ok = left >= rule.min_free_gb;
    DiskAdmission {
        admitted: ok,
        disk: Some(rule.path.clone()),
        free_gb: Some(free),
        min_free_gb: rule.min_free_gb,
        predicted_gb,
        rule: format!(
            "disk {}: free {:.1} GB - predicted {:.1} GB = {:.1} GB {} min_free_gb {:.1}",
            rule.path,
            free,
            predicted_gb,
            left,
            if ok { ">=" } else { "<" },
            rule.min_free_gb
        ),
    }
}

/// Free space at `path` in GB; `None` when it cannot be read.
#[cfg(windows)]
pub fn free_gb(path: &str) -> Option<f64> {
    {
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        }
        let w: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(Some(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: w is NUL-terminated; the three out-pointers are valid u64s.
        (unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } != 0).then(|| avail as f64 / 1073741824.0)
    }
}

/// Free space at `path` in GB; `None` when it cannot be read.
#[cfg(not(windows))]
pub fn free_gb(path: &str) -> Option<f64> {
    {
        let o = std::process::Command::new("df").args(["-Pk", path]).output().ok()?;
        if !o.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&o.stdout).into_owned();
        let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
        let cols: Vec<&str> = line.split_whitespace().collect();
        cols.get(3)?.parse::<f64>().ok().map(|k| k / 1048576.0)
    }
}

/// The size of a folder in MB, links not followed (a link is counted as the link itself, never the folder it points to).
pub fn dir_size_mb(root: &Path) -> f64 {
    fn walk(p: &Path, total: &mut u64) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            if md.is_dir() {
                walk(&e.path(), total);
            } else if md.is_file() {
                *total += md.len();
            }
        }
    }
    let mut t = 0u64;
    walk(root, &mut t);
    t as f64 / 1048576.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disks() -> Vec<DiskRule> {
        vec![DiskRule { path: "/mnt/fast".into(), min_free_gb: 50.0 }, DiskRule { path: "/mnt/fast/build".into(), min_free_gb: 20.0 }, DiskRule { path: "/mnt/hdd".into(), min_free_gb: 10.0 }]
    }

    #[test]
    fn the_longest_disk_that_holds_the_folder_decides_and_a_sibling_prefix_does_not() {
        let d = disks();
        assert_eq!(disk_of(&d, "/mnt/fast/build/lane-a/target").unwrap().path, "/mnt/fast/build");
        assert_eq!(disk_of(&d, "/mnt/fast/other").unwrap().path, "/mnt/fast");
        assert_eq!(disk_of(&d, "/mnt/hdd").unwrap().path, "/mnt/hdd");
        assert!(disk_of(&d, "/mnt/fastest/x").is_none(), "/mnt/fastest is not inside /mnt/fast");
        assert!(disk_of(&d, "/elsewhere").is_none());
    }

    #[test]
    fn a_run_that_would_drop_the_disk_under_its_minimum_is_refused_with_the_numbers() {
        let d = disks();
        let free = |_: &str| Some(100.0);
        // 100 free, the build folder grew to 90 GB on earlier runs, 20 must stay free: 100 - 90 = 10 < 20
        let a = admit(&d, "/mnt/fast/build/t", 90.0 * 1024.0, &free);
        assert!(!a.admitted, "{a:?}");
        assert!(a.rule.contains("free 100.0 GB - predicted 90.0 GB = 10.0 GB < min_free_gb 20.0"), "{}", a.rule);
        // 100 - 70 = 30 >= 20
        assert!(admit(&d, "/mnt/fast/build/t", 70.0 * 1024.0, &free).admitted);
        // exactly the minimum is allowed
        assert!(admit(&d, "/mnt/fast/build/t", 80.0 * 1024.0, &free).admitted);
    }

    #[test]
    fn no_history_predicts_nothing_and_a_disk_already_under_its_minimum_still_refuses() {
        let d = disks();
        assert!(admit(&d, "/mnt/hdd/x", 0.0, &|_| Some(30.0)).admitted);
        let a = admit(&d, "/mnt/hdd/x", 0.0, &|_| Some(4.0));
        assert!(!a.admitted && a.rule.contains("< min_free_gb 10.0"), "{a:?}");
    }

    #[test]
    fn unreadable_free_space_and_an_unconfigured_folder_say_so() {
        let d = disks();
        let a = admit(&d, "/mnt/hdd/x", 5.0, &|_| None);
        assert!(a.admitted && a.free_gb.is_none() && a.rule.contains("unknown"), "{a:?}");
        let a = admit(&d, "/elsewhere", 5.0, &|_| Some(1.0));
        assert!(a.admitted && a.disk.is_none() && a.rule.contains("nothing to protect"), "{a:?}");
    }

    #[test]
    fn hosts_toml_disks_and_the_ledger_key() {
        let t = "[[hosts]]\nname = \"pc\"\n[[hosts.disks]]\npath = \"D:/build\"\nmin_free_gb = 40\n[[hosts.disks]]\npath = \"E:/x\"\n";
        assert_eq!(disks_from_hosts_toml(t, "pc").unwrap(), vec![DiskRule { path: "D:/build".into(), min_free_gb: 40.0 }]);
        assert_eq!(step_key("D:\\Build\\Lane-A\\"), "target:d:/build/lane-a");
        assert!(disk_of(&disks_from_hosts_toml(t, "pc").unwrap(), "d:\\BUILD\\x").is_some(), "drive paths compare case-insensitively");
    }

    #[test]
    fn a_folder_size_counts_files_and_never_follows_a_link() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/b/f.bin"), vec![0u8; 2 * 1048576]).unwrap();
        std::fs::write(d.path().join("g.bin"), vec![0u8; 1048576]).unwrap();
        let mb = dir_size_mb(d.path());
        assert!((mb - 3.0).abs() < 0.01, "{mb}");
        assert!(free_gb(&d.path().to_string_lossy()).is_some_and(|f| f > 0.0), "the free space of a real folder is readable");
    }
}
