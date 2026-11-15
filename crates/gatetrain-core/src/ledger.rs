//! The ledger: opens the existing SQLite file (ledger/schema.sql, same tables and columns as gatetrain/db.py),
//! migrates it forward only (`PRAGMA user_version`), and keeps run snapshots instead of delete-and-reinsert.
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

/// The schema version this binary writes.
pub const CURRENT_VERSION: i64 = 3;

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
