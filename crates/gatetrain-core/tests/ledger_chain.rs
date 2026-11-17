//! The ledger schema is ONE forward-only chain: v1 (run snapshots, decisions) -> v2 (step_memory) -> v3 (batch_events)
//! -> v4 (engine v2 E1: run and step columns, step_skips, stamps, stamp_links) -> v5 (E2: step_shards)
//! -> v6 (E6: host_probes, disk_probes) -> v7 (E7: host_samples columns, disk_samples) -> v8 (E8: advice, advice_checks).
//! A file at any older version reaches v8 with its rows intact; a newer one is refused.
use gatetrain_core::ledger::{Ledger, CURRENT_VERSION};

const TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('step_memory','batch_events','run_snapshots','decisions')";
const V4_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('step_skips','stamps','stamp_links')";
const V5_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='step_shards'";
const V6_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('host_probes','disk_probes')";
const V7_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='disk_samples'";
const V8_TABLES: &str = "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('advice','advice_checks')";

/// A version-0 ledger as the Python side writes it: only the base tables.
fn v0(path: &std::path::Path) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(&std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ledger/schema.sql")).unwrap()).unwrap();
    c.execute_batch("INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','host-a','abc','2026-11-11T09:00:00+07:00','2026-11-11T09:01:00+07:00','green')").unwrap();
    c.execute_batch("INSERT INTO steps(run_id,name,exit_code,duration_sec) VALUES('r1','s',0,2.5)").unwrap();
    c.execute_batch("INSERT INTO host_samples(host,at,free_gb,load_1m) VALUES('host-a','2026-11-11T09:00:30+07:00',50.0,1.5)").unwrap();
}

fn count(l: &Ledger, sql: &str) -> i64 {
    l.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn columns(l: &Ledger, table: &str) -> Vec<String> {
    let mut st = l.conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
    st.query_map([], |r| r.get::<_, String>(1)).unwrap().map(Result::unwrap).collect()
}

fn all_new(l: &Ledger) -> [i64; 6] {
    [count(l, TABLES), count(l, V4_TABLES), count(l, V5_TABLES), count(l, V6_TABLES), count(l, V7_TABLES), count(l, V8_TABLES)]
}

#[test]
fn the_current_version_is_eight() {
    assert_eq!(CURRENT_VERSION, 8);
}

#[test]
fn from_v0_reaches_v8_and_keeps_rows() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    v0(&p);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(l.version().unwrap(), 0);
    assert_eq!(l.migrate().unwrap(), (0, 8));
    assert_eq!(l.version().unwrap(), 8);
    assert_eq!(all_new(&l), [4, 3, 1, 2, 1, 2]);
    assert_eq!(l.runs().unwrap().len(), 1);
    assert_eq!(l.step_count("r1").unwrap(), 1);
    assert_eq!(count(&l, "SELECT COUNT(*) FROM host_samples WHERE host='host-a' AND load_1m=1.5 AND cpu_busy_pct IS NULL"), 1, "the old sample row is unchanged, the new columns are unknown");
    assert_eq!(l.migrate().unwrap(), (8, 8), "idempotent");
}

/// Take a current ledger back to `v` the way an older binary left it: later tables and columns absent, user_version set.
fn at_version(path: &std::path::Path, v: i64) {
    let mut l = Ledger::open(path).unwrap();
    l.migrate().unwrap();
    l.conn.execute("INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome) VALUES('r1','chain','lane-a','host-a','abc','t','t','green')", []).unwrap();
    l.conn.execute("INSERT INTO steps(run_id,name,exit_code,duration_sec) VALUES('r1','s',0,1.0)", []).unwrap();
    l.conn.execute("INSERT INTO host_samples(host,at,free_gb) VALUES('h','t',9.0)", []).unwrap();
    if v < 8 {
        l.conn.execute_batch("DROP TABLE advice; DROP TABLE advice_checks;").unwrap();
    }
    if v < 7 {
        l.conn.execute_batch("DROP TRIGGER host_samples_no_update; DROP TRIGGER host_samples_no_delete; DROP TABLE disk_samples;").unwrap();
        for c in ["run_id", "running_steps", "cpu_busy_pct", "iowait_pct", "mem_free_mb", "stall_io_some", "stall_io_full", "stall_cpu_some", "stall_mem_some"] {
            l.conn.execute_batch(&format!("ALTER TABLE host_samples DROP COLUMN {c}")).unwrap();
        }
    }
    if v < 6 {
        l.conn.execute_batch("DROP TABLE host_probes; DROP TABLE disk_probes;").unwrap();
    }
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
fn from_every_version_to_eight_rows_unchanged() {
    for v in 1..=7 {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("l.db");
        at_version(&p, v);
        let mut l = Ledger::open(&p).unwrap();
        assert_eq!(l.version().unwrap(), v);
        assert!(l.record_step_peak("r1", "s", 5.0, "reported", "t").is_err(), "v{v}: writes are refused until migrated");
        assert_eq!(l.migrate().unwrap(), (v, 8), "from v{v}");
        assert_eq!(all_new(&l), [4, 3, 1, 2, 1, 2], "from v{v}");
        assert_eq!(l.runs().unwrap().len(), 1, "from v{v}");
        assert_eq!(count(&l, "SELECT COUNT(*) FROM steps WHERE run_id='r1' AND duration_sec=1.0"), 1, "from v{v}: the step row is unchanged");
        assert_eq!(count(&l, "SELECT COUNT(*) FROM host_samples WHERE host='h' AND free_gb=9.0"), 1, "from v{v}: the sample row is unchanged");
        assert!(columns(&l, "runs").contains(&"plan_digest".to_string()) && columns(&l, "steps").contains(&"shards".to_string()), "from v{v}");
        assert!(columns(&l, "host_samples").contains(&"stall_io_some".to_string()), "from v{v}");
        assert_eq!(count(&l, "SELECT COUNT(*) FROM runs WHERE mode IS NULL AND par IS NULL AND log_sha256 IS NULL AND host_class IS NULL"), 1);
        l.record_step_peak("r1", "s", 5.0, "reported", "t").unwrap();
    }
}

#[test]
fn from_v5_gets_only_the_hardware_additions() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    at_version(&p, 5);
    let mut l = Ledger::open(&p).unwrap();
    assert_eq!(count(&l, V6_TABLES) + count(&l, V7_TABLES) + count(&l, V8_TABLES), 0);
    assert_eq!(l.migrate().unwrap(), (5, 8));
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
        ("host_probes", "INSERT INTO host_probes(host,at) VALUES('h','t')", "UPDATE host_probes SET os='x'", "DELETE FROM host_probes"),
        ("disk_probes", "INSERT INTO disk_probes(host,at,path) VALUES('h','t','/')", "UPDATE disk_probes SET device='x'", "DELETE FROM disk_probes"),
        ("host_samples", "INSERT INTO host_samples(host,at,cpu_busy_pct) VALUES('h','t',1.0)", "UPDATE host_samples SET cpu_busy_pct=2", "DELETE FROM host_samples"),
        ("disk_samples", "INSERT INTO disk_samples(host,at,device) VALUES('h','t','sda')", "UPDATE disk_samples SET busy_pct=1", "DELETE FROM disk_samples"),
        ("advice", "INSERT INTO advice(advice_id,at,rule,confidence) VALUES('a','t','r','low')", "UPDATE advice SET rule='x'", "DELETE FROM advice"),
        ("advice_checks", "INSERT INTO advice_checks(advice_id,run_id,at,verdict) VALUES('a','r','t','v')", "UPDATE advice_checks SET verdict='x'", "DELETE FROM advice_checks"),
    ] {
        l.conn.execute(insert, []).unwrap();
        assert!(l.conn.execute(update, []).unwrap_err().to_string().contains("append-only"), "{table} update");
        assert!(l.conn.execute(delete, []).unwrap_err().to_string().contains("append-only"), "{table} delete");
        assert!(l.conn.execute(insert, []).is_err(), "{table}: the primary key refuses a repeat");
    }
}

#[test]
fn probes_and_samples_are_stored_and_ingest_twice_writes_once() {
    use gatetrain_core::ledger::{DiskProbeRow, HostProbeRow};
    let mut l = Ledger::open_in_memory().unwrap();
    l.migrate().unwrap();
    l.record_host_probe(&HostProbeRow { host: "h".into(), at: "t1".into(), cores: Some(8), cpu_one_mbps: Some(500.0), unknown: Some("{\"mem_total_mb\":\"x\"}".into()), ..Default::default() }).unwrap();
    l.record_disk_probe(&DiskProbeRow { host: "h".into(), at: "t1".into(), path: "/d".into(), rotational: None, rotational_source: Some("unknown: wsl".into()), fsync_p50_ms: Some(1.5), ..Default::default() }).unwrap();
    assert_eq!(count(&l, "SELECT COUNT(*) FROM disk_probes WHERE rotational IS NULL AND fsync_p50_ms=1.5"), 1, "unknown is stored as NULL");
    let file = "{\"unknown\":{\"iowait_pct\":\"x\"}}\n{\"at\":\"t1\",\"host\":\"h\",\"run_id\":\"r\",\"running_steps\":2,\"load_1m\":1.0,\"cpu_busy_pct\":55.5,\"iowait_pct\":null,\"disks\":[{\"device\":\"sda\",\"busy_pct\":80.0}]}\nnot json\n";
    assert_eq!(l.record_samples(file).unwrap(), 1);
    assert_eq!(l.record_samples(file).unwrap(), 0, "the same file twice writes nothing more");
    assert_eq!(count(&l, "SELECT COUNT(*) FROM host_samples WHERE cpu_busy_pct=55.5 AND iowait_pct IS NULL AND running_steps=2"), 1);
    assert_eq!(count(&l, "SELECT COUNT(*) FROM disk_samples WHERE device='sda' AND busy_pct=80.0"), 1);
}

#[test]
fn a_newer_version_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("l.db");
    rusqlite::Connection::open(&p).unwrap().pragma_update(None, "user_version", 9).unwrap();
    assert!(matches!(Ledger::open(&p), Err(gatetrain_core::Error::SchemaTooNew { found: 9, known: 8 })));
}
