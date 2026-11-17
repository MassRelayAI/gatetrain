//! `gatetrain chain` end to end: the real binary over small steps files whose steps are `gt-testchild` (never npm, docker or a git network).
//! Run with `cargo test --workspace` so the child is built.
use gatetrain_core::logfmt::parse_log;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

/// A step as TOML text: argv of the child (literal strings, so Windows paths survive).
fn step(name: &str, tier: &str, needs: &[&str], timeout: Option<u32>, args: &[&str]) -> String {
    let mut argv = vec![format!("'{}'", child())];
    argv.extend(args.iter().map(|a| format!("'{a}'")));
    let needs = needs.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
    let t = timeout.map(|t| format!("timeout_s = {t}\n")).unwrap_or_default();
    format!("[[step]]\nname = '{name}'\ntier = '{tier}'\nneeds = [{needs}]\n{t}argv = [{}]\n\n", argv.join(", "))
}

struct Run {
    o: Output,
    out: PathBuf,
    stdout: String,
}

fn chain(dir: &Path, steps_toml: &str, extra: &[&str]) -> Run {
    let f = dir.join("steps.toml");
    std::fs::write(&f, steps_toml).unwrap();
    let out = dir.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()])
        .args(extra)
        .current_dir(dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
    Run { o, out, stdout }
}

fn code(r: &Run) -> i32 {
    r.o.status.code().unwrap()
}

#[test]
fn two_fast_steps_overlap_in_time() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("a", "fast", &[], None, &["sleep", "1.5"]) + &step("b", "fast", &[], None, &["sleep", "1.5"]);
    let t0 = Instant::now();
    let r = chain(d.path(), &toml, &["--par", "2"]);
    let took = t0.elapsed().as_secs_f64();
    assert_eq!(code(&r), 0, "{}", r.stdout);
    assert!(took < 2.9, "two 1.5 s steps with --par 2 took {took:.1} s: they did not overlap");
    let p = parse_log(&r.stdout).unwrap();
    let (a, b) = (&p.steps[0], &p.steps[1]);
    assert!(b.start.as_ref().unwrap().instant_us() < a.end().unwrap().instant_us(), "b started after a ended");
    assert_eq!(p.parallel, Some(2));
    assert_eq!(p.mode.as_deref(), Some("preflight"));
}

#[test]
fn par_one_runs_serially() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("a", "fast", &[], None, &["sleep", "1"]) + &step("b", "fast", &[], None, &["sleep", "1"]);
    let t0 = Instant::now();
    let r = chain(d.path(), &toml, &["--par", "1"]);
    assert_eq!(code(&r), 0);
    assert!(t0.elapsed().as_secs_f64() >= 2.0);
}

#[test]
fn a_red_fast_step_reports_every_fast_red_and_skips_slow() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("f1", "fast", &[], None, &["exit", "3"])
        + &step("f2", "fast", &[], None, &["exit", "4"])
        + &step("f3", "fast", &[], None, &["pass"])
        + &step("s1", "slow", &[], None, &["print", "slow ran"]);
    let r = chain(d.path(), &toml, &["--par", "4"]);
    assert_eq!(code(&r), 1);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.red_summary.as_deref(), Some("f1(rc=3) f2(rc=4)"), "{}", r.stdout);
    assert_eq!(p.red_now.len(), 2);
    assert!(p.gates_done);
    assert!(p.steps.iter().all(|s| s.name != "s1"), "slow must not start: {}", r.stdout);
    assert!(!r.out.join("s1.full").exists());
}

#[test]
fn keep_going_runs_slow_after_a_red_fast_step() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("f1", "fast", &[], None, &["exit", "3"]) + &step("s1", "slow", &[], None, &["print", "slow ran"]);
    let r = chain(d.path(), &toml, &["--par", "2", "--keep-going"]);
    assert_eq!(code(&r), 1);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.steps.iter().find(|s| s.name == "s1").unwrap().exit_code, Some(0), "{}", r.stdout);
    assert_eq!(p.red_summary.as_deref(), Some("f1(rc=3)"));
    assert!(std::fs::read_to_string(r.out.join("s1.full")).unwrap().contains("slow ran"));
}

#[test]
fn needs_orders_steps_and_a_red_dependency_skips_dependents() {
    let d = tempfile::tempdir().unwrap();
    // ordering: b needs a; both sleep 1 s with plenty of parallelism, so b must start after a ends
    let toml = step("b", "fast", &["a"], None, &["sleep", "1"]) + &step("a", "fast", &[], None, &["sleep", "1"]);
    let r = chain(d.path(), &toml, &["--par", "4"]);
    assert_eq!(code(&r), 0, "{}", r.stdout);
    let p = parse_log(&r.stdout).unwrap();
    let a = p.steps.iter().find(|s| s.name == "a").unwrap();
    let b = p.steps.iter().find(|s| s.name == "b").unwrap();
    // STEP_TIME is rounded to 0.1 s, so allow that much slack: a ran 1 s, b must start at least ~0.9 s after a started
    assert!(b.start.as_ref().unwrap().instant_us() - a.start.as_ref().unwrap().instant_us() >= 900_000, "b began before a ended");

    // a red dependency: dependents are skipped (no start, no exit), the independent step still runs
    let d2 = tempfile::tempdir().unwrap();
    let toml = step("regen", "fast", &[], None, &["exit", "2"])
        + &step("lint", "fast", &["regen"], None, &["print", "must not run"])
        + &step("docs", "fast", &["lint"], None, &["print", "must not run"])
        + &step("free", "fast", &[], None, &["pass"]);
    let r = chain(d2.path(), &toml, &["--par", "4"]);
    assert_eq!(code(&r), 1);
    assert!(r.stdout.contains("STEP_SKIPPED lint"), "{}", r.stdout);
    assert!(r.stdout.contains("STEP_SKIPPED docs"), "{}", r.stdout);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.red_summary.as_deref(), Some("regen(rc=2)"));
    assert!(p.steps.iter().all(|s| s.name != "lint" && s.name != "docs"));
    assert_eq!(p.steps.iter().find(|s| s.name == "free").unwrap().exit_code, Some(0));
    assert!(!r.out.join("lint.full").exists());
}

#[test]
fn a_full_file_holds_every_line_of_a_long_output() {
    let d = tempfile::tempdir().unwrap();
    let r = chain(d.path(), &step("big", "fast", &[], None, &["lines", "25000"]), &["--par", "1"]);
    assert_eq!(code(&r), 0);
    let full = std::fs::read_to_string(r.out.join("big.full")).unwrap();
    assert_eq!(full.lines().count(), 25000);
    assert!(full.contains("line 24999"));
    assert!(!r.stdout.contains("line 100"), "step output must stay out of stdout");
}

#[test]
fn a_timeout_kills_the_child_and_its_grandchild() {
    let d = tempfile::tempdir().unwrap();
    let marker = d.path().join("grandchild-alive.txt");
    let toml = step("slowpoke", "fast", &[], Some(1), &["gcmarker", marker.to_str().unwrap(), "4"]);
    let t0 = Instant::now();
    let r = chain(d.path(), &toml, &["--par", "1"]);
    // Exit 124 below proves the timeout fired and the missing marker proves the tree was killed; a wall-clock bound under the
    // step's own 4 s sleep flaked on a loaded host (taskkill /T and pgrep take time), so it only guards against a hang now.
    assert!(t0.elapsed().as_secs_f64() < 30.0, "the 1 s timeout did not end the step (hung)");
    assert_eq!(code(&r), 1);
    let p = parse_log(&r.stdout).unwrap();
    assert_eq!(p.steps[0].exit_code, Some(124), "{}", r.stdout);
    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(!marker.exists(), "the grandchild survived the timeout and wrote its marker");
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn stamps(out: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(out)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("stamp-")).collect())
        .unwrap_or_default()
}

#[test]
fn a_clean_green_tree_gets_a_stamp_and_a_dirty_one_does_not() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("f.txt"), "one").unwrap();
    git(repo.path(), &["add", "f.txt"]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    let toml = format!("cwd = '{}'\n\n{}", repo.path().display(), step("ok", "fast", &[], None, &["pass"]));
    let work = tempfile::tempdir().unwrap();

    let r = chain(work.path(), &toml, &["--par", "1"]);
    assert_eq!(code(&r), 0, "{}", r.stdout);
    assert!(r.stdout.starts_with("branch "), "{}", r.stdout);
    let s = stamps(&r.out);
    assert_eq!(s.len(), 1, "a green clean run writes one stamp");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&s[0]).unwrap()).unwrap();
    assert_eq!(v["sha"].as_str().unwrap().len(), 40);
    assert!(s[0].file_name().unwrap().to_string_lossy().contains(v["sha"].as_str().unwrap()));
    for k in ["steps", "host", "digest"] {
        assert!(v.get(k).is_some(), "stamp lacks {k}");
    }

    std::fs::write(repo.path().join("f.txt"), "two").unwrap(); // dirty
    let work2 = tempfile::tempdir().unwrap();
    let r = chain(work2.path(), &toml, &["--par", "1"]);
    assert_eq!(code(&r), 0, "a dirty tree is reported, the run itself is still green");
    assert!(stamps(&r.out).is_empty(), "a dirty tree must write no stamp");
    assert!(String::from_utf8_lossy(&r.o.stderr).contains("dirty"), "{}", String::from_utf8_lossy(&r.o.stderr));
}

#[test]
fn the_output_round_trips_through_parse_and_diagnose() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("g", "fast", &[], None, &["pass"]) + &step("r", "fast", &[], None, &["exit", "1"]);
    let r = chain(d.path(), &toml, &["--par", "2"]);
    let log = d.path().join("run.log");
    std::fs::write(&log, &r.stdout).unwrap();
    let bin = env!("CARGO_BIN_EXE_gatetrain");
    let o = Command::new(bin).args(["parse", log.to_str().unwrap()]).output().unwrap();
    assert!(o.status.success());
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "2 steps, 1 red_now, gates_done=true");
    let o = Command::new(bin).args(["diagnose", log.to_str().unwrap()]).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(text.contains("r"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
    // a green run reads green
    let d2 = tempfile::tempdir().unwrap();
    let r = chain(d2.path(), &step("g", "fast", &[], None, &["pass"]), &["--par", "1"]);
    assert!(r.stdout.contains("RED_SUMMARY: none") && r.stdout.trim_end().ends_with("GATES_DONE"));
}

#[test]
fn config_errors_exit_two() {
    let bad: Vec<(&str, String)> = vec![
        ("unknown key", "[[step]]\nname='a'\ntier='fast'\nrun='x'\nbogus=1\n".into()),
        ("duplicate", format!("{}{}", step("a", "fast", &[], None, &["pass"]), step("a", "fast", &[], None, &["pass"]))),
        ("unknown need", step("a", "fast", &["nope"], None, &["pass"])),
        ("cycle", step("a", "fast", &["b"], None, &["pass"]) + &step("b", "fast", &["a"], None, &["pass"])),
        ("bad tier", "[[step]]\nname='a'\ntier='medium'\nrun='x'\n".into()),
        ("run and argv", "[[step]]\nname='a'\ntier='fast'\nrun='x'\nargv=['y']\n".into()),
        ("fast needs slow", step("a", "fast", &["b"], None, &["pass"]) + &step("b", "slow", &[], None, &["pass"])),
        ("not toml", "this is [ not toml".into()),
    ];
    for (why, toml) in bad {
        let d = tempfile::tempdir().unwrap();
        let r = chain(d.path(), &toml, &["--par", "1"]);
        assert_eq!(code(&r), 2, "{why}: {}", String::from_utf8_lossy(&r.o.stderr));
        assert!(r.stdout.is_empty(), "{why}: nothing on stdout for a config error");
    }
    let d = tempfile::tempdir().unwrap();
    let r = chain(d.path(), &step("a", "fast", &[], None, &["pass"]), &["--par", "1", "--only", "ghost"]);
    assert_eq!(code(&r), 2, "--only naming no step");
}

#[test]
fn only_and_skip_select_steps() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("a", "fast", &[], None, &["pass"]) + &step("b", "fast", &[], None, &["exit", "1"]) + &step("c", "fast", &[], None, &["pass"]);
    let r = chain(d.path(), &toml, &["--par", "2", "--only", "a,c"]);
    assert_eq!(code(&r), 0);
    assert_eq!(parse_log(&r.stdout).unwrap().steps.len(), 2);
    let r = chain(d.path(), &toml, &["--par", "2", "--skip", "b"]);
    assert_eq!(code(&r), 0);
    assert_eq!(parse_log(&r.stdout).unwrap().steps.len(), 2);
}

#[test]
fn a_timeout_keeps_the_output_written_before_the_kill() {
    let d = tempfile::tempdir().unwrap();
    let r = chain(d.path(), &step("hang", "fast", &[], Some(1), &["printsleep", "BEFORE-THE-KILL", "6"]), &["--par", "1"]);
    assert_eq!(code(&r), 1);
    let full = std::fs::read_to_string(r.out.join("hang.full")).unwrap();
    assert!(full.contains("BEFORE-THE-KILL"), "the partial output was lost: {full:?}");
    assert!(full.contains("timed out"), "the timeout note is missing: {full:?}");
}

#[test]
fn a_full_file_grows_while_the_step_runs() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("live", "fast", &[], None, &["printsleep", "LIVE-LINE", "3"]);
    let f = d.path().join("steps.toml");
    std::fs::write(&f, toml).unwrap();
    let out = d.path().join("out");
    let mut c = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap(), "--par", "1"])
        .current_dir(d.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let mid = std::fs::read_to_string(out.join("live.full")).unwrap_or_default();
    let _ = c.wait();
    assert!(mid.contains("LIVE-LINE"), "the .full file was empty while the step ran: {mid:?}");
}

#[test]
fn a_skipped_step_is_a_parsed_marker() {
    let d = tempfile::tempdir().unwrap();
    let toml = step("regen", "fast", &[], None, &["exit", "2"]) + &step("lint", "fast", &["regen"], None, &["pass"]) + &step("api", "slow", &[], None, &["pass"]);
    let r = chain(d.path(), &toml, &["--par", "2"]);
    assert_eq!(code(&r), 1);
    let p = parse_log(&r.stdout).unwrap();
    let names: Vec<&str> = p.skipped.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["lint", "api"], "{}", r.stdout);
    assert!(p.steps.iter().all(|s| s.name != "lint" && s.name != "api"), "a skipped step is not a step that ran");
}

#[test]
fn a_relative_cwd_resolves_against_the_steps_file() {
    let d = tempfile::tempdir().unwrap();
    let sub = d.path().join("conf");
    std::fs::create_dir_all(sub.join("work")).unwrap();
    std::fs::write(sub.join("work").join("here.txt"), "x").unwrap();
    let f = sub.join("steps.toml");
    std::fs::write(&f, format!("cwd = 'work'\n{}", step("look", "fast", &[], None, &["exists", "here.txt"]))).unwrap();
    let out = d.path().join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()])
        .current_dir(d.path())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stdout));
}

// ---- the step cache (`--cache <dir>`) ----

/// A step with `inputs` that touches `marker` when it really runs (or runs `args`).
fn cstep(name: &str, needs: &[&str], inputs: &[&str], marker: &Path, args: &[&str]) -> String {
    let m = marker.to_str().unwrap().to_string();
    let default = ["marker", m.as_str(), "0"];
    let mut s = step(name, "fast", needs, None, if args.is_empty() { &default } else { args });
    if !inputs.is_empty() {
        let list = inputs.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ");
        s = s.replacen("argv = ", &format!("inputs = [{list}]\nargv = "), 1);
    }
    s
}

fn cache_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("in.txt"), "one").unwrap();
    std::fs::write(repo.path().join("other.txt"), "x").unwrap();
    git(repo.path(), &["add", "in.txt", "other.txt"]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    repo
}

fn with_cwd(repo: &Path, body: &str) -> String {
    format!("cwd = '{}'\n\n{body}", repo.display())
}

fn entries(cache: &Path, step: &str) -> usize {
    std::fs::read_dir(cache.join(step)).map(|rd| rd.flatten().count()).unwrap_or(0)
}

#[test]
fn a_second_run_with_unchanged_inputs_is_cached_and_does_not_run() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let toml = with_cwd(repo.path(), &cstep("a", &[], &["in.txt"], &marker, &[]));
    let c = cache.path().to_str().unwrap();
    let r1 = chain(work.path(), &toml, &["--cache", c]);
    assert_eq!(code(&r1), 0, "{}", r1.stdout);
    assert!(marker.exists() && !r1.stdout.contains("STEP_CACHED"), "{}", r1.stdout);
    assert_eq!(entries(cache.path(), "a"), 1);
    std::fs::remove_file(&marker).unwrap();
    let r2 = chain(work.path(), &toml, &["--cache", c]);
    assert_eq!(code(&r2), 0, "{}", r2.stdout);
    assert!(r2.stdout.contains("STEP_CACHED a"), "{}", r2.stdout);
    assert!(!marker.exists(), "a cached step must not run");
    let full = std::fs::read_to_string(r2.out.join("a.full")).unwrap();
    assert!(full.starts_with("cached: green at "), "{full}");
    // the log parses with the existing parser and says cached
    let p = parse_log(&r2.stdout).unwrap();
    assert!(p.steps[0].cached && p.steps[0].exit_code == Some(0), "{}", r2.stdout);
    // the stamp lists what was cached
    let st = stamps(&r2.out);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&st[0]).unwrap()).unwrap();
    assert_eq!(v["cached"], serde_json::json!(["a"]));
}

#[test]
fn without_the_flag_nothing_is_cached() {
    let repo = cache_repo();
    let (work, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let toml = with_cwd(repo.path(), &cstep("a", &[], &["in.txt"], &marker, &[]));
    chain(work.path(), &toml, &[]);
    std::fs::remove_file(&marker).unwrap();
    let r = chain(work.path(), &toml, &[]);
    assert!(marker.exists() && !r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
}

#[test]
fn changing_an_input_runs_the_step_again() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let toml = with_cwd(repo.path(), &cstep("a", &[], &["in.txt"], &marker, &[]));
    let c = cache.path().to_str().unwrap();
    chain(work.path(), &toml, &["--cache", c]);
    std::fs::remove_file(&marker).unwrap();
    std::fs::write(repo.path().join("in.txt"), "two").unwrap();
    git(repo.path(), &["commit", "-q", "-am", "change"]);
    let r = chain(work.path(), &toml, &["--cache", c]);
    assert!(marker.exists() && !r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
    assert_eq!(entries(cache.path(), "a"), 2);
    // a change outside the inputs keeps the hit
    std::fs::remove_file(&marker).unwrap();
    std::fs::write(repo.path().join("other.txt"), "y").unwrap();
    git(repo.path(), &["commit", "-q", "-am", "other"]);
    let r = chain(work.path(), &toml, &["--cache", c]);
    assert!(!marker.exists() && r.stdout.contains("STEP_CACHED a"), "{}", r.stdout);
}

#[test]
fn a_dirty_input_runs_the_step_and_writes_no_entry() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    std::fs::write(repo.path().join("in.txt"), "dirty").unwrap();
    let toml = with_cwd(repo.path(), &cstep("a", &[], &["in.txt"], &marker, &[]));
    let c = cache.path().to_str().unwrap();
    for _ in 0..2 {
        let _ = std::fs::remove_file(&marker);
        let r = chain(work.path(), &toml, &["--cache", c]);
        assert!(marker.exists() && !r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
        let full = std::fs::read_to_string(r.out.join("a.full")).unwrap();
        assert!(full.contains("not cached: inputs are dirty"), "{full}");
    }
    assert_eq!(entries(cache.path(), "a"), 0);
}

#[test]
fn a_step_without_inputs_always_runs() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let toml = with_cwd(repo.path(), &cstep("a", &[], &[], &marker, &[]));
    let c = cache.path().to_str().unwrap();
    chain(work.path(), &toml, &["--cache", c]);
    std::fs::remove_file(&marker).unwrap();
    let r = chain(work.path(), &toml, &["--cache", c]);
    assert!(marker.exists() && !r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
    assert_eq!(entries(cache.path(), "a"), 0);
}

#[test]
fn a_red_step_writes_no_entry() {
    let repo = cache_repo();
    let (work, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let toml = with_cwd(repo.path(), &cstep("a", &[], &["in.txt"], Path::new("x"), &["exit", "3"]));
    let c = cache.path().to_str().unwrap();
    for _ in 0..2 {
        let r = chain(work.path(), &toml, &["--cache", c]);
        assert_eq!(code(&r), 1, "{}", r.stdout);
        assert!(!r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
    }
    assert_eq!(entries(cache.path(), "a"), 0);
}

#[test]
fn a_dependent_of_a_cached_step_runs_normally() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (ma, mb) = (mk.path().join("a"), mk.path().join("b"));
    let toml = with_cwd(repo.path(), &(cstep("a", &[], &["in.txt"], &ma, &[]) + &cstep("b", &["a"], &[], &mb, &[])));
    let c = cache.path().to_str().unwrap();
    chain(work.path(), &toml, &["--cache", c]);
    std::fs::remove_file(&ma).unwrap();
    std::fs::remove_file(&mb).unwrap();
    let r = chain(work.path(), &toml, &["--cache", c]);
    assert_eq!(code(&r), 0, "{}", r.stdout);
    assert!(!ma.exists() && mb.exists(), "{}", r.stdout);
    let p = parse_log(&r.stdout).unwrap();
    assert!(p.steps[0].cached && !p.steps[1].cached && p.skipped.is_empty(), "{}", r.stdout);
}

#[test]
fn no_cache_for_forces_a_run() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let toml = with_cwd(repo.path(), &cstep("a", &[], &["in.txt"], &marker, &[]));
    let c = cache.path().to_str().unwrap();
    chain(work.path(), &toml, &["--cache", c]);
    std::fs::remove_file(&marker).unwrap();
    let r = chain(work.path(), &toml, &["--cache", c, "--no-cache-for", "a"]);
    assert!(marker.exists() && !r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
    let r = chain(work.path(), &toml, &["--cache", c, "--no-cache-for", "nope"]);
    assert_eq!(code(&r), 2, "an unknown step name is a config error");
}

#[test]
fn the_salt_is_part_of_the_key() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let base = cstep("a", &[], &["in.txt"], &marker, &[]);
    let c = cache.path().to_str().unwrap();
    chain(work.path(), &with_cwd(repo.path(), &base), &["--cache", c]);
    std::fs::remove_file(&marker).unwrap();
    let salted = base.replacen("argv = ", "cache_salt = 'v2'\nargv = ", 1);
    let r = chain(work.path(), &with_cwd(repo.path(), &salted), &["--cache", c]);
    assert!(marker.exists() && !r.stdout.contains("STEP_CACHED"), "{}", r.stdout);
}

#[test]
fn diagnose_reads_a_log_with_a_cached_step() {
    let repo = cache_repo();
    let (work, cache, mk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let marker = mk.path().join("ran");
    let toml = with_cwd(repo.path(), &(cstep("a", &[], &["in.txt"], &marker, &[]) + &step("r", "fast", &[], None, &["exit", "1"])));
    let c = cache.path().to_str().unwrap();
    chain(work.path(), &toml, &["--cache", c]);
    let r = chain(work.path(), &toml, &["--cache", c]);
    assert!(r.stdout.contains("STEP_CACHED a"), "{}", r.stdout);
    let log = work.path().join("run.log");
    std::fs::write(&log, &r.stdout).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["diagnose", log.to_str().unwrap()]).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    assert!(text.contains('r') && !text.contains("panicked"), "{text}");
}
