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
    assert!(b.start.as_ref().unwrap().instant_us() >= a.end().unwrap().instant_us(), "b began before a ended");

    // a red dependency: dependents are skipped (no start, no exit), the independent step still runs
    let d2 = tempfile::tempdir().unwrap();
    let toml = step("regen", "fast", &[], None, &["exit", "2"])
        + &step("lint", "fast", &["regen"], None, &["print", "must not run"])
        + &step("docs", "fast", &["lint"], None, &["print", "must not run"])
        + &step("free", "fast", &[], None, &["pass"]);
    let r = chain(d2.path(), &toml, &["--par", "4"]);
    assert_eq!(code(&r), 1);
    assert!(r.stdout.contains("SKIPPED lint"), "{}", r.stdout);
    assert!(r.stdout.contains("SKIPPED docs"), "{}", r.stdout);
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
    assert!(t0.elapsed().as_secs_f64() < 3.5, "the 1 s timeout did not end the step on time");
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
