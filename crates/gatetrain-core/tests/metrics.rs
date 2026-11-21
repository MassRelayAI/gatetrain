//! The minutes-lost view over a fixture ledger. Every expected number is worked out by hand in the comments of `fixture`.
use gatetrain_core::ledger::Ledger;
use gatetrain_core::metrics::{compute, to_csv, to_json, to_text};
use rusqlite::{params, Connection};

fn run(c: &Connection, id: &str, batch: &str, s: &str, e: &str, outcome: &str, why: Option<&str>) {
    c.execute("INSERT INTO runs(id,kind,batch,host,started_at,ended_at,outcome,stop_reason) VALUES(?,?,?,?,?,?,?,?)", params![id, "chain", batch, "h1", format!("2026-11-01T{s}:00Z"), format!("2026-11-01T{e}:00Z"), outcome, why]).unwrap();
}

#[allow(clippy::too_many_arguments)]
fn step(c: &Connection, rid: &str, name: &str, start: Option<&str>, end: Option<&str>, dur: Option<f64>, exit: Option<i64>, cached: i64) {
    c.execute(
        "INSERT INTO steps(run_id,name,started_at,ended_at,duration_sec,exit_code,cached) VALUES(?,?,?,?,?,?,?)",
        params![rid, name, start.map(|t| format!("2026-11-01T{t}:00Z")), end.map(|t| format!("2026-11-01T{t}:00Z")), dur, exit, cached],
    )
    .unwrap();
}

/// b1: b1-1 red 10:00-10:30 (30 min; unit fails at 10:02, e2e at 10:20, so the cause is unit), b1-2 green 10:40-11:00 (20 min; lint cached,
///     unit 1100 s is the critical path, 3 unit shards of 100/200/300 s). Wait after b1-1: 10 min. One T6a event.
/// b2: b2-1 stopped 12:00-12:10 (10 min, e2e still open), b2-2 red 12:15-12:25 (10 min, e2e fails), b2-3 green 12:50-13:10.
///     Waits: 5 min after b2-1, 25 min after b2-2.
/// totals: red 40, restart 10, wait 40 = 90 min. causes: unit 40 (red 30 + wait 10), e2e 35 (red 10 + wait 25), e2e (open at stop) 15.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("g.db");
    let mut l = Ledger::open(&p).unwrap();
    l.migrate().unwrap();
    drop(l);
    let c = Connection::open(&p).unwrap();
    run(&c, "b1-1", "b1", "10:00", "10:30", "red", None);
    step(&c, "b1-1", "lint", Some("10:00"), Some("10:02"), Some(120.0), Some(0), 0);
    step(&c, "b1-1", "unit", Some("10:02"), Some("10:12"), Some(600.0), Some(1), 0);
    step(&c, "b1-1", "e2e", Some("10:20"), Some("10:30"), Some(600.0), Some(1), 0);
    run(&c, "b1-2", "b1", "10:40", "11:00", "green", None);
    step(&c, "b1-2", "lint", Some("10:40"), Some("10:41"), Some(1.0), Some(0), 1);
    step(&c, "b1-2", "unit", Some("10:41"), Some("10:59"), Some(1100.0), Some(0), 0);
    step(&c, "b1-2", "e2e", Some("10:41"), Some("10:46"), Some(300.0), Some(0), 0);
    for (i, dur) in [100.0, 200.0, 300.0].iter().enumerate() {
        c.execute("INSERT INTO step_shards(run_id,step,shard,of,duration_sec,exit_code) VALUES('b1-2','unit',?,3,?,0)", params![i as i64, dur]).unwrap();
    }
    c.execute("INSERT INTO batch_events(batch,transition,phase,attempt,at,detail) VALUES('b1','T6a','done',1,'2026-11-01T10:35:00Z','{}')", []).unwrap();
    run(&c, "b2-1", "b2", "12:00", "12:10", "stopped", Some("manual"));
    step(&c, "b2-1", "e2e", Some("12:01"), None, None, None, 0);
    run(&c, "b2-2", "b2", "12:15", "12:25", "red", None);
    step(&c, "b2-2", "e2e", Some("12:16"), Some("12:25"), Some(540.0), Some(1), 0);
    run(&c, "b2-3", "b2", "12:50", "13:10", "green", None);
    step(&c, "b2-3", "unit", Some("12:50"), Some("13:09"), Some(1140.0), Some(0), 0);
    (d, p)
}

fn open(p: &std::path::Path) -> Ledger {
    Ledger::open(p).unwrap()
}

#[test]
fn minutes_lost_are_attributed_to_the_red_step() {
    let (_d, p) = fixture();
    let m = compute(&open(&p), 0).unwrap();
    assert_eq!((m.chains, m.red), (5, 2));
    assert_eq!((m.total.red_min, m.total.restart_min, m.total.wait_min), (40.0, 10.0, 40.0));
    let c: Vec<(&str, f64)> = m.causes.iter().map(|c| (c.cause.as_str(), c.lost.total())).collect();
    assert_eq!(c, [("unit", 40.0), ("e2e", 35.0), ("e2e (open at stop)", 15.0)]);
    assert_eq!(m.causes[0].lost.red_min, 30.0);
    assert_eq!(m.causes[0].lost.wait_min, 10.0);
}

#[test]
fn batches_count_chains_reds_and_restart_events() {
    let (_d, p) = fixture();
    let m = compute(&open(&p), 0).unwrap();
    let b1 = &m.batches[0];
    assert_eq!((b1.batch.as_str(), b1.chains, b1.red, b1.green, b1.other, b1.restart_events), ("b1", 2, 1, 1, 0, 1));
    assert_eq!(b1.top_cause.as_deref(), Some("unit"));
    let b2 = &m.batches[1];
    assert_eq!((b2.chains, b2.red, b2.green, b2.other, b2.lost.total()), (3, 1, 1, 1, 50.0));
}

#[test]
fn the_last_n_batches_only() {
    let (_d, p) = fixture();
    let m = compute(&open(&p), 1).unwrap();
    assert_eq!(m.batches.len(), 1);
    assert_eq!(m.batches[0].batch, "b2");
    assert_eq!(m.total.total(), 50.0);
}

#[test]
fn step_stats_critical_path_cache_and_shards() {
    let (_d, p) = fixture();
    let m = compute(&open(&p), 0).unwrap();
    let unit = m.steps.iter().find(|s| s.step == "unit").unwrap();
    assert_eq!((unit.ran, unit.cached, unit.red), (3, 0, 1));
    assert_eq!(unit.median_s, Some(1100.0)); // 600, 1100, 1140
    assert_eq!(unit.max_s, Some(1140.0));
    assert_eq!(unit.critical, 2); // b1-2 and b2-3
    assert_eq!((unit.shard_groups, unit.avg_slowest_s, unit.avg_mean_s, unit.skew), (1, Some(300.0), Some(200.0), Some(1.5)));
    let lint = m.steps.iter().find(|s| s.step == "lint").unwrap();
    assert_eq!((lint.ran, lint.cached), (1, 1), "the cached run is counted apart and its 1 s is not a duration");
    assert_eq!(lint.max_s, Some(120.0));
    assert_eq!(m.steps[0].step, "unit", "longest first");
}

#[test]
fn text_json_and_csv_carry_the_same_numbers() {
    let (_d, p) = fixture();
    let m = compute(&open(&p), 0).unwrap();
    let t = to_text(&m);
    assert!(t.starts_with("minutes lost: 90.0 min in 3 red/stopped chain(s) of 5 chain(s) over 2 batch(es): red 40.0, restart 10.0, wait 40.0"), "{t}");
    assert!(t.contains("40.0 min  unit"), "{t}");
    assert!(t.contains("slowest 5m00s vs mean 3m20s (skew 1.50)"), "{t}");
    let j = to_json(&m);
    assert_eq!(j["lost"]["total_min"], 90.0);
    assert_eq!(j["causes"][0]["cause"], "unit");
    let csv = to_csv(&m);
    assert_eq!(csv[2].1.lines().next().unwrap(), "cause,chains,red_min,restart_min,wait_min,total_min");
    assert!(csv[2].1.contains("unit,1,30.0,0.0,10.0,40.0"), "{}", csv[2].1);
    assert!(csv[2].1.contains("\"e2e (open at stop)\"") || csv[2].1.contains("e2e (open at stop),1,0.0,10.0,5.0,15.0"), "{}", csv[2].1);
}

#[test]
fn an_empty_ledger_says_nothing_was_lost() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("e.db");
    let mut l = Ledger::open(&p).unwrap();
    l.migrate().unwrap();
    let m = compute(&l, 5).unwrap();
    assert_eq!((m.chains, m.batches.len()), (0, 0));
    assert!(to_text(&m).contains("none: no chain ended red or stopped"));
}

#[test]
fn an_unmigrated_ledger_is_refused_not_guessed() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("old.db");
    Connection::open(&p).unwrap().execute("CREATE TABLE runs(id TEXT)", []).unwrap();
    let l = Ledger::open(&p).unwrap();
    assert!(compute(&l, 0).is_err());
}
