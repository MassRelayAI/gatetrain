//! `gatetrain hold` guards in real use: the memory rule without a step list, the disk budget, and the pinned working directory
//! (the runaway-build case, L70). Synthetic fixtures only.
use gatetrain_core::ledger::Ledger;
use std::path::Path;
use std::process::{Command, Output};

fn gt(d: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).current_dir(d).env_remove("GATETRAIN_PARALLEL").args(args).output().unwrap()
}
fn out(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}
fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}
fn git(d: &Path, args: &[&str]) {
    let o = Command::new("git").arg("-C").arg(d).args(["-c", "user.name=t", "-c", "user.email=t@example.com"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
fn decisions(db: &Path) -> Vec<(String, String)> {
    let l = Ledger::open(db).unwrap();
    let mut st = l.conn.prepare("SELECT kind, reason FROM decisions ORDER BY seq").unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect()
}

const PRINT_CWD: &str = "import os; print('CWD=' + os.path.realpath(os.getcwd()))";

// ---- the memory rule applies to a hold with no step list

const HOSTS: &str = "[[hosts]]\nname = \"pc\"\nmemory_gb = 31\nmemory_reserve_gb = 12\n";

#[test]
fn a_hold_without_a_step_list_is_admitted_on_its_own_recorded_chain_peak() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("hosts.toml"), HOSTS).unwrap();
    // an earlier run of the whole chain peaked at 26 GB; 31 - 12 = 19 GB are available
    let o = gt(d.path(), &["memory", "record", "--run", "r0", "--step", "(chain)", "--peak-mb", &(26 * 1024).to_string()]);
    assert!(o.status.success(), "{}", out(&o));
    let o = gt(d.path(), &["hold", "r1", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--", python(), "-c", "print('RAN')"]);
    assert_eq!(o.status.code(), Some(76), "{}", out(&o));
    assert!(out(&o).contains("REFUSED-MEMORY") && !out(&o).contains("RAN"), "{}", out(&o));
    assert!(decisions(&d.path().join("gatetrain.db")).iter().any(|(k, _)| k == "host.refuse-memory"));
    // with no history at all nothing is invented: step_default_gb is REQUIRED once memory is configured
    let d2 = tempfile::tempdir().unwrap();
    std::fs::write(d2.path().join("hosts.toml"), HOSTS).unwrap();
    let o = gt(d2.path(), &["hold", "r1", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--", python(), "-c", "print('RAN')"]);
    assert_eq!(o.status.code(), Some(76), "{}", out(&o));
    assert!(out(&o).contains("step_default_gb") && !out(&o).contains("RAN"), "{}", out(&o));
    // with the default set, the unknown chain counts it and fits
    std::fs::write(d2.path().join("hosts.toml"), format!("{HOSTS}step_default_gb = 4\n")).unwrap();
    let o = gt(d2.path(), &["hold", "r2", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--", python(), "-c", "print('RAN')"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&o).contains("RAN"));
}

// ---- the disk budget

#[test]
fn a_hold_is_refused_when_the_disk_would_drop_under_its_minimum_and_nothing_starts() {
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    // a minimum no disk can meet: the configured disk is already under it
    let hosts = format!("[[hosts]]\nname = \"pc\"\nmemory_check = false\n[[hosts.disks]]\npath = \"{}\"\nmin_free_gb = 100000000\n", fwd(d.path()));
    std::fs::write(d.path().join("hosts.toml"), hosts).unwrap();
    let o = gt(d.path(), &["hold", "r1", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--target-dir", &fwd(&target), "--", python(), "-c", "print('RAN')"]);
    assert_eq!(o.status.code(), Some(77), "{}", out(&o));
    let s = out(&o);
    assert!(s.contains("REFUSED-DISK") && s.contains("< min_free_gb"), "{s}");
    assert!(!s.contains("RAN") && !s.contains("HOLDS"), "the chain must not start: {s}");
    assert!(decisions(&d.path().join("gatetrain.db")).iter().any(|(k, r)| k == "host.refuse-disk" && r.contains("min_free_gb")));
    // a folder no configured disk holds is not this rule's business
    let other = tempfile::tempdir().unwrap();
    let hosts = "[[hosts]]\nname = \"pc\"\nmemory_check = false\n[[hosts.disks]]\npath = \"/nowhere/at/all\"\nmin_free_gb = 100000000\n";
    std::fs::write(other.path().join("hosts.toml"), hosts).unwrap();
    let o = gt(other.path(), &["hold", "r1", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--target-dir", &fwd(other.path()), "--", python(), "-c", "print('RAN')"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
}

#[test]
fn the_size_a_target_folder_reached_is_recorded_per_run_and_becomes_the_prediction() {
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("target");
    let hosts = format!("[[hosts]]\nname = \"pc\"\nmemory_check = false\n[[hosts.disks]]\npath = \"{}\"\nmin_free_gb = 0\n", fwd(d.path()));
    std::fs::write(d.path().join("hosts.toml"), hosts).unwrap();
    // the chain writes 3 MB into its build folder
    let script = format!("import os; os.makedirs(r'{0}', exist_ok=True); open(r'{0}/big.bin', 'wb').write(b'x' * 3 * 1048576)", fwd(&target));
    let o = gt(d.path(), &["hold", "r1", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--target-dir", &fwd(&target), "--", python(), "-c", &script]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let hist = Ledger::open(&d.path().join("gatetrain.db")).unwrap().step_memory_history().unwrap();
    let key = gatetrain_core::disk::step_key(&gatetrain_core::disk::canon(&fwd(&target)));
    let mb = hist[&key][0];
    assert!((mb - 3.0).abs() < 0.05, "recorded {mb} MB under {key}");
    assert!(out(&o).contains("TARGET r1"), "{}", out(&o));
    // the next run prints the prediction it admits on
    let o = gt(d.path(), &["hold", "r2", "--lock", "h.lock", "--hosts", "hosts.toml", "--host", "pc", "--target-dir", &fwd(&target), "--", python(), "-c", "print('RAN')"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&o).contains("predicted 0.0 GB"), "3 MB rounds to 0.0 GB: {}", out(&o));
}

// ---- the pinned working directory (the runaway-build case)

#[test]
fn a_hold_runs_the_chain_in_the_repo_root_it_was_given_whatever_the_callers_directory_is() {
    let d = tempfile::tempdir().unwrap();
    let repo = d.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let elsewhere = d.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    // the caller stands in the wrong folder (a script that fell back); the chain still runs in the repository root
    let o = gt(&elsewhere, &["hold", "r1", "--lock", "h.lock", "--repo-root", &fwd(&repo), "--", python(), "-c", PRINT_CWD]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let want = repo.canonicalize().unwrap();
    let want = want.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    let got = out(&o).lines().find_map(|l| l.strip_prefix("CWD=")).unwrap().trim().to_string();
    assert_eq!(got.replace('\\', "/").to_lowercase(), want.replace('\\', "/").to_lowercase(), "{}", out(&o));
}

#[test]
fn a_hold_whose_root_is_not_a_repository_top_level_is_refused_before_anything_runs() {
    let d = tempfile::tempdir().unwrap();
    let repo = d.path().join("repo");
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let plain = d.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    for (root, why) in [(plain, "not inside a git repository"), (repo.join("sub"), "not its top level"), (d.path().join("missing"), "is not a folder")] {
        let o = gt(d.path(), &["hold", "r1", "--lock", "h.lock", "--repo-root", &fwd(&root), "--", python(), "-c", "print('RAN')"]);
        assert_eq!(o.status.code(), Some(78), "{root:?}: {}", out(&o));
        assert!(out(&o).contains("REFUSED-CWD") && out(&o).contains(why), "{root:?}: {}", out(&o));
        assert!(!out(&o).contains("RAN") && !out(&o).contains("HOLDS"), "the chain must not start: {}", out(&o));
    }
    let ds = decisions(&d.path().join("gatetrain.db"));
    assert_eq!(ds.iter().filter(|(k, _)| k == "host.refuse-cwd").count(), 3, "{ds:?}");
    assert!(!d.path().join("h.lock").exists(), "no lock was taken");
}


// ---- the lock sidecar names the build folders, so tidy leaves them alone while the hold runs

#[test]
fn the_lock_sidecar_names_the_target_folders_of_the_run() {
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    // the chain reads its own sidecar while it holds the lock
    let script = "import json,sys; j=json.load(open(sys.argv[1] + '.info')); print('SIDECAR=' + json.dumps(j))";
    let lock = fwd(&d.path().join("h.lock"));
    let o = gt(d.path(), &["hold", "r1", "--lock", &lock, "--target-dir", &fwd(&target), "--", python(), "-c", script, &lock]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    let line = out(&o).lines().find_map(|l| l.strip_prefix("SIDECAR=").map(String::from)).unwrap();
    let j: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(j["state"], "held");
    assert_eq!(j["targets"][0].as_str().unwrap().to_lowercase(), gatetrain_core::disk::canon(&fwd(&target)).to_lowercase());
}
