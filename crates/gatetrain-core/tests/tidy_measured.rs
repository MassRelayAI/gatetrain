//! Reclaimed bytes are MEASURED, never the plan (2026-11-17, a real host): a `docker builder prune --filter until=168h` that freed
//! about 1 GB was reported as "86.1 GB reclaimed (measured ...)" because the cache and image actions returned `after = 0`, so
//! before-minus-after counted the whole planned upper bound. The fix measures Docker's own usage of that type before and after.
mod common;
use common::*;
use gatetrain_core::tidy::apply::apply_plan_report;
use gatetrain_core::tidy::RunOut;
use serde_json::json;


fn report(sim: &Shared, classes: &[&str], d: &tempfile::TempDir) -> (usize, usize, i64) {
    let log = d.path().join("l");
    let extra = json!({"log": log.to_string_lossy()});
    let (h, t, inv) = gather_sim(sim, extra, false, 50.0);
    let classes: Vec<String> = classes.iter().map(|c| c.to_string()).collect();
    apply_plan_report(&h, sim, &t, &inv, &classes, Some(NOW as f64), None, false).unwrap()
}

#[test]
fn a_cache_prune_that_frees_little_reports_what_it_freed_not_the_plan() {
    let sim = linux_sim();
    // the planned upper bound is 6 GB reclaimable of 8 GB; the prune only removes 0.5 GB (the rest is younger than the age filter)
    sim.pre(Box::new(|s, c| {
        if c.starts_with("docker builder prune") {
            s.cache_row = Some("Build Cache|7.5GB|5.5GB\nImages|3GB|1GB\n".into());
            return Some(RunOut::done(0, ""));
        }
        None
    }));
    let d = tempfile::tempdir().unwrap();
    let (done, failed, reclaimed) = report(&sim, &["cache"], &d);
    assert_eq!((done, failed), (1, 0));
    assert!(reclaimed < 600_000_000, "reported {reclaimed} bytes reclaimed; the prune freed 0.5 GB, not the 6 GB planned");
    assert!(reclaimed >= 499_000_000, "reported {reclaimed}; expected about 0.5 GB");
}

#[test]
fn removing_an_image_whose_layers_are_shared_reports_zero_freed() {
    let sim = linux_sim();
    // `docker rmi` succeeds, but Docker's Images total does not move (the layers are shared with a kept image)
    sim.pre(Box::new(|s, c| {
        if c.starts_with("docker rmi ") {
            let nm = c.split(' ').nth(2).unwrap().to_string();
            s.images.retain(|x| !x.starts_with(&format!("{nm}|")));
            return Some(RunOut::done(0, ""));
        }
        None
    }));
    let d = tempfile::tempdir().unwrap();
    let (done, _failed, reclaimed) = report(&sim, &["images"], &d);
    assert!(done >= 1, "the fixture plans at least one image");
    assert_eq!(reclaimed, 0, "Docker's image total did not change, so nothing was freed");
}

#[test]
fn when_docker_usage_cannot_be_read_the_freed_size_is_unknown_not_the_plan() {
    let sim = linux_sim();
    sim.pre(Box::new(|s, c| {
        if c.starts_with("docker builder prune") {
            s.cache_row = None; // `docker system df` now answers nothing usable
            return Some(RunOut::done(0, ""));
        }
        None
    }));
    let d = tempfile::tempdir().unwrap();
    let (done, _failed, reclaimed) = report(&sim, &["cache"], &d);
    assert_eq!(done, 1);
    assert_eq!(reclaimed, 0, "an unmeasured action contributes nothing to 'reclaimed'");
}

// ---- GT-3 (a): the prune's own report is the measurement; the planned number is never "reclaimed", and a real prune is never "0 B"

#[test]
fn a_prune_that_ran_reports_the_total_docker_printed_even_when_df_did_not_move() {
    let sim = linux_sim();
    // `docker system df` still shows 8 GB afterwards (the old reading of "0 B"), but docker printed what it removed
    sim.pre(Box::new(|_s, c| if c.starts_with("docker builder prune") { Some(RunOut::done(0, "Deleted build cache objects:\nabc\ndef\n\nTotal:\t2.5GB\n")) } else { None }));
    let d = tempfile::tempdir().unwrap();
    let (done, failed, reclaimed) = report(&sim, &["cache"], &d);
    assert_eq!((done, failed), (1, 0));
    assert_eq!(reclaimed, 2_500_000_000, "docker says 2.5GB, which is decimal");
    let log = std::fs::read_to_string(d.path().join("l")).unwrap();
    assert!(log.contains("as docker reported"), "the note names the measurement: {log}");
}

#[test]
fn the_older_total_reclaimed_space_line_is_read_too() {
    let sim = linux_sim();
    sim.pre(Box::new(|_s, c| if c.starts_with("docker builder prune") { Some(RunOut::done(0, "Total reclaimed space: 700MB\n")) } else { None }));
    let d = tempfile::tempdir().unwrap();
    let (_done, _failed, reclaimed) = report(&sim, &["cache"], &d);
    assert_eq!(reclaimed, 700_000_000);
}

#[test]
fn docker_saying_zero_is_zero_whatever_df_says() {
    let sim = linux_sim();
    sim.pre(Box::new(|s, c| {
        if c.starts_with("docker builder prune") {
            s.cache_row = Some("Build Cache|2GB|1GB\nImages|3GB|1GB\n".into()); // df moved by 6 GB for another reason
            return Some(RunOut::done(0, "Total:\t0B\n"));
        }
        None
    }));
    let d = tempfile::tempdir().unwrap();
    let (_done, _failed, reclaimed) = report(&sim, &["cache"], &d);
    assert_eq!(reclaimed, 0, "the command's own report wins over a before/after of the whole host");
}
