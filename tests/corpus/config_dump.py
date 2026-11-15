"""Reference output of the config corpus: what gatetrain/config.py reads from a hosts.toml, as JSON.

`python tests/corpus/config_dump.py <hosts.toml>` prints the same bytes as `gatetrain config dump <hosts.toml>`:
the loaded Config as sorted-key, no-whitespace JSON plus a newline; or `ERROR <message>` plus a newline for a
ConfigError. For a TOML syntax error only only `not valid TOML` is kept (no path, no parser detail) ("not valid TOML"),
because tomllib and the Rust toml crate word the detail differently. tests/corpus/config/*/expected-stdout is this
script's output.
"""
import dataclasses
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from gatetrain import config  # noqa: E402


def dump(path: str) -> str:
    try:
        cfg = config.load(path)
    except config.ConfigError as e:
        msg = str(e)
        if "not valid TOML" in msg:
            msg = "not valid TOML"
        return f"ERROR {msg}\n"
    return json.dumps(dataclasses.asdict(cfg), sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n"


if __name__ == "__main__":
    sys.stdout.buffer.write(dump(sys.argv[1]).encode("utf-8"))
