//! Byte parity of the `gatetrain` binary with live Python (`python -m gatetrain`) on a synthetic corpus on a REAL disk:
//! real git worktrees, a real link, cargo-like build folders and a recycle bin with set modification times.
//! `check`, `check --json` and `tidy` (dry run) must print identical bytes and exit with the same code.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

fn fwd(p: &Path) -> String {
    let s = p.canonicalize().unwrap().to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").map(String::from).unwrap_or(s)
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn write(p: impl AsRef<Path>, text: &str) {
    if let Some(d) = p.as_ref().parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, text).unwrap();
}

fn set_age(p: &Path, secs: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs);
    #[cfg(windows)]
    let f = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().access_mode(0x100).share_mode(7).custom_flags(0x0200_0000).open(p).unwrap() // FILE_WRITE_ATTRIBUTES, BACKUP_SEMANTICS
    };
    #[cfg(not(windows))]
    let f = std::fs::File::open(p).unwrap();
    f.set_modified(t).unwrap();
}

fn cargo_like(dir: &Path, bytes: usize, age: u64) {
    write(dir.join("debug/big.o"), &"x".repeat(bytes));
    write(dir.join("tmp/t.txt"), "t");
    write(dir.join("CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55\n");
    for f in ["debug/big.o", "tmp/t.txt", "CACHEDIR.TAG"] {
        set_age(&dir.join(f), age);
    }
    set_age(&dir.join("debug"), age);
    set_age(&dir.join("tmp"), age);
    set_age(dir, age);
}

fn dir_link(link: &Path, target: &Path) {
    if cfg!(windows) {
        let o = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link.to_string_lossy().replace('/', "\\")).arg(target.to_string_lossy().replace('/', "\\")).output().unwrap();
        assert!(o.status.success());
    } else {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

const TEN_DAYS: u64 = 10 * 86400 + 600; // +10 min: far from an hour boundary, so "240h" cannot flip between the two runs

/// Build the corpus; returns the hosts.toml path.
fn corpus(root: &Path) -> String {
    let main = root.join("main");
    std::fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    write(main.join("f.txt"), "one\n");
    write(main.join("shared/keep.txt"), "shared\n");
    write(main.join(".gitignore"), "jlink\n");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-q", "-m", "base"]);
    let wt = |name: &str, branch: &str| {
        git(&main, &["worktree", "add", "-q", "-b", branch, &root.join("wt").join(name).to_string_lossy()]);
        root.join("wt").join(name)
    };
    let a = wt("lane-a", "lane-a"); // merged, clean, holds a link
    dir_link(&a.join("jlink"), &main.join("shared"));
    let b = wt("lane-b", "lane-b"); // not merged
    write(b.join("only-here.txt"), "work\n");
    git(&b, &["add", "."]);
    git(&b, &["commit", "-q", "-m", "b"]);
    let c = wt("lane-c", "lane-c"); // merged, dirty
    write(c.join("scratch.txt"), "dirty\n");
    let e = wt("lane-e", "lane-e"); // merged by content (a squash)
    write(e.join("e.txt"), "e1\ne2\n");
    git(&e, &["add", "."]);
    git(&e, &["commit", "-q", "-m", "e"]);
    wt("zz", "line/zz"); // a slash in the branch name
    write(main.join("e.txt"), "e1\ne2\ne3\n");
    git(&main, &["add", "e.txt"]);
    git(&main, &["commit", "-q", "-m", "squash of e, plus more"]);
    // build folders
    for (n, age) in [("lane-a", TEN_DAYS), ("lane-b", TEN_DAYS), ("lane-e", TEN_DAYS), ("new", 300)] {
        cargo_like(&root.join("targets").join(n), 3000, age);
    }
    write(root.join("targets/orphan/blob.bin"), "orphan data");
    set_age(&root.join("targets/orphan/blob.bin"), TEN_DAYS);
    set_age(&root.join("targets/orphan"), TEN_DAYS);
    cargo_like(&root.join("chain"), 5000, TEN_DAYS);
    // recycle bin
    write(root.join("trash/old/x.bin"), &"y".repeat(2048));
    write(root.join("trash/new/x.bin"), "z");
    set_age(&root.join("trash/old/x.bin"), 20 * 86400 + 600);
    set_age(&root.join("trash/old"), 20 * 86400 + 600);
    set_age(&root.join("trash/new/x.bin"), 86400);
    set_age(&root.join("trash/new"), 86400);
    // a browser profile where none belongs
    write(root.join("evidence/run1/profile/SingletonLock"), "");
    let r = fwd(root);
    let toml = format!(
        r#"[[hosts]]
name = "box"
reach = "local"

[[hosts.disks]]
path = "{r}/no-such-disk"
min_free_gb = 5

[hosts.tidy]
main_checkout = "{r}/main"
root_checkout = "{r}/root"
trash_dir = "{r}/trash"
target_roots = ["{r}/targets"]
evidence_dirs = ["{r}/evidence"]
log = "{r}/tidy-log.jsonl"
targets = [{{ path = "{r}/chain", budget_gb = 0.000001 }}]
"#
    );
    let p = root.join("hosts.toml");
    std::fs::write(&p, toml).unwrap();
    fwd(&p)
}

const PY_MAIN: &str = "import sys; sys.stdout.reconfigure(newline='\\n', encoding='utf-8'); sys.stderr.reconfigure(newline='\\n', encoding='utf-8'); sys.path.insert(0, sys.argv[1]); from gatetrain.cli import main; sys.exit(main(sys.argv[2:]))";

struct Ran {
    out: Vec<u8>,
    err: Vec<u8>,
    rc: Option<i32>,
}

fn run_python(cwd: &Path, args: &[&str]) -> Ran {
    let o = Command::new(python()).current_dir(cwd).env("PYTHONUTF8", "1").arg("-c").arg(PY_MAIN).arg(repo()).args(args).output().expect("python must be installed to prove parity");
    Ran { out: o.stdout, err: o.stderr, rc: o.status.code() }
}

fn run_rust(cwd: &Path, args: &[&str]) -> Ran {
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).current_dir(cwd).env("GATETRAIN_PYROOT", repo()).env("PYTHONUTF8", "1").args(args).output().unwrap();
    // On Windows `check` and any removal are forwarded to Python, whose console text mode writes CRLF; the committed truth is LF.
    let lf = |b: Vec<u8>| String::from_utf8_lossy(&b).replace('\r', "").into_bytes();
    let (out, err) = if cfg!(windows) { (lf(o.stdout), lf(o.stderr)) } else { (o.stdout, o.stderr) };
    Ran { out, err, rc: o.status.code() }
}

#[test]
#[ignore = "Rust check/tidy are NOT ported past the hardening of c46717c; the CLI forwards them to Python until a lane ports it (spec tests/test_tidy_harden.py)"]
fn check_json_and_tidy_dry_run_match_live_python_on_a_real_corpus() {
    let d = tempfile::Builder::new().prefix("gt-parity-").tempdir().unwrap();
    let conf = corpus(d.path());
    let cases: Vec<Vec<&str>> = vec![
        vec!["check", "--config", &conf],
        vec!["check", "--config", &conf, "--json"],
        vec!["tidy", "--config", &conf],
        vec!["tidy", "--config", &conf, "--class", "targets,trash"],
    ];
    let mut compared = 0usize;
    let mut bad = vec![];
    for args in &cases {
        let (py, rs) = std::thread::scope(|s| {
            let a = s.spawn(|| run_python(d.path(), args));
            let b = s.spawn(|| run_rust(d.path(), args));
            (a.join().unwrap(), b.join().unwrap())
        });
        // the run must have produced real output: a pass with nothing printed has proven nothing
        let text = String::from_utf8_lossy(&py.out).into_owned();
        assert!(text.contains("lane-a") && (text.contains("would tidy") || text.contains("\"plan\"")), "python printed nothing useful: {text}\n{}", String::from_utf8_lossy(&py.err));
        compared += py.out.len();
        if py.out != rs.out || py.err != rs.err || py.rc != rs.rc {
            bad.push(format!(
                "args {args:?}\n--- python rc={:?}\n{}{}\n--- rust rc={:?}\n{}{}",
                py.rc,
                String::from_utf8_lossy(&py.out),
                String::from_utf8_lossy(&py.err),
                rs.rc,
                String::from_utf8_lossy(&rs.out),
                String::from_utf8_lossy(&rs.err)
            ));
        }
    }
    println!("real-corpus parity: {} cases, {} bytes of python output compared", cases.len(), compared);
    assert!(bad.is_empty(), "{}", bad.join("\n=====\n"));
}
