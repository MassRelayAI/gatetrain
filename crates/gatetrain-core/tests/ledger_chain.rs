//! The ledger schema is ONE forward-only chain: v1 (run snapshots, decisions) -> v2 (step_memory) -> v3 (batch_events)
//! -> v4 (engine v2 E1: run and step columns, step_skips, stamps, stamp_links) -> v5 (E2: step_shards).
//! A file at any older version reaches v5 with its rows intact; a newer one is refused.
use gatetrain_core::ledger::{Ledger, CURRENT_VERSION};

const TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('step_memory','batch_events','run_snapshots','decisions')";
const V4_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('step_skips','stamps','stamp_links')";
const V5_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='step_shards'";

/// A version-0 ledger as the Python side writes it: only the base tables.
fn v0(path: &std::path::Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(&std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ledger/schema.sql")).unwrap()).unwrap();
    c.execute_batch("INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','host-a','abc','2026-11-11T09:00:00+07:00','2026-11-11T09:01:00+07:00','green')").unwrap();
    c.execute_batch("INSERT INTO steps(run_id,name,exit_code,duration_sec) VALUES('r1','s',0,2.5)").unwrap();
}

fn count(l: &Ledger, sql: &str) -> i64 {
    l.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn columns(l: &Ledger, table: &str) -> Vec<String> {
    let mut st = l.conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
    st.query_map([], |r| r.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
}

#[test]
fn the_current_version_is_five() {
    assert_eq!(CURRENT_VERSION, 5);
}

#[test]
fn from_v0_reaches_v5_and_keeps_rows() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    v0(&p);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(l.version().unwrap(), 0);
    assert_eq!(l.migrate().unwrap(), (0, 5));
    assert_eq!(l.version().unwrap(), 5);
    assert_eq!(count(&l, TABLES), 4);
    assert_eq!(count(&l, V4_TABLES), 3);
    assert_eq!(count(&l, V5_TABLES), 1);
    assert_eq!(l.runs().unwrap().len(), 1);
    assert_eq!(l.step_count("r1").unwrap(), 1);
    assert_eq!(l.migrate().unwrap(), (5, 5), "idempotent");
}

/// Take a current ledger back to `v` the way an older binary left it: later tables and columns absent, user_version set.
fn at_version(path: &std::path::Path, v: i64) {
    let mut l = Ledger::open(path).unwrap();
    l.migrate().unwrap();
    l.conn.execute("INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','host-a','abc','t','t','green')", []).unwrap();
    l.conn.execute("INSERT INTO steps(run_id,name,exit_code,duration_sec) VALUES('r1','s',0,1.0)", []).unwrap();
    if v < 5 {
        l.conn.execute_batch("DROP TABLE step_shards").unwrap();
    }
    if v < 4 {
        l.conn.execute_batch("DROP TABLE step_skips; DROP TABLE stamps; DROP TABLE stamp_links;").unwrap();
        for c in ["mode", "par", "plan_digest", "log_sha256", "config_hash", "host_class", "gatetrain_version"] {
            l.conn.execute_batch(&format!("ALTER TABLE runs DROP COLUMN {c}")).unwrap();
        }
        for c in ["tier", "input_key", "cached_from", "shards"] {
            l.conn.execute_batch(&format!("ALTER TABLE steps DROP COLUMN {c}")).unwrap();
        }
    }
    if v < 3 {
        l.conn.execute_batch("DROP TABLE batch_events").unwrap();
    }
    if v < 2 {
        l.conn.execute_batch("DROP TABLE step_memory").unwrap();
    }
    l.conn.pragma_update(None, "user_version", v).unwrap();
}

#[test]
fn from_every_version_to_five_rows_unchanged() {
    for v in 1..=4 {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("l.db");
        at_version(&p, v);
        let mut l = Ledger::open(&p).unwrap();
        assert_eq!(l.version().unwrap(), v);
        assert!(l.record_step_peak("r1", "s", 5.0, "reported", "t").is_err(), "v{v}: writes are refused until migrated");
        assert_eq!(l.migrate().unwrap(), (v, 5), "from v{v}");
        assert_eq!((count(&l, TABLES), count(&l, V4_TABLES), count(&l, V5_TABLES)), (4, 3, 1), "from v{v}");
        assert_eq!(l.runs().unwrap().len(), 1, "from v{v}");
        assert_eq!(count(&l, "SELECT COUNT(*) FROM steps WHERE run_id='r1' AND duration_sec=1.0"), 1, "from v{v}: the step row is unchanged");
        assert!(columns(&l, "runs").contains(&"plan_digest".to_string()) && columns(&l, "steps").contains(&"shards".to_string()), "from v{v}");
        // the old row reads NULL (unknown) in every new column
        assert_eq!(count(&l, "SELECT COUNT(*) FROM runs WHERE mode IS NULL AND par IS NULL AND log_sha256 IS NULL AND host_class IS NULL"), 1);
        l.record_step_peak("r1", "s", 5.0, "reported", "t").unwrap();
    }
}

#[test]
fn from_v3_gets_only_the_engine_v2_additions_and_an_old_ledger_keeps_working() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    at_version(&p, 3);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(count(&l, V4_TABLES), 0);
    assert_eq!(l.migrate().unwrap(), (3, 5));
    assert_eq!(l.runs().unwrap().len(), 1);
}

#[test]
fn the_new_tables_are_insert_only() {
    let d = tempfile::tempdir().unwrap();
    let mut l = Ledger::open(&d.path().join("l.db")).unwrap();
    l.migrate().unwrap();
    for (table, insert, update, delete) in [
        ("step_skips", "INSERT INTO step_skips(run_id,step,reason) VALUES('r','s','why')", "UPDATE step_skips SET reason='x'", "DELETE FROM step_skips"),
        ("stamps", "INSERT INTO stamps(digest,kind,sha,host,at,body) VALUES('d','full','s','h','t','{}')", "UPDATE stamps SET kind='x'", "DELETE FROM stamps"),
        ("stamp_links", "INSERT INTO stamp_links(digest,prev_digest) VALUES('d','p')", "UPDATE stamp_links SET prev_digest='x'", "DELETE FROM stamp_links"),
        ("step_shards", "INSERT INTO step_shards(run_id,step,shard,of) VALUES('r','s',1,2)", "UPDATE step_shards SET exit_code=1", "DELETE FROM step_shards"),
    ] {
        l.conn.execute(insert, []).unwrap();
        assert!(l.conn.execute(update, []).unwrap_err().to_string().contains("append-only"), "{table} update");
        assert!(l.conn.execute(delete, []).unwrap_err().to_string().contains("append-only"), "{table} delete");
        assert!(l.conn.execute(insert, []).is_err(), "{table}: the primary key refuses a repeat");
    }
}

#[test]
fn a_newer_version_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    rusqlite::Connection::open(&p).unwrap().pragma_update(None, "user_version", 6).unwrap();
    assert!(matches!(Ledger::open(&p), Err(gatetrain_core::Error::SchemaTooNew { found: 6, known: 5 })));
}
