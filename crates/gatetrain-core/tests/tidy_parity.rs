//! Byte parity of `check` and `tidy` with live Python, on the simulated hosts of tests/test_tidy.py.
//! Python runs the scenarios (tests/tidy_parity_driver.py) and prints one transcript; this test runs the same scenarios on
//! the Rust port over its own simulation and compares the text, scenario by scenario.
mod common;
use common::*;
use gatetrain_core::tidy::{cmd_check, cmd_tidy, CliOut};
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

pub fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn cli(sim: &Shared, args: &[&str], conf: &str) -> CliOut {
    let mk = |_h: &gatetrain_core::config::Host| -> Option<Box<dyn gatetrain_core::tidy::Runner>> { Some(Box::new(sim.clone())) };
    let now = Some(NOW as f64);
    let host = args.iter().position(|a| *a == "--host").map(|i| args[i + 1]);
    match args[0] {
        "check" => cmd_check(conf, host, args.contains(&"--json"), &mk, now),
        "tidy" => {
            let cls = args.iter().position(|a| *a == "--class").map(|i| args[i + 1]);
            cmd_tidy(conf, host, args.contains(&"--apply"), cls, args.contains(&"--auto"), &mk, now)
        }
        _ => unreachable!(),
    }
}

fn small_chain(sim: Shared) -> Shared {
    {
        let mut s = sim.s();
        s.free = 500.0;
        s.fs.iter_mut().find(|(k, _)| k == "/build/chain/debug/big.o").unwrap().1.size = MB;
    }
    sim
}

fn slow(p: &str) -> Shared {
    let s = linux_sim();
    s.s().slow = vec![p.to_string()];
    s
}

struct Scenario {
    name: &'static str,
    sim: Shared,
    args: Vec<&'static str>,
}

fn scenarios() -> Vec<Scenario> {
    let sc = |name, sim, args: &[&'static str]| Scenario { name, sim, args: args.to_vec() };
    vec![
        sc("check_text", linux_sim(), &["check"]),
        sc("check_json", linux_sim(), &["check", "--json"]),
        sc("check_ok", small_chain(linux_sim()), &["check"]),
        sc("check_slow_text", slow("/trash/old"), &["check"]),
        sc("check_slow_json", slow("/trash/old"), &["check", "--json"]),
        sc("check_all_slow", slow(""), &["check"]),
        sc("tidy_dry", linux_sim(), &["tidy"]),
        sc("tidy_dry_class", linux_sim(), &["tidy", "--class", "trash,containers"]),
        sc("tidy_auto", small_chain(linux_sim()), &["tidy", "--auto"]),
        sc("tidy_apply_all", linux_sim(), &["tidy", "--apply"]),
        sc("tidy_apply_worktrees_win", win_sim(), &["tidy", "--apply", "--class", "worktrees"]),
        sc("tidy_bad_class", linux_sim(), &["tidy", "--class", "bogus"]),
        sc("tidy_unknown_host", linux_sim(), &["tidy", "--host", "nope"]),
    ]
}

fn run_python(dir: &Path) -> String {
    let o = Command::new(python())
        .current_dir(repo())
        .env("PYTHONUTF8", "1")
        .env("GATETRAIN_PROGRESS", "0") // the library side is quiet; progress parity is the CLI-to-CLI test (tidy_real_parity.rs)
        .arg("tests/tidy_parity_driver.py")
        .arg(dir)
        .output()
        .expect("python must be installed to prove parity");
    assert!(o.status.success(), "python driver failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap()
}

#[test]
fn check_and_tidy_text_and_json_match_live_python_on_every_scenario() {
    let d = tempfile::tempdir().unwrap();
    let want = run_python(d.path());
    let want_by: Vec<&str> = want.split("### end\n").filter(|s| !s.is_empty()).collect();
    let all = scenarios();
    assert_eq!(want_by.len(), all.len(), "python and rust run the same number of scenarios");
    let mut bad = vec![];
    for (sc, want) in all.iter().zip(want_by) {
        let conf = d.path().join(format!("{}.toml", sc.name));
        let out = cli(&sc.sim, &sc.args, conf.to_str().unwrap());
        let log = d.path().join("l.jsonl");
        let mut got = format!("### {} rc={}\n{}--- stderr\n{}", sc.name, out.rc, out.stdout, out.stderr);
        if log.exists() {
            got += &format!("--- log\n{}", std::fs::read_to_string(&log).unwrap());
            std::fs::remove_file(&log).unwrap();
        }
        if got != want {
            bad.push(format!("scenario {}:\n--- python\n{}\n--- rust\n{}", sc.name, want, got));
        }
    }
    println!("parity: {} scenarios compared byte for byte", all.len());
    assert!(bad.is_empty(), "{}", bad.join("\n=====\n"));
}
