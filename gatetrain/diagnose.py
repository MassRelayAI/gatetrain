"""gatetrain diagnose: classify every red step of a chain log with evidence, a remedy and whether a full rerun is needed.

The classifier is DATA: RULES is a table of dicts, first match wins, so a new failure class is a new row (or a row in a JSON
file given with --rules), never new code. Keys per rule:
  id        short name of the rule
  cls       the class printed (flake-load, infra-port, ...)
  pattern   regex searched in the step's output (multiline)
  absent    optional regex; when it matches anywhere in the output, the rule does not apply (a real assertion beats a flake)
  also      optional regex that must ALSO match somewhere in the output
  when      optional condition on the step: "slow" = duration far above the step's median
  shrink    optional true: the pattern's groups 1 and 2 are (expected, got) and the rule applies only when got < expected
  remedy    what to do
  rerun     true when the red needs only a rerun (plus the remedy), false when a lane fix comes first
A red that no rule matches is `real`: never silently a flake.
"""
import json
import re
from dataclasses import dataclass
from statistics import median
from typing import Dict, List, Optional

from .analysis import concurrency, fmt_dur
from .parse import parse_log

SLOW_FACTOR = 3.0

RULES = [
    {"id": "port-collision", "cls": "infra-port",
     "pattern": r"failed to bind host port|address already in use|EADDRINUSE|port is already allocated",
     "remedy": "a gate's port was taken: keep random port ranges below the ephemeral range (for example 20000-31999) or inside "
               "ip_local_reserved_ports, and give every gate a unique fixed port; run `gatetrain doctor`",
     "rerun": True},
    {"id": "windows-artefact", "cls": "platform-windows",
     "pattern": r"The command line is too long|path separator|"
                r"expected ['\"][^'\"\n]*/[^'\"\n]*['\"],? (?:got|received) ['\"][^'\"\n]*\\[^'\"\n]*['\"]|"
                r"expected ['\"][^'\"\n]*\\[^'\"\n]*['\"],? (?:got|received) ['\"][^'\"\n]*/[^'\"\n]*['\"]",
     "remedy": "this red only happens on Windows (long command line, backslash vs slash in an allow-list): run that step on Linux "
               "for the merge decision",
     "rerun": True},
    {"id": "generated-not-committed", "cls": "generated-uncommitted",
     "pattern": r"(?i)\bmissing:|\bstray\b",
     "also": r"(?i)on[- ]disk|disk (?:equals|==|vs|matches)|differs from (?:the )?build",
     "remedy": "a generator made a NEW file that was never committed (the stack committed tracked changes only): regenerate, "
               "`git add` the new file by name, commit on the batch; `gatetrain generated` lists such files before the chain",
     "rerun": True},
    {"id": "baseline-shrink-count", "cls": "baseline-shrink",
     "pattern": r"expected length (\d+),? (?:got|received) (\d+)", "shrink": True,
     "remedy": "a shrink-only list got shorter because a lane fixed something: delete the entry and lower the pinned count "
               "in the same commit",
     "rerun": True},
    {"id": "baseline-shrink-entry", "cls": "baseline-shrink",
     "pattern": r"(?i)now (?:witnessed|reach(?:es)? the gate)\b.*\bdelete\b|\bdelete (?:the|this) entry\b",
     "remedy": "a shrink-only list still names something that is now fixed: delete the entry and lower the pinned count "
               "in the same commit",
     "rerun": True},
    {"id": "baseline-missing", "cls": "baseline-missing",
     "pattern": r"\bNEW\b.*\bnot in\b|nobody classified",
     "remedy": "a new route or table is not yet on a list it must join: add it with a reason, in the same commit",
     "rerun": True},
    {"id": "load-flake", "cls": "flake-load",
     "pattern": r"(?i)(?:hook|test) timed out in \d+ ?m?s|timed out after \d+ ?m?s",
     "absent": r"AssertionError|^\s*Tests\s.*?(?<!\d)[1-9]\d* failed", "when": "slow",
     "remedy": "a timeout with no failed test on a slow, loaded step: raise the hook or test timeout for the slow engine, or reduce "
               "the step's parallel neighbours. Under a zero-red merge rule it still needs a full green rerun",
     "rerun": True},
]

REAL_REMEDY = "a real failure until shown otherwise: read the evidence, write a failing test, fix it on the lane"


@dataclass
class Finding:
    step: str
    rc: Optional[int]
    cls: str
    evidence: str
    remedy: str
    rerun: bool
    rule: str = ""
    block: str = ""

    def as_dict(self):
        return self.__dict__.copy()


def load_rules(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def step_output(text: str) -> Dict[str, List[str]]:
    """Lines between a `=== <step>` banner and the next marker line, per step (a banner names its step)."""
    out, cur = {}, None
    for line in text.splitlines():
        if line.startswith("=== "):
            cur = line[4:].strip()
            out.setdefault(cur, [])
        elif re.match(r"(STEP_|RED_|GATES_DONE|CHAIN_|branch )", line):
            if not line.startswith("STEP_START"):
                continue
            cur = None
        elif cur is not None:
            out[cur].append(line.rstrip())
    return out


def _is_slow(step, steps, history):
    ref = list((history or {}).get(step.name, []))
    if not ref:
        ref = [s.duration for s in steps if s is not step and s.duration is not None and s.exit_code in (0, None)]
    m = median(ref) if ref else 0
    return bool(step.duration and m > 0 and step.duration > SLOW_FACTOR * m)


_LIST_HEAD = re.compile(r"^\s*FAIL\s+\d+\s+problems?(?:\(s\))?\s*:")
_BULLET = re.compile(r"^\s+(?:[-*•]|\d+[.)])\s+\S")
_START = re.compile(r"^\s*(?:FAIL(?:ED)?\s+(?!\d+\s+problem)\S|✖\s|not ok\s|---- .+ stdout ----\s*$|thread '.*' panicked at)")
_END = re.compile(r"^\s*(?:⎯{3,}|Test Files\s|Tests\s|Duration\s|test result:|failures:|✔\s|✓\s|ok \d+)")
_TAIL = 30


def failure_blocks(lines):
    """Split a red step's output into FAILURE BLOCKS: vitest `FAIL  file > test`, node:test `✖ name` / `not ok`, cargo
    `---- test stdout ----` / `panicked at`, and the items of a `FAIL  N problem(s):` list. Text outside a block (passing-test
    stderr, logs) belongs to no block and can explain nothing. Output with no recognisable block is one block: its last lines."""
    blocks, cur, in_list = [], None, False
    for ln in lines:
        if _LIST_HEAD.match(ln):
            cur, in_list = None, True
        elif in_list and _BULLET.match(ln):
            cur = [ln]
            blocks.append(cur)
        elif in_list and cur is not None and ln.startswith((" ", chr(9))) and ln.strip():
            cur.append(ln)
        elif _START.match(ln) and not (cur is not None and ln.lstrip().startswith("thread ") and cur[0].startswith("----")):
            cur, in_list = [ln], False
            blocks.append(cur)
        elif _END.match(ln):
            cur, in_list = None, False
        elif cur is not None:
            cur.append(ln)
        elif in_list and not ln.strip():
            in_list = False
    if not blocks:
        tail = [x for x in lines if x.strip()][-_TAIL:]
        return [tail] if tail else []
    return blocks


def _line_of(text, pos):
    a = text.rfind(chr(10), 0, pos) + 1
    b = text.find(chr(10), pos)
    return text[a:b if b >= 0 else len(text)].strip()


def _match(body, r):
    m = re.search(r["pattern"], body, re.M)
    if not m:
        return None
    if r.get("absent") and re.search(r["absent"], body, re.M):
        return None
    if r.get("also") and not re.search(r["also"], body, re.M):
        return None
    if r.get("shrink") and not (m.lastindex and m.lastindex >= 2 and int(m[2]) < int(m[1])):
        return None
    return m


def classify(step, lines, steps, history=None, rules=None) -> List[Finding]:
    """One finding per failure block. A rule applies inside ONE block (pattern, also and absent all within it). A step
    whose every block is a timeout, with no assertion in any block, on a slow step, is one `flake-load` finding. A block no rule
    explains is `real`: a failed assertion is never excused."""
    rules = RULES if rules is None else rules
    blocks = failure_blocks(lines)
    out, timeouts = [], []
    for blk in blocks:
        body = chr(10).join(blk)
        hit = None
        for r in rules:
            m = _match(body, r)
            if m:
                hit = (r, m)
                break
        title = next((x.strip() for x in blk if x.strip()), "")[:100]
        if hit and hit[0].get("when") == "slow":
            timeouts.append((blk, hit))
            out.append(None)
        elif hit:
            r, m = hit
            out.append(Finding(step.name, step.exit_code, r["cls"], _line_of(body, m.start()), r["remedy"], bool(r["rerun"]), r.get("id", ""), title))
        else:
            ev = next((x.strip() for x in blk if re.search(r"Error|assert|panick|expected|FAIL|fail", x)), title) or "(no output captured)"
            out.append(Finding(step.name, step.exit_code, "real", ev, REAL_REMEDY, False, "default", title))
    if timeouts:
        every = len(timeouts) == len(blocks)
        if every and _is_slow(step, steps, history):
            r, m = timeouts[0][1]
            body = chr(10).join(timeouts[0][0])
            fl = Finding(step.name, step.exit_code, r["cls"], _line_of(body, m.start()), r["remedy"], bool(r["rerun"]), r.get("id", ""),
                         f"{len(timeouts)} timeout block(s)")
            return [fl]
        for k, o in enumerate(out):
            if o is None:
                blk = blocks[k]
                ev = next((x.strip() for x in blk if re.search(r"timed out", x, re.I)), blk[0].strip())
                why = ("a timeout next to a failed assertion is not a load flake" if not every else
                       "a timeout with no load evidence (the step is not far above its median)")
                out[k] = Finding(step.name, step.exit_code, "real", ev, REAL_REMEDY + "; " + why, False, "default", blk[0].strip()[:100])
    return [o for o in out if o is not None] or [Finding(step.name, step.exit_code, "real", "(no output captured for this step)", REAL_REMEDY, False, "default")]


def diagnose_text(text: str, history=None, rules=None) -> List[Finding]:
    p = parse_log(text)
    outs = step_output(text)
    res = []
    for s in p.steps:
        if s.exit_code not in (None, 0):
            res += classify(s, outs.get(s.name, []), p.steps, history, rules)
    return res


def recorded_parallelism(p):
    """(value, source): the CHAIN_PARALLEL marker when the log has one, else the highest concurrency seen."""
    if p.parallel is not None:
        return p.parallel, "recorded"
    mc = concurrency(p.steps)[0]
    return (mc, "observed") if mc else (None, "")


def parallelism_note(p, configured) -> Optional[str]:
    """A run whose parallelism is below the host's configured one is slow for a reason that is not the code."""
    if not configured:
        return None
    got, how = recorded_parallelism(p)
    if got is None or got >= configured:
        return None
    return (f"note: {how} parallelism {got} is below the configured {configured}: this run is slower than the host can do "
            f"(expect about {configured / got:.1f}x the wall time on its parallel stretch)")


def _group(cls):
    if cls == "real":
        return "real"
    if cls.startswith("baseline") or cls == "generated-uncommitted":
        return "baseline"
    return "flake"


def format_findings(findings: List[Finding]) -> str:
    if not findings:
        return "no red steps"
    L, steps = [], {}
    for f in findings:
        steps.setdefault(f.step, []).append(f)
    for name, fs in steps.items():
        verdict = "real" if any(f.cls == "real" for f in fs) else ", ".join(sorted({f.cls for f in fs}))
        L.append(f"{name}: {verdict} (rc={fs[0].rc}, {len(fs)} block(s))")
        for f in fs:
            L += [f"  [{f.cls}] {f.block}" if f.block else f"  [{f.cls}]", f"    evidence: {f.evidence}", f"    remedy: {f.remedy}",
                  "    full rerun needed: " + ("yes (no lane defect found; a green run under the zero-red merge rule is still required)"
                                               if f.rerun else "after the fix, the lane's defect comes first")]
    c = {"real": 0, "flake": 0, "baseline": 0}
    for f in findings:
        c[_group(f.cls)] += 1
    sv = {"real": 0, "flake": 0, "baseline": 0}
    for fs in steps.values():
        sv["real" if any(f.cls == "real" for f in fs) else _group(fs[0].cls)] += 1
    L.append(f"{len(steps)} red steps: {sv['real']} real step(s), {sv['flake']} flake or infrastructure, {sv['baseline']} baseline")
    L.append(f"{len(findings)} blocks: {c['real']} real, {c['flake']} flake or infrastructure, {c['baseline']} baseline")
    return chr(10).join(L)


def diagnosis_block(text, parallel=None, history=None, rules=None) -> str:
    p = parse_log(text)
    block = "diagnosis:\n" + "\n".join("  " + ln for ln in format_findings(diagnose_text(text, history, rules)).splitlines())
    note = parallelism_note(p, parallel)
    return block + ("\n" + note if note else "")
