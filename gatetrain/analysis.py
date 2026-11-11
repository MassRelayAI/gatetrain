"""Report, trace, eta and tick rules. Pure functions over parsed steps."""
from datetime import datetime, timedelta
from statistics import median, quantiles
from typing import Dict, List, Optional, Tuple

from .parse import Step


def timed(steps: List[Step]) -> List[Step]:
    """Steps with a known start and end."""
    return [s for s in steps if s.start is not None and s.end is not None]


def concurrency(steps: List[Step]) -> Tuple[int, Optional[datetime], Optional[datetime], float]:
    """(max concurrency, start, end of the first stretch at that level, average concurrency)."""
    ev = []
    for s in timed(steps):
        if s.duration > 0:
            ev.append((s.start, 1))
            ev.append((s.end, -1))
    if not ev:
        return 0, None, None, 0.0
    ev.sort(key=lambda e: (e[0], e[1]))  # ends before starts at the same instant
    cur = best = 0
    best_start = best_end = None
    busy = 0.0
    prev = ev[0][0]
    for t, d in ev:
        busy += cur * (t - prev).total_seconds()
        if cur == best and best_start is not None and best_end is None and d == -1:
            best_end = t
        prev = t
        cur += d
        if cur > best:
            best, best_start, best_end = cur, t, None
    span = (ev[-1][0] - ev[0][0]).total_seconds()
    return best, best_start, best_end, (busy / span if span > 0 else 0.0)


def fmt_dur(sec: float) -> str:
    sec = int(round(sec))
    h, r = divmod(sec, 3600)
    m, s = divmod(r, 60)
    return f"{h}h{m:02d}m{s:02d}s" if h else (f"{m}m{s:02d}s" if m else f"{s}s")


def report_text(run_id: str, meta: dict, steps: List[Step]) -> str:
    t = timed(steps)
    reds = [s for s in steps if s.exit_code not in (None, 0)]
    first = min((s.start for s in t), default=None)
    last = max((s.end for s in t), default=None)
    wall = (last - first).total_seconds() if first and last else 0
    busy = sum(s.duration or 0 for s in steps)
    unknown = [s for s in steps if s.start is None]
    mc, cs, ce, avg = concurrency(steps)
    L = [f"run {run_id}: {meta.get('outcome')}  commit {meta.get('commit')}  host {meta.get('host')}",
         f"steps {len(steps)}  finished {sum(1 for s in steps if s.finished)}  red {len(reds)}  "
         f"cached {sum(1 for s in steps if s.cached)}  unknown start {len(unknown)}",
         f"wall time {fmt_dur(wall)}  busy time {fmt_dur(busy)}  average parallelism {avg:.1f}"]
    L.append("reds: " + (", ".join(f"{s.name}(rc={s.exit_code})" for s in reds) if reds else "none"))
    L.append("10 slowest steps:")
    for s in sorted(steps, key=lambda s: -(s.duration or 0))[:10]:
        L.append(f"  {fmt_dur(s.duration or 0):>9}  {s.name}")
    cached = [s.name for s in steps if s.cached]
    L.append("steps cached: " + (", ".join(cached) if cached else "none"))
    L.append("critical path (approximation, steps overlap):")
    if mc:
        L.append(f"  busiest stretch: {mc} steps at once from {cs.isoformat()}" +
                 (f" to {ce.isoformat()}" if ce else ""))
        ends = sorted(t, key=lambda s: s.end, reverse=True)[:5]
        L.append("  steps that end last: " + ", ".join(f"{s.name} ({fmt_dur(s.duration)}, ends {s.end.strftime('%H:%M:%S')})" for s in ends))
        L.append("  longest single steps bound the wall time from below: " + fmt_dur(max(s.duration for s in t)))
    else:
        L.append("  no timed steps")
    return "\n".join(L)


def trace_events(steps: List[Step], reds_only_names=None) -> List[dict]:
    t = sorted(timed(steps), key=lambda s: (s.start, s.end))
    if not t:
        return []
    origin = t[0].start
    lane_end: List[int] = []
    out = []
    for s in t:
        ts = int((s.start - origin).total_seconds() * 1_000_000)
        dur = max(int(s.duration * 1_000_000), 1)
        for i, e in enumerate(lane_end):
            if e <= ts:
                lane_end[i] = ts + dur
                tid = i
                break
        else:
            lane_end.append(ts + dur)
            tid = len(lane_end) - 1
        red = s.exit_code not in (None, 0)
        out.append({"name": s.name, "ph": "X", "ts": ts, "dur": dur, "pid": 1, "tid": tid,
                    "cname": "terrible" if red else "good",
                    "args": {"exit": s.exit_code, "cached": s.cached, "start": s.start.isoformat()}})
    return out


# ---- prediction -----------------------------------------------------------

def _prefix(name: str) -> str:
    return name.split(":", 1)[0] if ":" in name else ""


def step_medians(hist: Dict[str, List[float]]) -> Dict[str, float]:
    return {k: median(v) for k, v in hist.items() if v}


def estimate_step(name: str, med: Dict[str, float]) -> Tuple[float, str]:
    if name in med:
        return med[name], "own history"
    p = _prefix(name)
    if p:
        same = [v for k, v in med.items() if _prefix(k) == p]
        if same:
            return median(same), f"prefix {p}:"
    return median(med.values()), "overall median"


def predict(steps: List[Step], hist: Dict[str, List[float]], ref: datetime):
    """Return dict with eta, band, dominant, mode. ref = the last timestamp seen in the log."""
    done = {s.name for s in steps if s.finished and s.exit_code is not None}
    inprog = [s for s in steps if s.name not in done]
    med = step_medians(hist)
    t = timed(steps)
    mc, _, _, avg = concurrency(steps)
    par = max(1.0, avg)
    first = min((s.start for s in t), default=ref)
    span_min = max((ref - first).total_seconds() / 60, 1e-9)
    rate = len(t) / span_min  # steps finished per minute
    if not med:
        # no history: only this run's own pace
        own = [s.duration for s in t if s.duration is not None]
        if not own or not inprog:
            return {"mode": "rough", "eta": ref, "lo": ref, "hi": ref, "dominant": [],
                    "par": par, "mc": mc, "rate": rate, "remaining": len(inprog)}
        d = median(own)
        rem = [(s.name, d) for s in inprog]
        wall = max(d, sum(x for _, x in rem) / par)
        return {"mode": "rough", "eta": ref + timedelta(seconds=wall), "lo": ref + timedelta(seconds=wall * 0.5),
                "hi": ref + timedelta(seconds=wall * 2), "dominant": sorted(rem, key=lambda x: -x[1])[:5],
                "par": par, "mc": mc, "rate": rate, "remaining": len(inprog)}
    names = [s.name for s in inprog] + [n for n in med if n not in done and n not in {s.name for s in inprog}]
    rem = []
    for n in names:
        d, _ = estimate_step(n, med)
        # a step already running has used some of its time; unknown here, so keep the full estimate
        rem.append((n, d))
    total = sum(d for _, d in rem)
    longest = max((d for _, d in rem), default=0)
    wall = max(longest, total / par) if rem else 0
    vals = list(med.values())
    if len(vals) >= 4:
        q = quantiles(vals, n=4)
        m = median(vals)
        lo_f, hi_f = (q[0] / m if m else 1), (q[2] / m if m else 1)
    else:
        lo_f, hi_f = 0.75, 1.5
    lo_wall = max(longest * lo_f, total * lo_f / par) if rem else 0
    hi_wall = max(longest * hi_f, total * hi_f / par) if rem else 0
    return {"mode": "history", "eta": ref + timedelta(seconds=wall), "lo": ref + timedelta(seconds=lo_wall),
            "hi": ref + timedelta(seconds=hi_wall), "dominant": sorted(rem, key=lambda x: -x[1])[:5],
            "par": par, "mc": mc, "rate": rate, "remaining": len(rem)}


def eta_text(pred: dict, ref: datetime, finished: int) -> str:
    L = []
    if pred["mode"] == "rough":
        L.append("no history - rough estimate (this run's own pace only)")
    L.append(f"eta {pred['eta'].isoformat()}")
    L.append(f"band p25-p75: {pred['lo'].isoformat()} .. {pred['hi'].isoformat()}")
    L.append(f"basis: {finished} steps finished at {pred['rate']:.1f}/min, parallelism {pred['par']:.1f} "
             f"(max seen {pred['mc']}), {pred['remaining']} steps remaining, last log line {ref.isoformat()}")
    if pred["dominant"]:
        L.append("dominant remaining steps: " + ", ".join(f"{n} (~{fmt_dur(d)})" for n, d in pred["dominant"]))
    else:
        L.append("dominant remaining steps: none known")
    return "\n".join(L)


# ---- cadence --------------------------------------------------------------

def next_tick(first: Optional[datetime], last: Optional[datetime], reds: List[datetime],
              remaining_sec: Optional[float], now: datetime, lo: int, hi: int) -> Tuple[int, str]:
    clamp = lambda m: int(max(lo, min(hi, round(m))))
    if any((now - t).total_seconds() <= 5 * 60 for t in reds if t):
        return clamp(lo), "a red appeared in the last 5 minutes: look again at the minimum"
    elapsed = ((now - first).total_seconds() / 60) if first else 0
    if elapsed < 10:
        return clamp(max(lo, 5)), "run just started: catch early reds"
    if remaining_sec is not None:
        rem = remaining_sec / 60
        if rem <= 15:
            return clamp(lo), f"predicted end in about {rem:.0f} min: stay close"
        m = clamp(rem / 3)
        if reds:
            m = min(m, 30)
        return m, f"mid-run, about {rem:.0f} min predicted left: a third of it" + ("; capped at 30 because reds exist" if reds else "")
    m = clamp(elapsed / 3)
    return (min(m, 30) if reds else m), "no history: a third of the elapsed time"
