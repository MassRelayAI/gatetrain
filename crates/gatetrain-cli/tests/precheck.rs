//! `gatetrain batch precheck` on real temporary git repositories (no network; the "remote" is a local bare repository).
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(r: &Path, a: &[&str]) {
    let o = Command::new("git").arg("-C").arg(r).args(a).output().unwrap();
    assert!(o.status.success(), "{a:?}: {}", String::from_utf8_lossy(&o.stderr));
}
fn write(r: &Path, f: &str, c: &str) {
    std::fs::write(r.join(f), c).unwrap();
}
fn commit(r: &Path, f: &str, c: &str, m: &str) {
    write(r, f, c);
    git(r, &["add", "--", f]);
    git(r, &["commit", "-q", "-m", m]);
}
/// main has `shared` and `x`; lane l1 edits shared; l2 edits shared differently (conflicts with l1); l3 is clean and adds `new.txt`.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let r = t.path().join("repo");
    let bare = t.path().join("origin.git");
    std::fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q", "-b", "main"]);
    for (k, v) in [("user.email", "t@example.com"), ("user.name", "t"), ("commit.gpgsign", "false")] {
        git(&r, &["config", k, v]);
    }
    commit(&r, "shared", "base\n", "init");
    commit(&r, "x", "x\n", "x");
    for (b, f, c) in [("l1", "shared", "one\n"), ("l2", "shared", "two\n"), ("l3", "new.txt", "n\n")] {
        git(&r, &["switch", "-q", "-c", b, "main"]);
        commit(&r, f, c, b);
    }
    git(&r, &["switch", "-q", "main"]);
    git(t.path(), &["init", "-q", "--bare", "-b", "main", "origin.git"]);
    git(&r, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&r, &["push", "-q", "origin", "main"]);
    (t, r, bare)
}
fn run(t: &Path, r: &Path, toml: &str, args: &[&str]) -> (i32, String) {
    let cfg = t.join("gt.toml");
    std::fs::write(&cfg, toml).unwrap();
    let o: Output = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["batch", "precheck", "--config", cfg.to_str().unwrap(), "--repo", r.to_str().unwrap(), "--base", "main"])
        .args(args)
        .output()
        .unwrap();
    (o.status.code().unwrap(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

#[test]
fn green_all_clear() {
    let (t, r, _b) = fixture();
    let (c, o) = run(t.path(), &r, "[precheck]\nlane_commands=[\"echo ok\"]\nhost_commands=[\"echo ready\"]\nknown_reds=[{name=\"k\",detect=\"exit 0\",hint=\"h\"}]\n", &["l1", "l3"]);
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("0 red, 0 warning -> GREEN"), "{o}");
}
#[test]
fn pairwise_conflict_found_with_file_name() {
    let (t, r, _b) = fixture();
    let (c, o) = run(t.path(), &r, "", &["l1", "l2", "l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("RED  pair-conflict: l1 x l2: shared"), "{o}");
    assert!(!o.contains("l3 x"), "{o}");
}
#[test]
fn lane_vs_base_conflict_found() {
    let (t, r, _b) = fixture();
    commit(&r, "shared", "main moved\n", "main moves");
    git(&r, &["push", "-q", "origin", "main"]);
    let (c, o) = run(t.path(), &r, "", &["l1", "l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("base-conflict: l1 x main: shared"), "{o}");
    assert!(!o.contains("l3 x main"), "{o}");
}
#[test]
fn red_lane_command_with_tail_and_lane_env() {
    let (t, r, _b) = fixture();
    // new.txt exists only in l3's checkout: the command proves it ran in the lane's own tree
    let (c, o) = run(t.path(), &r, "[precheck]\nlane_commands=[\"echo boom-line && exit 3\"]\n", &["l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("lane-command: l3:") && o.contains("exited 3") && o.contains("boom-line"), "{o}");
    let file_cmd = if cfg!(windows) { "if exist new.txt (echo has) else (exit 1)" } else { "test -f new.txt && echo has" };
    let toml = format!("[precheck]\nlane_commands=['{file_cmd}']\n");
    let (c, o) = run(t.path(), &r, &toml, &["l3"]);
    assert_eq!(c, 0, "{o}");
    let (c, o) = run(t.path(), &r, &toml, &["l1"]);
    assert_eq!(c, 1, "{o}");
}
#[test]
fn host_main_behind_remote() {
    let (t, r, bare) = fixture();
    // advance the remote from a second clone
    let c2 = t.path().join("clone2");
    git(t.path(), &["clone", "-q", bare.to_str().unwrap(), "clone2"]);
    for (k, v) in [("user.email", "t@example.com"), ("user.name", "t"), ("commit.gpgsign", "false")] {
        git(&c2, &["config", k, v]);
    }
    commit(&c2, "y", "y\n", "remote moves");
    git(&c2, &["push", "-q", "origin", "main"]);
    let (c, o) = run(t.path(), &r, "", &["l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("host-base: local main") && o.contains("not at") && o.contains("origin/main"), "{o}");
    // once fetched, the wording is exact
    git(&r, &["fetch", "-q", "origin"]);
    let (c, o) = run(t.path(), &r, "", &["l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("is behind origin/main"), "{o}");
}
#[test]
fn untracked_file_where_a_lane_adds_one() {
    let (t, r, _b) = fixture();
    write(&r, "new.txt", "stray\n");
    let (c, o) = run(t.path(), &r, "", &["l1", "l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("untracked-collision: l3 adds new.txt"), "{o}");
}
#[test]
fn silent_command_is_a_warning_not_a_red() {
    let (t, r, _b) = fixture();
    let (c, o) = run(t.path(), &r, "[precheck]\nhost_commands=[\"cd .\"]\n", &["l3"]);
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("WARN host-command") && o.contains("no output"), "{o}");
}
#[test]
fn known_red_reported_with_hint() {
    let (t, r, _b) = fixture();
    let (c, o) = run(t.path(), &r, "[precheck]\nknown_reds=[{name=\"stale-packages\",detect=\"exit 1\",hint=\"run build:packages\"}]\n", &["l3"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("known-red: stale-packages: run build:packages"), "{o}");
}
#[test]
fn all_findings_at_once_and_repo_unchanged() {
    let (t, r, _b) = fixture();
    write(&r, "new.txt", "stray\n");
    let head = |r: &Path| String::from_utf8(Command::new("git").arg("-C").arg(r).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    let status = |r: &Path| String::from_utf8(Command::new("git").arg("-C").arg(r).args(["status", "--porcelain"]).output().unwrap().stdout).unwrap();
    let (h, s) = (head(&r), status(&r));
    let (c, o) = run(t.path(), &r, "[precheck]\nlane_commands=[\"exit 4\"]\nknown_reds=[{name=\"k\",detect=\"exit 1\",hint=\"hh\"}]\n", &["l1", "l2", "l3"]);
    assert_eq!(c, 1, "{o}");
    for m in ["pair-conflict", "lane-command: l1", "untracked-collision", "known-red"] {
        assert!(o.contains(m), "{m} missing in {o}");
    }
    assert_eq!((head(&r), status(&r)), (h, s));
}
#[test]
fn bad_config_is_exit_2_and_unknown_ref_is_red() {
    let (t, r, _b) = fixture();
    let (c, o) = run(t.path(), &r, "[precheck]\nlane_commands = 5\n", &["l3"]);
    assert_eq!(c, 2, "{o}");
    let (c, o) = run(t.path(), &r, "", &["nope"]);
    assert_eq!(c, 1, "{o}");
    assert!(o.contains("refs: nope"), "{o}");
}
#[test]
fn self_test_reports_the_known_conflict() {
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["batch", "precheck", "--self-test"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("known conflicting pair"));
}
