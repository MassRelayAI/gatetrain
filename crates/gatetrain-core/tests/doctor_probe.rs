//! `doctor` compares a disk's `fast` flag with what `probe` measured (engine v2 10.1): a flagged disk whose p95 durable write is more
//! than slow_factor (5) times the best disk of the host gets a WARN naming both figures. The flag is never changed.
use gatetrain_core::doctor::measured_speed_checks;

fn p(path: &str, p95: f64) -> (String, f64) {
    (path.to_string(), p95)
}

#[test]
fn a_fast_flag_the_probe_contradicts_is_a_warn_naming_both_figures() {
    let r = measured_speed_checks("h", &[("/a".into(), true), ("/b".into(), false)], &[p("/a", 60.0), p("/b", 2.0)], 5.0);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].check.as_str(), r[0].status.as_str(), r[0].host.as_str()), ("disk-speed-measured", "WARN", "h"));
    assert!(r[0].message.contains("/a") && r[0].message.contains("60.0 ms") && r[0].message.contains("/b") && r[0].message.contains("2.0 ms"), "{}", r[0].message);
}

#[test]
fn no_warn_within_the_factor_for_an_unflagged_disk_or_without_a_probe() {
    assert!(measured_speed_checks("h", &[("/a".into(), true), ("/b".into(), false)], &[p("/a", 9.0), p("/b", 2.0)], 5.0).is_empty(), "4.5x is within 5x");
    assert!(measured_speed_checks("h", &[("/a".into(), false), ("/b".into(), false)], &[p("/a", 60.0), p("/b", 2.0)], 5.0).is_empty(), "no flag, no contradiction");
    assert!(measured_speed_checks("h", &[("/a".into(), true)], &[], 5.0).is_empty(), "never probed: unknown, not a warning");
    assert!(measured_speed_checks("h", &[("/a".into(), true), ("/b".into(), false)], &[p("/a", 60.0)], 5.0).is_empty(), "one figure has nothing to be compared with");
}
