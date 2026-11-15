#!/usr/bin/env python3
"""Fail when any file of the repository holds a word on the deny-list.

The deny-list (`.privacy-denylist`, one sha256 hex per line) holds HASHES of lowercase words, never the words. Every tracked file
(and every untracked file git does not ignore) is read, lower-cased and split into words (runs of [a-z0-9]); each word is hashed
and compared. File paths are checked the same way. A hit prints the file and line number, never the word.

    python tools/privacy_check.py            # exit 1 on a hit, 0 when clean
    python tools/privacy_check.py --hash W   # print the line to add for the word W
    python tools/privacy_check.py --root D   # check another repository (its own .privacy-denylist)
"""
import hashlib
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DENYLIST = ".privacy-denylist"
WORD = re.compile(r"[a-z0-9]+")


def sha(word: str) -> str:
    return hashlib.sha256(word.encode("utf-8")).hexdigest()


def load_denylist(root: Path = ROOT) -> set:
    out = set()
    for line in (root / DENYLIST).read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            assert re.fullmatch(r"[0-9a-f]{64}", line), f"{DENYLIST}: not a sha256 hex line"
            out.add(line)
    return out


def files(root: Path = ROOT) -> list:
    r = subprocess.run(["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"], capture_output=True, check=True)
    return [p for p in r.stdout.decode("utf-8", "replace").split("\0") if p]


def scan_text(text: str, deny: set) -> bool:
    return any(sha(w) in deny for w in set(WORD.findall(text.lower())))


def check(root: Path = ROOT, deny: set = None) -> list:
    """Return `file[:line]` of every hit (no word is ever returned)."""
    deny = load_denylist(root) if deny is None else deny
    hits = []
    for rel in files(root):
        if rel == DENYLIST:
            continue
        if scan_text(rel, deny):
            hits.append(f"{rel} (path)")
            continue
        p = root / rel
        if not p.is_file():
            continue
        data = p.read_bytes()
        text = data.decode("utf-8", "ignore")
        if not scan_text(text, deny):
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if scan_text(line, deny):
                hits.append(f"{rel}:{n}")
    return hits


def main(argv) -> int:
    if len(argv) == 3 and argv[1] == "--hash":
        print(sha(argv[2].strip().lower()))
        return 0
    root = Path(argv[2]) if len(argv) == 3 and argv[1] == "--root" else ROOT
    hits = check(root)
    for h in hits:
        print(f"privacy: deny-listed word in {h}")
    if hits:
        print(f"privacy: {len(hits)} hit(s); replace with a neutral name (builder-1, host-a, /srv/repos)")
        return 1
    print("privacy: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
