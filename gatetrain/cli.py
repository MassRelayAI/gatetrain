import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from statistics import median

from . import analysis as an
from . import db as dbm
from .parse import parse_iso, parse_log


def _now(s):
    return parse_iso(s) if s else datetime.now(timezone.utc)


def _read(path):
    return Path(path).read_text(encoding="utf-8", errors="replace")


def _run_id(path):
    return Path(path).stem


def cmd_ingest(a):
    p = parse_log(_read(a.log))
    rid = a.run_id or _run_id(a.log)
    c = dbm.connect(a.db)
    if dbm.run_exists(c, rid) and not a.replace:
        print(f"refused: run {rid} is already in {a.db} (append-only); use --replace only for a still-running run", file=sys.stderr)
        return 2
    if dbm.run_exists(c, rid):
        old = c.execute("SELECT outcome FROM runs WHERE id=?", (rid,)).fetchone()[0]
        if old != "running" and old != "stopped":
            print(f"refused: run {rid} is {old}, only a running or stopped run can be replaced", file=sys.stderr)
            return 2
    outcome = dbm.classify(p, _now(a.now), a.stale_min)
    dbm.insert_run(c, rid, p, outcome, a.host, replace=a.replace)
    reds = sum(1 for s in p.steps if s.exit_code not in (None, 0))
    print(f"ingested {rid}: {outcome}, {len(p.steps)} steps, {reds} red")
    return 0


def _load(a):
    """Return (run_id, meta, steps) from --run (ledger) or a log path."""
    if getattr(a, "run", None):
        c = dbm.connect(a.db)
        row = c.execute("SELECT outcome, commit_sha, host FROM runs WHERE id=?", (a.run,)).fetchone()
        if not row:
            raise SystemExit(f"no run {a.run} in {a.db}")
        return a.run, {"outcome": row[0], "commit": row[1], "host": row[2]}, dbm.load_steps(c, a.run)
    p = parse_log(_read(a.log))
    outcome = dbm.classify(p, _now(getattr(a, "now", None)), 30)
    return _run_id(a.log), {"outcome": outcome, "commit": p.commit, "host": p.host}, p.steps


def _need(a):
    if not (a.log or getattr(a, "run", None)):
        raise SystemExit("give a log path or --run ID")


def cmd_report(a):
    if not a.run and not a.log:
        c = dbm.connect(a.db)
        r = c.execute("SELECT id FROM runs ORDER BY started_at DESC LIMIT 1").fetchone()
        if not r:
            raise SystemExit("ledger is empty")
        a.run = r[0]
    rid, meta, steps = _load(a)
    print(an.report_text(rid, meta, steps))
    return 0


def cmd_trace(a):
    _need(a)
    _, _, steps = _load(a)
    print(json.dumps(an.trace_events(steps)))
    return 0


def _eta_inputs(a, log):
    p = parse_log(_read(log))
    rid = _run_id(log)
    c = dbm.connect(a.db)
    hist = dbm.history_durations(c, exclude_run=rid)
    ref = p.last_ts or _now(a.now)
    return p, hist, ref


def cmd_eta(a):
    p, hist, ref = _eta_inputs(a, a.log)
    if not p.steps:
        raise SystemExit("no steps in log")
    pred = an.predict(p.steps, hist, ref)
    print(an.eta_text(pred, ref, sum(1 for s in p.steps if s.finished)))
    if p.gates_done:
        print("note: this run already printed GATES_DONE")
    return 0


def cmd_next_tick(a):
    log = a.log
    if not log:
        raise SystemExit("--log is required (the running chain's log)")
    p, hist, ref = _eta_inputs(a, log)
    now = _now(a.now)
    rem = None
    if hist and not p.gates_done:
        rem = max(0.0, (an.predict(p.steps, hist, ref)["eta"] - now).total_seconds())
    minutes, why = an.next_tick(p.first_ts, p.last_ts, [t for _, _, t in p.red_now], rem, now, a.min, a.max)
    print(f"{minutes} min - {why}")
    return 0


def cmd_flaky(a):
    c = dbm.connect(a.db)
    rows = c.execute("SELECT name, commit_sha, green_runs, red_runs FROM flaky_steps ORDER BY name").fetchall()
    if not rows:
        print("no flaky steps")
        return 0
    print("step  commit  green_runs  red_runs")
    for r in rows:
        print("  ".join(str(x) for x in r))
    return 0


def build():
    ap = argparse.ArgumentParser(prog="gatetrain")
    sub = ap.add_subparsers(dest="cmd", required=True)

    def db(p):
        p.add_argument("--db", default="gatetrain.db")

    p = sub.add_parser("ingest"); p.add_argument("log"); db(p)
    p.add_argument("--run-id"); p.add_argument("--host"); p.add_argument("--replace", action="store_true")
    p.add_argument("--stale-min", type=float, default=30); p.add_argument("--now"); p.set_defaults(f=cmd_ingest)
    p = sub.add_parser("report"); p.add_argument("log", nargs="?"); db(p); p.add_argument("--run"); p.add_argument("--now")
    p.set_defaults(f=cmd_report)
    p = sub.add_parser("trace"); p.add_argument("log", nargs="?"); db(p); p.add_argument("--run"); p.add_argument("--now")
    p.set_defaults(f=cmd_trace)
    p = sub.add_parser("eta"); p.add_argument("log"); db(p); p.add_argument("--now"); p.set_defaults(f=cmd_eta)
    p = sub.add_parser("next-tick"); p.add_argument("--log"); db(p); p.add_argument("--now")
    p.add_argument("--min", type=int, default=5); p.add_argument("--max", type=int, default=60); p.set_defaults(f=cmd_next_tick)
    p = sub.add_parser("flaky"); db(p); p.set_defaults(f=cmd_flaky)
    return ap


def main(argv=None):
    a = build().parse_args(argv)
    try:
        return a.f(a)
    finally:
        dbm.close_all()
