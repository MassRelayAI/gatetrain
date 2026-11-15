"""Reference output of the analysis corpus: what the Python `report`, `trace` and `eta` print.

    python tests/corpus/analysis_dump.py <case-dir> report|trace|eta|eta-history

`<case-dir>` holds `input.log`. `eta-history` first ingests the shared history (the parse corpus's logs that
finished, run ids `hist-<case>`) into a throwaway ledger, as `gatetrain ingest` would. The Rust binary must print
the same bytes for the same inputs; tests/corpus/analysis/*/expected-<kind> is this script's output.
"""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
NOW = "2026-11-12T00:00:00Z"  # fixed so `running` vs `stopped` and the eta fallback do not depend on the clock


def history_logs():
    return sorted((ROOT / "tests" / "corpus" / "parse").glob("*/input.log"))


def build_history(db: str) -> None:
    for log in history_logs():
        subprocess.run([sys.executable, "-m", "gatetrain", "ingest", str(log), "--db", db, "--run-id", f"hist-{log.parent.name}",
                        "--now", NOW], cwd=ROOT, check=True, capture_output=True)


def args_for(case: Path, kind: str, db: str):
    log = str(case / "input.log")
    if kind == "report":
        return ["report", log, "--now", NOW]
    if kind == "trace":
        return ["trace", log, "--now", NOW]
    return ["eta", log, "--db", db, "--now", NOW]


def run(case: Path, kind: str) -> bytes:
    with tempfile.TemporaryDirectory() as d:
        db = os.path.join(d, "g.db")
        if kind == "eta-history":
            build_history(db)
        r = subprocess.run([sys.executable, "-m", "gatetrain", *args_for(case, kind, db)], cwd=ROOT, capture_output=True,
                           env={**os.environ, "PYTHONIOENCODING": "utf-8"})  # the same bytes on every platform
        out = r.stdout.replace(bytes([13, 10]), bytes([10]))  # Python's print writes CRLF on Windows; the binary and the committed files use LF
        return out + (b"" if r.returncode == 0 else f"\nEXIT {r.returncode}\n".encode())


if __name__ == "__main__":
    sys.stdout.buffer.write(run(Path(sys.argv[1]), sys.argv[2]))
