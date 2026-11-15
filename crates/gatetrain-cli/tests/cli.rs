//! Integration tests: run the built `gatetrain` binary.
use gatetrain_core::{declog, ledger::Ledger};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn gt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(args).env("GATETRAIN_PYROOT", repo()).env("PYTHONIOENCODING", "utf-8").output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn corpus_cases() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/corpus/parse")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    v.sort();
    assert!(v.len() >= 10, "corpus too small: {}", v.len());
    v
}

#[test]
fn parse_matches_the_committed_python_output_on_every_case() {
    for case in corpus_cases() {
        let o = gt(&["parse", case.join("input.log").to_str().unwrap(), "--json"]);
        assert!(o.status.success(), "{}: {}", case.display(), String::from_utf8_lossy(&o.stderr));
        let want = std::fs::read(case.join("expected-stdout")).unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&want), "case {}", case.display());
        assert_eq!(o.stdout, want, "bytes, case {}", case.display());
    }
}

#[test]
fn parse_matches_the_live_python_parser_on_every_case() {
    for case in corpus_cases() {
        let log = case.join("input.log");
        let py = Command::new(python()).arg(repo().join("tests/corpus/parse_dump.py")).arg(&log).output().expect("python is needed for the parity test");
        assert!(py.status.success(), "{}: {}", case.display(), String::from_utf8_lossy(&py.stderr));
        let rs = gt(&["parse", log.to_str().unwrap(), "--json"]);
        assert_eq!(rs.stdout, py.stdout, "python vs rust, case {}", case.display());
    }
}

fn old_ledger(dir: &Path) -> PathBuf {
    // A version-0 ledger as the Python side writes it: the base schema, user_version 0, a run with steps.
    let p = dir.join("old.db");
    let c = rusqlite::Connection::open(&p).unwrap();
    c.execute_batch(&std::fs::read_to_string(repo().join("ledger/schema.sql")).unwrap()).unwrap();
    c.execute_batch(
        "INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','builder-1','abc','2026-11-11T09:00:00+07:00','2026-11-11T09:01:00+07:00','green');
         INSERT INTO steps(run_id,name,started_at,duration_sec,exit_code,cached) VALUES('r1','lint','2026-11-11T09:00:00+07:00',12.5,0,0),('r1','test',NULL,30,0,0);",
    )
    .unwrap();
    assert_eq!(c.query_row::<i64, _, _>("PRAGMA user_version", [], |r| r.get(0)).unwrap(), 0);
    p
}

#[test]
fn ledger_migrate_old_format_keeps_rows_and_adds_v1() {
    let d = tempfile::tempdir().unwrap();
    let db = old_ledger(d.path());
    // reading the old file works before migration
    let l = Ledger::open(&db).unwrap();
    assert_eq!(l.runs().unwrap().len(), 1);
    assert_eq!(l.step_count("r1").unwrap(), 2);
    // writes are refused until migrated
    assert!(l.record_snapshot("r2", &Default::default(), "running", None, "t").is_err());
    drop(l);

    let o = gt(&["ledger", "migrate", "--db", db.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(out(&o).contains("version 0 -> 3"), "{}", out(&o));

    let l = Ledger::open(&db).unwrap();
    assert_eq!(l.version().unwrap(), 3);
    assert_eq!(l.runs().unwrap().len(), 1, "old rows survive");
    assert_eq!(l.step_count("r1").unwrap(), 2);
    let tables: i64 = l.conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name IN ('run_snapshots','decisions','run_current')", [], |r| r.get(0)).unwrap();
    assert_eq!(tables, 3);
    drop(l);

    // idempotent
    let o = gt(&["ledger", "migrate", "--db", db.to_str().unwrap()]);
    assert!(out(&o).contains("already at version 3"), "{}", out(&o));
}

#[test]
fn ledger_migrate_refuses_a_newer_version() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("future.db");
    rusqlite::Connection::open(&db).unwrap().pragma_update(None, "user_version", 99).unwrap();
    let o = gt(&["ledger", "migrate", "--db", db.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("newer"), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn migrated_ledger_is_still_read_and_written_by_python() {
    let d = tempfile::tempdir().unwrap();
    let db = old_ledger(d.path());
    assert!(gt(&["ledger", "migrate", "--db", db.to_str().unwrap()]).status.success());
    // `flaky` is forwarded to Python and opens the same file
    let o = gt(&["flaky", "--db", db.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // Python ingest into the v1 file
    let log = repo().join("tests/corpus/parse/green/input.log");
    let o = gt(&["ingest", log.to_str().unwrap(), "--db", db.to_str().unwrap(), "--run-id", "py1"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(Ledger::open(&db).unwrap().runs().unwrap().len(), 2);
}

#[test]
fn snapshots_never_delete() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("s.db");
    let mut l = Ledger::open(&db).unwrap();
    l.migrate().unwrap();
    let running = gatetrain_core::logfmt::parse_log("branch b at c on h\nSTEP_START a 2026-11-11T09:00:00Z\n").unwrap();
    let done = gatetrain_core::logfmt::parse_log("branch b at c on h\nSTEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 1s\nSTEP_EXIT a 0\nRED_SUMMARY: none\nGATES_DONE\n").unwrap();
    l.record_snapshot("r", &running, "running", None, "2026-11-11T09:00:05Z").unwrap();
    l.record_snapshot("r", &done, "green", None, "2026-11-11T09:00:09Z").unwrap();
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM run_snapshots WHERE run_id='r'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "both snapshots are kept");
    assert_eq!(l.current_snapshot("r").unwrap().unwrap().0.as_deref(), Some("green"), "newest wins in the view");
    assert!(l.conn.execute("DELETE FROM run_snapshots", []).is_err());
    assert!(l.conn.execute("UPDATE run_snapshots SET outcome='red'", []).is_err());
}

fn seeded(dir: &Path, n: i64) -> PathBuf {
    let db = dir.join("log.db");
    let mut l = Ledger::open(&db).unwrap();
    l.migrate().unwrap();
    for i in 1..=n {
        declog::append(
            &mut l,
            &declog::NewEntry {
                at: format!("2026-11-15T10:00:{i:02}Z"),
                actor: "engine".into(),
                kind: "transition".into(),
                evidence: Some(json!({"n": i})),
                outcome: "allowed".into(),
                reason: "seed".into(),
                ..Default::default()
            },
        )
        .unwrap();
    }
    db
}

#[test]
fn log_verify_is_green_on_an_intact_chain_and_red_on_a_tampered_one() {
    let d = tempfile::tempdir().unwrap();
    let db = seeded(d.path(), 6);
    let o = gt(&["log", "verify", "--db", db.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));
    assert!(out(&o).contains("6 entries"), "{}", out(&o));

    // Tamper with row 4 (dropping the trigger is what an attacker with file access would do).
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("DROP TRIGGER decisions_no_update; UPDATE decisions SET outcome='refused' WHERE seq=4;").unwrap();
    drop(c);
    let o = gt(&["log", "verify", "--db", db.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("seq 4"), "names the first bad row: {}", out(&o));
}

#[test]
fn log_verify_flags_a_deleted_row() {
    let d = tempfile::tempdir().unwrap();
    let db = seeded(d.path(), 5);
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch("DROP TRIGGER decisions_no_delete; DELETE FROM decisions WHERE seq=2;").unwrap();
    drop(c);
    let o = gt(&["log", "verify", "--db", db.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(out(&o).contains("seq 3"), "{}", out(&o));
}

#[test]
fn log_verify_does_not_create_a_missing_ledger() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("nope.db");
    let o = gt(&["log", "verify", "--db", db.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(!db.exists());
}

#[test]
fn unported_commands_forward_to_python_with_the_exit_code() {
    // `next-tick` with no logs is a Python command; compare against running Python directly.
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("f.db");
    let ours = gt(&["next-tick", "--db", db.to_str().unwrap()]);
    let py = Command::new(python()).args(["-m", "gatetrain", "next-tick", "--db", db.to_str().unwrap()]).env("PYTHONPATH", repo()).output().unwrap();
    assert_eq!(ours.status.code(), py.status.code());
    assert_eq!(ours.stdout, py.stdout);
    // a usage error keeps Python's exit code (argparse: 2)
    let bad = gt(&["ingest"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"), "{}", String::from_utf8_lossy(&bad.stderr));
}

// ---- diagnose: the red classifier, parity with gatetrain/diagnose.py on tests/corpus/diagnose ----

fn diag_cases() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/corpus/diagnose")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).map(|p| PathBuf::from(p.to_str().unwrap().strip_prefix(r"\\?\").unwrap_or(p.to_str().unwrap()))).collect();
    v.sort();
    assert!(v.len() >= 30, "diagnose corpus too small: {}", v.len());
    v
}

/// `input.log` plus the case's `args` file (one argument per line, `@DIR@` = the case directory).
fn diag_args(case: &Path) -> Vec<String> {
    let mut a = vec!["diagnose".to_string(), case.join("input.log").to_str().unwrap().to_string()];
    if let Ok(t) = std::fs::read_to_string(case.join("args")) {
        a.extend(t.lines().filter(|l| !l.is_empty()).map(|l| l.replace("@DIR@", case.to_str().unwrap())));
    }
    a
}

#[test]
fn diagnose_matches_the_committed_python_output_on_every_case() {
    for case in diag_cases() {
        let args = diag_args(&case);
        let o = gt(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(o.status.success(), "{}: {}", case.display(), String::from_utf8_lossy(&o.stderr));
        let want = std::fs::read(case.join("expected-stdout")).unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&want), "case {}", case.display());
        assert_eq!(o.stdout, want, "bytes, case {}", case.display());
    }
}

#[test]
fn diagnose_matches_the_live_python_classifier_on_every_case() {
    for case in diag_cases() {
        let py = Command::new(python()).arg(repo().join("tests/corpus/diagnose_dump.py")).arg(&case).output().expect("python is needed for the parity test");
        assert!(py.status.success(), "{}: {}", case.display(), String::from_utf8_lossy(&py.stderr));
        let args = diag_args(&case);
        let rs = gt(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(rs.stdout, py.stdout, "python vs rust, case {}", case.display());
    }
}

#[test]
fn diagnose_never_prints_zero_real_over_a_real_block() {
    for case in diag_cases() {
        let args = diag_args(&case);
        let text = out(&gt(&args.iter().map(String::as_str).collect::<Vec<_>>()));
        let real_blocks = text.lines().filter(|l| l.starts_with("  [real]")).count();
        if real_blocks > 0 {
            assert!(text.lines().any(|l| l.contains(": real (rc=")), "a real block but no real step verdict: {}", case.display());
            assert!(!text.contains("\n0 blocks:") && !text.contains(": 0 real,"), "real block counted as 0 real: {}", case.display());
            assert!(!text.contains("red steps: 0 real step(s)"), "real block, 0 real steps: {}", case.display());
        }
    }
}

#[test]
fn diagnose_extra_rules_file_is_tried_first() {
    let case = diag_cases().into_iter().find(|c| c.ends_with("extra-rules")).unwrap();
    let text = out(&gt(&diag_args(&case).iter().map(String::as_str).collect::<Vec<_>>()));
    assert!(text.contains("infra-quota"), "{text}");
}

#[test]
fn batch_new_advance_status_and_exit_3_on_a_refusal() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path().join("repo");
    std::fs::create_dir_all(&r).unwrap();
    let g = |a: &[&str]| {
        let o = Command::new("git").arg("-C").arg(&r).args(a).output().unwrap();
        assert!(o.status.success(), "{a:?}");
    };
    g(&["init", "-q", "-b", "main"]);
    g(&["config", "user.email", "t@example.com"]);
    g(&["config", "user.name", "t"]);
    std::fs::write(r.join("a"), "a").unwrap();
    g(&["add", "--", "a"]);
    g(&["commit", "-q", "-m", "init"]);
    g(&["switch", "-q", "-c", "lane1"]);
    std::fs::write(r.join("b"), "b").unwrap();
    g(&["add", "--", "b"]);
    g(&["commit", "-q", "-m", "feat: b"]);
    g(&["switch", "-q", "main"]);
    let db = t.path().join("g.db");
    let (db, cfg) = (db.to_str().unwrap().to_string(), t.path().join("gatetrain.toml").to_str().unwrap().to_string());
    assert!(gt(&["ledger", "migrate", "--db", &db]).status.success());
    let o = gt(&["batch", "new", "b1", "--base", "main", "--workdir", r.to_str().unwrap(), "lane1", "--db", &db, "--config", &cfg]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(gt(&["batch", "advance", "b1", "--db", &db, "--config", &cfg]).status.code(), Some(0));
    assert!(out(&gt(&["batch", "status", "b1", "--db", &db])).contains("b1: stacked"));
    std::fs::write(r.join("stray"), "x").unwrap();
    let o = gt(&["batch", "advance", "b1", "--db", &db, "--config", &cfg]);
    assert_eq!(o.status.code(), Some(3), "{}", out(&o));
    assert!(out(&o).contains("floor.named-paths"));
    assert_eq!(gt(&["log", "verify", "--db", &db]).status.code(), Some(0));
}

// ---- analysis parity: report, trace, eta (slice port-a) -------------------------------------------------------

const NOW: &str = "2026-11-12T00:00:00Z";

fn analysis_cases() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/corpus/analysis")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    v.sort();
    assert!(v.len() >= 10, "analysis corpus too small: {}", v.len());
    v
}

/// The same ledger the Python dump script builds: the finished parse-corpus logs, ingested through the binary.
fn history_db(dir: &Path) -> PathBuf {
    let db = dir.join("g.db");
    let mut logs: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/corpus/parse")).unwrap().map(|e| e.unwrap().path().join("input.log")).collect();
    logs.sort();
    for log in logs {
        let id = format!("hist-{}", log.parent().unwrap().file_name().unwrap().to_str().unwrap());
        let o = gt(&["ingest", log.to_str().unwrap(), "--db", db.to_str().unwrap(), "--run-id", &id, "--now", NOW]);
        assert!(o.status.success(), "ingest {id}: {}", String::from_utf8_lossy(&o.stderr));
    }
    db
}

fn rust_kind(case: &Path, kind: &str, db: &Path) -> Vec<u8> {
    let log = case.join("input.log");
    let log = log.to_str().unwrap();
    let o = match kind {
        "report" | "trace" => gt(&[kind, log, "--now", NOW]),
        _ => gt(&["eta", log, "--db", db.to_str().unwrap(), "--now", NOW]),
    };
    // a forwarded Python answer carries Windows CRLF line ends; the committed files are LF
    let mut b: Vec<u8> = Vec::with_capacity(o.stdout.len());
    for (i, c) in o.stdout.iter().enumerate() {
        if !(*c == b'\r' && o.stdout.get(i + 1) == Some(&b'\n')) {
            b.push(*c);
        }
    }
    if !o.status.success() {
        b.extend(format!("\nEXIT {}\n", o.status.code().unwrap()).bytes());
    }
    b
}

#[test]
fn analysis_matches_the_committed_python_output_on_every_case() {
    let tmp = tempfile::tempdir().unwrap();
    let hist = history_db(tmp.path());
    let none = tmp.path().join("none.db");
    let mut n = 0;
    for case in analysis_cases() {
        for kind in ["report", "trace", "eta", "eta-history"] {
            let db = if kind == "eta-history" { &hist } else { &none };
            let got = rust_kind(&case, kind, db);
            let want = std::fs::read(case.join(format!("expected-{kind}"))).unwrap();
            assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(&want), "{kind} {}", case.display());
            assert_eq!(got, want, "bytes, {kind} {}", case.display());
            n += 1;
        }
    }
    assert!(n >= 52, "compared {n}");
}

#[test]
fn analysis_matches_the_live_python_output_on_every_case() {
    let tmp = tempfile::tempdir().unwrap();
    let hist = history_db(tmp.path());
    let none = tmp.path().join("none.db");
    for case in analysis_cases() {
        for kind in ["report", "trace", "eta", "eta-history"] {
            let py = Command::new(python()).arg(repo().join("tests/corpus/analysis_dump.py")).arg(&case).arg(kind).output().expect("python is needed for the parity test");
            assert!(py.status.success(), "{}: {}", case.display(), String::from_utf8_lossy(&py.stderr));
            let db = if kind == "eta-history" { &hist } else { &none };
            assert_eq!(rust_kind(&case, kind, db), py.stdout, "python vs rust, {kind} {}", case.display());
        }
    }
}

/// Proof the parity above is not the Python fallback answering: with forwarding switched off the ported paths still answer.
#[test]
fn ported_report_trace_eta_never_forward_to_python() {
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(args).env("GATETRAIN_PYROOT", repo()).env("GATETRAIN_NO_FORWARD", "1").output().unwrap();
    let case = repo().join("tests/corpus/analysis/green/input.log");
    let log = case.to_str().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("none.db");
    let r = run(&["report", log, "--now", NOW]);
    assert!(r.status.success(), "report: {}", String::from_utf8_lossy(&r.stderr));
    assert!(out(&r).starts_with("run input: "));
    for case in analysis_cases() {
        let t = run(&["trace", case.join("input.log").to_str().unwrap(), "--now", NOW]);
        assert!(t.status.success(), "trace {}: {}", case.display(), String::from_utf8_lossy(&t.stderr));
    }
    let e = run(&["eta", log, "--db", db.to_str().unwrap(), "--now", NOW]);
    assert!(e.status.success(), "eta: {}", String::from_utf8_lossy(&e.stderr));
    assert!(!db.exists(), "eta must not create a ledger as a side effect of reading history");
}

// ---- config parity: hosts.toml (slice port-a) ---------------------------------------------------------------------

fn config_cases() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/corpus/config")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
    v.sort();
    assert!(v.len() >= 25, "config corpus too small: {}", v.len());
    v
}

#[test]
fn config_dump_matches_the_committed_python_output_on_every_case() {
    for case in config_cases() {
        let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["config", "dump", case.join("hosts.toml").to_str().unwrap()]).env("GATETRAIN_NO_FORWARD", "1").output().unwrap();
        let want = std::fs::read(case.join("expected-stdout")).unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&want), "case {}: {}", case.display(), String::from_utf8_lossy(&o.stderr));
        assert_eq!(o.stdout, want, "bytes, case {}", case.display());
    }
}

#[test]
fn config_dump_matches_the_live_python_loader_on_every_case() {
    for case in config_cases() {
        let hosts = case.join("hosts.toml");
        let py = Command::new(python()).arg(repo().join("tests/corpus/config_dump.py")).arg(&hosts).output().expect("python is needed for the parity test");
        assert!(py.status.success(), "{}: {}", case.display(), String::from_utf8_lossy(&py.stderr));
        let rs = gt(&["config", "dump", hosts.to_str().unwrap()]);
        assert_eq!(rs.stdout, py.stdout, "python vs rust, case {}", case.display());
    }
}

// ---- check and tidy (every mode) are answered by the Rust binary and match Python ----

#[test]
fn check_and_tidy_every_mode_run_in_rust_and_match_python() {
    let d = tempfile::tempdir().unwrap();
    let cfg = d.path().join("hosts.toml");
    std::fs::write(&cfg, "[[hosts]]
name = \"h\"
reach = \"local\"
").unwrap();
    let c = cfg.to_str().unwrap();
    for args in [vec!["tidy"], vec!["tidy", "--class", "targets"], vec!["tidy", "--apply"], vec!["tidy", "--auto"], vec!["check"], vec!["check", "--json"]] {
        let mut a = args.clone();
        a.extend(["--config", c]);
        // forwarding switched off: the Rust binary must answer on its own (a forward would exit 126)
        let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(&a).env("GATETRAIN_NO_FORWARD", "1").output().unwrap();
        assert_ne!(o.status.code(), Some(126), "{args:?} was forwarded: {}", String::from_utf8_lossy(&o.stderr));
        // and the exit code and the text are Python's
        let mut pa = vec!["-m", "gatetrain"];
        pa.extend(a.iter().copied());
        let py = Command::new(python()).args(&pa).env("PYTHONPATH", repo()).env("PYTHONUTF8", "1").output().unwrap();
        let lf = |b: &[u8]| String::from_utf8_lossy(b).replace('\r', "");
        assert_eq!(o.status.code(), py.status.code(), "{args:?}");
        assert_eq!(lf(&o.stdout), lf(&py.stdout), "{args:?}");
    }
}

#[test]
fn every_clap_subcommand_is_in_ported_and_nothing_else_is() {
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(args).env("GATETRAIN_NO_FORWARD", "1").output().unwrap();
    let help = String::from_utf8_lossy(&run(&["--help"]).stdout).into_owned();
    let cmds: Vec<String> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next().map(String::from))
        .filter(|c| c != "help")
        .collect();
    assert!(cmds.len() >= 15, "{cmds:?}");
    for c in &cmds {
        let o = run(&[c, "--help"]);
        assert_eq!(o.status.code(), Some(0), "`{c}` is a clap subcommand but not in PORTED (it would forward to Python): {}", String::from_utf8_lossy(&o.stderr));
    }
    // and a name that is not a subcommand forwards
    assert_eq!(run(&["ingest", "--help"]).status.code(), Some(126));
}
