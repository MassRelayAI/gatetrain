//! Byte parity of `check` and `tidy` with the transcript the Python version printed, on simulated hosts.
//! tests/golden/tidy_parity.txt holds that transcript (the scratch folder is written `{DIR}`); this test runs the same scenarios
//! over the Rust simulation and compares the text, scenario by scenario. The Python version is retired; the file is the proof.
mod common;
use common::*;
use gatetrain_core::tidy::{cmd_check, cmd_tidy, CliOut};
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
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
    win: bool,
}

fn scenarios() -> Vec<Scenario> {
    let sc = |name, sim, args: &[&'static str]| Scenario { name, sim, args: args.to_vec(), win: false };
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
        {
            let mut w = sc("tidy_apply_worktrees_win", win_sim(), &["tidy", "--apply", "--class", "worktrees"]);
            w.win = true;
            w
        },
        sc("tidy_bad_class", linux_sim(), &["tidy", "--class", "bogus"]),
        sc("tidy_unknown_host", linux_sim(), &["tidy", "--host", "nope"]),
    ]
}

/// The log lines and the plan carry the `rule` the Python version did not have, and the log lines the `why`; the transcript predates both.
fn without_rules(t: &str) -> String {
    use regex::Regex;
    let a = Regex::new(r#"(?m)^\s*"rule": "[^"]*",\n"#).unwrap();
    let b = Regex::new(r#", "rule": "(?:[^"\\]|\\.)*""#).unwrap();
    let c = Regex::new(r#", "why": "(?:[^"\\]|\\.)*""#).unwrap();
    c.replace_all(&b.replace_all(&a.replace_all(t, ""), ""), "").into_owned()
}

#[test]
fn check_and_tidy_text_and_json_match_the_recorded_transcript_on_every_scenario() {
    let d = tempfile::tempdir().unwrap();
    let want = std::fs::read_to_string(repo().join("tests/golden/tidy_parity.txt")).unwrap().replace("\r\n", "\n");
    let dir = d.path().to_string_lossy().replace('\\', "/");
    let norm = |t: String| without_rules(&t.replace(&dir, "{DIR}").replace(&dir.replace('/', "\\"), "{DIR}"));
    let want_by: Vec<&str> = want.split("### end\n").filter(|s| !s.is_empty()).collect();
    let all = scenarios();
    assert_eq!(want_by.len(), all.len(), "the transcript and the test hold the same number of scenarios");
    let mut bad = vec![];
    for (sc, want) in all.iter().zip(want_by) {
        let log = d.path().join("l.jsonl");
        let extra = if sc.win { win_extra() } else { serde_json::json!({}) };
        let mut extra = extra;
        extra.as_object_mut().unwrap().insert("log".into(), serde_json::json!(log.to_string_lossy().replace('\\', "/")));
        let written = write_conf(d.path(), extra, true);
        let mut text = std::fs::read_to_string(&written).unwrap();
        if sc.win {
            text = text.replace("os = \"linux\"", "os = \"windows\"");
        }
        std::fs::remove_file(&written).unwrap();
        let conf = d.path().join(format!("{}.toml", sc.name));
        std::fs::write(&conf, text).unwrap();
        let out = cli(&sc.sim, &sc.args, conf.to_str().unwrap());
        let mut got = format!("### {} rc={}\n{}--- stderr\n{}", sc.name, out.rc, out.stdout, out.stderr);
        if log.exists() {
            got += &format!("--- log\n{}", std::fs::read_to_string(&log).unwrap());
            std::fs::remove_file(&log).unwrap();
        }
        let got = norm(got);
        if got != want {
            bad.push(format!("scenario {}:\n--- recorded\n{}\n--- rust\n{}", sc.name, want, got));
        }
    }
    println!("parity: {} scenarios compared byte for byte", all.len());
    assert!(bad.is_empty(), "{}", bad.join("\n=====\n"));
}
