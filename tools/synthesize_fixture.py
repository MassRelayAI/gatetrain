"""Turn a real chain log into a publishable sample: generic step names, a fake commit, a generic branch; timings and structure kept.

Usage: python tools/synthesize_fixture.py <in.log> <out.log> [--map map.json]
Steps whose name is already generic (see KEEP) keep it; others become db-gate-NNN, e2e-NNN or step-NNN in order of first appearance.
The same --map file across several logs keeps one step's name identical in all of them (needed for history-based predictions).
"""
import json, re, sys
from pathlib import Path

KEEP = re.compile(r"^(lint-[a-z]+|test-[a-z]+|cargo-test|build-packages|typecheck|check-tracker)$")
STEP_LINE = re.compile(r"^(STEP_START|STEP_EXIT|STEP_TIME|STEP_CACHED|===|RED_NOW) (\S+)(.*)$")

def generic(name, mapping, counters):
    if KEEP.match(name):
        return name
    if name not in mapping:
        kind = "db-gate" if name.endswith("-db") else "e2e" if name.endswith("-e2e") else "step"
        counters[kind] = counters.get(kind, 0) + 1
        mapping[name] = f"{kind}-{counters[kind]:03d}"
    return mapping[name]

def main(src, dst, map_path=None):
    state = json.loads(Path(map_path).read_text()) if map_path and Path(map_path).exists() else {"map": {}, "counters": {}}
    mapping, counters = state["map"], state["counters"]
    out = []
    for line in Path(src).read_text(encoding="utf-8").splitlines():
        if line.startswith("branch "):
            line = "branch batch-a at 0a1b2c3d4 on builder-1"
        elif line.startswith("RED_SUMMARY:"):
            body = line.split(":", 1)[1].strip()
            if body != "none":
                body = " ".join(re.sub(r"^([^()]+)", lambda m: generic(m.group(1), mapping, counters), part) for part in body.split())
            line = f"RED_SUMMARY: {body}"
        else:
            m = STEP_LINE.match(line)
            if m:
                line = f"{m.group(1)} {generic(m.group(2), mapping, counters)}{m.group(3)}"
        out.append(line)
    Path(dst).write_text("\n".join(out) + "\n", encoding="utf-8")
    if map_path:
        Path(map_path).write_text(json.dumps({"map": mapping, "counters": counters}, indent=1))

if __name__ == "__main__":
    a = sys.argv[1:]
    mp = a[a.index("--map") + 1] if "--map" in a else None
    main(a[0], a[1], mp)
