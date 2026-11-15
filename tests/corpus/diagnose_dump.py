"""Reference output of the diagnose corpus: what `python -m gatetrain diagnose` prints for tests/corpus/diagnose/<case>/input.log.

`python tests/corpus/diagnose_dump.py <case-dir>` prints the same bytes as `gatetrain diagnose <case>/input.log [args]` in Rust
(UTF-8, "\n" newlines). A case may hold an `args` file, one argument per line, `@DIR@` standing for the case directory.
tests/corpus/diagnose/*/expected-stdout is this script's output.
"""
import io
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from gatetrain.cli import main  # noqa: E402


def case_args(case: Path):
    f = case / "args"
    extra = [ln.replace("@DIR@", str(case)) for ln in f.read_text(encoding="utf-8").splitlines() if ln] if f.exists() else []
    return ["diagnose", str(case / "input.log")] + extra


if __name__ == "__main__":
    out = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", newline="\n")
    sys.stdout = out
    rc = main(case_args(Path(sys.argv[1])))
    out.flush()
    sys.exit(rc)
