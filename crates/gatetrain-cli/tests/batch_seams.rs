//! `gatetrain batch` with the real seams (the holder's lock, the memory rule, the disk budget, the working directory, doctor): a batch is
//! refused with the exit codes of `gatetrain hold`. Synthetic fixtures only.
use std::path::Path;
use std::process::{Command, Output};

fn gt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).env_remove("GATETRAIN_PARALLEL").args(args).output().unwrap()
}
fn out(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// A repository with one lane, a ledger and a batch `b1` that is stacked and settled, so the next step is the preflight (admission "light").
struct Fx {
    _t: tempfile::TempDir,
    root: std::path::PathBuf,
    db: String,
}

impl Fx {
    fn new(cfg_extra: &str) -> Fx {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        let r = root.join("repo");
        std::fs::create_dir_all(&r).unwrap();
        let g = |a: &[&str]| {
            let o = Command::new("git").arg("-C").arg(&r).args(a).output().unwrap();
            assert!(o.status.success(), "{a:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@example.com"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        std::fs::write(r.join("a"), "a").unwrap();
        g(&["add", "--", "a"]);
        g(&["commit", "-q", "-m", "init"]);
        g(&["switch", "-q", "-c", "lane1"]);
        std::fs::write(r.join("b"), "b").unwrap();
        g(&["add", "--", "b"]);
        g(&["commit", "-q", "-m", "feat: b"]);
        g(&["switch", "-q", "main"]);
        let db = fwd(&root.join("g.db"));
        std::fs::write(root.join("gatetrain.toml"), format!("[preflight]\ncommand = [\"sh\", \"-c\", \"true\"]\n[chain]\ncommand = [\"sh\", \"-c\", \"true\"]\n{cfg_extra}")).unwrap();
        let fx = Fx { _t: t, root, db };
        assert!(gt(&["ledger", "migrate", "--db", &fx.db]).status.success());
        let o = gt(&["batch", "new", "b1", "--base", "main", "--workdir", &fwd(&r), "lane1", "--db", &fx.db, "--config", &fx.cfg()]);
        assert!(o.status.success(), "{}", out(&o));
        for _ in 0..2 {
            assert_eq!(gt(&["batch", "advance", "b1", "--db", &fx.db, "--config", &fx.cfg()]).status.code(), Some(0)); // stacked, then settled
        }
        fx
    }
    fn cfg(&self) -> String {
        fwd(&self.root.join("gatetrain.toml"))
    }
    fn advance(&self, extra: &[&str]) -> Output {
        let mut a = vec!["batch", "advance", "b1", "--db", &self.db, "--config"];
        let c = self.cfg();
        a.push(&c);
        a.extend(extra);
        gt(&a)
    }
}

#[test]
fn a_batch_whose_working_directory_is_not_the_repo_root_is_refused_with_78() {
    let t = tempfile::tempdir().unwrap();
    let plain = fwd(t.path());
    let fx = Fx::new(&format!("repo_root = \"{plain}\"\n"));
    let o = fx.advance(&[]);
    assert_eq!(o.status.code(), Some(78), "{}", out(&o));
    assert!(out(&o).contains("host.refuse-cwd") && out(&o).contains("not inside a git repository"), "{}", out(&o));
    assert!(out(&gt(&["batch", "status", "b1", "--db", &fx.db])).contains("b1: settled"), "the batch did not move");
}

#[test]
fn a_batch_that_would_drop_the_disk_under_its_minimum_is_refused_with_77() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let fx = Fx::new(&format!("target_dirs = [\"{}\"]\n", fwd(&target)));
    let hosts = fx.root.join("hosts.toml");
    std::fs::write(&hosts, format!("[[hosts]]\nname = \"h\"\nreach = \"local\"\nmemory_check = false\n[[hosts.disks]]\npath = \"{}\"\nmin_free_gb = 100000000\n", fwd(t.path()))).unwrap();
    let o = fx.advance(&["--hosts", &fwd(&hosts), "--host", "h"]);
    assert_eq!(o.status.code(), Some(77), "{}", out(&o));
    assert!(out(&o).contains("host.refuse-disk") && out(&o).contains("< min_free_gb"), "{}", out(&o));
}

#[test]
fn a_batch_whose_chain_does_not_fit_the_memory_is_refused_with_76() {
    let fx = Fx::new("");
    let hosts = fx.root.join("hosts.toml");
    std::fs::write(&hosts, "[[hosts]]\nname = \"h\"\nreach = \"local\"\nmemory_gb = 8\nmemory_reserve_gb = 2\nstep_default_gb = 100\n").unwrap();
    let o = fx.advance(&["--hosts", &fwd(&hosts), "--host", "h"]);
    assert_eq!(o.status.code(), Some(76), "{}", out(&o));
    assert!(out(&o).contains("host.refuse-memory") && out(&o).contains("REFUSED"), "{}", out(&o));
}

#[test]
fn a_host_that_fails_doctor_refuses_the_batch_with_79_and_a_host_that_passes_does_not() {
    let fx = Fx::new("");
    let hosts = fx.root.join("hosts.toml");
    // the disk is below its minimum: doctor FAILs on it
    std::fs::write(&hosts, format!("[[hosts]]\nname = \"h\"\nreach = \"local\"\nmemory_check = false\n[[hosts.disks]]\npath = \"{}\"\nmin_free_gb = 100000000\n", fwd(&fx.root))).unwrap();
    let o = fx.advance(&["--hosts", &fwd(&hosts), "--host", "h"]);
    assert_eq!(o.status.code(), Some(79), "{}", out(&o));
    assert!(out(&o).contains("host.refuse-doctor") && out(&o).contains("disk"), "{}", out(&o));
    // the same batch with doctor switched off in gatetrain.toml goes on to the preflight
    let c = std::fs::read_to_string(fx.root.join("gatetrain.toml")).unwrap();
    std::fs::write(fx.root.join("gatetrain.toml"), format!("{c}[batch]\ndoctor = false\n")).unwrap();
    let o = fx.advance(&["--hosts", &fwd(&hosts), "--host", "h"]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&gt(&["batch", "status", "b1", "--db", &fx.db])).contains("b1: preflighted"));
}
