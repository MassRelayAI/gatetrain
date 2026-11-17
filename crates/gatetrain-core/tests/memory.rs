//! Memory in the host model: admission, memory-based parallelism, history in the ledger. Synthetic fixtures only.
use gatetrain_core::ledger::Ledger;
use gatetrain_core::memory::{admit, cheap_set, parallelism, typical_mb, wslconfig_memory_gb, MemoryModel};
use std::collections::HashMap;

const HOSTS: &str = r#"
[[hosts]]
name = "pc"
cores = 16
reserve_cores = 4
max_parallel_steps = 12
memory_gb = 31
memory_reserve_gb = 12
memory_per_step_gb = 4
memory_per_cargo_job_gb = 2
step_default_gb = 1.5
"#;

fn hist() -> HashMap<String, Vec<f64>> {
    // gigabytes per step; sums to 40 over the 8 steps
    let mut h = HashMap::new();
    for (n, g) in [("cargo-test", 12.0), ("test-api", 9.0), ("web-build", 6.0), ("db-gate", 5.0), ("lint", 2.0), ("typecheck", 3.0), ("fmt", 1.0), ("docs", 2.0)] {
        h.insert(n.to_string(), vec![g * 1024.0, g * 1024.0, g * 1024.0]);
    }
    h
}

fn model() -> MemoryModel {
    MemoryModel::from_hosts_toml(HOSTS, "pc").unwrap()
}

#[test]
fn a_31_gb_host_with_a_12_gb_reserve_refuses_a_40_gb_chain_and_admits_the_cheap_set() {
    let m = model();
    assert_eq!(m.available_gb(), Some(19.0));
    let all: Vec<String> = hist().keys().cloned().collect();
    let a = admit(&m, &all, &hist());
    assert!(!a.admitted);
    assert!((a.predicted_gb - 40.0).abs() < 1e-9, "{}", a.predicted_gb);
    assert!(a.rule.contains("40.0") && a.rule.contains("19.0"), "{}", a.rule);
    let cheap = cheap_set(&m, &all, &hist());
    assert!(!cheap.is_empty() && cheap.len() < all.len());
    assert!(admit(&m, &cheap, &hist()).admitted);
    assert!(!cheap.contains(&"cargo-test".to_string()), "the biggest step does not fit with the others: {cheap:?}");
}

#[test]
fn a_step_with_no_history_counts_the_configured_default() {
    let a = admit(&model(), &["brand-new".to_string()], &HashMap::new());
    assert_eq!(a.predicted_gb, 1.5);
    assert_eq!(a.unpredicted, vec!["brand-new".to_string()]);
}

#[test]
fn memory_based_parallelism_is_lower_than_core_based() {
    let m = model();
    let p = parallelism(&m, 12);
    assert_eq!(p.core_based, 12);
    assert_eq!(p.memory_based, Some(4)); // 19 GB / 4 GB per step, rounded down
    assert_eq!(p.parallel, 4);
    assert!(p.parallel < p.core_based);
    assert_eq!(p.cargo_jobs, Some(9)); // 19 / 2
}

#[test]
fn no_memory_keys_means_no_memory_limit_and_no_invented_number() {
    let m = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"x\"\ncores = 8\nmemory_gb = 16\n", "x").unwrap();
    let p = parallelism(&m, 6);
    assert_eq!((p.parallel, p.memory_based, p.cargo_jobs), (6, None, None));
}

#[test]
fn a_wsl_vm_cap_lowers_the_total() {
    assert_eq!(wslconfig_memory_gb("[wsl2]\nmemory=24GB\nprocessors=8\n"), Some(24.0));
    assert_eq!(wslconfig_memory_gb("[wsl2]\nmemory = 8192MB\n"), Some(8.0));
    assert_eq!(wslconfig_memory_gb("[wsl2]\nprocessors=8\n"), None);
    let m = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"w\"\nmemory_gb = 31\nwsl_memory_gb = 20\nmemory_reserve_gb = 4\n", "w").unwrap();
    assert_eq!(m.available_gb(), Some(16.0));
}

#[test]
fn step_peaks_are_stored_append_only_and_typical_memory_is_a_median() {
    let mut l = Ledger::open_in_memory().unwrap();
    assert_eq!(l.migrate().unwrap().1, 8);
    for (run, mb) in [("r1", 1000.0), ("r2", 3000.0), ("r3", 2000.0)] {
        l.record_step_peak(run, "test-api", mb, "cgroup", "2026-11-15T00:00:00Z").unwrap();
    }
    let h = l.step_memory_history().unwrap();
    assert_eq!(h["test-api"].len(), 3);
    assert_eq!(typical_mb(&h["test-api"]), 2000.0);
    assert!(l.conn.execute("UPDATE step_memory SET peak_mb = 1", []).is_err(), "append-only");
    assert!(l.conn.execute("DELETE FROM step_memory", []).is_err(), "append-only");
}


#[test]
fn unknown_total_memory_is_not_unlimited_memory() {
    let m = MemoryModel { configured: true, total_gb: None, vm_cap_gb: None, reserve_gb: 2.0, per_step_gb: Some(4.0), per_cargo_job_gb: None, step_default_gb: Some(1.0) };
    assert_eq!(m.available_gb(), None);
    let a = admit(&m, &["lint".to_string()], &HashMap::new());
    assert!(!a.admitted && a.rule.contains("unknown"), "{}", a.rule);
    assert!(cheap_set(&m, &["lint".to_string()], &HashMap::new()).is_empty());
    assert_eq!(parallelism(&m, 6).memory_based, None, "no memory number is invented for the parallelism either");
}

#[test]
fn a_step_with_no_history_needs_step_default_gb_in_the_config() {
    let m = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"x\"\nmemory_gb = 16\nmemory_reserve_gb = 2\n", "x").unwrap();
    let a = admit(&m, &["brand-new".to_string(), "lint".to_string()], &hist());
    assert!(!a.admitted && a.rule.contains("step_default_gb") && a.rule.contains("brand-new"), "{}", a.rule);
    // with history for every step it is admitted without a default
    assert!(admit(&m, &["lint".to_string()], &hist()).admitted);
    // and with the default set the unknown step counts it
    let m2 = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"x\"\nmemory_gb = 16\nmemory_reserve_gb = 2\nstep_default_gb = 3\n", "x").unwrap();
    let a = admit(&m2, &["brand-new".to_string()], &HashMap::new());
    assert!(a.admitted && a.predicted_gb == 3.0, "{}", a.rule);
}

#[test]
fn a_host_with_no_memory_keys_is_refused_not_silently_skipped() {
    // a misspelt or missing memory key must be loud: memory is checked unless the host says `memory_check = false`
    let m = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"x\"\ncores = 8\n", "x").unwrap();
    assert!(m.configured);
    let a = admit(&m, &["anything".to_string()], &HashMap::new());
    assert!(!a.admitted, "{}", a.rule);
}

#[test]
fn memory_check_false_is_the_only_way_to_opt_out() {
    let m = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"x\"\ncores = 8\nmemory_check = false\n", "x").unwrap();
    assert!(!m.configured);
    let a = admit(&m, &["anything".to_string()], &HashMap::new());
    assert!(a.admitted && a.rule.contains("memory_check = false"), "{}", a.rule);
    let m = MemoryModel::from_hosts_toml("[[hosts]]\nname = \"x\"\nmemory_check = true\n", "x").unwrap();
    assert!(m.configured);
}

#[test]
fn a_misspelt_memory_key_is_an_error_in_every_reader_of_hosts_toml() {
    // `memory_reserve_gbb` used to give a reserve of 0 and admit a 60 GB step on a 31 GB host
    let bad = "[[hosts]]\nname = \"pc\"\nmemory_gb = 31\nmemory_reserve_gbb = 12\nstep_default_gb = 1\n";
    let e = MemoryModel::from_hosts_toml(bad, "pc").unwrap_err().to_string();
    assert!(e.contains("unknown key") && e.contains("memory_reserve_gbb"), "{e}");
    let e = gatetrain_core::hold::host_capacity(bad, "pc").unwrap_err().to_string();
    assert!(e.contains("memory_reserve_gbb"), "{e}");
    let e = gatetrain_core::disk::disks_from_hosts_toml(bad, "pc").unwrap_err().to_string();
    assert!(e.contains("memory_reserve_gbb"), "{e}");
}
