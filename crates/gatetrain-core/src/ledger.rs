//! The ledger: opens the existing SQLite file (ledger/schema.sql, same tables and columns as gatetrain/db.py),
//! migrates it forward only (`PRAGMA user_version`), and keeps run snapshots instead of delete-and-reinsert.
use crate::chain::RunRec;
use crate::logfmt::ParsedLog;
use crate::{Error, Result};
use rusqlite::{params, Connection};
use std::path::Path;

/// The schema every version carries (version 0 = a ledger written by the Python side before versions existed).
const BASE_SCHEMA: &str = include_str!("../../../ledger/schema.sql");

/// Version 1: run snapshots (a running run is a series of rows, the final row is inserted once, nothing is deleted)
/// and the decision log, both insert-only with triggers that abort UPDATE and DELETE.
const V1: &str = r#"
CREATE TABLE IF NOT EXISTS run_snapshots (
  snapshot_id  INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id       TEXT NOT NULL,
  taken_at     TEXT NOT NULL,
  kind         TEXT NOT NULL,
  batch        TEXT,
  host         TEXT NOT NULL,
  commit_sha   TEXT,
  started_at   TEXT NOT NULL,
  ended_at     TEXT,
  outcome      TEXT,
  stop_reason  TEXT,
  steps        TEXT NOT NULL            -- JSON array of the steps as parsed at that moment
);
CREATE INDEX IF NOT EXISTS run_snapshots_run ON run_snapshots(run_id, snapshot_id);
CREATE TRIGGER IF NOT EXISTS run_snapshots_no_update BEFORE UPDATE ON run_snapshots
  BEGIN SELECT RAISE(ABORT, 'run_snapshots is append-only'); END;
CREATE TRIGGER IF NOT EXISTS run_snapshots_no_delete BEFORE DELETE ON run_snapshots
  BEGIN SELECT RAISE(ABORT, 'run_snapshots is append-only'); END;
-- newest snapshot of each run wins
CREATE VIEW IF NOT EXISTS run_current AS
SELECT s.* FROM run_snapshots s
WHERE s.snapshot_id = (SELECT MAX(snapshot_id) FROM run_snapshots WHERE run_id = s.run_id);

CREATE TABLE IF NOT EXISTS decisions (
  seq          INTEGER PRIMARY KEY,
  at           TEXT NOT NULL,
  actor        TEXT NOT NULL,
  kind         TEXT NOT NULL,
  batch        TEXT,
  subject      TEXT,
  rule         TEXT,
  policy_hash  TEXT,
  evidence     TEXT,                    -- canonical JSON or NULL
  outcome      TEXT NOT NULL,
  reason       TEXT NOT NULL,
  signature    TEXT,
  prev_hash    TEXT NOT NULL,
  hash         TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS decisions_no_update BEFORE UPDATE ON decisions
  BEGIN SELECT RAISE(ABORT, 'decisions is append-only'); END;
CREATE TRIGGER IF NOT EXISTS decisions_no_delete BEFORE DELETE ON decisions
  BEGIN SELECT RAISE(ABORT, 'decisions is append-only'); END;
"#;

/// Version 2: the peak memory of each step of each run (append-only). Typical memory per step is the median of these rows.
const V2: &str = r#"
CREATE TABLE IF NOT EXISTS step_memory (
  run_id       TEXT NOT NULL,
  step         TEXT NOT NULL,
  peak_mb      REAL NOT NULL,
  source       TEXT NOT NULL,           -- cgroup | rss-sampled | working-set | reported
  taken_at     TEXT NOT NULL,
  PRIMARY KEY (run_id, step, taken_at)
);
CREATE INDEX IF NOT EXISTS step_memory_step ON step_memory(step);
CREATE TRIGGER IF NOT EXISTS step_memory_no_update BEFORE UPDATE ON step_memory
  BEGIN SELECT RAISE(ABORT, 'step_memory is append-only'); END;
CREATE TRIGGER IF NOT EXISTS step_memory_no_delete BEFORE DELETE ON step_memory
  BEGIN SELECT RAISE(ABORT, 'step_memory is append-only'); END;
"#;

/// Version 3: the batch state machine. `batch_events` is insert-only; the state of a batch is derived
/// by folding it (the newest `done` event), there is no status column.
const V3: &str = r#"
CREATE TABLE IF NOT EXISTS batch_events (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  batch        TEXT NOT NULL,
  transition   TEXT NOT NULL,
  phase        TEXT NOT NULL CHECK (phase IN ('intent','done','failed')),
  attempt      INTEGER NOT NULL,
  state_to     TEXT,
  input_sha    TEXT,
  output_sha   TEXT,
  run_id       TEXT,
  host         TEXT,
  at           TEXT NOT NULL,
  decision_seq INTEGER,
  detail       TEXT NOT NULL            -- JSON object: the facts persisted on entry (design 3.1)
);
CREATE INDEX IF NOT EXISTS batch_events_batch ON batch_events(batch, seq);
CREATE TRIGGER IF NOT EXISTS batch_events_no_update BEFORE UPDATE ON batch_events
  BEGIN SELECT RAISE(ABORT, 'batch_events is append-only'); END;
CREATE TRIGGER IF NOT EXISTS batch_events_no_delete BEFORE DELETE ON batch_events
  BEGIN SELECT RAISE(ABORT, 'batch_events is append-only'); END;
"#;

/// Version 4 (engine v2, slice E1): `chain --db` writes its own run. Columns are only added (NULL = unknown); the new tables are insert-only.
const V4: &str = r#"
ALTER TABLE runs ADD COLUMN mode TEXT;
ALTER TABLE runs ADD COLUMN par INTEGER;
ALTER TABLE runs ADD COLUMN plan_digest TEXT;
ALTER TABLE runs ADD COLUMN log_sha256 TEXT;
ALTER TABLE runs ADD COLUMN config_hash TEXT;
ALTER TABLE runs ADD COLUMN host_class TEXT;
ALTER TABLE runs ADD COLUMN gatetrain_version TEXT;
ALTER TABLE steps ADD COLUMN tier TEXT;
ALTER TABLE steps ADD COLUMN input_key TEXT;
ALTER TABLE steps ADD COLUMN cached_from TEXT;
ALTER TABLE steps ADD COLUMN shards INTEGER;
CREATE TABLE IF NOT EXISTS step_skips (
  run_id       TEXT NOT NULL,
  step         TEXT NOT NULL,
  reason       TEXT NOT NULL,
  PRIMARY KEY (run_id, step)
);
CREATE TABLE IF NOT EXISTS stamps (
  digest       TEXT PRIMARY KEY,
  kind         TEXT NOT NULL,
  sha          TEXT NOT NULL,
  host         TEXT NOT NULL,
  run_id       TEXT,
  plan_digest  TEXT,
  at           TEXT NOT NULL,
  body         TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS stamp_links (
  digest       TEXT NOT NULL,
  prev_digest  TEXT NOT NULL,
  PRIMARY KEY (digest, prev_digest)
);
CREATE TRIGGER IF NOT EXISTS step_skips_no_update BEFORE UPDATE ON step_skips
  BEGIN SELECT RAISE(ABORT, 'step_skips is append-only'); END;
CREATE TRIGGER IF NOT EXISTS step_skips_no_delete BEFORE DELETE ON step_skips
  BEGIN SELECT RAISE(ABORT, 'step_skips is append-only'); END;
CREATE TRIGGER IF NOT EXISTS stamps_no_update BEFORE UPDATE ON stamps
  BEGIN SELECT RAISE(ABORT, 'stamps is append-only'); END;
CREATE TRIGGER IF NOT EXISTS stamps_no_delete BEFORE DELETE ON stamps
  BEGIN SELECT RAISE(ABORT, 'stamps is append-only'); END;
CREATE TRIGGER IF NOT EXISTS stamp_links_no_update BEFORE UPDATE ON stamp_links
  BEGIN SELECT RAISE(ABORT, 'stamp_links is append-only'); END;
CREATE TRIGGER IF NOT EXISTS stamp_links_no_delete BEFORE DELETE ON stamp_links
  BEGIN SELECT RAISE(ABORT, 'stamp_links is append-only'); END;
"#;

/// Version 5 (engine v2, slice E2): one row per shard of a sharded step.
const V5: &str = r#"
CREATE TABLE IF NOT EXISTS step_shards (
  run_id       TEXT NOT NULL,
  step         TEXT NOT NULL,
  shard        INTEGER NOT NULL,
  of           INTEGER NOT NULL,
  started_at   TEXT,
  duration_sec REAL,
  exit_code    INTEGER,
  log_ref      TEXT,
  PRIMARY KEY (run_id, step, shard)
);
CREATE TRIGGER IF NOT EXISTS step_shards_no_update BEFORE UPDATE ON step_shards
  BEGIN SELECT RAISE(ABORT, 'step_shards is append-only'); END;
CREATE TRIGGER IF NOT EXISTS step_shards_no_delete BEFORE DELETE ON step_shards
  BEGIN SELECT RAISE(ABORT, 'step_shards is append-only'); END;
"#;

/// Version 6 (engine v2 E6): what `probe` measured, per host and per disk. NULL = unknown, `unknown` (JSON) says why.
const V6: &str = r#"
CREATE TABLE IF NOT EXISTS host_probes (
  host           TEXT NOT NULL,
  at             TEXT NOT NULL,
  os             TEXT,
  arch           TEXT,
  virt           TEXT,
  cores          INTEGER,
  mem_total_mb   REAL,
  cpu_one_mbps   REAL,
  cpu_all_mbps   REAL,
  host_class     TEXT,
  tool_versions  TEXT,
  duration_ms    INTEGER,
  unknown        TEXT,
  PRIMARY KEY (host, at)
);
CREATE TABLE IF NOT EXISTS disk_probes (
  host              TEXT NOT NULL,
  at                TEXT NOT NULL,
  path              TEXT NOT NULL,
  device            TEXT,
  fs_type           TEXT,
  rotational        INTEGER,
  rotational_source TEXT,
  fsync_p50_ms      REAL,
  fsync_p95_ms      REAL,
  seq_write_mbps    REAL,
  smallfile_per_s   REAL,
  free_gb           REAL,
  total_gb          REAL,
  bytes_written     INTEGER,
  duration_ms       INTEGER,
  unknown           TEXT,
  PRIMARY KEY (host, at, path)
);
CREATE TRIGGER IF NOT EXISTS host_probes_no_update BEFORE UPDATE ON host_probes
  BEGIN SELECT RAISE(ABORT, 'host_probes is append-only'); END;
CREATE TRIGGER IF NOT EXISTS host_probes_no_delete BEFORE DELETE ON host_probes
  BEGIN SELECT RAISE(ABORT, 'host_probes is append-only'); END;
CREATE TRIGGER IF NOT EXISTS disk_probes_no_update BEFORE UPDATE ON disk_probes
  BEGIN SELECT RAISE(ABORT, 'disk_probes is append-only'); END;
CREATE TRIGGER IF NOT EXISTS disk_probes_no_delete BEFORE DELETE ON disk_probes
  BEGIN SELECT RAISE(ABORT, 'disk_probes is append-only'); END;
"#;

/// Version 7 (engine v2 E7): the sampler. Columns are added to `host_samples` (which nothing wrote before); `disk_samples` is new.
const V7: &str = r#"
ALTER TABLE host_samples ADD COLUMN run_id TEXT;
ALTER TABLE host_samples ADD COLUMN running_steps INTEGER;
ALTER TABLE host_samples ADD COLUMN cpu_busy_pct REAL;
ALTER TABLE host_samples ADD COLUMN iowait_pct REAL;
ALTER TABLE host_samples ADD COLUMN mem_free_mb REAL;
ALTER TABLE host_samples ADD COLUMN stall_io_some REAL;
ALTER TABLE host_samples ADD COLUMN stall_io_full REAL;
ALTER TABLE host_samples ADD COLUMN stall_cpu_some REAL;
ALTER TABLE host_samples ADD COLUMN stall_mem_some REAL;
CREATE TABLE IF NOT EXISTS disk_samples (
  host       TEXT NOT NULL,
  at         TEXT NOT NULL,
  device     TEXT NOT NULL,
  path       TEXT,
  busy_pct   REAL,
  read_mbps  REAL,
  write_mbps REAL,
  free_gb    REAL,
  PRIMARY KEY (host, at, device)
);
CREATE TRIGGER IF NOT EXISTS host_samples_no_update BEFORE UPDATE ON host_samples
  BEGIN SELECT RAISE(ABORT, 'host_samples is append-only'); END;
CREATE TRIGGER IF NOT EXISTS host_samples_no_delete BEFORE DELETE ON host_samples
  BEGIN SELECT RAISE(ABORT, 'host_samples is append-only'); END;
CREATE TRIGGER IF NOT EXISTS disk_samples_no_update BEFORE UPDATE ON disk_samples
  BEGIN SELECT RAISE(ABORT, 'disk_samples is append-only'); END;
CREATE TRIGGER IF NOT EXISTS disk_samples_no_delete BEFORE DELETE ON disk_samples
  BEGIN SELECT RAISE(ABORT, 'disk_samples is append-only'); END;
"#;

/// Version 8 (engine v2 E8): the advice `advise` printed, and the later comparison of its prediction with what was measured.
const V8: &str = r#"
CREATE TABLE IF NOT EXISTS advice (
  advice_id    TEXT PRIMARY KEY,
  at           TEXT NOT NULL,
  rule         TEXT NOT NULL,
  host         TEXT,
  subject      TEXT,
  move_json    TEXT,
  minutes_low  REAL,
  minutes_high REAL,
  confidence   TEXT NOT NULL,
  evidence     TEXT,
  config_hash  TEXT
);
CREATE TABLE IF NOT EXISTS advice_checks (
  advice_id        TEXT NOT NULL,
  run_id           TEXT NOT NULL,
  at               TEXT NOT NULL,
  measured_minutes REAL,
  verdict          TEXT NOT NULL,
  PRIMARY KEY (advice_id, run_id)
);
CREATE TRIGGER IF NOT EXISTS advice_no_update BEFORE UPDATE ON advice
  BEGIN SELECT RAISE(ABORT, 'advice is append-only'); END;
CREATE TRIGGER IF NOT EXISTS advice_no_delete BEFORE DELETE ON advice
  BEGIN SELECT RAISE(ABORT, 'advice is append-only'); END;
CREATE TRIGGER IF NOT EXISTS advice_checks_no_update BEFORE UPDATE ON advice_checks
  BEGIN SELECT RAISE(ABORT, 'advice_checks is append-only'); END;
CREATE TRIGGER IF NOT EXISTS advice_checks_no_delete BEFORE DELETE ON advice_checks
  BEGIN SELECT RAISE(ABORT, 'advice_checks is append-only'); END;
"#;

/// The schema version this binary writes.
pub const CURRENT_VERSION: i64 = 8;

/// One row of `host_probes` (E6).
#[derive(Debug, Clone, Default)]
pub struct HostProbeRow {
    pub host: String,
    pub at: String,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub virt: Option<String>,
    pub cores: Option<i64>,
    pub mem_total_mb: Option<f64>,
    pub cpu_one_mbps: Option<f64>,
    pub cpu_all_mbps: Option<f64>,
    pub host_class: Option<String>,
    pub tool_versions: Option<String>,
    pub duration_ms: Option<i64>,
    pub unknown: Option<String>,
}

/// One row of `disk_probes` (E6).
#[derive(Debug, Clone, Default)]
pub struct DiskProbeRow {
    pub host: String,
    pub at: String,
    pub path: String,
    pub device: Option<String>,
    pub fs_type: Option<String>,
    pub rotational: Option<i64>,
    pub rotational_source: Option<String>,
    pub fsync_p50_ms: Option<f64>,
    pub fsync_p95_ms: Option<f64>,
    pub seq_write_mbps: Option<f64>,
    pub smallfile_per_s: Option<f64>,
    pub free_gb: Option<f64>,
    pub total_gb: Option<f64>,
    pub bytes_written: Option<i64>,
    pub duration_ms: Option<i64>,
    pub unknown: Option<String>,
}

pub struct Ledger {
    pub conn: Connection,
}

#[derive(Debug, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub kind: String,
    pub batch: Option<String>,
    pub host: String,
    pub started_at: String,
    pub outcome: Option<String>,
}

impl Ledger {
    /// Open an existing ledger without changing it. A version newer than this binary is refused.
    pub fn open(path: &Path) -> Result<Ledger> {
        let l = Ledger { conn: Connection::open(path)? };
        let v = l.version()?;
        if v > CURRENT_VERSION {
            return Err(Error::SchemaTooNew { found: v, known: CURRENT_VERSION });
        }
        Ok(l)
    }

    pub fn open_in_memory() -> Result<Ledger> {
        Ok(Ledger { conn: Connection::open_in_memory()? })
    }

    pub fn version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Forward-only migration, in one transaction. Returns (from, to). Idempotent at the current version.
    pub fn migrate(&mut self) -> Result<(i64, i64)> {
        let from = self.version()?;
        if from > CURRENT_VERSION {
            return Err(Error::SchemaTooNew { found: from, known: CURRENT_VERSION });
        }
        if from == CURRENT_VERSION {
            return Ok((from, from));
        }
        let tx = self.conn.transaction()?;
        if from < 1 {
            tx.execute_batch(BASE_SCHEMA)?;
            tx.execute_batch(V1)?;
        }
        if from < 2 {
            tx.execute_batch(V2)?;
        }
        if from < 3 {
            tx.execute_batch(V3)?;
        }
        if from < 4 {
            tx.execute_batch(V4)?;
        }
        if from < 5 {
            tx.execute_batch(V5)?;
        }
        if from < 6 {
            tx.execute_batch(V6)?;
        }
        if from < 7 {
            tx.execute_batch(V7)?;
        }
        if from < 8 {
            tx.execute_batch(V8)?;
        }
        tx.pragma_update(None, "user_version", CURRENT_VERSION)?;
        tx.commit()?;
        Ok((from, CURRENT_VERSION))
    }

    /// Writes need the current version; a v0 file must be migrated first.
    pub fn require_current(&self) -> Result<()> {
        let v = self.version()?;
        if v < CURRENT_VERSION {
            return Err(Error::NeedsMigration { found: v, known: CURRENT_VERSION });
        }
        Ok(())
    }

    pub fn runs(&self) -> Result<Vec<RunRow>> {
        let mut st = self.conn.prepare("SELECT id,kind,batch,host,started_at,outcome FROM runs ORDER BY rowid")?;
        let rows = st.query_map([], |r| {
            Ok(RunRow { id: r.get(0)?, kind: r.get(1)?, batch: r.get(2)?, host: r.get(3)?, started_at: r.get(4)?, outcome: r.get(5)? })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one step's peak memory. Append-only; a step measured twice has two rows.
    pub fn record_step_peak(&self, run_id: &str, step: &str, peak_mb: f64, source: &str, taken_at: &str) -> Result<()> {
        self.require_current()?;
        self.conn.execute("INSERT INTO step_memory(run_id,step,peak_mb,source,taken_at) VALUES(?,?,?,?,?)", params![run_id, step, peak_mb, source, taken_at])?;
        Ok(())
    }

    /// Every recorded peak per step name, in megabytes.
    pub fn step_memory_history(&self) -> Result<std::collections::HashMap<String, Vec<f64>>> {
        let mut m: std::collections::HashMap<String, Vec<f64>> = Default::default();
        let mut st = self.conn.prepare("SELECT step, peak_mb FROM step_memory ORDER BY rowid")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))? {
            let (k, v) = r?;
            m.entry(k).or_default().push(v);
        }
        Ok(m)
    }

    /// Write one finished `chain` run (engine v2, slice E1): its run row, steps, shards, skips and stamp, in one transaction.
    /// Append-only: a run id already present is an error and nothing is written.
    pub fn record_chain_run(&self, rid: &str, r: &RunRec) -> Result<()> {
        self.require_current()?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome,mode,par,plan_digest,log_sha256,gatetrain_version)              VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![rid, "chain", r.branch, r.host, r.sha, r.started_at, r.ended_at, r.outcome, r.mode, r.par as i64, r.plan_digest, r.log_sha256, env!("CARGO_PKG_VERSION")],
        )?;
        for s in &r.steps {
            tx.execute(
                "INSERT INTO steps(run_id,name,started_at,ended_at,duration_sec,exit_code,cached,log_ref,tier,input_key,cached_from,shards) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                params![rid, s.name, s.started_at, s.ended_at, s.secs, s.rc, s.cached as i64, s.log_ref, s.tier, s.key, s.cached_from, s.shards],
            )?;
        }
        for h in &r.shards {
            tx.execute(
                "INSERT INTO step_shards(run_id,step,shard,of,started_at,duration_sec,exit_code,log_ref) VALUES(?,?,?,?,?,?,?,?)",
                params![rid, h.step, h.shard, h.of, h.started_at, h.secs, h.rc, h.log_ref],
            )?;
        }
        for (step, why) in &r.skips {
            tx.execute("INSERT INTO step_skips(run_id,step,reason) VALUES(?,?,?)", params![rid, step, why])?;
        }
        if let Some((digest, body)) = &r.stamp {
            tx.execute(
                "INSERT INTO stamps(digest,kind,sha,host,run_id,plan_digest,at,body) VALUES(?,?,?,?,?,?,?,?)",
                params![digest, r.mode, r.sha, r.host, rid, r.plan_digest, r.ended_at, body],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Store one probe of a host (E6). `unknown` is a JSON object of figure to reason.
    pub fn record_host_probe(&self, p: &HostProbeRow) -> Result<()> {
        self.require_current()?;
        self.conn.execute(
            "INSERT INTO host_probes(host,at,os,arch,virt,cores,mem_total_mb,cpu_one_mbps,cpu_all_mbps,host_class,tool_versions,duration_ms,unknown) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![p.host, p.at, p.os, p.arch, p.virt, p.cores, p.mem_total_mb, p.cpu_one_mbps, p.cpu_all_mbps, p.host_class, p.tool_versions, p.duration_ms, p.unknown],
        )?;
        Ok(())
    }

    pub fn record_disk_probe(&self, p: &DiskProbeRow) -> Result<()> {
        self.require_current()?;
        self.conn.execute(
            "INSERT INTO disk_probes(host,at,path,device,fs_type,rotational,rotational_source,fsync_p50_ms,fsync_p95_ms,seq_write_mbps,smallfile_per_s,free_gb,total_gb,bytes_written,duration_ms,unknown) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![p.host, p.at, p.path, p.device, p.fs_type, p.rotational, p.rotational_source, p.fsync_p50_ms, p.fsync_p95_ms, p.seq_write_mbps, p.smallfile_per_s, p.free_gb, p.total_gb, p.bytes_written, p.duration_ms, p.unknown],
        )?;
        Ok(())
    }

    /// Ingest the lines of a `samples.jsonl` (E7): the `{"unknown": ..}` header is skipped. A line whose host and time are already stored
    /// is skipped (the file may be ingested twice). Returns how many rows were written.
    pub fn record_samples(&self, text: &str) -> Result<usize> {
        self.require_current()?;
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        for l in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = match serde_json::from_str(l) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let (Some(host), Some(at)) = (v["host"].as_str(), v["at"].as_str()) else { continue };
            let f = |k: &str| v[k].as_f64();
            let added = tx.execute(
                "INSERT OR IGNORE INTO host_samples(host,at,run_id,running_steps,load_1m,cpu_busy_pct,iowait_pct,mem_free_mb,stall_io_some,stall_io_full,stall_cpu_some,stall_mem_some) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                params![host, at, v["run_id"].as_str(), v["running_steps"].as_i64(), f("load_1m"), f("cpu_busy_pct"), f("iowait_pct"), f("mem_free_mb"), f("stall_io_some"), f("stall_io_full"), f("stall_cpu_some"), f("stall_mem_some")],
            )?;
            if added > 0 {
                n += 1;
                for d in v["disks"].as_array().into_iter().flatten() {
                    if let Some(dev) = d["device"].as_str() {
                        tx.execute("INSERT OR IGNORE INTO disk_samples(host,at,device,busy_pct) VALUES(?,?,?,?)", params![host, at, dev, d["busy_pct"].as_f64()])?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn step_count(&self, run_id: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM steps WHERE run_id=?", [run_id], |r| r.get(0))?)
    }

    /// Record the state of a run as a new snapshot row. Never deletes, never updates
    /// (replaces Python's `insert_run(replace=True)`).
    pub fn record_snapshot(&self, run_id: &str, p: &ParsedLog, outcome: &str, host: Option<&str>, taken_at: &str) -> Result<i64> {
        self.require_current()?;
        let (first, last) = (p.first_ts(), p.last_ts());
        let started = first.or(last).map(|t| t.iso()).unwrap_or_else(|| "unknown".into());
        let ended = if matches!(outcome, "green" | "red") { last.map(|t| t.iso()) } else { None };
        let stop = if outcome == "stopped" { Some("no GATES_DONE; log went quiet") } else { None };
        let steps = serde_json::to_string(&p.to_json()["steps"])?;
        self.conn.execute(
            "INSERT INTO run_snapshots(run_id,taken_at,kind,batch,host,commit_sha,started_at,ended_at,outcome,stop_reason,steps) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![run_id, taken_at, "chain", p.branch, host.or(p.host.as_deref()).unwrap_or("unknown"), p.commit, started, ended, outcome, stop, steps],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The newest snapshot's (outcome, step count) for a run, if any.
    pub fn current_snapshot(&self, run_id: &str) -> Result<Option<(Option<String>, i64)>> {
        let r = self.conn.query_row(
            "SELECT outcome, steps FROM run_current WHERE run_id=?",
            [run_id],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
        );
        match r {
            Ok((o, s)) => {
                let n = serde_json::from_str::<serde_json::Value>(&s)?.as_array().map_or(0, |a| a.len() as i64);
                Ok(Some((o, n)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
