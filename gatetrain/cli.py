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


def _suggest(a, log, hist=None):
    """(minutes, reason, run id, finished) for one log."""
    p = parse_log(_read(log))
    rid = _run_id(log)
    if hist is None:
        hist = dbm.history_durations(dbm.connect(a.db), exclude_run=rid)
    ref = p.last_ts or _now(a.now)
    now = _now(a.now)
    rem = None
    if hist and not p.gates_done:
        rem = max(0.0, (an.predict(p.steps, hist, ref)["eta"] - now).total_seconds())
    minutes, why = an.next_tick(p.first_ts, p.last_ts, [t for _, _, t in p.red_now], rem, now, a.min, a.max)
    return minutes, why, rid, p.gates_done


def cmd_next_tick(a):
    logs = a.log or []
    if not logs:
        raise SystemExit("--log is required (the running chain's log)")
    if len(logs) == 1:
        minutes, why, _, _ = _suggest(a, logs[0])
        print(f"{minutes} min - {why}")
        return 0
    got = [_suggest(a, lg) for lg in logs]
    live = [g for g in got if not g[3]] or got  # a finished run never decides while another is live
    minutes, why, rid, _ = min(live, key=lambda g: g[0])
    print(f"{minutes} min - {why} (decided by {rid})")
    return 0


def cmd_status(a):
    c = dbm.connect(a.db)
    now = _now(a.now)
    rows = []
    for lg in a.log:
        p = parse_log(_read(lg))
        rid = _run_id(lg)
        outcome = dbm.classify(p, now, a.stale_min)
        if not dbm.run_exists(c, rid):
            dbm.insert_run(c, rid, p, outcome, None)
        elif c.execute("SELECT outcome FROM runs WHERE id=?", (rid,)).fetchone()[0] in ("running", "stopped"):
            dbm.insert_run(c, rid, p, outcome, None, replace=True)
    for lg in a.log:
        p = parse_log(_read(lg))
        rid = _run_id(lg)
        outcome = dbm.classify(p, now, a.stale_min)
        hist = dbm.history_durations(c, exclude_run=rid)
        known = {s.name for s in p.steps} | set(hist)
        done = sum(1 for s in p.steps if s.finished)
        reds = len({s.name for s in p.steps if s.exit_code not in (None, 0)} | {n for n, _, _ in p.red_now})
        ref = p.last_ts or now
        eta = band = "-"
        if p.steps and outcome == "running":
            pr = an.predict(p.steps, hist, ref)
            eta = pr["eta"].strftime("%H:%M")
            band = pr["lo"].strftime("%H:%M") + "-" + pr["hi"].strftime("%H:%M")
        nxt = None
        if outcome == "running":
            m, why, _, _ = _suggest(a, lg, hist)
            nxt = (m, why)
        rows.append((rid, p.host or "-", f"{done}/{len(known)}", str(reds), outcome, eta, band, nxt))
    head = ("run", "host", "steps", "reds", "state", "eta", "band", "next look (min)")
    table = [head] + [r[:7] + (str(r[7][0]) if r[7] else "-",) for r in rows]
    w = [max(len(x[i]) for x in table) for i in range(len(head))]
    for t in table:
        print("  ".join(t[i].ljust(w[i]) for i in range(len(head))).rstrip())
    live = [(r[7][0], r[0], r[7][1]) for r in rows if r[7]]
    if live:
        m, rid, why = min(live, key=lambda x: x[0])
        print(f"next look: {m} min ({rid}: {why})")
    else:
        print("next look: none (no run is active)")
    return 0


def cmd_doctor(a):
    from . import config as cfgm
    from . import doctor as dr
    try:
        cfg = cfgm.load(a.config)
    except (cfgm.ConfigError, OSError) as e:
        print(f"config error: {e}", file=sys.stderr)
        return 2
    res = dr.run_doctor(cfg, a.host)
    if a.json:
        print(json.dumps([r.as_dict() for r in res], indent=2))
    else:
        print(dr.format_text(res))
    return dr.exit_code(res)


def cmd_stop(a):
    from . import config as cfgm
    from . import doctor as dr
    from . import stop as st
    from .runner import LocalRunner
    runner, plat = LocalRunner(), sys.platform
    if a.config and a.host:
        cfg = cfgm.load(a.config)
        h = next((x for x in cfg.hosts if x.name == a.host), None)
        if h is None:
            raise SystemExit(f"no host {a.host} in {a.config}")
        runner, plat = dr.make_runner(h), dr.host_os(h)
        if runner is None:
            raise SystemExit(f"host {a.host} has reach {h.reach!r}; stop needs local or ssh")
    if not str(plat).startswith("linux"):
        print("not supported: gatetrain stop works on Linux only")
        return 2
    return st.stop(runner, a.pattern, a.follower or [], a.yes)


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
    p = sub.add_parser("next-tick"); p.add_argument("--log", action="append"); db(p); p.add_argument("--now")
    p.add_argument("--min", type=int, default=5); p.add_argument("--max", type=int, default=60); p.set_defaults(f=cmd_next_tick)
    p = sub.add_parser("flaky"); db(p); p.set_defaults(f=cmd_flaky)
    p = sub.add_parser("status"); p.add_argument("--log", action="append", required=True); p.add_argument("--db", default=":memory:")
    p.add_argument("--now"); p.add_argument("--stale-min", type=float, default=30)
    p.add_argument("--min", type=int, default=5); p.add_argument("--max", type=int, default=60); p.set_defaults(f=cmd_status)
    p = sub.add_parser("doctor"); p.add_argument("--config", default="hosts.toml"); p.add_argument("--host"); p.add_argument("--json", action="store_true")
    p.set_defaults(f=cmd_doctor)
    p = sub.add_parser("stop"); p.add_argument("pattern"); p.add_argument("--follower", action="append")
    p.add_argument("--yes", action="store_true"); p.add_argument("--config"); p.add_argument("--host"); p.set_defaults(f=cmd_stop)
    return ap


def main(argv=None):
    a = build().parse_args(argv)
    try:
        return a.f(a)
    finally:
        dbm.close_all()
