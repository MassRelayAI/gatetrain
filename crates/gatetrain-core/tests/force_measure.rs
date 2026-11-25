//! `gatetrain measure`: what holds disk, each number with the method that produced it. Stubbed probes only; nothing is read or removed.
use gatetrain_core::force::exec::StubExec;
use gatetrain_core::force::measure::*;
use gatetrain_core::tidy::inventory::{ImageInfo, Inventory, TargetInfo, TrashInfo, WtInfo};

const GB: i64 = 1024 * 1024 * 1024;

fn inv() -> Inventory {
    Inventory {
        host: "pc".into(),
        worktrees: vec![
            WtInfo { path: "F:/repos/proj-wt-a".into(), branch: Some("a".into()), merged: Some(false), dirty: Some(true), links: Some(2), bytes: Some(30 * GB), keep: None },
            WtInfo { path: "F:/repos/proj-wt-b".into(), branch: Some("b".into()), merged: Some(true), dirty: Some(false), links: Some(0), bytes: Some(2 * GB), keep: Some("live lane".into()) },
        ],
        targets: vec![TargetInfo { path: "F:/build/t1".into(), lane: Some("a".into()), bytes: Some(80 * GB), idle_min: Some(600.0), cargo: true, tag: Some(true), merged: Some(false), branch: Some("a".into()), budget_gb: None, over: false, keep: None }],
        images: vec![ImageInfo { name: "mr-api:dev".into(), bytes: 3 * GB, in_use: true }],
        containers: vec!["mr-gate-1".into()],
        trash: vec![TrashInfo { path: "F:/repos/.trash/x".into(), bytes: Some(GB), age_days: Some(9.0), keep: None }],
        ..Default::default()
    }
}

#[test]
fn the_inventory_becomes_rows_each_with_a_method() {
    let rows = from_inventory(&inv(), true);
    let by = |c: &str| rows.iter().filter(|r| r.class == c).collect::<Vec<_>>();
    assert_eq!(by("worktree").len(), 2);
    let a = by("worktree")[0];
    assert_eq!((a.bytes, a.item.as_str()), (Some(30 * GB), "F:/repos/proj-wt-a"));
    assert!(a.state.contains("dirty") && a.state.contains("unmerged"), "{}", a.state);
    assert!(by("worktree")[1].state.contains("kept"), "{}", by("worktree")[1].state);
    assert_eq!(by("cargo-target")[0].bytes, Some(80 * GB));
    assert_eq!(by("docker-image")[0].bytes, Some(3 * GB));
    assert!(by("docker-image")[0].state.contains("in use"));
    assert_eq!(by("docker-container")[0].item, "mr-gate-1");
    assert_eq!(by("trash")[0].bytes, Some(GB));
    assert!(rows.iter().all(|r| !r.method.is_empty()), "a number without its method: {rows:?}");
    assert!(by("worktree")[0].method.contains("Get-ChildItem"), "{}", by("worktree")[0].method);
    assert!(from_inventory(&inv(), false)[0].method.contains("du -sk"));
}

#[test]
fn an_unknown_size_stays_unknown() {
    let mut i = inv();
    i.worktrees[0].bytes = None;
    let rows = from_inventory(&i, false);
    assert_eq!(rows[0].bytes, None);
    assert!(table(&Measure { host: "pc".into(), rows, free: vec![] }).contains("unknown"));
}

#[test]
fn docker_totals_come_from_docker_system_df() {
    let e = StubExec::new().on(
        "docker system df",
        0,
        "Images|12.5GB|4GB (32%)\nContainers|1.2MB|0B (0%)\nLocal Volumes|3GB|1GB (33%)\nBuild Cache|2.5GB|2.5GB\n",
    );
    let rows = docker_rows(&e);
    let get = |c: &str| rows.iter().find(|r| r.class == c).unwrap_or_else(|| panic!("no {c} in {rows:?}")).clone();
    assert_eq!(get("docker-volume").bytes, Some(3_000_000_000));
    assert_eq!(get("docker-cache").bytes, Some(2_500_000_000));
    assert!(get("docker-volume").state.contains("reclaimable 1GB"), "{}", get("docker-volume").state);
    assert!(get("docker-volume").method.contains("docker system df"));
    // docker not answering is one unknown row, never silence
    let rows = docker_rows(&StubExec::new());
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].bytes, rows[0].state.as_str()), (None, "unknown: docker did not answer"));
}

#[test]
fn a_vm_disk_is_measured_by_what_it_occupies_not_its_logical_size() {
    let e = StubExec::new().on("powershell", 0, "1073741824000|268435456000\n");
    let rows = vm_rows(&e, true, &["C:/Users/x/ext4.vhdx".to_string()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bytes, Some(268_435_456_000), "the on-disk size is the number");
    assert!(rows[0].state.contains("logical 1000.0 GB") && rows[0].state.contains("on disk 250.0 GB"), "{}", rows[0].state);
    assert!(rows[0].method.contains("GetCompressedFileSizeW"), "{}", rows[0].method);
    assert_eq!(rows[0].class, "vm-disk");
    // linux: allocated blocks, from du
    let e = StubExec::new().on("stat -c %s", 0, "1073741824\n").on("du -sk", 0, "524288\tx.img\n");
    let rows = vm_rows(&e, false, &["/vm/x.img".to_string()]);
    assert_eq!(rows[0].bytes, Some(536_870_912));
    assert!(rows[0].method.contains("du -sk"));
    // a missing or unreadable file is said, never counted as 0
    let rows = vm_rows(&StubExec::new().on("powershell", 0, "missing\n"), true, &["C:/nope.vhdx".to_string()]);
    assert_eq!((rows[0].bytes, rows[0].state.as_str()), (None, "missing"));
    let rows = vm_rows(&StubExec::new(), true, &["C:/x.vhdx".to_string()]);
    assert_eq!((rows[0].bytes, rows[0].state.as_str()), (None, "unknown: the probe did not answer"));
}

#[test]
fn the_table_names_the_method_and_the_json_carries_it() {
    let m = Measure { host: "pc".into(), rows: from_inventory(&inv(), true), free: vec![("F:/build".into(), Some(120.5))] };
    let t = table(&m);
    for w in ["== pc", "worktree", "cargo-target", "30.0 GB", "method", "Get-ChildItem", "free", "F:/build", "total"] {
        assert!(t.contains(w), "{w} missing from\n{t}");
    }
    let j = to_json(&m);
    assert_eq!(j["host"], "pc");
    assert_eq!(j["rows"].as_array().unwrap().len(), m.rows.len());
    assert!(j["rows"][0]["method"].as_str().unwrap().contains("Get-ChildItem"));
    assert_eq!(j["rows"][0]["bytes"], 30 * GB);
}
