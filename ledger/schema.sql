-- gatetrain ledger: append-only. SQLite or Postgres. One row per thing that happened; nothing is updated in place.

CREATE TABLE IF NOT EXISTS runs (
  id           TEXT PRIMARY KEY,           -- e.g. "<batch>-<n>" or "<lane>-gates-<n>"
  kind         TEXT NOT NULL,              -- chain | preflight | lane-gates | review | agent | stack | merge | tidy
  batch        TEXT,
  lane         TEXT,
  host         TEXT NOT NULL,
  commit_sha   TEXT,
  started_at   TEXT NOT NULL,              -- ISO 8601 with zone
  ended_at     TEXT,
  outcome      TEXT,                       -- green | red | stopped | error
  stop_reason  TEXT,
  cost_usd     REAL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS steps (
  run_id       TEXT NOT NULL REFERENCES runs(id),
  name         TEXT NOT NULL,
  started_at   TEXT NOT NULL,
  ended_at     TEXT,
  exit_code    INTEGER,
  cached       INTEGER DEFAULT 0,
  waited_sec   REAL DEFAULT 0,             -- time queued for a lock, a slot or a port
  peak_rss_mb  REAL,
  log_ref      TEXT,
  PRIMARY KEY (run_id, name)
);

CREATE TABLE IF NOT EXISTS agent_handbacks (
  id           TEXT PRIMARY KEY,
  run_id       TEXT REFERENCES runs(id),
  model        TEXT NOT NULL,
  task_kind    TEXT NOT NULL,              -- implement | review | research | jury
  tokens       INTEGER,
  cost_usd     REAL,
  first_try_green INTEGER,                 -- 1 when no orchestrator fix was needed
  fixes_needed INTEGER DEFAULT 0,
  findings     INTEGER DEFAULT 0,          -- for reviews: confirmed findings
  citations_checked INTEGER DEFAULT 0,
  citations_wrong   INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS host_samples (
  host         TEXT NOT NULL,
  at           TEXT NOT NULL,
  free_gb      REAL,
  load_1m      REAL,
  awake        INTEGER,
  leftover_containers INTEGER,
  PRIMARY KEY (host, at)
);

-- flaky: the same step on the same commit, red once and green once
CREATE VIEW IF NOT EXISTS flaky_steps AS
SELECT s.name, r.commit_sha, COUNT(DISTINCT s.exit_code) AS outcomes
FROM steps s JOIN runs r ON r.id = s.run_id
WHERE r.kind = 'chain'
GROUP BY s.name, r.commit_sha
HAVING COUNT(DISTINCT CASE WHEN s.exit_code = 0 THEN 0 ELSE 1 END) > 1;
