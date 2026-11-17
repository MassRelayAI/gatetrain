//! `ingest`, `report`, `trace`, `eta`, `next-tick`, `status`, `flaky` and `stop` through the built binary,
//! on the synthetic logs of tests/fixtures. (These were the Python suite's tests/test_gatetrain.py and test_ops.py.)
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}
fn fx(n: &str) -> String {
    repo().join("tests/fixtures").join(n).to_string_lossy().into_owned()
}
fn gt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(args).output().unwrap()
}
fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}
fn db() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.db").to_string_lossy().into_owned();
    (d, p)
}
fn q(db: &str, sql: &str) -> String {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0)).map(|v| match v {
        rusqlite::types::Value::Text(t) => t,
        rusqlite::types::Value::Integer(i) => i.to_string(),
        o => format!("{o:?}"),
    }).unwrap()
}

const PARTIAL_NOW: &str = "2026-11-11T18:20:00+07:00";
const NOW: &str = "2026-11-11T18:15:00+07:00";

#[test]
fn ingest_outcomes() {
    let (_d, db) = db();
    assert_eq!(code(&gt(&["ingest", &fx("run-one-red.log"), "--db", &db])), 0);
    gt(&["ingest", &fx("run-partial-red.log"), "--db", &db, "--now", PARTIAL_NOW]);
    gt(&["ingest", &fx("run-stopped.log"), "--db", &db, "--now", "2026-11-12T20:00:00+07:00"]);
    assert_eq!(q(&db, "select outcome from runs where id='run-one-red'"), "red", "the fixture carries one real red: e2e-001");
    assert_eq!(q(&db, "select outcome from run_current where run_id='run-partial-red'"), "running");
    assert_eq!(q(&db, "select outcome from run_current where run_id='run-stopped'"), "stopped");
    assert_eq!(q(&db, "select count(*) from steps where run_id='run-one-red'"), "466");
}

#[test]
fn a_complete_green_run_is_green_with_no_red() {
    let (_d, db) = db();
    let o = gt(&["ingest", &fx("run-green.log"), "--db", &db]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(out(&o).starts_with("ingested run-green: green, 466 steps, 0 red"), "{}", out(&o));
    assert_eq!(q(&db, "select outcome from runs where id='run-green'"), "green");
    assert_eq!(q(&db, "select count(*) from steps where run_id='run-green' and exit_code<>0"), "0");
}

#[test]
fn a_run_id_is_refused_twice_and_only_a_running_run_can_be_replaced() {
    let (_d, db) = db();
    assert_eq!(code(&gt(&["ingest", &fx("run-one-red.log"), "--db", &db])), 0);
    let o = gt(&["ingest", &fx("run-one-red.log"), "--db", &db]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("already in"), "{}", err(&o));
    let o = gt(&["ingest", &fx("run-one-red.log"), "--db", &db, "--replace"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("only a running or stopped run can be replaced"), "{}", err(&o));
    let p = fx("run-partial-red.log");
    assert_eq!(code(&gt(&["ingest", &p, "--db", &db, "--now", PARTIAL_NOW])), 0);
    assert_eq!(code(&gt(&["ingest", &p, "--db", &db, "--now", PARTIAL_NOW])), 2);
    assert_eq!(code(&gt(&["ingest", &p, "--db", &db, "--now", PARTIAL_NOW, "--replace"])), 0);
    // nothing was deleted: the replaced run has two snapshots
    assert_eq!(q(&db, "select count(*) from run_snapshots where run_id='run-partial-red'"), "2");
}

#[test]
fn ingest_writes_a_ledger_a_new_file_needs_no_migrate_for() {
    let (_d, db) = db();
    gt(&["ingest", &fx("run-stopped.log"), "--db", &db, "--now", "2026-11-12T20:00:00+07:00"]);
    assert_eq!(q(&db, "pragma user_version"), "8");
}

#[test]
fn trace_is_valid_and_lanes_do_not_overlap() {
    for (log, n) in [("run-one-red.log", 466), ("run-partial-red.log", 250)] {
        let o = gt(&["trace", &fx(log)]);
        assert_eq!(code(&o), 0);
        let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
        let ev = v.as_array().unwrap();
        assert_eq!(ev.len(), n);
        let mut lanes: std::collections::BTreeMap<i64, Vec<(i64, i64)>> = Default::default();
        for e in ev {
            for k in ["name", "ph", "ts", "dur", "pid", "tid", "args"] {
                assert!(e.get(k).is_some(), "{k}");
            }
            lanes.entry(e["tid"].as_i64().unwrap()).or_default().push((e["ts"].as_i64().unwrap(), e["dur"].as_i64().unwrap()));
        }
        for mut l in lanes.into_values() {
            l.sort();
            for w in l.windows(2) {
                assert!(w[0].0 + w[0].1 <= w[1].0);
            }
        }
        let cname = |n: &str| ev.iter().find(|e| e["name"] == n).unwrap()["cname"].as_str().unwrap().to_string();
        if log == "run-one-red.log" {
            assert_eq!(cname("e2e-001"), "terrible");
            assert_eq!(cname("build-packages"), "good");
        }
    }
}

#[test]
fn trace_and_report_read_a_run_back_from_the_ledger() {
    let (_d, db) = db();
    gt(&["ingest", &fx("run-one-red.log"), "--db", &db]);
    let o = gt(&["trace", "--run", "run-one-red", "--db", &db]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out(&o)).unwrap().as_array().unwrap().len(), 466);
    let from_log = out(&gt(&["report", &fx("run-one-red.log")]));
    let from_db = out(&gt(&["report", "--run", "run-one-red", "--db", &db]));
    let head = |s: &str| s.lines().take(4).collect::<Vec<_>>().join("\n");
    assert_eq!(head(&from_db), head(&from_log));
    // no log and no --run: the newest run
    assert_eq!(out(&gt(&["report", "--db", &db])), from_db);
    // a running run is read from its snapshot
    gt(&["ingest", &fx("run-partial-red.log"), "--db", &db, "--now", PARTIAL_NOW]);
    let o = gt(&["trace", "--run", "run-partial-red", "--db", &db]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&out(&o)).unwrap().as_array().unwrap().len(), 250);
}

#[test]
fn missing_run_and_empty_ledger_are_plain_errors() {
    let (_d, db) = db();
    let o = gt(&["report", "--db", &db]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("ledger is empty"));
    let o = gt(&["trace", "--run", "nope", "--db", &db]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("no run nope"));
    let o = gt(&["trace"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("give a log path or --run ID"));
}

#[test]
fn report_of_a_log_with_reds_carries_the_diagnosis_block() {
    let o = gt(&["report", &fx("run-one-red.log")]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).contains("\ndiagnosis:\n  "), "{}", out(&o));
    assert!(out(&o).contains("e2e-001"));
}

#[test]
fn diagnose_takes_the_parallelism_from_a_hosts_file() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path().join("hosts.toml");
    std::fs::write(&h, "[[hosts]]\nname = \"builder-1\"\nmax_parallel_steps = 64\nreach = \"local\"\n").unwrap();
    let o = gt(&["diagnose", &fx("run-one-red.log"), "--config", h.to_str().unwrap(), "--host", "builder-1"]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(out(&o).contains("below the configured 64"), "{}", out(&o));
}

#[test]
fn eta_with_history_and_without() {
    let (_d, db) = db();
    let o = gt(&["eta", &fx("run-partial-red.log"), "--db", &db, "--now", PARTIAL_NOW]);
    assert!(out(&o).contains("no history - rough estimate"), "{}", out(&o));
    gt(&["ingest", &fx("run-one-red.log"), "--db", &db]);
    let o = gt(&["eta", &fx("run-partial-red.log"), "--db", &db, "--now", PARTIAL_NOW]);
    assert_eq!(code(&o), 0);
    let t = out(&o);
    assert!(t.lines().any(|l| l.starts_with("eta ")) && t.contains("dominant") && t.contains("band"), "{t}");
    assert!(!t.contains("no history"));
}

#[test]
fn next_tick_cases() {
    let (_d, db) = db();
    gt(&["ingest", &fx("run-one-red.log"), "--db", &db]);
    let o = gt(&["next-tick", "--log", &fx("run-partial-red.log"), "--db", &db, "--now", NOW]);
    assert_eq!(out(&o).trim(), "5 min - a red appeared in the last 5 minutes: look again at the minimum");
    // mid-run, before the first red
    let text = std::fs::read_to_string(fx("run-partial-red.log")).unwrap();
    let lines: Vec<&str> = text.lines().take(78).collect();
    let d = tempfile::tempdir().unwrap();
    let mid = d.path().join("mid.log");
    std::fs::write(&mid, lines.join("\n") + "\n").unwrap();
    let last = out(&gt(&["parse", mid.to_str().unwrap(), "--json"]));
    let v: serde_json::Value = serde_json::from_str(&last).unwrap();
    let last_ts = v["last_ts"].as_str().unwrap();
    // one minute after the last line: a longer look than the minimum
    let now = bump(last_ts);
    let o = gt(&["next-tick", "--log", mid.to_str().unwrap(), "--db", &db, "--now", &now]);
    let m: i64 = out(&o).split_whitespace().next().unwrap().parse().unwrap();
    assert!(m > 5 && m <= 60, "{}", out(&o));
}

/// `last_ts` plus one minute, for the same offset (the log timestamps carry +07:00).
fn bump(ts: &str) -> String {
    // 2026-11-11T18:14:03+07:00 -> minute + 1 (the fixtures never sit on minute 59)
    let (head, tail) = ts.split_at(14);
    let min: u32 = tail[..2].parse().unwrap();
    assert!(min < 59);
    format!("{head}{:02}{}", min + 1, &tail[2..])
}

#[test]
fn next_tick_without_a_log_is_an_error() {
    let o = gt(&["next-tick"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("--log is required"));
}

#[test]
fn next_tick_over_several_logs_picks_the_minimum() {
    let (_d, db) = db();
    let mut singles = vec![];
    for lg in ["run-stopped.log", "run-partial-red.log"] {
        let o = out(&gt(&["next-tick", "--log", &fx(lg), "--db", &db, "--now", NOW]));
        let (m, why) = o.trim().split_once(" min - ").unwrap();
        singles.push((m.parse::<i64>().unwrap(), why.to_string(), lg.trim_end_matches(".log").to_string()));
    }
    let best = singles.iter().fold(&singles[0], |b, s| if s.0 < b.0 { s } else { b });
    let o = out(&gt(&["next-tick", "--log", &fx("run-stopped.log"), "--log", &fx("run-partial-red.log"), "--db", &db, "--now", NOW]));
    assert_eq!(o.trim(), format!("{} min - {} (decided by {})", best.0, best.1, best.2));
}

#[test]
fn status_has_one_row_per_run_and_the_earliest_look() {
    let o = gt(&["status", "--log", &fx("run-one-red.log"), "--log", &fx("run-partial-red.log"), "--log", &fx("run-stopped.log"), "--now", NOW]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let t = out(&o);
    for rid in ["run-one-red", "run-partial-red", "run-stopped"] {
        assert_eq!(t.lines().filter(|l| l.starts_with(rid)).count(), 1, "{t}");
    }
    assert!(t.contains("next look: 5 min (run-partial-red:"), "{t}");
}

#[test]
fn status_with_a_ledger_replaces_a_running_run_and_keeps_one_row() {
    let (_d, db) = db();
    gt(&["status", "--log", &fx("run-partial-red.log"), "--db", &db, "--now", NOW]);
    let o = gt(&["status", "--log", &fx("run-partial-red.log"), "--db", &db, "--now", NOW]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).lines().filter(|l| l.starts_with("run-partial-red")).count(), 1);
}

#[test]
fn flaky_names_a_step_red_in_one_run_and_green_in_another() {
    let (d, db) = db();
    let mk = |rc: i32| format!("branch b at abc1 on h\nSTEP_START s 2026-01-01T00:00:00+00:00\n=== s\nSTEP_TIME s 5s\nSTEP_EXIT s {rc}\nGATES_DONE\n");
    let o = gt(&["flaky", "--db", &db]);
    assert_eq!(out(&o).trim(), "no flaky steps");
    for (n, rc) in [("a", 1), ("b", 0)] {
        let p = d.path().join(format!("{n}.log"));
        std::fs::write(&p, mk(rc)).unwrap();
        assert_eq!(code(&gt(&["ingest", p.to_str().unwrap(), "--db", &db])), 0);
    }
    let o = out(&gt(&["flaky", "--db", &db]));
    assert!(o.contains("step  commit  green_runs  red_runs") && o.contains("s  abc1  1  1"), "{o}");
}

#[test]
fn a_ledger_of_an_older_version_is_read_and_written_after_ingest_migrates_it() {
    let (_d, db) = db();
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(&std::fs::read_to_string(repo().join("ledger/schema.sql")).unwrap()).unwrap();
    drop(c);
    assert_eq!(code(&gt(&["ingest", &fx("run-one-red.log"), "--db", &db])), 0);
    assert_eq!(q(&db, "pragma user_version"), "8");
}

#[test]
fn a_ledger_newer_than_this_binary_is_refused() {
    let (_d, db) = db();
    rusqlite::Connection::open(&db).unwrap().pragma_update(None, "user_version", 9).unwrap();
    let o = gt(&["ingest", &fx("run-one-red.log"), "--db", &db]);
    assert_ne!(code(&o), 0);
    assert!(err(&o).contains("newer"), "{}", err(&o));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn stop_is_not_supported_off_linux() {
    let o = gt(&["stop", "run-chain"]);
    assert_eq!(code(&o), 2);
    assert!(out(&o).contains("not supported"));
}

#[test]
fn every_clap_subcommand_answers_help_without_python() {
    let o = gt(&["--help"]);
    let t = out(&o);
    let start = t.find("Commands:").unwrap();
    let names: Vec<String> = t[start..].lines().skip(1).take_while(|l| l.starts_with("  ")).map(|l| l.split_whitespace().next().unwrap().to_string()).filter(|n| n != "help").collect();
    for want in ["parse", "ingest", "status", "next-tick", "flaky", "stop", "report", "trace", "eta", "diagnose", "check", "tidy", "watch", "doctor", "lanes", "generated", "batch", "hold", "running", "memory", "ledger", "log", "config"] {
        assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
    }
    for n in &names {
        let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args([n.as_str(), "--help"]).env("PATH", "").output().unwrap();
        assert_eq!(code(&o), 0, "`{n} --help`: {}", err(&o));
    }
}
