//! `gatetrain watch` on a real git repository: every kind of lane is reported in its place, the next assignments come from the
//! ledger and the plan index, and the repository is byte-for-byte unchanged afterwards (no index refresh, no object, no ref).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const NOW: i64 = 1_800_000_000;

fn git(dir: &Path, args: &[&str], date: Option<i64>) {
    let mut c = Command::new("git");
    c.args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"]).args(args).current_dir(dir);
    let d = format!("@{} +0000", date.unwrap_or(NOW - 600));
    c.env("GIT_AUTHOR_DATE", &d).env("GIT_COMMITTER_DATE", &d);
    let o = c.output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn commit(dir: &Path, file: &str, text: &str, msg: &str, date: Option<i64>) {
    write(&dir.join(file), text);
    git(dir, &["add", "--", file], date);
    git(dir, &["commit", "-q", "-m", msg], date);
}

/// Every file under `root` with its length and modification time: a write of any kind shows up here.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    let mut m = BTreeMap::new();
    let mut st = vec![root.to_path_buf()];
    while let Some(d) = st.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = std::fs::symlink_metadata(e.path()).unwrap();
            if md.is_dir() {
                st.push(e.path());
            }
            m.insert(e.path(), (md.len(), md.modified().unwrap()));
        }
    }
    m
}

struct Fixture {
    _d: tempfile::TempDir,
    root: PathBuf,
    conf: PathBuf,
}

fn fixture() -> Fixture {
    let d = tempfile::Builder::new().prefix("gt-watch-").tempdir().unwrap();
    let root = d.path().to_path_buf();
    let m = root.join("main");
    std::fs::create_dir_all(&m).unwrap();
    git(&m, &["init", "-q", "-b", "main"], None);
    commit(&m, "a.txt", "one\n", "base", Some(NOW - 30 * 86400));
    // merged by ancestry, with a clean worktree left behind
    git(&m, &["branch", "line/old-done"], None);
    commit(&m, "b.txt", "b\n", "main moves on", Some(NOW - 20 * 86400));
    git(&m, &["worktree", "add", "-q", root.join("wt-old-done").to_str().unwrap(), "line/old-done"], None);
    // merged by patch: the same change cherry-picked onto main
    git(&m, &["checkout", "-q", "-b", "line/picked"], None);
    commit(&m, "p.txt", "p\n", "picked change", Some(NOW - 5 * 86400));
    git(&m, &["checkout", "-q", "main"], None);
    git(&m, &["cherry-pick", "line/picked"], Some(NOW - 4 * 86400));
    // live and ready: built in the ledger, clean worktree, merges into main
    git(&m, &["checkout", "-q", "-b", "line/live-ready"], None);
    commit(&m, "r.txt", "r\n", "ready work", None);
    // live, built, but the worktree has uncommitted work
    git(&m, &["checkout", "-q", "-b", "line/half-done", "main"], None);
    commit(&m, "h.txt", "h\n", "half", None);
    // live and conflicting with main
    git(&m, &["checkout", "-q", "-b", "line/clash", "main"], None);
    commit(&m, "a.txt", "two\n", "clash", None);
    // landed through a batch with its own resolution: the plan index says merged, git cannot show it
    git(&m, &["checkout", "-q", "-b", "line/landed", "main"], None);
    commit(&m, "l.txt", "lane version\n", "landed", Some(NOW - 6 * 86400));
    // live, no commit for ten days
    git(&m, &["checkout", "-q", "-b", "line/sleepy", "main"], None);
    commit(&m, "s.txt", "s\n", "sleepy", Some(NOW - 10 * 86400));
    git(&m, &["checkout", "-q", "main"], None);
    commit(&m, "a.txt", "one changed on main\n", "main edits a", Some(NOW - 3 * 86400));
    commit(&m, "l.txt", "batch version\n", "the batch carried landed, resolved", Some(NOW - 3 * 86400));
    git(&m, &["worktree", "add", "-q", root.join("wt-live-ready").to_str().unwrap(), "line/live-ready"], None);
    git(&m, &["worktree", "add", "-q", root.join("wt-half-done").to_str().unwrap(), "line/half-done"], None);
    write(&root.join("wt-half-done/h.txt"), "h, edited\n");
    // the ledger and the plan index, as files in the main checkout (not committed: watch reads the files)
    write(
        &m.join("plans/lanes.csv"),
        "\"id\",\"batch\",\"branch\",\"worktree\",\"status\",\"report\"\n\
         \"live-ready\",\"B\",\"line/live-ready\",\"../wt-live-ready\",\"built\",\"\"\n\
         \"half-done\",\"B\",\"line/half-done\",\"../wt-half-done\",\"built\",\"a note\nover two lines, with a \"\"quote\"\"\"\n\
         \"ghost\",\"B\",\"line/ghost\",\"\",\"dispatched\",\"\"\n\
         \"gone\",\"A\",\"line/gone\",\"\",\"merged\",\"\"\n\
         \"in-batch\",\"A\",\"line/in-batch\",\"\",\"stacked\",\"\"\n",
    );
    write(
        &m.join("plans/INDEX.csv"),
        "tracker_id,kind,status,plan_file,last_change\n\
         ALPHA,written,open,2026-11-01-alpha.md,2026-11-01\n\
         BETA,open,open,2026-11-03-beta.md,2026-11-03\n\
         LIVE-READY,written,open,2026-11-02-live-ready.md,2026-11-02\n\
         PARKED,written,parked,2026-11-04-parked.md,2026-11-04\n\
         DONE,written,merged,2026-11-05-done.md,2026-11-05\n\
         LANDED,written,merged,2026-11-06-landed.md,2026-11-06\n",
    );
    let conf = root.join("watch.toml");
    write(
        &conf,
        &format!(
            "[hosts_are_ignored]\nx = 1\n\n[watch]\nrepo = {:?}\nlanes_csv = \"plans/lanes.csv\"\nplans_index = \"plans/INDEX.csv\"\nstale_days = 3\nidle_hours = 24\n",
            m.to_string_lossy().replace('\\', "/")
        ),
    );
    Fixture { _d: d, root, conf }
}

fn watch(conf: &Path) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(["watch", "--config", conf.to_str().unwrap(), "--now", &NOW.to_string()]).output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).replace("\r\n", "\n"), String::from_utf8_lossy(&o.stderr).into_owned())
}

fn row<'a>(out: &'a str, branch: &str) -> &'a str {
    out.lines().find(|l| l.trim_start().starts_with(&format!("{branch} "))).unwrap_or_else(|| panic!("no row for {branch}:\n{out}"))
}

#[test]
fn every_lane_is_judged_and_the_next_steps_follow() {
    let f = fixture();
    let (rc, out, err) = watch(&f.conf);
    assert_eq!(rc, 0, "{err}\n{out}");
    assert!(out.contains("lanes: 5 live (1 ready to stack, 2 blocked, 0 stacked, 1 stale, 0 idle, 1 merged by the ledger only), 2 merged awaiting tidy, 1 ledger row(s) without a branch"), "{out}");
    // the next steps, in order: stack, unblock, tidy, assign
    assert!(out.contains("  1. stack the ready lanes: line/live-ready\n"), "{out}");
    assert!(out.contains("rebase 1 lane(s) that conflict with main: line/clash\n"), "{out}");
    assert!(out.contains("check by hand 1 lane(s) the ledger calls merged (git cannot prove it), then drop them: line/landed\n"), "{out}");
    assert!(out.contains("start or drop 1 dispatched lane(s) that have no branch: ghost\n"), "{out}");
    assert!(!out.contains("line/in-batch"), "a stacked row is carried by its batch: {out}");
    assert!(out.contains("unblock line/half-done: ledger says built but the worktree has uncommitted work"), "{out}");
    assert!(out.contains("tidy: 2 merged lane(s) (1 with a worktree) - `gatetrain tidy` (dry run) shows the plan; 1 merged by patch only, which tidy keeps (check by hand): line/picked\n"), "{out}");
    // the written plan before the open one; the plan a lane carries, a parked and a merged plan are not suggested
    let a = out.find("assign ALPHA (2026-11-01-alpha.md").expect(&out);
    let b = out.find("assign BETA (2026-11-03-beta.md").expect(&out);
    assert!(a < b, "{out}");
    for no in ["assign LIVE-READY", "assign PARKED", "assign DONE"] {
        assert!(!out.contains(no), "{no}:\n{out}");
    }
    assert!(out.contains("review 1 stale lane(s)"), "{out}");
    // the tables
    let r = row(&out, "line/live-ready");
    assert!(r.contains("wt-live-ready") && r.contains(" no ") && r.contains(" built ") && r.contains(" ready "), "{r}");
    assert!(row(&out, "line/half-done").contains(" yes "), "dirty: {out}");
    assert!(row(&out, "line/sleepy").contains(" 10d ") && row(&out, "line/sleepy").contains(" stale "), "{out}");
    assert!(row(&out, "line/old-done").contains("ancestor") && row(&out, "line/old-done").contains("wt-old-done"), "{out}");
    assert!(row(&out, "line/picked").contains("by patch"), "{out}");
    assert!(row(&out, "ghost").contains("line/ghost") && row(&out, "ghost").contains("dispatched"), "{out}");
    assert!(!out.contains("line/gone"), "a merged ledger row with no branch is not a problem: {out}");
}

#[test]
fn watch_writes_nothing_to_the_repository_or_its_worktrees() {
    let f = fixture();
    // the merge results already exist in the repository (an earlier `merge-tree --write-tree`, as tidy's merge proof does): a
    // command that writes the same object again "freshens" it (a new mtime), even through an alternate object directory
    let m = f.root.join("main");
    for br in ["line/live-ready", "line/half-done", "line/sleepy", "line/clash", "line/landed"] {
        let _ = Command::new("git").args(["merge-tree", "--write-tree", "main", br]).current_dir(&m).output().unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let before = snapshot(&f.root);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (rc, out, err) = watch(&f.conf);
    assert_eq!(rc, 0, "{err}\n{out}");
    let after = snapshot(&f.root);
    let changed: Vec<_> = after.iter().filter(|(p, v)| before.get(*p) != Some(v)).map(|(p, _)| p.clone()).collect();
    let gone: Vec<_> = before.keys().filter(|p| !after.contains_key(*p)).cloned().collect();
    assert!(changed.is_empty() && gone.is_empty(), "watch wrote: {changed:?}, removed: {gone:?}");
}

#[test]
fn a_config_without_a_watch_table_or_with_an_unknown_key_is_an_error() {
    let d = tempfile::tempdir().unwrap();
    let c = d.path().join("c.toml");
    write(&c, "[tidy]\nmain_checkout = \"/x\"\n");
    let (rc, _, err) = watch(&c);
    assert_eq!(rc, 1);
    assert!(err.contains("no [watch] table"), "{err}");
    write(&c, "[watch]\nrepo = \"/x\"\nstale_dayz = 3\n");
    let (rc, _, err) = watch(&c);
    assert_eq!(rc, 1);
    assert!(err.contains("unknown key(s) [\"stale_dayz\"]"), "{err}");
}
