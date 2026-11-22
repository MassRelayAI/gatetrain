//! Adversarial tests for `chain --since` (docs/since.md): each asserts the SAFE behaviour, a red here is a false green.
use std::path::{Path, PathBuf};
use std::process::Command;

fn child() -> String {
    // the child sits next to the test binary's target folder; newer cargo keeps test binaries in a deeper build folder, so look upward
    let exe = std::env::current_exe().unwrap();
    let name = format!("gt-testchild{}", std::env::consts::EXE_SUFFIX);
    let p = exe.ancestors().skip(1).take(6).map(|d| d.join(&name)).find(|p| p.is_file());
    p.unwrap_or_else(|| panic!("{name} is not built: run `cargo test --workspace`")).to_string_lossy().into_owned()
}
fn step(name: &str, extra: &str, args: &[&str]) -> String {
    let mut argv = vec![format!("'{}'", child())];
    argv.extend(args.iter().map(|a| format!("'{a}'")));
    format!("[[step]]\nname = '{name}'\ntier = 'fast'\n{extra}\nargv = [{}]\n\n", argv.join(", "))
}
fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}
fn put(repo: &Path, rel: &str, text: &str) {
    let p = repo.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
}
fn commit_all(repo: &Path, m: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", m]);
    git(repo, &["rev-parse", "HEAD"])
}
fn chain(repo: &Path, work: &Path, body: &str, extra: &[&str]) -> (i32, String, PathBuf) {
    let f = work.join("steps.toml");
    std::fs::write(&f, format!("cwd = '{}'\n\n{body}", repo.display())).unwrap();
    let out = work.join("out");
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["chain", "--steps", f.to_str().unwrap(), "--out", out.to_str().unwrap()]).args(extra).current_dir(work).output().unwrap();
    (o.status.code().unwrap(), String::from_utf8_lossy(&o.stdout).into_owned(), out)
}

/// ADV-1: a gitignored file inside the declared inputs changes; git sees nothing; a fresh run is red.
#[test]
fn adv1_ignored_file_inside_inputs() {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    put(r.path(), ".gitignore", "a/local.cfg\n");
    put(r.path(), "a/x.txt", "1");
    put(r.path(), "a/local.cfg", "k=v");
    let sha1 = commit_all(r.path(), "init");
    let cfg = r.path().join("a/local.cfg");
    let body = step("A", "inputs = ['a/**']", &["exists", cfg.to_str().unwrap()]);
    let (c1, o1, _) = chain(r.path(), w.path(), &body, &["--run-id", "run-1"]);
    assert_eq!(c1, 0, "{o1}");
    std::fs::remove_file(&cfg).unwrap();
    let (c2, o2, out) = chain(r.path(), w.path(), &body, &["--since", &sha1, "--run-id", "run-2"]);
    let stamp = out.join(format!("stamp-{sha1}.json"));
    let st: serde_json::Value = std::fs::read_to_string(&stamp).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    eprintln!("ADV-1 rc={c2} stamp partial={:?} steps={}", st["partial"], st["steps"]);
    assert_ne!(c2, 0, "FALSE GREEN: ignored input changed, A reused, rc=0: {o2}");
}

/// ADV-2: inputs that match no tracked file (stale after a move, or a typo) never change, so the step is reused forever.
#[test]
fn adv2_inputs_matching_nothing() {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    put(r.path(), "src/x.txt", "1");
    let sha1 = commit_all(r.path(), "init");
    let x = r.path().join("src/x.txt");
    let body = step("A", "inputs = ['old-src/**']", &["exists", x.to_str().unwrap()]);
    let (c1, o1, _) = chain(r.path(), w.path(), &body, &["--run-id", "run-1"]);
    assert_eq!(c1, 0, "{o1}");
    git(r.path(), &["rm", "-q", "src/x.txt"]);
    put(r.path(), "keep.txt", "k");
    commit_all(r.path(), "break");
    let (c2, o2, _) = chain(r.path(), w.path(), &body, &["--since", &sha1, "--run-id", "run-2"]);
    assert_ne!(c2, 0, "FALSE GREEN: inputs match no tracked file yet A was reused: {o2}");
}

/// ADV-3 (Unix: a tracked symlink): a tracked symlink inside the inputs points outside them; the target is deleted in a commit.
#[cfg(unix)]
#[test]
fn adv3_symlink_out_of_inputs() {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    put(r.path(), "shared/f.txt", "1");
    put(r.path(), "a/x.txt", "1");
    std::os::unix::fs::symlink("../shared", r.path().join("a/link")).unwrap();
    let sha1 = commit_all(r.path(), "init");
    let t = r.path().join("a/link/f.txt");
    let body = step("A", "inputs = ['a/**']", &["exists", t.to_str().unwrap()]);
    let (c1, o1, _) = chain(r.path(), w.path(), &body, &["--run-id", "run-1"]);
    assert_eq!(c1, 0, "{o1}");
    git(r.path(), &["rm", "-q", "shared/f.txt"]);
    put(r.path(), "shared/g.txt", "g");
    commit_all(r.path(), "break");
    let (c2, o2, _) = chain(r.path(), w.path(), &body, &["--since", &sha1, "--run-id", "run-2"]);
    assert_ne!(c2, 0, "FALSE GREEN: symlinked target changed, A reused: {o2}");
}

/// CONTROL: a rename OUT of the inputs and a delete inside them are both seen (expected to pass).
#[test]
fn control_rename_out_of_inputs_reruns() {
    let (r, w) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    git(r.path(), &["init", "-q"]);
    put(r.path(), "a/x.txt", "some longer content so rename detection pairs it\n");
    let sha1 = commit_all(r.path(), "init");
    let x = r.path().join("a/x.txt");
    let body = step("A", "inputs = ['a/**']", &["exists", x.to_str().unwrap()]);
    let (c1, _, _) = chain(r.path(), w.path(), &body, &["--run-id", "run-1"]);
    assert_eq!(c1, 0);
    git(r.path(), &["mv", "a/x.txt", "c.txt"]);
    commit_all(r.path(), "mv");
    let (c2, o2, _) = chain(r.path(), w.path(), &body, &["--since", &sha1, "--run-id", "run-2"]);
    assert_ne!(c2, 0, "rename out missed: {o2}");
    assert!(o2.contains("STEP_RERUN A its inputs changed"), "{o2}");
}
