//! The ledger schema is ONE forward-only chain: v1 (run snapshots, decisions) -> v2 (step_memory) -> v3 (batch_events).
//! A file at any older version reaches v3 with its rows intact; a newer one is refused.
use gatetrain_core::ledger::{Ledger, CURRENT_VERSION};

const V3_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('step_memory','batch_events','run_snapshots','decisions')";

/// A version-0 ledger as the Python side writes it: only the base tables.
fn v0(path: &std::path::Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(&std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ledger/schema.sql")).unwrap()).unwrap();
    c.execute_batch("INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','host-a','abc','2026-11-11T09:00:00+07:00','2026-11-11T09:01:00+07:00','green')").unwrap();
}

fn tables(l: &Ledger) -> i64 {
    l.conn.query_row(V3_TABLES, [], |r| r.get(0)).unwrap()
}

#[test]
fn the_current_version_is_three() {
    assert_eq!(CURRENT_VERSION, 3);
}

#[test]
fn from_v0_reaches_v3_and_keeps_rows() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    v0(&p);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(l.version().unwrap(), 0);
    assert_eq!(l.migrate().unwrap(), (0, 3));
    assert_eq!(l.version().unwrap(), 3);
    assert_eq!(tables(&l), 4);
    assert_eq!(l.runs().unwrap().len(), 1);
    assert_eq!(l.migrate().unwrap(), (3, 3), "idempotent");
}

/// Take a current ledger back to `v` the way an older binary left it: later tables absent, user_version set.
fn at_version(path: &std::path::Path, v: i64) {
    let mut l = Ledger::open(path).unwrap();
    l.migrate().unwrap();
    l.conn.execute("INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','host-a','abc','t','t','green')", []).unwrap();
    if v < 3 {
        l.conn.execute_batch("DROP TABLE batch_events").unwrap();
    }
    if v < 2 {
        l.conn.execute_batch("DROP TABLE step_memory").unwrap();
    }
    l.conn.pragma_update(None, "user_version", v).unwrap();
}

#[test]
fn from_v1_gets_step_memory_then_batch_events() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    at_version(&p, 1);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(tables(&l), 2);
    assert_eq!(l.migrate().unwrap(), (1, 3));
    assert_eq!(tables(&l), 4);
    assert_eq!(l.runs().unwrap().len(), 1);
}

#[test]
fn from_v2_gets_only_batch_events() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    at_version(&p, 2);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(tables(&l), 3);
    l.record_step_peak("r1", "s", 5.0, "reported", "t").unwrap_err(); // writes refused until migrated
    assert_eq!(l.migrate().unwrap(), (2, 3));
    assert_eq!(tables(&l), 4);
    l.record_step_peak("r1", "s", 5.0, "reported", "t").unwrap();
    assert_eq!(l.runs().unwrap().len(), 1);
}

#[test]
fn a_newer_version_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    rusqlite::Connection::open(&p).unwrap().pragma_update(None, "user_version", 4).unwrap();
    assert!(matches!(Ledger::open(&p), Err(gatetrain_core::Error::SchemaTooNew { found: 4, known: 3 })));
}
