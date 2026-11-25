//! The forced commands as the binary runs them: help, argument errors, and the verdict record.
use std::process::{Command, Output};

fn gt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(args).output().unwrap()
}
fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn every_new_command_has_help_with_its_options() {
    for (cmd, words) in [
        ("measure", vec!["--json", "--host"]),
        ("land", vec!["--plan", "--main", "--remote", "--db"]),
        ("verdict", vec!["--by", "--pass", "--fail"]),
        ("main-watch", vec!["--once", "--deploy-cmd", "--interval-s"]),
        ("branches", vec!["--json", "--check", "--no-size"]),
    ] {
        let o = gt(&[cmd, "--help"]);
        assert!(o.status.success(), "{cmd}: {}", text(&o));
        for w in words {
            assert!(text(&o).contains(w), "{cmd} --help lacks {w}:\n{}", text(&o));
        }
    }
    let o = gt(&["tidy", "--help"]);
    assert!(text(&o).contains("--force") && text(&o).contains("--keep"), "{}", text(&o));
}

#[test]
fn keep_without_force_is_refused() {
    let o = gt(&["tidy", "--keep", "x", "--config", "no-such.toml"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("--keep needs --force"));
}

#[test]
fn a_verdict_needs_pass_or_fail_and_is_recorded() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("l.db");
    let db = db.to_str().unwrap();
    let o = gt(&["verdict", "abc123", "--by", "bob", "--db", db]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    let o = gt(&["verdict", "abc123", "--by", "bob", "--pass", "--note", "read it", "--db", db]);
    assert!(o.status.success(), "{}", text(&o));
    let l = gatetrain_core::ledger::Ledger::open(std::path::Path::new(db)).unwrap();
    assert_eq!(gatetrain_core::force::land::verdict_for(&l, "abc123").as_deref(), Some("bob"));
}
