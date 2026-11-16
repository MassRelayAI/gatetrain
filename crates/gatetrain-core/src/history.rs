//! What the ledger knows about earlier runs, for `eta`: step durations of finished chain runs (port of
//! gatetrain/db.py `history_durations`), and the run outcome a log implies (`classify`).
use crate::logfmt::{ParsedLog, Ts};
use std::path::Path;

/// Step name -> durations, names in the order a first row names them (a Python dict keeps insertion order,
/// and the prediction's tie-breaking depends on it).
pub type History = Vec<(String, Vec<f64>)>;

/// Successful, uncached step durations from completed (green or red) chain runs, excluding `exclude_run`.
/// A missing ledger is an empty history and is NOT created (Python's `connect` would create it; the output is the same).
/// `None` means the file exists but is not a ledger this reader understands: the caller falls back to Python.
pub fn history_durations(db: &Path, exclude_run: &str) -> Option<History> {
    if !db.exists() {
        return Some(Vec::new());
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let c = rusqlite::Connection::open_with_flags(db, flags).ok()?;
    history_from(&c, exclude_run)
}

/// The same query on an open connection (a ledger in memory, or one just migrated).
pub fn history_from(c: &rusqlite::Connection, exclude_run: &str) -> Option<History> {
    let mut st = c
        .prepare(
            "SELECT s.name, s.duration_sec FROM steps s JOIN runs r ON r.id=s.run_id \
             WHERE r.kind='chain' AND r.outcome IN ('green','red') AND s.exit_code=0 AND s.cached=0 \
             AND s.duration_sec IS NOT NULL AND r.id<>?",
        )
        .ok()?;
    let rows = st.query_map([exclude_run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))).ok()?;
    let mut h: History = Vec::new();
    for row in rows {
        let (name, d) = row.ok()?;
        match h.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => v.push(d),
            None => h.push((name, vec![d])),
        }
    }
    Some(h)
}

/// db.py `classify`: green / red once GATES_DONE is seen, else running while the last timestamp is fresh, else stopped.
pub fn classify(p: &ParsedLog, now: &Ts, stale_min: f64) -> &'static str {
    if p.gates_done {
        let reds = p.red_summary.as_deref().unwrap_or("none").trim().to_lowercase();
        let has_exit = p.steps.iter().any(|s| matches!(s.exit_code, Some(x) if x != 0));
        return if (reds.is_empty() || reds == "none") && !has_exit { "green" } else { "red" };
    }
    match p.last_ts() {
        Some(last) if now.secs_since(&last) < stale_min * 60.0 => "running",
        _ => "stopped",
    }
}
