//! `[tidy] image_archive`: an unused image is saved to `<trash_dir>/docker-images/<name>@<id>.tar` before it is removed, and removed only
//! when the save exited 0 and left a non-empty file. The archive ages out like any trash entry. Sim runner only: never a real docker.
mod common;
use common::*;
use gatetrain_core::config::{Config, Disk, Host, Num};
use gatetrain_core::tidy::apply::apply_plan_report;
use gatetrain_core::tidy::config::{build_cfg, TidyCfg};
use gatetrain_core::tidy::inventory::{gather, Inventory, Item};
use serde_json::{json, Value};

const TAR: &str = "/trash/docker-images/gate-img_1@aaa111.tar";

/// A host whose disks are `/build` and `/trash`; `/trash` may hold the archive (`use = ["archive"]`) with the given floor.
fn host(extra: Value, archive_use: bool, floor: f64) -> (Host, TidyCfg) {
    let h = Host {
        name: "h1".into(),
        reach: "local".into(),
        os: "linux".into(),
        disks: vec![
            Disk { path: "/build".into(), min_free_gb: Num::F(1.0), measure: "du".into(), ..Default::default() },
            Disk { path: "/trash".into(), min_free_gb: Num::F(floor), measure: "du".into(), r#use: if archive_use { vec!["archive".into()] } else { vec![] }, ..Default::default() },
        ],
        tidy: to_table(&merged(extra)),
        ..Default::default()
    };
    let cfg = Config { hosts: vec![h.clone()], ..Default::default() };
    let t = build_cfg(&cfg, &h).unwrap();
    (h, t)
}

fn arch(d: &tempfile::TempDir) -> Value {
    json!({"image_archive": true, "log": d.path().join("l").to_string_lossy()})
}

fn run_images(sim: &Shared, extra: Value, archive_use: bool, floor: f64) -> (Vec<Value>, usize, usize) {
    let (h, t) = host(extra, archive_use, floor);
    let inv = gather(&h, sim, &t, Some(NOW as f64)).unwrap();
    let log = std::path::PathBuf::from(t.log.clone());
    let (done, failed, _) = apply_plan_report(&h, sim, &t, &inv, &["images".to_string()], Some(NOW as f64), None, false).unwrap();
    (if log.exists() { records(&log) } else { vec![] }, done, failed)
}

fn saved_calls(sim: &Shared) -> usize {
    sim.s().calls.iter().filter(|c| c.contains("docker save")).count()
}

fn has_image(sim: &Shared, name: &str) -> bool {
    sim.s().images.iter().any(|x| x.starts_with(&format!("{name}|")))
}

#[test]
fn an_unused_image_is_archived_then_removed_and_the_row_names_the_archive() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    let d = tempfile::tempdir().unwrap();
    let (rows, done, failed) = run_images(&sim, arch(&d), true, 1.0);
    assert_eq!((done, failed), (1, 0), "{rows:?}");
    assert!(sim.s().get(TAR).is_some_and(|e| e.size == 2 * (1 << 30)), "the tar holds the image: {:?}", sim.s().fs.iter().map(|x| &x.0).collect::<Vec<_>>());
    assert!(!has_image(&sim, "gate-img:1"), "removed after the save");
    let calls = sim.s().calls.clone();
    let (i_save, i_rmi) = (calls.iter().position(|c| c.contains("docker save")).unwrap(), calls.iter().position(|c| c.starts_with("docker rmi")).unwrap());
    assert!(i_save < i_rmi, "save first, then rmi");
    let row = serde_json::to_string(&rows).unwrap();
    assert!(row.contains(&format!("archived: {TAR} ")) && row.contains("freed: "), "{row}");
    assert!(has_image(&sim, "gate-img:2"), "the used image is untouched");
}

#[test]
fn a_failed_save_keeps_the_image() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    sim.s().save_fails = true;
    let d = tempfile::tempdir().unwrap();
    let (rows, done, failed) = run_images(&sim, arch(&d), true, 1.0);
    assert_eq!((done, failed), (0, 1), "{rows:?}");
    assert!(has_image(&sim, "gate-img:1"), "kept");
    assert!(!sim.s().calls.iter().any(|c| c.starts_with("docker rmi")), "no rmi after a failed save");
}

#[test]
fn a_zero_size_tar_keeps_the_image() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    sim.s().save_zero = true;
    let d = tempfile::tempdir().unwrap();
    let (_rows, done, failed) = run_images(&sim, arch(&d), true, 1.0);
    assert_eq!((done, failed), (0, 1));
    assert!(has_image(&sim, "gate-img:1"));
    assert!(!sim.s().calls.iter().any(|c| c.starts_with("docker rmi")));
}

#[test]
fn an_archive_disk_below_its_floor_or_not_configured_for_archive_refuses_before_any_save() {
    // free 5 GB, image 2 GB, floor 50 GB
    let sim = linux_sim();
    let d = tempfile::tempdir().unwrap();
    let (rows, done, failed) = run_images(&sim, arch(&d), true, 50.0);
    assert_eq!((done, failed), (0, 1), "{rows:?}");
    assert_eq!(saved_calls(&sim), 0, "free space is checked before the save");
    assert!(has_image(&sim, "gate-img:1") && serde_json::to_string(&rows).unwrap().contains("floor"), "{rows:?}");
    // the disk is not configured to hold `archive`
    let sim = linux_sim();
    sim.s().free = 500.0;
    let d = tempfile::tempdir().unwrap();
    let (rows, done, failed) = run_images(&sim, arch(&d), false, 1.0);
    assert_eq!((done, failed), (0, 1));
    assert_eq!(saved_calls(&sim), 0);
    assert!(serde_json::to_string(&rows).unwrap().contains("archive"), "{rows:?}");
    assert!(has_image(&sim, "gate-img:1"));
}

#[test]
fn keep_patterns_win_over_image_patterns() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    let extra = json!({"image_patterns": ["gate-img:*"], "image_keep_patterns": ["gate-img:1"]});
    let (h, t) = host(extra.clone(), true, 1.0);
    let inv: Inventory = gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
    assert!(by_key(&inv.items, "gate-img:1").is_empty(), "kept by pattern: never planned");
    // a plan item that slipped in is refused at apply time and nothing is saved
    let mut inv2 = inv;
    inv2.items.push(Item::new("images", "rm-image", "gate-img:1", Some(2 << 30), "x").ruled("unused-image"));
    let r = apply_plan_report(&h, &sim, &t, &inv2, &["images".to_string()], Some(NOW as f64), None, false);
    assert!(r.is_err(), "the guard stops it");
    assert_eq!(saved_calls(&sim), 0);
    assert!(has_image(&sim, "gate-img:1"));
    // without the keep pattern it is planned
    let (h, t) = host(json!({"image_patterns": ["gate-img:*"]}), true, 1.0);
    assert_eq!(by_key(&gather(&h, &sim, &t, Some(NOW as f64)).unwrap().items, "gate-img:1").len(), 1);
}

#[test]
fn a_used_image_is_never_saved_or_removed() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    let (h, t) = host(json!({"image_archive": true}), true, 1.0);
    let mut inv = gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
    assert!(by_key(&inv.items, "gate-img:2").is_empty(), "in use: not planned");
    inv.items.retain(|i| i.cls != "images"); // only the forced item: the unused image would be archived in this same apply
    inv.items.push(Item::new("images", "rm-image", "gate-img:2", Some(1 << 30), "x").ruled("unused-image"));
    let r = apply_plan_report(&h, &sim, &t, &inv, &["images".to_string()], Some(NOW as f64), None, false);
    assert!(r.is_err());
    assert_eq!(saved_calls(&sim), 0);
    assert!(has_image(&sim, "gate-img:2"));
}

#[test]
fn restore_loads_only_an_exact_tar_inside_the_archive_folder() {
    let sim = linux_sim();
    sim.s().free = 500.0;
    let d = tempfile::tempdir().unwrap();
    run_images(&sim, arch(&d), true, 1.0);
    assert!(!has_image(&sim, "gate-img:1"));
    let conf = write_conf(d.path(), json!({"image_archive": true}), false);
    let mk = |_h: &Host| -> Option<Box<dyn gatetrain_core::tidy::Runner>> { Some(Box::new(sim.clone())) };
    for bad in ["/trash/old/x", "/trash/docker-images/../old/x.tar", "/trash/docker-images/sub/a.tar", "/etc/x.tar", "/trash/docker-images", "/trash/docker-images/gate-img_1@aaa111.txt"] {
        let o = gatetrain_core::tidy::cmd_restore_image(&conf, None, bad, &mk);
        assert_ne!(o.rc, 0, "{bad}: {}{}", o.stdout, o.stderr);
    }
    assert!(!sim.s().calls.iter().any(|c| c.starts_with("docker load")), "nothing was loaded from outside the folder");
    let o = gatetrain_core::tidy::cmd_restore_image(&conf, None, TAR, &mk);
    assert_eq!(o.rc, 0, "{}{}", o.stdout, o.stderr);
    assert!(has_image(&sim, "gate-img:1"), "loaded back");
    // a missing file
    assert_ne!(gatetrain_core::tidy::cmd_restore_image(&conf, None, "/trash/docker-images/none@x.tar", &mk).rc, 0);
}

#[test]
fn an_old_archive_ages_out_through_the_trash_purge_one_tar_at_a_time() {
    let sim = linux_sim();
    {
        let s = &mut *sim.s();
        s.dir("/trash/docker-images", 0);
        s.file("/trash/docker-images/old@1.tar", 100 * MB, 10 * DAY);
        s.file("/trash/docker-images/new@2.tar", 100 * MB, DAY);
        let sp = "/trash/.gatetrain-seen.json".to_string();
        let mut seen: serde_json::Map<String, Value> = s.scripts.get(&sp).and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
        seen.insert("docker-images/old@1.tar".into(), json!({"t": NOW - 10 * DAY, "m": null}));
        seen.insert("docker-images/new@2.tar".into(), json!({"t": NOW - DAY, "m": null}));
        s.scripts.insert(sp, serde_json::to_string(&seen).unwrap());
    }
    let (h, t) = host(json!({"image_archive": true}), true, 1.0);
    let inv = gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
    assert_eq!(by_key(&inv.items, "/trash/docker-images/old@1.tar").len(), 1, "an archive older than trash_days is purged");
    assert!(by_key(&inv.items, "/trash/docker-images/new@2.tar").is_empty());
    assert!(by_key(&inv.items, "/trash/docker-images").is_empty(), "the folder itself is never an item");
    let d = tempfile::tempdir().unwrap();
    let (h, t) = host(json!({"image_archive": true, "log": d.path().join("l").to_string_lossy()}), true, 1.0);
    let inv = gather(&h, &sim, &t, Some(NOW as f64)).unwrap();
    apply_plan_report(&h, &sim, &t, &inv, &["trash".to_string()], Some(NOW as f64), None, false).unwrap();
    assert!(sim.s().get("/trash/docker-images/old@1.tar").is_none() && sim.s().get("/trash/docker-images/new@2.tar").is_some());
}
