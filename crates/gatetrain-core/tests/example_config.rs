//! config/hosts.example.toml is the template people copy: it must load, and say what its comments say.
//! (Ported from tests/test_ops.py TestConfig.test_example_toml_loads.)
use gatetrain_core::config;
use std::path::Path;

#[test]
fn the_example_hosts_file_loads_with_the_values_it_documents() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/hosts.example.toml");
    let cfg = config::load(&p).unwrap();
    assert_eq!(cfg.profile, "mixed");
    assert_eq!(cfg.hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["builder-1", "workstation-1", "cloud-runner", "linux-worker", "laptop-js"]);
    assert_eq!(cfg.hosts[0].disks[0].min_free_gb.f(), 100.0);
    assert!(cfg.hosts[0].disks[0].fast);
    assert_eq!(cfg.hosts[0].disks[0].r#use, ["chain", "cache"]);
    assert!(cfg.hosts[1].disks[1].removable);
    assert_eq!(cfg.needs.chain_gb.f(), 120.0);
    assert_eq!(cfg.models.len(), 3);
    assert!(!cfg.hosts[2].enabled);
    // the multi-host workers ship disabled, so a copied file never reaches a machine by accident
    for w in &cfg.hosts[3..] {
        assert!(w.roles == ["worker"] && !w.enabled && w.worker.repo.is_some() && w.worker.workdir.is_some(), "{}", w.name);
    }
    assert_eq!(cfg.hosts[4].worker.tags, ["js"]);
    assert_eq!(cfg.hosts[4].worker.command_prefix, ["caffeinate", "-i"]);
    let mh = gatetrain_core::multihost::mh_config(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(mh.probes_for(&["js".into()]), ["node -v"]);
    assert_eq!(mh.max_host_failures, 2);
}
