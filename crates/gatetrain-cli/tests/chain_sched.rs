//! Scheduling: `--order longest|file` (history medians decide who starts first) and `cores = N` with `--cores K` (a core budget).
//! Orders are asserted from the log's STEP_START / STEP_EXIT lines, never from wall-clock bounds.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn child() -> String {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p: PathBuf = dir.join(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} is not built: run `cargo test --workspace`", p.display());
    p.to_string_lossy().into_owned()
}

/// `extra` is raw TOML lines placed in the step.
fn step(name: &str, extra: &str, secs: &str) -> String {
    format!("[[step]]\nname = '{name}'\ntier = 'fast'\n{extra}\nargv = ['{}', 'sleep', '{secs}']\n\n", child())
}

fn run(work: &Path, body: &str, extra: &[&str]) -> Output {
    let f = work.join("steps.toml");
    std::fs::write(&f, body).unwrap();
    let out = work.join(format!("out-{}", std::fs::read_dir(work).unwrap().count()));
    Command::new(env!("CARGO_BIN_EXE_gatetrain"))
        .args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap(), "--samples", "0"])
        .args(extra)
        .current_dir(work)
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// the names of the STEP_START lines, in the order they were printed
fn starts(o: &Output) -> Vec<String> {
    text(o).lines().filter_map(|l| l.strip_prefix("STEP_START ")).map(|l| l.split(' ').next().unwrap().to_string()).collect()
}

/// the position of a line in the log
fn at(o: &Output, prefix: &str) -> usize {
    text(o).lines().position(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no {prefix} line in:\n{}", text(o)))
}

/// `long` is listed last and sleeps the longest
fn three() -> String {
    step("a", "", "0.05") + &step("b", "", "0.05") + &step("long", "", "0.6")
}

fn with_history() -> (tempfile::TempDir, PathBuf) {
    let w = tempfile::tempdir().unwrap();
    let db = w.path().join("ledger.db");
    let first = run(w.path(), &three(), &["--par", "1", "--db", db.to_str().unwrap()]);
    assert_eq!(first.status.code(), Some(0), "{}", String::from_utf8_lossy(&first.stderr));
    assert_eq!(starts(&first), ["a", "b", "long"], "with no history the first run is in file order");
    (w, db)
}

#[test]
fn with_history_the_longest_step_starts_first() {
    let (w, db) = with_history();
    let o = run(w.path(), &three(), &["--par", "1", "--db", db.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(starts(&o), ["long", "a", "b"], "ties keep file order: {}", text(&o));
    assert!(text(&o).contains("CHAIN_ORDER longest"), "{}", text(&o));
}

#[test]
fn order_file_keeps_file_order_even_with_history() {
    let (w, db) = with_history();
    let o = run(w.path(), &three(), &["--par", "1", "--db", db.to_str().unwrap(), "--order", "file"]);
    assert_eq!(starts(&o), ["a", "b", "long"]);
    assert!(text(&o).contains("CHAIN_ORDER file"));
}

#[test]
fn without_history_the_order_is_the_file_order() {
    let w = tempfile::tempdir().unwrap();
    let o = run(w.path(), &three(), &["--par", "1"]);
    assert_eq!(starts(&o), ["a", "b", "long"]);
    assert!(text(&o).contains("CHAIN_ORDER file"), "{}", text(&o));
    // asking for longest with nothing to go on is file order too, and the header says so
    let o = run(w.path(), &three(), &["--par", "1", "--order", "longest"]);
    assert_eq!(starts(&o), ["a", "b", "long"]);
    assert!(text(&o).contains("CHAIN_ORDER file"), "{}", text(&o));
}

#[test]
fn two_three_core_steps_never_overlap_under_four_cores_and_a_one_core_step_fills_the_gap() {
    let w = tempfile::tempdir().unwrap();
    let body = step("x", "cores = 3", "0.6") + &step("y", "cores = 3", "0.6") + &step("z", "cores = 1", "0.1");
    let o = run(w.path(), &body, &["--par", "4", "--cores", "4"]);
    assert_eq!(o.status.code(), Some(0), "{}{}", text(&o), String::from_utf8_lossy(&o.stderr));
    assert!(at(&o, "STEP_START z") < at(&o, "STEP_EXIT x"), "z fills the gap beside x: {}", text(&o));
    assert!(at(&o, "STEP_START y") > at(&o, "STEP_EXIT x"), "y waits for x's cores: {}", text(&o));
    assert!(text(&o).contains("CHAIN_CORES 4"));
}

#[test]
fn a_step_wanting_more_cores_than_the_budget_is_a_config_error() {
    let w = tempfile::tempdir().unwrap();
    let o = run(w.path(), &step("big", "cores = 5", "0"), &["--cores", "4"]);
    assert_eq!(o.status.code(), Some(2));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("big") && e.contains("cores"), "{e}");
    assert!(starts(&o).is_empty());
}

#[test]
fn cores_zero_is_a_config_error() {
    let w = tempfile::tempdir().unwrap();
    assert_eq!(run(w.path(), &step("s", "cores = 0", "0"), &[]).status.code(), Some(2));
}

#[cfg(target_os = "linux")]
#[test]
fn a_pinned_step_really_runs_on_its_reserved_cpus() {
    let n = std::thread::available_parallelism().map_or(1, |n| n.get());
    if n < 2 || Command::new("taskset").arg("--version").output().is_err() {
        return;
    }
    let w = tempfile::tempdir().unwrap();
    let body = format!("[[step]]\nname = 'p'\ntier = 'fast'\ncores = 2\nrun = \"grep Cpus_allowed_list /proc/self/status\"\n");
    let o = run(w.path(), &body, &["--cores", &n.to_string()]);
    assert_eq!(o.status.code(), Some(0));
    let full = std::fs::read_dir(w.path()).unwrap().filter_map(Result::ok).find(|e| e.file_name().to_string_lossy().starts_with("out-")).unwrap().path().join("p.full");
    let got = std::fs::read_to_string(full).unwrap();
    assert!(got.contains("0-1"), "{got}");
}
