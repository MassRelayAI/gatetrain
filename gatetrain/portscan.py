"""Find RANDOM port defaults in gate scripts: `N + ((process.pid * k + Date.now()) % M)`, `random.randint(a, b)`, `$((N + RANDOM % M))`.
Returns (line, low, high) for each; the doctor compares the range with the host's ephemeral range (L21, L52)."""
import re

_HINT = re.compile(r"pid|random|RANDOM|Date\.now|time\(|randint|randrange", re.I)
_MOD = re.compile(r"(?<![\w.])(\d{4,5})\s*\+\s*[\(\$].*?%\s*(\d{2,5})\b")
_SH = re.compile(r"\$\(\(\s*(\d{4,5})\s*\+\s*\$?RANDOM\s*%\s*(\d{2,5})")
_RAND = re.compile(r"(?:randint|randrange|uniform)\(\s*(\d{4,5})\s*,\s*(\d{4,5})\s*\)")


def scan_random_ports(text):
    out = []
    for n, line in enumerate(text.splitlines(), 1):
        m = _SH.search(line) or _MOD.search(line)
        if m and _HINT.search(line):
            lo = int(m[1])
            out.append((n, lo, lo + int(m[2]) - 1))
            continue
        m = _RAND.search(line)
        if m:
            out.append((n, int(m[1]), int(m[2])))
    return out


def uncovered(lo, hi, ephemeral, reserved):
    """Parts of [lo, hi] inside the ephemeral range that no reserved range covers; [] means safe."""
    a, b = max(lo, ephemeral[0]), min(hi, ephemeral[1])
    if a > b:
        return []
    gaps = [(a, b)]
    for ra, rb in sorted(reserved):
        nxt = []
        for ga, gb in gaps:
            if rb < ga or ra > gb:
                nxt.append((ga, gb))
                continue
            if ra > ga:
                nxt.append((ga, ra - 1))
            if rb < gb:
                nxt.append((rb + 1, gb))
        gaps = nxt
    return gaps
