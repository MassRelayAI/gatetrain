//! `check` and `tidy` (dry run) on a synthetic corpus on a REAL disk: real git worktrees, a real link, cargo-like build folders and
//! a recycle bin with set modification times. The byte-for-byte comparison with the Python version was made at the commit that
//! retired it (the same corpus, four commands, identical bytes); what stays is what that text said, line by line.
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

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
    // a Windows host must prove its link listing works: one folder holding one known junction (the canary)
    let canary = if cfg!(windows) {
        std::fs::create_dir_all(root.join("canary-target")).unwrap();
        std::fs::create_dir_all(root.join("canary")).unwrap();
        dir_link(&root.join("canary/l"), &root.join("canary-target"));
        format!("link_canary = \"{}/canary\"
link_canary_count = 1
", fwd(root))
    } else {
        String::new()
    };
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
worktree_min_idle_days = 0
{canary}"#
    );
    let p = root.join("hosts.toml");
    std::fs::write(&p, toml).unwrap();
    fwd(&p)
}

struct Ran {
    out: Vec<u8>,
    err: Vec<u8>,
    rc: Option<i32>,
}

fn run_rust(cwd: &Path, args: &[&str]) -> Ran {
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).current_dir(cwd).args(args).output().unwrap();
    let lf = |b: Vec<u8>| String::from_utf8_lossy(&b).replace('\r', "").into_bytes();
    let (out, err) = (lf(o.stdout), lf(o.stderr));
    Ran { out, err, rc: o.status.code() }
}

/// stderr with the ` [12s]` suffix of slow progress lines removed.
fn untimed(err: &[u8]) -> String {
    String::from_utf8_lossy(err)
        .lines()
        .map(|l| match l.rfind(" [") {
            Some(i) if l.starts_with("tidy: ") && l.ends_with("s]") && l[i + 2..l.len() - 2].chars().all(|c| c.is_ascii_digit()) && l.len() > i + 4 => &l[..i],
            _ => l,
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

#[test]
fn untimed_drops_only_the_timing_suffix() {
    assert_eq!(untimed(b"tidy: target 2/6 /x: tidy (3.0 KB) [5s]\nguard [5s]\ntidy: a [x5s]\n"), "tidy: target 2/6 /x: tidy (3.0 KB)\nguard [5s]\ntidy: a [x5s]\n");
}

#[test]
fn check_json_and_tidy_dry_run_say_what_the_corpus_holds() {
    let d = tempfile::Builder::new().prefix("gt-parity-").tempdir().unwrap();
    let conf = corpus(d.path());
    let text = |args: &[&str]| {
        let r = run_rust(d.path(), args);
        (String::from_utf8_lossy(&r.out).into_owned(), untimed(&r.err), r.rc)
    };
    let (check, err, rc) = text(&["check", "--config", &conf]);
    assert_eq!(rc, Some(1), "a missing disk and an over-budget target cross a threshold: {check}");
    assert!(err.contains("tidy: worktree 1/"), "the CLI streams progress: {err}");
    assert!(check.contains("would tidy"), "{check}");
    for lane in ["lane-a", "lane-e"] {
        assert!(check.lines().any(|l| l.contains(lane) && l.contains("worktree")) || check.contains(lane), "{lane} is merged and clean: {check}");
    }
    let (json, _, _) = text(&["check", "--config", &conf, "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let plan = v[0]["plan"].as_array().or_else(|| v["plan"].as_array()).expect("a plan array");
    let keys: Vec<&str> = plan.iter().map(|i| i["key"].as_str().unwrap()).collect();
    assert!(keys.iter().any(|k| k.ends_with("wt/lane-a")), "{keys:?}");
    assert!(keys.iter().any(|k| k.ends_with("wt/zz")), "a branch with a slash in its name: {keys:?}");
    assert!(!keys.iter().any(|k| k.ends_with("wt/lane-b")), "lane-b is not merged: {keys:?}");
    assert!(!keys.iter().any(|k| k.ends_with("wt/lane-c")), "lane-c is dirty: {keys:?}");
    assert!(!keys.iter().any(|k| k.contains("trash")), "a recycle-bin entry ages from when tidy first saw it, so none is old on a first run: {keys:?}");
    let (tidy, _, _) = text(&["tidy", "--config", &conf]);
    assert!(tidy.contains("lane-a") && tidy.contains("would tidy"), "{tidy}");
    let (tidy2, _, _) = text(&["tidy", "--config", &conf, "--class", "targets,trash"]);
    assert!(tidy2.contains("would tidy"), "{tidy2}");
    // a dry run changes nothing
    assert!(d.path().join("wt/lane-a").exists() && d.path().join("trash/old").exists());
}
