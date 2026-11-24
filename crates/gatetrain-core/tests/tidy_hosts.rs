//! GT-TIDY-HOSTS: the naming report (flag only) and the protect check for a live lane and uncommitted work, on real temp git
//! repositories. The junction proof lives in tidy_real.rs
//! (`a_junction_inside_a_removed_worktree_is_unlinked_and_never_followed_into_its_target`).
use gatetrain_core::config::{Config, Host};
use gatetrain_core::tidy::config::build_cfg;
use gatetrain_core::tidy::inventory::gather;
use gatetrain_core::tidy::naming::{report_block, scan, Finding};
use gatetrain_core::tidy::LocalRunner;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"]).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn fwd(p: &Path) -> String {
    let s = p.canonicalize().unwrap().to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").map(String::from).unwrap_or(s)
}

fn clone_with_remote(root: &Path, folder: &str, url: &str) -> std::path::PathBuf {
    let m = root.join(folder);
    std::fs::create_dir_all(&m).unwrap();
    git(&m, &["init", "-q", "-b", "main"]);
    std::fs::write(m.join("f.txt"), "one\n").unwrap();
    git(&m, &["add", "."]);
    git(&m, &["commit", "-q", "-m", "base"]);
    git(&m, &["remote", "add", "origin", url]);
    m
}

fn names(f: &[Finding]) -> Vec<String> {
    let mut v: Vec<String> = f.iter().map(|x| Path::new(&x.path).file_name().unwrap().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn tidy_with_naming(extra: &[(&str, toml::Value)], roots: &[String], exempt: &[&str]) -> (Config, Host) {
    let mut tidy = toml::Table::new();
    let mut nm = toml::Table::new();
    nm.insert("roots".into(), toml::Value::Array(roots.iter().map(|r| toml::Value::String(r.clone())).collect()));
    nm.insert("exempt".into(), toml::Value::Array(exempt.iter().map(|r| toml::Value::String(r.to_string())).collect()));
    tidy.insert("naming".into(), toml::Value::Table(nm));
    for (k, v) in extra {
        tidy.insert(k.to_string(), v.clone());
    }
    let h = Host { name: "h".into(), os: "linux".into(), ..Default::default() }; // the naming report needs no link canary
    (Config { hosts: vec![h.clone()], tidy, ..Default::default() }, h)
}

#[test]
fn naming_flags_misnamed_worktrees_bare_prefixes_and_clone_folder_mismatch() {
    let d = tempfile::Builder::new().prefix("gt-name-").tempdir().unwrap();
    let root = d.path();
    let app = clone_with_remote(root, "app", "https://example.test/org/app.git");
    git(&app, &["worktree", "add", "-q", "-b", "l1", &root.join("app-wt-ok").to_string_lossy()]);
    git(&app, &["worktree", "add", "-q", "-b", "l2", &root.join("app-extra").to_string_lossy()]); // (a)
    git(&app, &["worktree", "add", "-q", "-b", "l3", &root.join("kk-fix-x").to_string_lossy()]); // (a) and (b)
    clone_with_remote(root, "oldname", "https://example.test/org/newname.git"); // (c)
    clone_with_remote(root, "standing", "https://example.test/org/other.git"); // (c) but exempt
    std::fs::create_dir_all(root.join("tmp_scratch")).unwrap(); // (b), not git
    std::fs::create_dir_all(root.join("tmp-scratch2")).unwrap();
    std::fs::create_dir_all(root.join("wt-bare")).unwrap();
    std::fs::create_dir_all(root.join("plain-folder")).unwrap(); // not git, no bad prefix: out of scope
    let f = scan(&[fwd(root)], &["standing".to_string()]);
    assert_eq!(names(&f), vec!["app-extra", "kk-fix-x", "oldname", "tmp-scratch2", "tmp_scratch", "wt-bare"], "{f:?}");
    let kk = f.iter().find(|x| x.path.ends_with("kk-fix-x")).unwrap();
    assert_eq!(kk.reasons.len(), 2, "{kk:?}");
    // the remote is origin, else the first remote
    let m = clone_with_remote(root, "first-remote", "https://example.test/org/first-remote.git");
    git(&m, &["remote", "rename", "origin", "up"]);
    assert!(!names(&scan(&[fwd(root)], &["standing".to_string()])).contains(&"first-remote".to_string()));
}

#[test]
fn naming_report_block_comes_from_the_config_and_flags_only() {
    let d = tempfile::Builder::new().prefix("gt-name-").tempdir().unwrap();
    let root = d.path();
    std::fs::create_dir_all(root.join("wt-bare")).unwrap();
    let (cfg, h) = tidy_with_naming(&[], &[fwd(root)], &[]);
    let t = build_cfg(&cfg, &h).unwrap();
    assert_eq!(t.naming_roots, vec![fwd(root)]);
    let block = report_block(&t);
    assert!(block.contains("wt-bare") && block.to_lowercase().contains("naming"), "{block}");
    assert!(root.join("wt-bare").exists(), "flag only: nothing renamed");
    let (cfg2, h2) = tidy_with_naming(&[], &[fwd(root)], &["wt-bare"]);
    let block2 = report_block(&build_cfg(&cfg2, &h2).unwrap());
    assert!(!block2.contains("wt-bare"), "{block2}");
}

#[test]
fn naming_config_rejects_unknown_keys() {
    let h = Host { name: "h".into(), ..Default::default() };
    let mut tidy = toml::Table::new();
    let mut nm = toml::Table::new();
    nm.insert("rots".into(), toml::Value::Array(vec![]));
    tidy.insert("naming".into(), toml::Value::Table(nm));
    let cfg = Config { hosts: vec![h.clone()], tidy, ..Default::default() };
    assert!(build_cfg(&cfg, &h).is_err());
}

#[test]
fn a_live_lane_and_uncommitted_work_are_kept_with_their_reason_on_a_real_fixture() {
    let d = tempfile::Builder::new().prefix("gt-keep-").tempdir().unwrap();
    let root = d.path();
    let main = clone_with_remote(root, "main", "https://example.test/org/main.git");
    let wt = |n: &str| root.join(n).to_string_lossy().into_owned();
    git(&main, &["worktree", "add", "-q", "-b", "live", &wt("main-wt-live")]);
    std::fs::write(root.join("main-wt-live/new.txt"), "unmerged\n").unwrap();
    git(&root.join("main-wt-live"), &["add", "."]);
    git(&root.join("main-wt-live"), &["commit", "-q", "-m", "unmerged work"]);
    git(&main, &["worktree", "add", "-q", "-b", "dirty", &wt("main-wt-dirty")]);
    std::fs::write(root.join("main-wt-dirty/f.txt"), "edited\n").unwrap();
    git(&main, &["worktree", "add", "-q", "-b", "done", &wt("main-wt-done")]);
    let mut tidy = toml::Table::new();
    tidy.insert("main_checkout".into(), toml::Value::String(fwd(&main)));
    tidy.insert("worktree_prefix".into(), toml::Value::String("main-wt-".into()));
    tidy.insert("worktree_min_idle_days".into(), toml::Value::Integer(0));
    tidy.insert("log".into(), toml::Value::String(format!("{}/log.jsonl", fwd(root))));
    if cfg!(windows) {
        let c = root.join("canary");
        let tg = root.join("canary-target");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&tg).unwrap();
        let o = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(c.join("l").to_string_lossy().replace('/', "\\")).arg(tg.to_string_lossy().replace('/', "\\")).output().unwrap();
        assert!(o.status.success());
        tidy.insert("link_canary".into(), toml::Value::String(fwd(&c)));
        tidy.insert("link_canary_count".into(), toml::Value::Integer(1));
    }
    let h = Host { name: "h".into(), tidy, ..Default::default() };
    let cfg = Config { hosts: vec![h.clone()], ..Default::default() };
    let t = build_cfg(&cfg, &h).unwrap();
    let inv = gather(&h, &LocalRunner, &t, None).unwrap();
    let keep = |suffix: &str| inv.worktrees.iter().find(|w| w.path.ends_with(suffix)).unwrap_or_else(|| panic!("{suffix} listed")).keep.clone();
    assert_eq!(keep("main-wt-live").as_deref(), Some("not merged"));
    assert_eq!(keep("main-wt-dirty").as_deref(), Some("uncommitted work"));
    assert_eq!(keep("main-wt-done"), None, "the merged clean one is the only plan");
    assert_eq!(inv.items.iter().filter(|i| i.cls == "worktrees").count(), 1);
}
