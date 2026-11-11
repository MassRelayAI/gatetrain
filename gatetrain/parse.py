"""Parse a chain log (see docs/log-format.md) into steps and run facts."""
import re
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import List, Optional, Tuple


@dataclass
class Step:
    name: str
    start: Optional[datetime] = None
    duration: Optional[float] = None  # seconds
    exit_code: Optional[int] = None
    cached: bool = False

    @property
    def end(self) -> Optional[datetime]:
        if self.start is None or self.duration is None:
            return None
        return self.start + timedelta(seconds=self.duration)

    @property
    def finished(self) -> bool:
        return self.exit_code is not None or self.duration is not None


@dataclass
class ParsedLog:
    branch: Optional[str] = None
    commit: Optional[str] = None
    host: Optional[str] = None
    mode: Optional[str] = None
    steps: List[Step] = field(default_factory=list)
    red_now: List[Tuple[str, int, Optional[datetime]]] = field(default_factory=list)
    red_summary: Optional[str] = None
    gates_done: bool = False

    @property
    def last_ts(self) -> Optional[datetime]:
        ts = []
        for s in self.steps:
            if s.start:
                ts.append(s.start)
            if s.end:
                ts.append(s.end)
        ts += [t for _, _, t in self.red_now if t]
        return max(ts) if ts else None

    @property
    def first_ts(self) -> Optional[datetime]:
        ts = [s.start for s in self.steps if s.start]
        return min(ts) if ts else None


def parse_iso(s: str) -> datetime:
    return datetime.fromisoformat(s.strip())


_BRANCH = re.compile(r"^branch (\S+) at (\S+) on (\S+)")
_START = re.compile(r"^STEP_START (\S+) (\S+)")
_TIME = re.compile(r"^STEP_TIME (\S+) (\d+(?:\.\d+)?)s")
_EXIT = re.compile(r"^STEP_EXIT (\S+) (-?\d+)")
_CACHED = re.compile(r"^STEP_CACHED (\S+)")
_RED = re.compile(r"^RED_NOW (\S+) rc=(-?\d+)(?: at (\S+))?")
_SUMMARY = re.compile(r"^RED_SUMMARY:\s*(.*)")


def parse_log(text: str) -> ParsedLog:
    r = ParsedLog()
    by = {}

    def step(name):
        if name not in by:
            by[name] = Step(name)
            r.steps.append(by[name])
        return by[name]

    for line in text.splitlines():
        line = line.rstrip()
        if line.startswith("STEP_START"):
            m = _START.match(line)
            if m:
                step(m[1]).start = parse_iso(m[2])
        elif line.startswith("STEP_TIME"):
            m = _TIME.match(line)
            if m:
                step(m[1]).duration = float(m[2])
        elif line.startswith("STEP_EXIT"):
            m = _EXIT.match(line)
            if m:
                step(m[1]).exit_code = int(m[2])
        elif line.startswith("STEP_CACHED"):
            m = _CACHED.match(line)
            if m:
                step(m[1]).cached = True
        elif line.startswith("RED_NOW"):
            m = _RED.match(line)
            if m:
                r.red_now.append((m[1], int(m[2]), parse_iso(m[3]) if m[3] else None))
        elif line.startswith("RED_SUMMARY"):
            m = _SUMMARY.match(line)
            r.red_summary = m[1].strip() if m else ""
        elif line.startswith("GATES_DONE"):
            r.gates_done = True
        elif line.startswith("CHAIN_MODE"):
            parts = line.split(None, 1)
            r.mode = parts[1].strip() if len(parts) > 1 else None
        elif line.startswith("branch ") and r.branch is None:
            m = _BRANCH.match(line)
            if m:
                r.branch, r.commit, r.host = m[1], m[2], m[3]
    return r
