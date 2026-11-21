//! Engine v2 E6 and E8 through the binary: `probe` (bounded, refused during a proof, stored, read-only discover) and `advise` (prints,
//! stores, applies nothing). No real disk tuning: the probe is given tiny sizes.
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

fn gt(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_gatetrain"));
    c.current_dir(dir).env("GATETRAIN_BOOT_ID", "boot-A");
    c
}
fn child() -> String {
    Path::new(env!("CARGO_BIN_EXE_gatetrain")).with_file_name(format!("gt-testchild{}", std::env::consts::EXE_SUFFIX)).to_string_lossy().into_owned()
}
fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr)
}
fn q(db: &Path, sql: &str) -> String {
    let c = rusqlite::Connection::open(db).unwrap();
    c.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0))
        .map(|v| match v {
            rusqlite::types::Value::Null => "NULL".into(),
            rusqlite::types::Value::Integer(i) => i.to_string(),
            rusqlite::types::Value::Real(f) => f.to_string(),
            rusqlite::types::Value::Text(t) => t,
            rusqlite::types::Value::Blob(_) => "blob".into(),
        })
        .unwrap_or_else(|e| format!("ERR {e}"))
}
fn leftovers(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".gatetrain-probe-")).count()
}
fn tiny() -> [&'static str; 8] {
    ["--small-writes", "10", "--seq-mb", "1", "--files", "20", "--budget-s", "8"]
}

#[test]
fn probe_measures_stores_and_leaves_no_file() {
    let d = tempfile::tempdir().unwrap();
    let disk = tempfile::tempdir().unwrap();
    let db = d.path().join("l.db");
    let o = gt(d.path()).args(["probe", "--host", "px", "--path", disk.path().to_str().unwrap(), "--db", db.to_str().unwrap()]).args(tiny()).output().unwrap();
    assert!(o.status.success(), "{}", out(&o));
    let t = out(&o);
    assert!(t.contains("host px:") && t.contains("durable write p50") && t.contains("rotational "), "{t}");
    assert_eq!(q(&db, "select count(*) from host_probes where host='px' and cpu_one_mbps>0 and cores>=1"), "1");
    assert_eq!(q(&db, "select count(*) from disk_probes where host='px' and fsync_p50_ms>0 and bytes_written>0"), "1");
    // rotational is read from the OS, never inferred from the numbers: NULL means unknown, 0/1 a reading
    let rot = q(&db, "select rotational from disk_probes");
    assert!(["NULL", "0", "1"].contains(&rot.as_str()), "{rot}");
    assert_eq!(leftovers(disk.path()), 0);
}

#[test]
fn probe_is_refused_while_a_proof_holds_the_host_and_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let disk = tempfile::tempdir().unwrap();
    let (db, lock) = (d.path().join("l.db"), d.path().join("host.lock"));
    let mut holder: Child = gt(d.path()).args(["hold", "proof-1", "--lock", lock.to_str().unwrap(), "--db", db.to_str().unwrap(), "--", &child(), "sleep", "6"]).stdout(Stdio::piped()).spawn().unwrap();
    let mut r = BufReader::new(holder.stdout.take().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "HOLDS proof-1");
    let o = gt(d.path()).args(["probe", "--path", disk.path().to_str().unwrap(), "--db", db.to_str().unwrap(), "--lock", lock.to_str().unwrap()]).args(tiny()).output().unwrap();
    assert_eq!(o.status.code(), Some(75), "{}", out(&o));
    assert!(out(&o).contains("probe.host-busy"), "{}", out(&o));
    assert_eq!(q(&db, "select count(*) from host_probes"), "0");
    assert_eq!(leftovers(disk.path()), 0, "the probe wrote nothing");
    // --discover only reads, and is allowed
    let o = gt(d.path()).args(["probe", "--discover", "--lock", lock.to_str().unwrap(), "--db", db.to_str().unwrap()]).output().unwrap();
    assert!(o.status.success(), "{}", out(&o));
    assert_eq!(q(&db, "select count(*) from host_probes"), "0", "discover writes nothing");
    let _ = holder.kill();
    let _ = holder.wait();
}

#[test]
fn a_disk_under_its_floor_is_refused_and_a_config_names_the_disks() {
    let d = tempfile::tempdir().unwrap();
    let disk = tempfile::tempdir().unwrap();
    let cfg = d.path().join("hosts.toml");
    std::fs::write(&cfg, format!("[[hosts]]\nname = \"h\"\nkind = \"local\"\nroles = [\"chain\"]\nreach = \"local\"\n[[hosts.disks]]\npath = '{}'\nmin_free_gb = 100000000\nuse = [\"chain\"]\n", disk.path().display())).unwrap();
    let db = d.path().join("l.db");
    let o = gt(d.path()).args(["probe", "--config", cfg.to_str().unwrap(), "--host", "h", "--db", db.to_str().unwrap()]).args(tiny()).output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("probe.disk-low"), "{}", out(&o));
    assert_eq!(q(&db, "select count(*) from disk_probes"), "0");
    assert_eq!(leftovers(disk.path()), 0);
}

fn seed_ledger(db: &Path, host: &str) {
    let mut l = gatetrain_core::ledger::Ledger::open(db).unwrap();
    l.migrate().unwrap();
    for (i, rid) in ["a1", "a2", "a3"].iter().enumerate() {
        let day = 10 + i;
        l.conn
            .execute("INSERT INTO runs(id,kind,host,started_at,ended_at,outcome) VALUES(?,?,?,?,?,?)", rusqlite::params![rid, "chain", host, format!("2026-11-{day}T10:00:00+00:00"), format!("2026-11-{day}T10:16:40+00:00"), "green"])
            .unwrap();
        for (n, st, dur) in [("big", 0, 700), ("b", 0, 200), ("c", 700, 200)] {
            let s = 10 * 3600 + st;
            let start = format!("2026-11-{day}T{:02}:{:02}:{:02}+00:00", s / 3600, s % 3600 / 60, s % 60);
            l.conn.execute("INSERT INTO steps(run_id,name,started_at,duration_sec,exit_code,shards) VALUES(?,?,?,?,0,1)", rusqlite::params![rid, n, start, dur as f64]).unwrap();
        }
        for k in 0..6 {
            let secs = k * 100;
            l.conn
                .execute(
                    "INSERT INTO host_samples(host,at,run_id,running_steps,cpu_busy_pct,iowait_pct,mem_free_mb) VALUES(?,?,?,?,?,?,?)",
                    rusqlite::params![host, format!("2026-11-{day}T10:{:02}:{:02}+00:00", secs / 60, secs % 60), rid, 2, 35.0, 3.0, 20000.0],
                )
                .unwrap();
        }
    }
}

const HX: &str = "[[hosts]]\nname = \"hx\"\nkind = \"local\"\nroles = [\"chain\"]\nreach = \"local\"\nmax_parallel_steps = 8\n";

#[test]
fn advise_prints_evidence_stores_the_advice_and_changes_nothing_else() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("l.db");
    seed_ledger(&db, "hx");
    let cfg = d.path().join("hosts.toml");
    std::fs::write(&cfg, HX).unwrap();
    let before = std::fs::read(&cfg).unwrap();
    let args = ["advise", "--db", db.to_str().unwrap(), "--config", cfg.to_str().unwrap(), "--host", "hx"];
    let o = gt(d.path()).args(args).output().unwrap();
    assert!(o.status.success(), "{}", out(&o));
    let t = out(&o);
    assert!(t.contains("[advise.plan-shape]") && t.contains("evidence: cpu busy p50 35%") && t.contains("minutes:") && t.contains("confidence:"), "{t}");
    assert!(t.contains("[advise.shard-step] big"), "the longest step is 70% of the wall and the host has free slots: {t}");
    assert_eq!(std::fs::read(&cfg).unwrap(), before, "advise never edits a file");
    assert!(q(&db, "select count(*) from advice where host='hx'").parse::<i64>().unwrap() >= 2);
    // the same advice again is stored once
    let n = q(&db, "select count(*) from advice");
    gt(d.path()).args(args).output().unwrap();
    assert_eq!(q(&db, "select count(*) from advice"), n);
    // JSON
    let o = gt(d.path()).args(args).arg("--json").output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(v["advice"].as_array().unwrap().iter().any(|a| a["rule"] == "advise.shard-step" && a["minutes"]["high"].is_number()), "{v}");
    // the config changes and a faster run follows: it is compared with the prediction
    std::fs::write(&cfg, format!("{HX}# sharded\n")).unwrap();
    {
        let l = rusqlite::Connection::open(&db).unwrap();
        // the advice was recorded at the real time of the call above: the run after it starts now, not on a fixed date
        let start = gatetrain_core::logfmt::Ts::now_utc().plus_seconds(60.0);
        l.execute("INSERT INTO runs(id,kind,host,started_at,ended_at,outcome) VALUES('a4','chain','hx',?,?,'green')", [start.iso(), start.plus_seconds(600.0).iso()]).unwrap();
    }
    let o = gt(d.path()).args(args).output().unwrap();
    assert!(o.status.success(), "{}", out(&o));
    assert!(q(&db, "select count(*) from advice_checks where run_id='a4'").parse::<i64>().unwrap() >= 1, "a run after a config change is compared: {}", out(&o));
}

#[test]
fn advise_without_a_ledger_says_nothing_has_been_measured() {
    let d = tempfile::tempdir().unwrap();
    let o = gt(d.path()).args(["advise", "--db", d.path().join("missing.db").to_str().unwrap()]).output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(out(&o).contains("nothing has been measured"), "{}", out(&o));
}

#[test]
fn doctor_with_a_db_warns_when_a_fast_flag_contradicts_the_probe_and_without_one_is_unchanged() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let cfg = d.path().join("hosts.toml");
    std::fs::write(
        &cfg,
        format!("[[hosts]]\nname = \"h\"\nkind = \"local\"\nroles = [\"chain\"]\nreach = \"local\"\n[[hosts.disks]]\npath = '{}'\nmin_free_gb = 1\nfast = true\nuse = [\"chain\"]\n[[hosts.disks]]\npath = '{}'\nmin_free_gb = 1\nuse = [\"cache\"]\n", a.path().display(), b.path().display()),
    )
    .unwrap();
    let db = d.path().join("l.db");
    let mut l = gatetrain_core::ledger::Ledger::open(&db).unwrap();
    l.migrate().unwrap();
    for (p, ms) in [(a.path(), 60.0), (b.path(), 2.0)] {
        l.conn.execute("INSERT INTO disk_probes(host,at,path,fsync_p95_ms) VALUES('h','2026-11-17T10:00:00+00:00',?,?)", rusqlite::params![p.to_string_lossy(), ms]).unwrap();
    }
    let plain = gt(d.path()).args(["doctor", "--config", cfg.to_str().unwrap(), "--host", "h"]).output().unwrap();
    assert!(!out(&plain).contains("disk-speed-measured"), "without --db nothing changes: {}", out(&plain));
    let o = gt(d.path()).args(["doctor", "--config", cfg.to_str().unwrap(), "--host", "h", "--db", db.to_str().unwrap()]).output().unwrap();
    let t = out(&o);
    assert!(t.contains("disk-speed-measured") && t.contains("60.0 ms") && t.contains("2.0 ms"), "{t}");
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("fast = true"), "the flag is not changed");
}
