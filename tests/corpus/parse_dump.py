"""Reference output of the parse corpus: what gatetrain/parse.py reads, as JSON.

`python tests/corpus/parse_dump.py <log>` prints the same bytes as `gatetrain parse <log> --json`
(sorted keys, no whitespace, one trailing newline). The Rust parser must match it on every
tests/corpus/parse/*/input.log; tests/corpus/parse/*/expected-stdout is this script's output.
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from gatetrain.parse import parse_log  # noqa: E402


def dump(text: str) -> str:
    p = parse_log(text)
    iso = lambda t: t.isoformat() if t else None  # noqa: E731
    d = {
        "branch": p.branch, "commit": p.commit, "host": p.host, "mode": p.mode, "parallel": p.parallel,
        "gates_done": p.gates_done, "red_summary": p.red_summary,
        "first_ts": iso(p.first_ts), "last_ts": iso(p.last_ts),
        "steps": [{"name": s.name, "start": iso(s.start), "end": iso(s.end), "duration": s.duration,
                   "exit_code": s.exit_code, "cached": s.cached} for s in p.steps],
        "red_now": [{"step": n, "rc": rc, "at": iso(at)} for n, rc, at in p.red_now],
    }
    return json.dumps(d, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"


if __name__ == "__main__":
    # same decoding as cli._read: utf-8 with replacement
    sys.stdout.buffer.write(dump(Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")).encode("utf-8"))
