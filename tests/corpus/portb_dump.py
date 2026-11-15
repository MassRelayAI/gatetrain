"""Reference output of the doctor corpus (GT-PORT-B): what gatetrain/doctor.py prints for a case directory.

A case directory holds `hosts.toml` and `transcript.json`: {"<host name>": {"<exact command>": [rc, "stdout"] | {"raise": "message"}}}.
A command the transcript does not name answers (127, ""), as the FakeRunner of tests/test_ops.py does.

    python tests/corpus/portb_dump.py doctor <case> [--json]   # stdout is what `gatetrain doctor` prints; exit code is the CLI's
The Rust core must print the same bytes (crates/gatetrain-cli/tests/portb.rs); expected-* files in the case are this script's output.
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from gatetrain import config as cfgm  # noqa: E402
from gatetrain import doctor as dr  # noqa: E402


class TranscriptRunner:
    def __init__(self, table):
        self.table = table

    def run(self, cmd, timeout=60):
        e = self.table.get(cmd)
        if e is None:
            return 127, ""
        if isinstance(e, dict):
            raise RuntimeError(e["raise"])
        return e[0], e[1]


def doctor(case, as_json, only=None):
    case = Path(case)
    cfg = cfgm.load(str(case / "hosts.toml"))
    tr = json.loads((case / "transcript.json").read_text(encoding="utf-8"))
    dr.make_runner = lambda h: TranscriptRunner(tr.get(h.name, {})) if h.reach == "local" or h.reach.startswith("ssh ") else None
    res = dr.run_doctor(cfg, only)
    out = json.dumps([r.as_dict() for r in res], indent=2) if as_json else dr.format_text(res)
    sys.stdout.reconfigure(encoding="utf-8", newline="\n")
    print(out)
    return dr.exit_code(res)


if __name__ == "__main__":
    if sys.argv[1] == "doctor":
        sys.exit(doctor(sys.argv[2], "--json" in sys.argv[3:]))
