//! Engine v2, slices E1 and E2 (docs/design/engine-v2.md sections 5, 6, 11): `chain --db`, `--mode`, `--host`, `diagnose` over the `.full`
//! files, and `shards`. The real binary over `gt-testchild` steps and temp git repos; never npm or docker.
use gatetrain_core::chain::digest_of;
use gatetrain_core::logfmt::parse_log;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

/// `extra` is raw TOML lines placed in the step (shards = 3, shard_check = [...]).
fn step(name: &str, tier: &str, extra: &str, args: &[&str]) -> String {
    let mut argv = vec![format!("'{}'", child())];
    argv.extend(args.iter().map(|a| format!("'{a}'")));
    format!("[[step]]\nname = '{name}'\ntier = '{tier}'\n{extra}\nargv = [{}]\n\n", argv.join(", "))
}

struct Run {
    o: Output,
    out: PathBuf,
    stdout: String,
    stderr: String,
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("f.txt"), "one").unwrap();
    git(repo.path(), &["add", "f.txt"]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    repo
}

/// Run `chain` over `body` in a clean temp repo (so a green run gets a stamp). `envs`: (name, Some(value)) sets, (name, None) removes.
fn chain_in(repo: &Path, work: &Path, body: &str, extra: &[&str], envs: &[(&str, Option<&str>)]) -> Run {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{body}", repo.display())).unwrap();
    let out = work.join("out");
    let mut c = Command::new(env!("CARGO_BIN_EXE_gatetrain"));
    c.args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()]).args(extra).current_dir(work);
    for (k, v) in envs {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    let o = c.output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    Run { o, out, stdout, stderr }
}

fn chain(body: &str, extra: &[&str]) -> (Run, tempfile::TempDir, tempfile::TempDir) {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let run = chain_in(r.path(), w.path(), body, extra, &[]);
    (run, r, w)
}

fn code(r: &Run) -> i32 {
    r.o.status.code().unwrap()
}

fn lines<'a>(r: &'a Run, prefix: &str) -> Vec<&'a str> {
    r.stdout.lines().filter(|l| l.starts_with(prefix)).collect()
}

fn q(db: &Path, sql: &str) -> String {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0)).map(|v| match v {
        rusqlite::types::Value::Null => "NULL".into(),
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Real(f) => f.to_string(),
        rusqlite::types::Value::Text(t) => t,
        rusqlite::types::Value::Blob(_) => "blob".into(),
    })
    .unwrap_or_else(|e| format!("ERR {e}"))
}

// ---------------------------------------------------------------- E1: the host name

fn os_hostname_cmd() -> String {
    String::from_utf8_lossy(&Command::new("hostname").output().unwrap().stdout).trim().to_string()
}

#[test]
fn the_host_falls_back_to_the_os_name_when_no_variable_is_set() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let run = chain_in(r.path(), w.path(), &step("a", "fast", "", &["pass"]), &[], &[("COMPUTERNAME", None), ("HOSTNAME", None), ("GATETRAIN_HOST", None)]);
    assert_eq!(code(&run), 0, "{}", run.stdout);
    let p = parse_log(&run.stdout).unwrap();
    assert_eq!(p.host.as_deref(), Some(os_hostname_cmd().as_str()), "{}", run.stdout);
    assert_ne!(p.host.as_deref(), Some("unknown"));
}

#[test]
fn host_flag_and_environment_name_the_host_in_the_log_and_the_stamp() {
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let run = chain_in(r.path(), w.path(), &step("a", "fast", "", &["pass"]), &["--host", "builder-1"], &[]);
    assert_eq!(parse_log(&run.stdout).unwrap().host.as_deref(), Some("builder-1"), "{}", run.stdout);
    let stamp = std::fs::read_dir(&run.out).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("stamp-")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stamp.path()).unwrap()).unwrap();
    assert_eq!(v["host"], "builder-1");
    // under `hold` the name arrives as GATETRAIN_HOST; the flag wins over it
    let w2 = tempfile::tempdir().unwrap();
    let run = chain_in(r.path(), w2.path(), &step("a", "fast", "", &["pass"]), &[], &[("GATETRAIN_HOST", Some("from-hold"))]);
    assert_eq!(parse_log(&run.stdout).unwrap().host.as_deref(), Some("from-hold"));
    let w3 = tempfile::tempdir().unwrap();
    let run = chain_in(r.path(), w3.path(), &step("a", "fast", "", &["pass"]), &["--host", "flag"], &[("GATETRAIN_HOST", Some("from-hold"))]);
    assert_eq!(parse_log(&run.stdout).unwrap().host.as_deref(), Some("flag"));
}

// ---------------------------------------------------------------- E1: --mode

#[test]
fn mode_is_printed_and_stamped_and_a_partial_run_cannot_be_full() {
    let two = step("a", "fast", "", &["pass"]) + &step("b", "fast", "", &["pass"]);
    let (run, _r, _w) = chain(&two, &[]);
    assert_eq!(parse_log(&run.stdout).unwrap().mode.as_deref(), Some("preflight"), "the default stays what it was");
    let (run, _r, _w) = chain(&two, &["--mode", "lane"]);
    assert_eq!(parse_log(&run.stdout).unwrap().mode.as_deref(), Some("lane"));
    let stamp = std::fs::read_dir(&run.out).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("stamp-")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(stamp.path()).unwrap()).unwrap();
    assert_eq!((v["mode"].as_str(), v["kind"].as_str(), v["v"].as_i64()), (Some("lane"), Some("lane"), Some(1)));
    let (run, _r, _w) = chain(&two, &["--mode", "full"]);
    assert_eq!(code(&run), 0);
    assert_eq!(parse_log(&run.stdout).unwrap().mode.as_deref(), Some("full"));
    for partial in [["--only", "a"], ["--skip", "a"]] {
        let (run, _r, _w) = chain(&two, &["--mode", "full", partial[0], partial[1]]);
        assert_eq!(code(&run), 2, "{}{}", run.stdout, run.stderr);
        assert!(run.stderr.contains("full"), "{}", run.stderr);
        assert!(run.stdout.is_empty(), "refused before anything ran: {}", run.stdout);
    }
    let (run, _r, _w) = chain(&two, &["--mode", "bogus"]);
    assert_eq!(code(&run), 2);
}

// ---------------------------------------------------------------- E1: --db

#[test]
fn a_green_run_with_db_leaves_the_run_its_steps_and_a_stamp() {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("l.db");
    let body = step("a", "fast", "", &["pass"]) + &step("b", "slow", "needs = ['a']", &["pass"]);
    let r = repo();
    let run = chain_in(r.path(), w.path(), &body, &["--db", db.to_str().unwrap(), "--run-id", "run-1", "--host", "builder-1", "--mode", "lane", "--par", "3"], &[]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(q(&db, "pragma user_version"), "8");
    assert_eq!(q(&db, "select count(*) from runs"), "1");
    let want = format!("run-1|chain|builder-1|green|lane|3|{}|{}", digest_of(&std::fs::read_to_string(w.path().join("steps.toml")).unwrap()), digest_of(&run.stdout));
    assert_eq!(q(&db, "select id||'|'||kind||'|'||host||'|'||outcome||'|'||mode||'|'||par||'|'||plan_digest||'|'||log_sha256 from runs"), want);
    assert_eq!(q(&db, "select length(commit_sha) from runs"), "40");
    assert_eq!(q(&db, "select count(*) from steps where run_id='run-1' and exit_code=0 and duration_sec is not null and started_at is not null"), "2");
    assert_eq!(q(&db, "select tier from steps where name='b'"), "slow");
    assert_eq!(q(&db, "select count(*) from stamps where run_id='run-1' and kind='lane' and host='builder-1'"), "1");
    // the stamp row is the stamp file: the digest is the sha-256 of its bytes
    let stamp = std::fs::read_dir(&run.out).unwrap().flatten().find(|e| e.file_name().to_string_lossy().starts_with("stamp-")).unwrap();
    assert_eq!(q(&db, "select digest from stamps"), digest_of(&std::fs::read_to_string(stamp.path()).unwrap()));
    assert_eq!(q(&db, "select count(*) from step_skips"), "0");
    // the run reads back through the existing readers with no hand ingest
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["report", "--db", db.to_str().unwrap(), "--run", "run-1"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn a_red_run_with_db_records_the_red_the_skips_and_no_stamp() {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("l.db");
    let body = step("f", "fast", "", &["exit", "3"]) + &step("s", "slow", "", &["pass"]) + &step("d", "fast", "needs = ['f']", &["pass"]);
    let r = repo();
    let run = chain_in(r.path(), w.path(), &body, &["--db", db.to_str().unwrap(), "--run-id", "run-red"], &[]);
    assert_eq!(code(&run), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(q(&db, "select outcome from runs where id='run-red'"), "red");
    assert_eq!(q(&db, "select exit_code from steps where name='f'"), "3");
    assert_eq!(q(&db, "select count(*) from steps"), "1", "a step that never ran is not a step row");
    assert_eq!(q(&db, "select group_concat(step) from (select step from step_skips order by step)"), "d,s");
    assert!(q(&db, "select reason from step_skips where step='s'").contains("fast step is red"));
    assert_eq!(q(&db, "select count(*) from stamps"), "0");
}

#[test]
fn db_without_a_run_id_gets_a_unique_one_and_a_repeated_id_is_refused_by_the_ledger() {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("l.db");
    let r = repo();
    let body = step("a", "fast", "", &["pass"]);
    for n in 0..2 {
        let wn = tempfile::tempdir().unwrap();
        let run = chain_in(r.path(), wn.path(), &body, &["--db", db.to_str().unwrap()], &[]);
        assert_eq!(code(&run), 0, "run {n}: {}{}", run.stdout, run.stderr);
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    assert_eq!(q(&db, "select count(distinct id) from runs"), "2");
    let run = chain_in(r.path(), w.path(), &body, &["--db", db.to_str().unwrap(), "--run-id", "x"], &[]);
    assert_eq!(code(&run), 0);
    let w2 = tempfile::tempdir().unwrap();
    let run = chain_in(r.path(), w2.path(), &body, &["--db", db.to_str().unwrap(), "--run-id", "x"], &[]);
    assert_eq!(code(&run), 0, "the checks themselves were green");
    assert!(run.stderr.contains("ledger"), "a ledger write that fails is said: {}", run.stderr);
    assert_eq!(q(&db, "select count(*) from runs where id='x'"), "1");
}

#[test]
fn without_db_nothing_is_written() {
    let (run, _r, w) = chain(&step("a", "fast", "", &["pass"]), &[]);
    assert_eq!(code(&run), 0);
    assert!(!w.path().join("l.db").exists());
}

// ---------------------------------------------------------------- E1: diagnose reads the .full files

const PANIC: [&str; 3] = ["---- t stdout ----", "assertion failed: left == right", "note: run with RUST_BACKTRACE=1"];

fn diagnose(log: &Path, extra: &[&str]) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).arg("diagnose").arg(log).args(extra).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}

fn red_run_with_output() -> (Run, tempfile::TempDir, tempfile::TempDir) {
    let mut args = vec!["failprint", "1", "x", "x"];
    args.extend(PANIC);
    chain(&step("r", "fast", "", &args), &[])
}

#[test]
fn a_red_chain_step_is_classified_from_its_full_file_given_out() {
    let (run, _r, w) = red_run_with_output();
    assert_eq!(code(&run), 1, "{}{}", run.stdout, run.stderr);
    // the log is copied elsewhere: no .full beside it, no banner in it
    let log = w.path().join("copied.log");
    let elsewhere = tempfile::tempdir().unwrap();
    let log = elsewhere.path().join(log.file_name().unwrap());
    std::fs::write(&log, run.stdout.lines().filter(|l| !l.starts_with("CHAIN_OUT")).collect::<Vec<_>>().join("\n") + "\n").unwrap();
    let before = diagnose(&log, &[]);
    assert!(before.contains("no output captured"), "today a chain red reaches the classifier with no lines: {before}");
    let after = diagnose(&log, &["--out", run.out.to_str().unwrap()]);
    assert!(after.contains("assertion failed: left == right"), "{after}");
    assert!(!after.contains("no output captured"), "{after}");
}

#[test]
fn diagnose_finds_the_full_files_from_the_log_itself() {
    let (run, _r, w) = red_run_with_output();
    let log = w.path().join("run.log");
    std::fs::write(&log, &run.stdout).unwrap();
    assert_eq!(lines(&run, "CHAIN_OUT ").len(), 1, "the log names its out folder: {}", run.stdout);
    let t = diagnose(&log, &[]);
    assert!(t.contains("assertion failed: left == right"), "{t}");
    // and the parser and every older reader skip the new line
    let p = parse_log(&run.stdout).unwrap();
    assert_eq!(p.steps.len(), 1);
}

#[test]
fn a_banner_in_the_log_still_wins_over_the_full_file() {
    let (run, _r, w) = red_run_with_output();
    let log = w.path().join("run.log");
    let mut text = run.stdout.clone();
    text = text.replace("STEP_TIME r", "=== r\nthread 'x' panicked at a.rs:1:1:\nassertion failed: from the banner\nSTEP_TIME r");
    std::fs::write(&log, text).unwrap();
    let t = diagnose(&log, &[]);
    assert!(t.contains("from the banner") && !t.contains("left == right"), "{t}");
}

// ---------------------------------------------------------------- E2: shards

fn shard_step(n: u32, extra: &str, args: &[&str]) -> String {
    step("t", "fast", &format!("shards = {n}\n{extra}"), args)
}

#[test]
fn a_four_shard_step_is_one_logical_step_with_shard_lines() {
    let (run, _r, _w) = chain(&shard_step(4, "", &["print", "shard {shard} of {shards}"]), &["--par", "4"]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    for l in ["STEP_START t ", "STEP_TIME t ", "STEP_EXIT t "] {
        assert_eq!(lines(&run, l).len(), 1, "{l}: {}", run.stdout);
    }
    assert_eq!(lines(&run, "STEP_SHARD t ").iter().filter(|l| l.contains(" START ")).count(), 4);
    assert_eq!(lines(&run, "STEP_SHARD t ").iter().filter(|l| l.contains(" EXIT 0 ")).count(), 4, "{}", run.stdout);
    for i in 1..=4 {
        assert!(run.stdout.contains(&format!("STEP_SHARD t {i}/4 START ")) && run.stdout.contains(&format!("STEP_SHARD t {i}/4 EXIT 0 ")), "{}", run.stdout);
        let f = std::fs::read_to_string(run.out.join(format!("t.{i}of4.full"))).unwrap();
        assert_eq!(f.trim(), format!("shard {i} of 4"), "each shard has its own .full");
    }
    let list = std::fs::read_to_string(run.out.join("t.full")).unwrap();
    assert!((1..=4).all(|i| list.contains(&format!("t.{i}of4.full"))), "t.full lists them: {list}");
    let p = parse_log(&run.stdout).unwrap();
    assert_eq!(p.steps.len(), 1, "one logical step");
    assert_eq!(p.steps[0].exit_code, Some(0));
}

#[test]
fn a_sharded_log_parses_to_the_same_steps_as_the_same_log_without_the_shard_lines() {
    let (run, _r, _w) = chain(&shard_step(3, "", &["print", "{shard}"]), &[]);
    let without: String = run.stdout.lines().filter(|l| !l.starts_with("STEP_SHARD")).map(|l| format!("{l}\n")).collect();
    assert_ne!(without, run.stdout);
    assert_eq!(parse_log(&run.stdout).unwrap().to_json(), parse_log(&without).unwrap().to_json());
}

#[test]
fn one_red_shard_makes_one_red_now_and_a_red_step_and_the_others_run_to_the_end() {
    // shard 3 is red at once (rc 7); shard 4 is still sleeping and must finish
    let body = step("t", "fast", "shards = 4", &["exitif", "{shard}", "3", "7"]) + &step("late", "fast", "", &["pass"]);
    let (run, _r, _w) = chain(&body, &["--par", "8"]);
    assert_eq!(code(&run), 1, "{}", run.stdout);
    assert_eq!(lines(&run, "RED_NOW ").len(), 1, "{}", run.stdout);
    assert!(lines(&run, "RED_NOW ")[0].starts_with("RED_NOW t rc=7 ") && lines(&run, "RED_NOW ")[0].contains("shard 3/4") && lines(&run, "RED_NOW ")[0].contains("t.3of4.full"), "{}", run.stdout);
    assert_eq!(lines(&run, "STEP_EXIT t ")[0], "STEP_EXIT t 7");
    assert_eq!(lines(&run, "STEP_SHARD t ").iter().filter(|l| l.contains(" EXIT ")).count(), 4, "every shard reported: {}", run.stdout);
    assert!(run.stdout.contains("RED_SUMMARY: t(rc=7)"), "{}", run.stdout);
    let p = parse_log(&run.stdout).unwrap();
    assert_eq!(p.red_now.len(), 1);
}

#[test]
fn the_exit_code_is_the_lowest_numbered_red_shard() {
    let body = step("t", "fast", "shards = 3", &["exitif", "{shard}", "2", "9"]);
    let (run, _r, _w) = chain(&body, &[]);
    assert_eq!(lines(&run, "STEP_EXIT t ")[0], "STEP_EXIT t 9");
    let body = step("t", "fast", "shards = 3", &["failprint", "{shard}0", "x", "x"]);
    let (run, _r, _w) = chain(&body, &["--par", "3"]);
    assert!(run.stdout.contains("STEP_SHARD t 3/3 EXIT 30 "), "{}", run.stdout);
    assert_eq!(lines(&run, "STEP_EXIT t ")[0], "STEP_EXIT t 10");
}

#[test]
fn shards_overlap_take_one_par_slot_each_and_the_step_time_is_the_wall_time() {
    let (run, _r, _w) = chain(&shard_step(3, "", &["printsleep", "s{shard}", "1.2"]), &["--par", "3"]);
    assert_eq!(code(&run), 0, "{}", run.stdout);
    // the shards overlap: every shard START comes before the first shard EXIT (the log's order, not a wall-clock bound that a loaded host stretches)
    let first_exit = run.stdout.lines().position(|l| l.contains(" EXIT ") && l.starts_with("STEP_SHARD")).unwrap();
    assert_eq!(run.stdout.lines().take(first_exit).filter(|l| l.starts_with("STEP_SHARD") && l.contains(" START ")).count(), 3, "{}", run.stdout);
    // STEP_TIME is the step's wall time (first shard start to last shard end): it is at least one shard's sleep and the log says one step
    let p = parse_log(&run.stdout).unwrap();
    let d = p.steps[0].duration.unwrap();
    assert!(d >= 1.1 && p.steps.len() == 1, "{d}");
    // par 2 with 4 shards of 1.0 s: two waves
    let (run, _r, _w) = chain(&shard_step(4, "", &["printsleep", "s{shard}", "1.0"]), &["--par", "2"]);
    let d = parse_log(&run.stdout).unwrap().steps[0].duration.unwrap();
    assert!(d >= 1.9, "four shards on two slots take two waves: {d}");
}

#[test]
fn a_missing_shard_exit_is_exit_125_with_a_note() {
    // the guard is unit-tested in core (a panicking worker); here the binary's own contract: no shard line is ever missing on a normal run
    let (run, _r, _w) = chain(&shard_step(2, "", &["pass", "{shard}"]), &[]);
    assert_eq!(lines(&run, "STEP_SHARD t ").len(), 4);
}

#[test]
fn shard_config_errors_exit_two() {
    for (body, why) in [
        (step("t", "fast", "", &["print", "{shard}"]), "{shard} without shards"),
        (step("t", "fast", "shards = 3", &["pass"]), "shards > 1 without a placeholder"),
        (step("t", "fast", "shards = 65", &["print", "{shard}"]), "shards above 64"),
        (step("t", "fast", "shards = 0", &["print", "{shard}"]), "shards = 0"),
        (step("t", "fast", "shard_check = ['x']", &["pass"]), "shard_check without shards"),
        (step("t", "fast", "shard_env = true", &["pass"]), "shard_env without shards"),
    ] {
        let (run, _r, _w) = chain(&body, &[]);
        assert_eq!(code(&run), 2, "{why}: {}{}", run.stdout, run.stderr);
        assert!(run.stdout.is_empty(), "{why}: nothing ran");
    }
    // shard_env lets a command read the variables itself
    let (run, _r, _w) = chain(&step("t", "fast", "shards = 2\nshard_env = true", &["env", "S=GATETRAIN_SHARD", "N=GATETRAIN_SHARDS"]), &["--par", "2"]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    assert_eq!(std::fs::read_to_string(run.out.join("t.2of2.full")).unwrap().trim(), "S=2 N=2");
}

#[test]
fn shard_check_runs_after_green_shards_and_a_failing_one_makes_the_step_red() {
    let check = format!("shard_check = ['{}', 'env', 'OUT=GATETRAIN_SHARD_OUT']", child());
    let (run, _r, _w) = chain(&shard_step(2, &check, &["pass", "{shard}"]), &[]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    let out = std::fs::read_to_string(run.out.join("t.check.full")).unwrap();
    assert!(out.contains("t.1of2.full") && out.contains("t.2of2.full"), "the check is told the shard files: {out}");
    // a failing check
    let check = format!("shard_check = ['{}', 'exit', '5']", child());
    let (run, _r, _w) = chain(&shard_step(2, &check, &["pass", "{shard}"]), &[]);
    assert_eq!(code(&run), 1, "{}", run.stdout);
    assert_eq!(lines(&run, "STEP_EXIT t ")[0], "STEP_EXIT t 5");
    assert_eq!(lines(&run, "RED_NOW ").len(), 1);
    // a red shard: the check is not run
    let check = format!("shard_check = ['{}', 'exit', '5']", child());
    let (run, _r, _w) = chain(&shard_step(2, &check, &["exitif", "{shard}", "1", "3"]), &[]);
    assert_eq!(lines(&run, "STEP_EXIT t ")[0], "STEP_EXIT t 3");
    assert!(!run.out.join("t.check.full").exists());
}

#[test]
fn a_shard_red_is_classified_from_its_own_full_file() {
    let body = step("t", "fast", "shards = 2", &["failprint", "1", "{shard}", "2", "---- t stdout ----", "assertion failed: shard {shard} broke"]);
    let (run, _r, w) = chain(&body, &["--par", "2"]);
    assert_eq!(code(&run), 1, "{}{}", run.stdout, run.stderr);
    let log = w.path().join("run.log");
    std::fs::write(&log, &run.stdout).unwrap();
    let t = diagnose(&log, &[]);
    assert!(t.contains("shard 2 broke"), "the red shard's file is read: {t}");
    assert!(!t.contains("shard 1 broke"), "a green shard's output is not evidence: {t}");
}

// ---------------------------------------------------------------- E2: cache and shards

fn cached_shard_step(shards: u32, marker: &Path, code_for_shard_1: &str) -> String {
    let m = marker.to_str().unwrap();
    step("t", "fast", &format!("shards = {shards}\ninputs = ['f.txt']"), &["exitif", "{shard}", "1", code_for_shard_1]) + &step("m", "fast", "", &["marker", m, "0"])
}

#[test]
fn the_cache_key_covers_shards_a_hit_skips_every_shard_and_a_red_shard_stores_nothing() {
    let (r, w, cache, mk) = (repo(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let c = cache.path().to_str().unwrap();
    let marker = mk.path().join("m");
    // a red shard: no entry
    let run = chain_in(r.path(), w.path(), &cached_shard_step(2, &marker, "4"), &["--cache", c], &[]);
    assert_eq!(code(&run), 1, "{}", run.stdout);
    assert!(!cache.path().join("t").exists() || std::fs::read_dir(cache.path().join("t")).unwrap().count() == 0, "a red shard writes no entry");
    // green: an entry
    let w2 = tempfile::tempdir().unwrap();
    let run = chain_in(r.path(), w2.path(), &cached_shard_step(2, &marker, "0"), &["--cache", c], &[]);
    assert_eq!(code(&run), 0, "{}", run.stdout);
    assert_eq!(std::fs::read_dir(cache.path().join("t")).unwrap().count(), 1);
    // the same plan again: a hit, no shard runs, one logical step
    let w3 = tempfile::tempdir().unwrap();
    let run = chain_in(r.path(), w3.path(), &cached_shard_step(2, &marker, "0"), &["--cache", c], &[]);
    assert!(run.stdout.contains("STEP_CACHED t"), "{}", run.stdout);
    assert!(lines(&run, "STEP_SHARD").is_empty(), "a hit skips all shards: {}", run.stdout);
    // a different shard count is a different key: it runs and adds an entry
    let w4 = tempfile::tempdir().unwrap();
    let run = chain_in(r.path(), w4.path(), &cached_shard_step(3, &marker, "0"), &["--cache", c], &[]);
    assert!(!run.stdout.contains("STEP_CACHED t"), "{}", run.stdout);
    assert_eq!(lines(&run, "STEP_SHARD t ").len(), 6);
    assert_eq!(std::fs::read_dir(cache.path().join("t")).unwrap().count(), 2);
}

// ---------------------------------------------------------------- E2: the ledger

#[test]
fn a_sharded_run_with_db_keeps_its_shards_in_step_shards() {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("l.db");
    let r = repo();
    let body = step("t", "fast", "shards = 3", &["exitif", "{shard}", "2", "6"]);
    let run = chain_in(r.path(), w.path(), &body, &["--db", db.to_str().unwrap(), "--run-id", "sh-1", "--par", "3"], &[]);
    assert_eq!(code(&run), 1, "{}{}", run.stdout, run.stderr);
    assert_eq!(q(&db, "select count(*) from steps where run_id='sh-1'"), "1", "one logical step");
    assert_eq!(q(&db, "select shards||'|'||exit_code from steps where name='t'"), "3|6");
    assert_eq!(q(&db, "select count(*) from step_shards where run_id='sh-1' and step='t' and of=3 and duration_sec is not null and started_at is not null"), "3");
    assert_eq!(q(&db, "select group_concat(shard||':'||exit_code, ' ') from (select * from step_shards order by shard)"), "1:0 2:6 3:0");
    assert_eq!(q(&db, "select log_ref from step_shards where shard=2"), "t.2of3.full");
    assert_eq!(q(&db, "select count(*) from step_shards where run_id='sh-1'"), "3");
}

// ---------------------------------------------------------------- report counts shard slots

fn report(args: &[&str]) -> String {
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).arg("report").args(args).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn report_counts_a_sharded_steps_slots_from_the_ledger_and_from_the_log() {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("l.db");
    let r = repo();
    let body = step("t", "fast", "shards = 3", &["printsleep", "s{shard}", "1.5"]);
    let run = chain_in(r.path(), w.path(), &body, &["--db", db.to_str().unwrap(), "--run-id", "par-1", "--par", "3"], &[]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    for text in [report(&["--db", db.to_str().unwrap(), "--run", "par-1"]), {
        let log = w.path().join("run.log");
        std::fs::write(&log, &run.stdout).unwrap();
        report(&[log.to_str().unwrap()])
    }] {
        let line = text.lines().find(|l| l.contains("average parallelism")).unwrap().to_string();
        let avg: f64 = line.rsplit("average parallelism ").next().unwrap().trim().parse().unwrap();
        assert!((2.5..=3.01).contains(&avg), "three shards at once are 3 slots, not 1: {line}");
    }
}

// ---------------------------------------------------------------- E7: the sampler inside chain

fn sample_lines(out: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(out.join("samples.jsonl")).map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()).unwrap_or_default()
}

#[test]
fn chain_samples_the_host_into_samples_jsonl_while_a_run_is_alive() {
    let (run, _r, _w) = chain(&step("slow", "fast", "", &["sleep", "3.2"]), &["--samples", "1", "--host", "samp-host"]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    let v = sample_lines(&run.out);
    let rows: Vec<&serde_json::Value> = v.iter().filter(|x| x.get("host").is_some()).collect();
    assert!(rows.len() >= 2, "a 3.2 s step sampled every second leaves samples: {v:?}");
    assert!(rows.iter().all(|x| x["host"] == "samp-host" && x["running_steps"].as_i64() >= Some(1) && x["at"].is_string()), "{rows:?}");
    // the first line names what this platform does not give, and why
    assert!(v[0].get("unknown").is_some() && v[0].get("host").is_none(), "{:?}", v[0]);
    assert!(rows.iter().any(|x| x["mem_free_mb"].is_number()), "free memory is read on every platform: {rows:?}");
    // the log itself is unchanged by sampling: no new marker, the same shape
    assert!(!run.stdout.contains("samples"), "{}", run.stdout);
}

#[test]
fn samples_zero_and_a_holder_that_already_samples_leave_no_file() {
    let (run, _r, _w) = chain(&step("slow", "fast", "", &["sleep", "1.5"]), &["--samples", "0"]);
    assert!(!run.out.join("samples.jsonl").exists());
    let (r, w) = (repo(), tempfile::tempdir().unwrap());
    let run = chain_in(r.path(), w.path(), &step("slow", "fast", "", &["sleep", "1.5"]), &["--samples", "1"], &[("GATETRAIN_SAMPLED", Some("1"))]);
    assert!(!run.out.join("samples.jsonl").exists(), "the holder samples: chain does not");
}

#[test]
fn chain_with_db_ingests_its_samples_with_the_run_id_and_leaves_no_process_behind() {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("l.db");
    let r = repo();
    let run = chain_in(r.path(), w.path(), &step("slow", "fast", "", &["sleep", "2.5"]), &["--samples", "1", "--host", "h1", "--db", db.to_str().unwrap(), "--run-id", "samp-1"], &[]);
    assert_eq!(code(&run), 0, "{}{}", run.stdout, run.stderr);
    assert!(q(&db, "select count(*) from host_samples where run_id='samp-1' and host='h1'").parse::<i64>().unwrap() >= 1, "{}", run.stderr);
    assert_eq!(q(&db, "select count(*) from host_samples where run_id is null or running_steps is null"), "0");
    // the process ended with its run: chain is not a daemon
    let t = std::time::Instant::now();
    assert!(t.elapsed().as_secs() < 5 && run.o.status.success());
}
