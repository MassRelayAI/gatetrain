"""Ledger access. The schema is ledger/schema.sql; rows are appended, never updated."""
import sqlite3
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional

from .parse import ParsedLog, Step, parse_iso

SCHEMA = Path(__file__).resolve().parent.parent / "ledger" / "schema.sql"


OPEN: list = []


def close_all():
    while OPEN:
        OPEN.pop().close()


def connect(path: str) -> sqlite3.Connection:
    c = sqlite3.connect(path)
    OPEN.append(c)
    c.executescript(SCHEMA.read_text())
    return c


def classify(p: ParsedLog, now: datetime, stale_min: float) -> str:
    if p.gates_done:
        reds = (p.red_summary or "none").strip().lower()
        has_exit = any(s.exit_code not in (None, 0) for s in p.steps)
        return "green" if reds in ("", "none") and not has_exit else "red"
    last = p.last_ts
    if last is not None and (now - last).total_seconds() < stale_min * 60:
        return "running"
    return "stopped"


def run_exists(c, run_id) -> bool:
    return c.execute("SELECT 1 FROM runs WHERE id=?", (run_id,)).fetchone() is not None


def insert_run(c, run_id: str, p: ParsedLog, outcome: str, host: Optional[str], replace=False):
    if replace:
        c.execute("DELETE FROM steps WHERE run_id=?", (run_id,))
        c.execute("DELETE FROM runs WHERE id=?", (run_id,))
    first, last = p.first_ts, p.last_ts
    c.execute(
        "INSERT INTO runs(id,kind,batch,host,commit_sha,started_at,ended_at,outcome,stop_reason) VALUES(?,?,?,?,?,?,?,?,?)",
        (run_id, "chain", p.branch, host or p.host or "unknown", p.commit,
         first.isoformat() if first else (last.isoformat() if last else "unknown"),
         last.isoformat() if (last and outcome in ("green", "red")) else None,
         outcome, "no GATES_DONE; log went quiet" if outcome == "stopped" else None))
    for s in p.steps:
        c.execute(
            "INSERT INTO steps(run_id,name,started_at,ended_at,exit_code,cached,duration_sec) VALUES(?,?,?,?,?,?,?)",
            (run_id, s.name, s.start.isoformat() if s.start else None,
             s.end.isoformat() if s.end else None, s.exit_code, int(s.cached), s.duration))
    c.commit()


def load_steps(c, run_id: str) -> List[Step]:
    out = []
    for name, st, ex, cached, dur in c.execute(
            "SELECT name,started_at,exit_code,cached,duration_sec FROM steps WHERE run_id=? ORDER BY rowid", (run_id,)):
        out.append(Step(name, parse_iso(st) if st else None, dur, ex, bool(cached)))
    return out


def history_durations(c, exclude_run: Optional[str] = None) -> Dict[str, List[float]]:
    """Successful, uncached step durations from completed runs (green or red)."""
    h: Dict[str, List[float]] = {}
    q = ("SELECT s.name, s.duration_sec FROM steps s JOIN runs r ON r.id=s.run_id "
         "WHERE r.kind='chain' AND r.outcome IN ('green','red') AND s.exit_code=0 AND s.cached=0 "
         "AND s.duration_sec IS NOT NULL AND r.id<>?")
    for name, d in c.execute(q, (exclude_run or "",)):
        h.setdefault(name, []).append(d)
    return h
