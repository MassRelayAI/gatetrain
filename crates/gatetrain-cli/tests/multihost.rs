//! `gatetrain chain --hosts` end to end on one machine: the workers are bare repositories plus worktrees in temporary folders, ssh is a
//! fixture (`gt-testchild fakessh`, through GATETRAIN_SSH) that runs the agent command here. No real host is contacted.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

fn gt() -> String {
    env!("CARGO_BIN_EXE_gatetrain").to_string()
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn toml_str(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy())
}

struct World {
    _d: tempfile::TempDir,
    root: PathBuf,
    coord: PathBuf,
    hosts: PathBuf,
}

/// The coordinator's repository with `steps`, and workers: (name, tool value). A worker whose tool value is not "1" differs in its
/// fingerprint (the fingerprint command reads `git config gt.tool`).
fn world(steps: &str, workers: &[(&str, &str)]) -> World {
    world_with(steps, &workers.iter().map(|(n, t)| (*n, *t, "")).collect::<Vec<_>>())
}

/// Like [`world`], with raw TOML lines added to each worker's table.
fn world_with(steps: &str, workers: &[(&str, &str, &str)]) -> World {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    let coord = root.join("coord");
    std::fs::create_dir_all(&coord).unwrap();
    git(&coord, &["init", "-q"]);
    for (k, v) in [("user.email", "t@example.invalid"), ("user.name", "t"), ("core.autocrlf", "false"), ("gt.tool", "1")] {
        git(&coord, &["config", k, v]);
    }
    std::fs::write(coord.join(".gitignore"), "out/\n").unwrap();
    std::fs::write(coord.join("steps.toml"), steps).unwrap();
    git(&coord, &["add", ".gitignore", "steps.toml"]);
    git(&coord, &["commit", "-q", "-m", "steps"]);
    let mut hosts = "[multihost]\nfingerprint = ['git config --get gt.tool']\nlocal_tags = ['js']\n".to_string();
    for (name, tool, extra) in workers {
        let bare = root.join(format!("{name}.git"));
        let o = Command::new("git").args(["init", "-q", "--bare"]).arg(&bare).output().unwrap();
        assert!(o.status.success());
        git(&bare, &["config", "gt.tool", tool]);
        hosts += &format!(
            "\n[[hosts]]\nname = '{name}'\nroles = ['worker']\nreach = 'ssh {name}'\ntags = ['{}', 'js']\nslots = 2\nrepo = {}\nworkdir = {}\npush_url = {}\ngatetrain_bin = {}\n{extra}\n",
            platform(),
            toml_str(&bare),
            toml_str(&root.join(format!("{name}-work"))),
            toml_str(&bare),
            toml_str(Path::new(&gt())),
        );
    }
    let hf = root.join("hosts.toml");
    std::fs::write(&hf, hosts).unwrap();
    World { _d: d, root, coord, hosts: hf }
}

fn step(name: &str, args: &[&str]) -> String {
    let argv: Vec<String> = std::iter::once(child()).chain(args.iter().map(|s| s.to_string())).map(|a| format!("'{a}'")).collect();
    format!("[[step]]\nname = '{name}'\ntier = 'slow'\nargv = [{}]\n\n", argv.join(", "))
}

fn chain(w: &World, extra: &[&str], drop: &str) -> Output {
    let ssh = serde_json::to_string(&[child(), "fakessh".to_string()]).unwrap();
    Command::new(gt())
        .args(["chain", "--steps"])
        .arg(w.coord.join("steps.toml"))
        .arg("--out")
        .arg(w.coord.join("out"))
        .arg("--hosts")
        .arg(&w.hosts)
        .args(["--host", "coord", "--samples", "0", "--run-id", "mh-1"])
        .arg("--db")
        .arg(w.root.join("ledger.db"))
        .args(extra)
        .env("GATETRAIN_SSH", ssh)
        .env("FAKESSH_DROP", drop)
        .current_dir(&w.root)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn q(db: &Path, sql: &str) -> String {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0)).map(|v| match v {
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Text(t) => t,
        other => format!("{other:?}"),
    }).unwrap()
}

/// The host each step ran on, from the stamp.
fn stamp_hosts(w: &World) -> Vec<(String, String)> {
    let f = std::fs::read_dir(w.coord.join("out")).unwrap().map(|e| e.unwrap().path()).find(|p| p.file_name().unwrap().to_string_lossy().starts_with("stamp-")).expect("a stamp");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
    assert_eq!(v["multi_host"], true);
    v["steps"].as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap().to_string(), s["host"].as_str().unwrap().to_string())).collect()
}

fn six() -> String {
    (1..=6).map(|i| step(&format!("s{i}"), &["print", &format!("ok-{i}")])).collect()
}

#[test]
fn steps_spread_over_the_workers_a_mismatched_one_is_refused_and_one_stamp_names_every_host() {
    let w = world(&six(), &[("w1", "1"), ("w2", "1"), ("bad", "2")]);
    let o = chain(&w, &["--local-slots", "1"], "");
    let log = text(&o);
    assert_eq!(o.status.code(), Some(0), "{log}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(log.contains("HOST_ACCEPTED w1") && log.contains("HOST_ACCEPTED w2"), "{log}");
    let refused = log.lines().find(|l| l.starts_with("HOST_REFUSED bad")).unwrap_or_else(|| panic!("{log}"));
    assert!(refused.contains("gt.tool"), "{refused}");
    let hosts = stamp_hosts(&w);
    assert_eq!(hosts.len(), 6);
    assert!(hosts.iter().all(|(_, h)| ["coord", "w1", "w2"].contains(&h.as_str())), "{hosts:?}");
    assert!(hosts.iter().any(|(_, h)| h != "coord"), "some step ran on a worker: {hosts:?}");
    for i in 1..=6 {
        let full = std::fs::read_to_string(w.coord.join("out").join(format!("s{i}.full"))).unwrap();
        assert!(full.contains(&format!("ok-{i}")), "the output of s{i} is in the coordinator's out: {full:?}");
    }
    let db = w.root.join("ledger.db");
    assert_eq!(q(&db, "SELECT COUNT(*) FROM steps WHERE run_id='mh-1' AND host IS NOT NULL AND exit_code=0"), "6");
    assert_eq!(q(&db, "SELECT COUNT(*) FROM run_hosts WHERE run_id='mh-1' AND accepted=0 AND host='bad'"), "1");
    // the log is an ordinary chain log
    let lf = w.root.join("run.log");
    std::fs::write(&lf, &log).unwrap();
    let p = Command::new(gt()).arg("parse").arg(&lf).arg("--json").output().unwrap();
    assert_eq!(p.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&p.stdout).unwrap();
    assert_eq!(v["steps"].as_array().map(Vec::len), Some(6), "{v}");
}

#[test]
fn a_worker_that_drops_every_step_is_removed_and_its_steps_pass_elsewhere() {
    let w = world(&six(), &[("w1", "1"), ("w2", "1")]);
    let o = chain(&w, &["--local-slots", "1"], "w1");
    let log = text(&o);
    assert_eq!(o.status.code(), Some(0), "{log}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(log.lines().any(|l| l.starts_with("STEP_HOST_LOST") && l.contains(" w1 ")), "{log}");
    assert!(log.contains("HOST_REMOVED w1"), "{log}");
    let hosts = stamp_hosts(&w);
    assert!(hosts.iter().all(|(_, h)| h != "w1"), "no step counts from the host that dropped it: {hosts:?}");
    // two slots: the second loss may arrive after a third step was handed out, and that one is lost too
    let lost = q(&w.root.join("ledger.db"), "SELECT COUNT(*) FROM host_events WHERE run_id='mh-1' AND event='lost' AND host='w1'");
    assert!(lost == "2" || lost == "3", "{lost}");
}

#[test]
fn a_red_step_on_a_worker_is_red_once() {
    let steps = step("red", &["exit", "3"]) + &step("ok", &["print", "fine"]);
    let w = world(&steps, &[("w1", "1")]);
    let o = chain(&w, &["--local-slots", "0"], "");
    let log = text(&o);
    assert_eq!(o.status.code(), Some(1), "{log}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(log.contains("RED_SUMMARY: red(rc=3)"), "{log}");
    assert_eq!(log.lines().filter(|l| l.starts_with("STEP_HOST red ")).count(), 1, "{log}");
    assert!(!std::fs::read_dir(w.coord.join("out")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("stamp-")));
}

#[test]
fn a_local_run_refuses_a_step_whose_tags_it_lacks() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("steps.toml");
    std::fs::write(&f, step("gpu", &["print", "x"]).replace("tier = 'slow'", "tier = 'slow'\nrequires = ['gpu']")).unwrap();
    let o = Command::new(gt()).args(["chain", "--steps"]).arg(&f).arg("--out").arg(d.path().join("out")).args(["--samples", "0"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("requires the tag \"gpu\""));
    let ok = Command::new(gt()).args(["chain", "--steps"]).arg(&f).arg("--out").arg(d.path().join("out")).args(["--samples", "0", "--tags", "gpu"]).output().unwrap();
    assert_eq!(ok.status.code(), Some(0), "{}", String::from_utf8_lossy(&ok.stderr));
}

#[test]
fn a_failing_prepare_refuses_the_host_and_a_passing_one_is_remembered_by_its_key() {
    let w = world_with(&six(), &[("broken", "1", "prepare = 'git no-such-subcommand'"), ("ready", "1", "prepare = 'git --version'\nprepare_key = ['steps.toml']")]);
    let o = chain(&w, &["--local-slots", "0"], "");
    let log = text(&o);
    assert_eq!(o.status.code(), Some(0), "{log}\n{}", String::from_utf8_lossy(&o.stderr));
    let refused = log.lines().find(|l| l.starts_with("HOST_REFUSED broken")).unwrap_or_else(|| panic!("{log}"));
    assert!(refused.contains("prepare"), "{refused}");
    assert!(stamp_hosts(&w).iter().all(|(_, h)| h == "ready"));
    let markers = walk(&w.root.join("ready.git")).iter().filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("prepared-")).count();
    assert_eq!(markers, 2, "the success is remembered under the key, once per checkout (slots = 2)");
    assert!(w.root.join("ready-work-slot1").is_dir(), "the second slot has its own checkout");
}

fn walk(d: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn stamps_in(w: &World) -> Vec<String> {
    std::fs::read_dir(w.coord.join("out")).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("stamp-")).collect()).unwrap_or_default()
}

#[test]
fn a_red_run_leaves_no_stamp_not_the_local_agents_and_not_an_old_one() {
    // the only worker is refused, so both steps run on the coordinator's own slot through the agent
    let steps = step("red", &["exit", "3"]) + &step("ok", &["print", "fine"]);
    let w = world(&steps, &[("bad", "2")]);
    let sha = String::from_utf8(Command::new("git").arg("-C").arg(&w.coord).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    std::fs::create_dir_all(w.coord.join("out")).unwrap();
    std::fs::write(w.coord.join("out").join(format!("stamp-{}.json", sha.trim())), "{\"sha\": \"old\"}").unwrap();
    let o = chain(&w, &["--local-slots", "1"], "");
    let log = text(&o);
    assert_eq!(o.status.code(), Some(1), "{log}");
    assert!(log.contains("HOST_REFUSED bad"), "{log}");
    assert_eq!(stamps_in(&w), Vec::<String>::new(), "{log}");
}

#[test]
fn a_step_that_dirties_a_worker_checkout_is_red_and_only_that_step() {
    let mut steps = step("dirt", &["marker", "dirt.txt", "0.2"]);
    for i in 1..=4 {
        steps += &step(&format!("c{i}"), &["sleep", "0.3"]);
    }
    let w = world(&steps, &[("w1", "1")]);
    let o = chain(&w, &["--local-slots", "0"], "");
    let log = text(&o);
    assert_eq!(o.status.code(), Some(1), "{log}\n{}", String::from_utf8_lossy(&o.stderr));
    let summary = log.lines().find(|l| l.starts_with("RED_SUMMARY:")).unwrap();
    assert!(summary.starts_with("RED_SUMMARY: dirt(") && summary.matches("(rc=").count() == 1, "only the step that dirtied its checkout: {log}");
    assert!(log.lines().any(|l| l.starts_with("RED_NOW dirt") && l.contains("dirty")), "{log}");
    assert_eq!(stamps_in(&w), Vec::<String>::new());
}
