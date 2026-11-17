//! Engine v2 E6 and E7 (docs/design/engine-v2.md section 10): the sampler's figures from fixture /proc files, rotational from a fake
//! /sys tree (1, 0 and unknown), and the bounded, cleaning-up disk probe. No real hardware is read here.
use gatetrain_core::hw::{self, DiskProbe, ProbeCfg, Rot, Sampler};
use std::path::Path;

fn w(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

// ---------------------------------------------------------------- the sampler

fn proc_fixture(root: &Path, user: u64, idle: u64, iowait: u64, io_ticks: u64, stall_io_total: u64) {
    w(root, "proc/stat", &format!("cpu  {user} 0 0 {idle} {iowait} 0 0 0 0 0\ncpu0 1 1 1 1 1 0 0 0 0 0\n"));
    w(root, "proc/loadavg", "3.50 2.00 1.00 4/200 1234\n");
    w(root, "proc/meminfo", "MemTotal:       16384000 kB\nMemFree:          100000 kB\nMemAvailable:    8192000 kB\n");
    w(root, "proc/diskstats", &format!("   8       0 sda 1 0 2 3 4 0 5 6 0 {io_ticks} 7\n   8       1 sda1 1 0 2 3 4 0 5 6 0 999999 7\n   7       0 loop0 0 0 0 0 0 0 0 0 0 5 0\n"));
    w(root, "proc/pressure/io", &format!("some avg10=1.00 avg60=1.00 avg300=1.00 total={stall_io_total}\nfull avg10=0.00 avg60=0.00 avg300=0.00 total={}\n", stall_io_total / 2));
    w(root, "proc/pressure/cpu", "some avg10=0.00 avg60=0.00 avg300=0.00 total=2000000\n");
    w(root, "proc/pressure/memory", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
    std::fs::create_dir_all(root.join("sys/block/sda/queue")).unwrap();
    std::fs::create_dir_all(root.join("sys/block/loop0/queue")).unwrap();
}

#[test]
fn linux_figures_come_from_proc_deltas() {
    let d = tempfile::tempdir().unwrap();
    proc_fixture(d.path(), 100, 700, 100, 1_000, 1_000_000);
    let mut s = Sampler::linux_at(&d.path().join("proc"), &d.path().join("sys"));
    let first = s.sample(10.0);
    assert_eq!(first.load_1m, Some(3.5));
    assert_eq!(first.mem_free_mb, Some(8000.0), "MemAvailable, not MemFree");
    assert_eq!(first.mem_total_mb, Some(16000.0));
    assert_eq!(first.cpu_busy_pct, None, "one reading has no delta");
    assert!(first.unknown.get("cpu_busy_pct").unwrap().contains("first"), "{:?}", first.unknown);
    // 10 s later: user +300, idle +500, iowait +200 of 1000 total ticks; sda spent 4000 ms of 10000 ms doing I/O; 2 s of I/O stall
    proc_fixture(d.path(), 400, 1200, 300, 5_000, 3_000_000);
    let r = s.sample(10.0);
    assert_eq!(r.cpu_busy_pct, Some(30.0));
    assert_eq!(r.iowait_pct, Some(20.0));
    assert_eq!(r.stall_io_some, Some(20.0), "2 000 000 us of 10 s");
    assert_eq!(r.stall_io_full, Some(10.0));
    assert_eq!(r.stall_cpu_some, Some(0.0));
    let sda = r.disks.iter().find(|x| x.device == "sda").expect("sda");
    assert_eq!(sda.busy_pct, Some(40.0));
    assert!(r.disks.iter().all(|x| x.device != "sda1" && x.device != "loop0"), "whole disks only: {:?}", r.disks);
    assert!(r.unknown.is_empty(), "{:?}", r.unknown);
}

#[test]
fn a_kernel_without_pressure_files_says_unknown_and_why() {
    let d = tempfile::tempdir().unwrap();
    proc_fixture(d.path(), 1, 1, 1, 1, 1);
    std::fs::remove_dir_all(d.path().join("proc/pressure")).unwrap();
    let mut s = Sampler::linux_at(&d.path().join("proc"), &d.path().join("sys"));
    s.sample(10.0);
    let r = s.sample(10.0);
    assert_eq!(r.stall_io_some, None);
    assert_eq!(r.stall_io_full, None);
    assert!(r.unknown.get("stall_io_some").unwrap().contains("pressure"), "{:?}", r.unknown);
    assert!(r.load_1m.is_some(), "the other figures stand");
}

#[test]
fn a_platform_without_a_source_writes_null_and_the_reason() {
    let d = tempfile::tempdir().unwrap(); // an empty /proc
    let mut s = Sampler::linux_at(&d.path().join("proc"), &d.path().join("sys"));
    let r = s.sample(10.0);
    assert!(r.load_1m.is_none() && r.mem_free_mb.is_none() && r.cpu_busy_pct.is_none());
    for k in ["load_1m", "mem_free_mb", "cpu_busy_pct", "iowait_pct"] {
        assert!(r.unknown.contains_key(k), "{k}: {:?}", r.unknown);
    }
    let line = r.to_json_line("h", "run-1", Some(3), "2026-11-17T10:00:00Z");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(v["load_1m"].is_null() && v["running_steps"] == 3 && v["host"] == "h" && v["run_id"] == "run-1");
}

// ---------------------------------------------------------------- rotational

fn sys_fixture(root: &Path) {
    w(root, "block/sda/queue/rotational", "1\n");
    std::fs::create_dir_all(root.join("block/sda/sda1")).unwrap();
    w(root, "block/nvme0n1/queue/rotational", "0\n");
    std::fs::create_dir_all(root.join("block/nvme0n1/nvme0n1p1")).unwrap();
    w(root, "block/sdb/queue/rotational", "0\n");
    // stacked devices: a device-mapper node says 0 whatever is under it, so its slaves decide
    w(root, "block/dm-0/queue/rotational", "0\n");
    std::fs::create_dir_all(root.join("block/dm-0/slaves/sda")).unwrap();
    std::fs::create_dir_all(root.join("block/dm-0/slaves/sdb")).unwrap();
    w(root, "block/dm-1/queue/rotational", "0\n");
    std::fs::create_dir_all(root.join("block/dm-1/slaves/sda")).unwrap();
    w(root, "block/weird/queue/rotational", "maybe\n");
}

#[test]
fn rotational_is_read_from_sys_never_guessed() {
    let d = tempfile::tempdir().unwrap();
    sys_fixture(d.path());
    let s = d.path();
    assert_eq!(hw::rotational_of(s, "sda", false), Rot::Yes);
    assert_eq!(hw::rotational_of(s, "sda1", false), Rot::Yes, "a partition reads its disk");
    assert_eq!(hw::rotational_of(s, "nvme0n1p1", false), Rot::No);
    assert_eq!(hw::rotational_of(s, "nvme0n1", false), Rot::No);
    assert_eq!(hw::rotational_of(s, "dm-1", false), Rot::Yes, "a stacked device reads its slaves, not its own 0");
    assert!(matches!(hw::rotational_of(s, "dm-0", false), Rot::Unknown(r) if r.contains("mixed")));
    assert!(matches!(hw::rotational_of(s, "weird", false), Rot::Unknown(_)));
    assert!(matches!(hw::rotational_of(s, "nosuch", false), Rot::Unknown(r) if r.contains("not found")));
    assert!(matches!(hw::rotational_of(s, "sda", true), Rot::Unknown(r) if r.contains("virtual")), "inside the Linux subsystem on Windows the disk is a file");
}

#[test]
fn a_path_maps_to_its_device_through_mountinfo_and_a_network_mount_is_flagged() {
    let mi = "22 1 8:1 / / rw - ext4 /dev/sda1 rw\n40 22 0:30 / /mnt/c rw - 9p C:\\ rw\n41 22 259:1 / /data rw - ext4 /dev/nvme0n1p1 rw\n42 22 0:50 / /net rw - nfs4 srv:/x rw\n";
    let m = hw::mount_of(mi, "/data/build/target").unwrap();
    assert_eq!((m.source.as_str(), m.fstype.as_str()), ("/dev/nvme0n1p1", "ext4"));
    assert_eq!(hw::mount_of(mi, "/home/u").unwrap().source, "/dev/sda1");
    assert!(hw::mount_of(mi, "/mnt/c/x").unwrap().network_or_virtual());
    assert!(hw::mount_of(mi, "/net/share").unwrap().network_or_virtual());
    assert!(!hw::mount_of(mi, "/data").unwrap().network_or_virtual());
}

// ---------------------------------------------------------------- the disk probe

fn small() -> ProbeCfg {
    ProbeCfg { budget_s: 20.0, small_writes: 20, seq_mb: 2, files: 50, max_bytes: 512 * 1024 * 1024 }
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(".gatetrain-probe-")).collect()
}

#[test]
fn a_probe_measures_every_figure_and_leaves_no_file() {
    let d = tempfile::tempdir().unwrap();
    let p: DiskProbe = hw::probe_disk(d.path(), &small(), 0.0).unwrap();
    assert!(p.fsync_p50_ms.unwrap() > 0.0 && p.fsync_p95_ms.unwrap() >= p.fsync_p50_ms.unwrap());
    assert!(p.seq_write_mbps.unwrap() > 0.0 && p.smallfile_per_s.unwrap() > 0.0);
    assert!(p.bytes_written >= 2 * 1024 * 1024 && p.bytes_written <= 512 * 1024 * 1024);
    assert!(p.free_gb.is_some() && p.total_gb.is_some());
    assert!(leftovers(d.path()).is_empty(), "the probe folder is removed at the end");
}

#[test]
fn a_probe_that_runs_out_of_budget_reports_unknown_budget_and_the_others_stand() {
    let d = tempfile::tempdir().unwrap();
    // the sequential write alone (64 MB) cannot finish in a hair of budget; the durable small writes are measured first and stand
    let cfg = ProbeCfg { budget_s: 0.000_001, small_writes: 5, seq_mb: 64, files: 10, max_bytes: 512 * 1024 * 1024 };
    let p = hw::probe_disk(d.path(), &cfg, 0.0).unwrap();
    assert!(p.unknown.values().any(|v| v == "budget"), "{:?}", p.unknown);
    assert!(p.seq_write_mbps.is_none() || p.smallfile_per_s.is_none());
    assert!(p.bytes_written <= 512 * 1024 * 1024);
    assert!(leftovers(d.path()).is_empty());
}

#[test]
fn the_probe_clears_a_crash_leftover_and_refuses_low_free_space_and_a_link() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".gatetrain-probe-99999")).unwrap();
    std::fs::write(d.path().join(".gatetrain-probe-99999/x"), b"left by a crash").unwrap();
    hw::probe_disk(d.path(), &small(), 0.0).unwrap();
    assert!(leftovers(d.path()).is_empty(), "the next probe removes a crashed one's folder");
    // free space below min_free_gb + 1
    let e = hw::probe_disk(d.path(), &small(), 1.0e9).unwrap_err();
    assert!(e.starts_with("probe.disk-low"), "{e}");
    assert!(hw::probe_disk(&d.path().join("missing"), &small(), 0.0).is_err());
    #[cfg(unix)]
    {
        let link = d.path().join("lnk");
        std::os::unix::fs::symlink(d.path(), &link).unwrap();
        let e = hw::probe_disk(&link, &small(), 0.0).unwrap_err();
        assert!(e.contains("link"), "{e}");
    }
}

#[test]
fn the_cpu_probe_is_bounded() {
    let t = std::time::Instant::now();
    let c = hw::probe_cpu(0.3);
    assert!(c.one_mbps > 0.0 && c.all_mbps > 0.0 && c.threads >= 1);
    assert!(t.elapsed().as_secs_f64() < 5.0);
}
