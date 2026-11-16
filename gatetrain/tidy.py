"""`gatetrain check` (read-only inventory) and `gatetrain tidy` (guarded cleanup). Lessons L12, L31, L43-L50.

Everything goes through a Runner (Local / Ssh / a test fake): run(cmd) -> (rc, out), put_file(path, text).
Nothing here uses a bulk prune, a glob, or a recursive force-remove; every removal names one exact path or object.
"""
import fnmatch
import json
import math
import re
import shlex
import subprocess
import time
from dataclasses import dataclass, field, asdict
from typing import List, Optional

from . import doctor as dr
from .config import ConfigError

CARGO_TAG_SIG = "Signature: 8a477f597d28d172789f06886806bc55"
CARGO_TAG = CARGO_TAG_SIG + "\n# This file is a cache directory tag created by cargo.\n# For information about cache directory tags see https://bford.info/cachedir/\n"
CLASSES = ("targets", "worktrees", "containers", "images", "cache", "trash")
DEFAULT_UNATTENDED = ("targets", "containers", "trash")
END = "__GT_END__"
TIMEOUT = "timeout"          # the rc of a command that did not finish in time
GB = 1024 ** 3
SAFE_BRANCH = re.compile(r"^[A-Za-z0-9._][A-Za-z0-9._/-]*$")          # anything else is kept, never removed
SAFE_CONTAINER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
SAFE_IMAGE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:/@-]*$")
WIN_BAD = set('"%^!&|<>') | {chr(c) for c in range(0x2018, 0x2020)}                                              # never interpolated into a Windows command line
RESOLVE_SH = 'p="$1"; rest=""; while [ -n "$p" ] && [ "$p" != "/" ] && [ ! -d "$p" ]; do rest="/${p##*/}$rest"; p="${p%/*}"; [ -z "$p" ] && p="/"; done; d=$(cd -- "$p" 2>/dev/null && pwd -P) || exit 1; out="${d%/}$rest"; printf \'%s\n\' "${out:-/}"'
STAMP = re.compile(r"-(\d{14})$")                                      # the timestamp `move` puts in a recycle-bin name


SEEN = ".gatetrain-seen.json"     # in the recycle bin: when tidy first saw each entry (L71)


class GuardViolation(Exception):
    pass


class ItemRefused(GuardViolation):
    """One item has a bad path or name (a control character, a Windows metacharacter, a `..`, an unsafe branch): refused before anything
    was changed. `apply_plan` reports it and carries on with the next item; every other GuardViolation still stops the run."""


# ---------------------------------------------------------------- config

@dataclass
class TidyCfg:
    main_checkout: Optional[str] = None
    root_checkout: Optional[str] = None
    main_branch: str = "main"
    main_volatile_paths: list = field(default_factory=lambda: [".obsidian/"])   # tools rewrite these in main while tidy runs; they do not count as a change of main
    keep_paths: List[str] = field(default_factory=list)
    keep_lanes: List[str] = field(default_factory=list)
    keep_branches: List[str] = field(default_factory=list)
    targets: List[dict] = field(default_factory=list)       # {path, lane?, budget_gb?}
    target_roots: List[str] = field(default_factory=list)   # each child folder is one lane's target, named after the lane
    worktree_prefix: str = ""
    min_idle_min: float = 60
    orphan_targets: str = "keep"                            # keep | clean (a target whose lane has no branch)
    trash_dir: Optional[str] = None
    trash_days: float = 7
    container_prefix: Optional[str] = None
    image_patterns: List[str] = field(default_factory=list)
    cache_days: float = 0                                   # 0 = never touch the docker build cache
    evidence_dirs: List[str] = field(default_factory=list)  # browser profiles must never sit in these
    unattended: List[str] = field(default_factory=lambda: list(DEFAULT_UNATTENDED))
    log: str = "tidy-log.jsonl"
    scratch_dir: Optional[str] = None
    link_canary: Optional[str] = None
    link_canary_count: Optional[int] = None
    size_timeout_s: float = 30                              # a size probe slower than this is unknown, never a crash
    remove_timeout_s: float = 1800                          # a removal, move or cargo clean gets this long before it is "stopped"
    worktree_min_idle_days: float = 2                       # a worktree with a file changed more recently than this is kept (0 = off)
    idle_walk_depth: float = 12                             # how deep the "newest file inside" walk goes
    link_list_timeout_s: float = 300                        # the link listing (`dir /AL /S /B`, `find -type l`) walks a whole worktree: it gets this long
    item_budget_s: float = 600                              # every probe of ONE item shares this many seconds; an item that runs out is skipped and reported (0 = none)


_LISTS = ("main_volatile_paths", "keep_paths", "keep_lanes", "keep_branches", "target_roots", "image_patterns", "evidence_dirs", "unattended")
_STRS = ("main_checkout", "root_checkout", "main_branch", "worktree_prefix", "orphan_targets", "trash_dir",
         "container_prefix", "log", "scratch_dir", "link_canary")
_NUMS = ("min_idle_min", "trash_days", "cache_days", "size_timeout_s", "remove_timeout_s", "worktree_min_idle_days", "idle_walk_depth", "link_list_timeout_s", "item_budget_s")


def build_cfg(cfg, host) -> TidyCfg:
    d = dict(cfg.tidy)
    d.update(host.tidy)
    known = set(TidyCfg.__dataclass_fields__)
    bad = sorted(set(d) - known)
    if bad:
        raise ConfigError(f"tidy: unknown key(s) {bad}; allowed {sorted(known)}")
    t = TidyCfg()
    for k, v in d.items():
        if k in _LISTS and not (isinstance(v, list) and all(isinstance(x, str) for x in v)):
            raise ConfigError(f"tidy.{k}: must be a list of strings, got {v!r}")
        if k in _STRS and not (isinstance(v, str) and v):
            raise ConfigError(f"tidy.{k}: must be a non-empty string, got {v!r}")
        if k in _NUMS and (isinstance(v, bool) or not isinstance(v, (int, float))):
            raise ConfigError(f"tidy.{k}: must be a number, got {v!r}")
        setattr(t, k, v)
    if t.orphan_targets not in ("keep", "clean"):
        raise ConfigError("tidy.orphan_targets: must be keep or clean")
    for c in t.unattended:
        if c not in CLASSES:
            raise ConfigError(f"tidy.unattended: unknown class {c!r}; allowed {list(CLASSES)}")
    for i, x in enumerate(t.targets):
        if not isinstance(x, dict) or not isinstance(x.get("path"), str):
            raise ConfigError(f"tidy.targets[{i}]: needs a table with a path")
    if any(c in t.worktree_prefix for c in "/\\"):
        raise ConfigError(f"tidy.worktree_prefix: {t.worktree_prefix!r} holds a path separator; it is a prefix of the worktree FOLDER NAME "
                          "(for example \"wt-\"), compared with the last part of each worktree path")
    for k in [t.main_checkout, t.root_checkout] + list(t.keep_paths):
        if k and (k.startswith("\\\\") or k.replace("\\", "/").startswith("//")):
            raise ConfigError(f"tidy: the path {k!r} is a UNC or extended-length (\\\\?\\) spelling; write the drive path (C:/dir) so the keep list compares like the removal guards")
    if not SAFE_BRANCH.match(t.main_branch):
        raise ConfigError(f"tidy.main_branch: {t.main_branch!r} is not a plain branch name")
    if d and dr.host_os(host) == "windows" and (not t.link_canary or t.link_canary_count is None):
        raise ConfigError("tidy: a Windows host needs link_canary (a folder with known links) and link_canary_count")
    if t.trash_dir:
        check_trash_dir(t)
    if not t.container_prefix:
        t.container_prefix = host.container_prefix
    return t


def check_trash_dir(t):
    """A recycle bin that is a root, a drive, or that holds or sits inside anything we keep would let `remove` delete what it must not."""
    try:
        safe_path(t.trash_dir)
    except GuardViolation as e:
        raise ConfigError(f"tidy.trash_dir: {e}")
    nt = norm(t.trash_dir)
    kept = [t.main_checkout, t.root_checkout] + list(t.keep_paths) + list(t.target_roots) + [x["path"] for x in t.targets]
    for k in kept:
        if not k:
            continue
        nk = norm(k)
        if nk == nt or nk.startswith(nt + "/"):
            raise ConfigError(f"tidy.trash_dir {t.trash_dir} is, or contains, {k}")
    for k in [t.main_checkout, t.root_checkout] + list(t.keep_paths):
        if k and nt.startswith(norm(k) + "/"):
            raise ConfigError(f"tidy.trash_dir {t.trash_dir} is inside {k}")


def safe_path(p, under=None):
    """One check for every path that may be removed: absolute, no `.` or `..` component, no control character, not a root, not a share.
    With `under`, the path must be exactly one component below that folder (a recycle-bin entry)."""
    if not isinstance(p, str) or not p:
        raise ItemRefused(f"refusing to remove {p!r}: empty")
    if any(ord(c) < 32 or ord(c) == 127 for c in p):
        raise ItemRefused(f"refusing {p!r}: control character in the path")
    s = p.replace("\\", "/")
    if s.startswith("//"):
        raise ItemRefused(f"refusing {p!r}: a network path")
    if not (s.startswith("/") or re.match(r"[A-Za-z]:/", s)):
        raise ItemRefused(f"refusing {p!r}: not an absolute path")
    comps = [c for c in s.split("/")[1:] if c != ""]
    if not comps:
        raise ItemRefused(f"refusing {p!r}: a root")
    if any(c in (".", "..") for c in comps):
        raise ItemRefused(f"refusing {p!r}: a . or .. component")
    if under is not None:
        u = norm(under)
        nn = norm(p)
        rest = nn[len(u) + 1:] if nn.startswith(u + "/") else None
        if not rest or "/" in rest:
            raise ItemRefused(f"{p} is not exactly one component below {under}")


# ---------------------------------------------------------------- host operations

PROGRESS = None   # where gather reports each item as it finishes (the CLI streams it to stderr); None = quiet


def _progress(line):
    if PROGRESS is not None:
        PROGRESS(line)


def _slow(t0):
    """` [12s]` when an item took 5 s or more, else nothing (fast items print the same text on every run)."""
    s = time.monotonic() - t0
    return f" [{s:.0f}s]" if s >= 5 else ""


def over_budget(cfg):
    return f"skipped: its probes ran past item_budget_s ({cfg.item_budget_s:g}s); left alone, size unknown"


def human(n):
    if n is None:
        return "?"
    n = float(n)
    for u in ("B", "KB", "MB", "GB", "TB"):
        if abs(n) < 1024 or u == "TB":
            return f"{n:.0f} {u}" if u == "B" else f"{n:.1f} {u}"
        n /= 1024


def parse_size(s):
    m = re.match(r"\s*([\d.]+)\s*([KMGT]?)i?B?\s*$", s.strip(), re.I)
    if not m:
        return 0
    return int(float(m.group(1)) * 1024 ** ("", "K", "M", "G", "T").index(m.group(2).upper()))


class Ops:
    def __init__(self, host, runner, cfg: TidyCfg):
        self.host, self.runner, self.cfg = host, runner, cfg
        self.os = dr.host_os(host)
        self.win = self.os == "windows"
        self._canary_ok = False
        self._deadline = None     # the end of the current item's time budget (item_budget_s); None outside an item
        self._over = False        # a probe of the current item was cut short or not run because the budget ran out

    # -- the per-item time budget
    def start_item(self):
        self._over = False
        self._deadline = time.monotonic() + self.cfg.item_budget_s if self.cfg.item_budget_s > 0 else None

    def end_item(self):
        """End the item; True when a probe of it was cut short or skipped by the budget (its findings are then not trusted)."""
        over, self._deadline, self._over = self._over, None, False
        return over

    def _budgeted(self, timeout):
        """(run it?, timeout): the caller's own limit, or what is left of the item's budget when that is less."""
        if self._deadline is None:
            return True, timeout
        left = self._deadline - time.monotonic()
        if left <= 0:
            self._over = True
            return False, timeout
        return True, (left if left < (60 if timeout is None else timeout) else timeout)

    # -- primitives
    def win_unsafe(self, v):
        return any(c in WIN_BAD or ord(c) < 32 for c in v)

    def _win_ok(self, v):
        bad = [c for c in v if c in WIN_BAD or ord(c) < 32]
        if bad:
            raise ItemRefused(f"refusing to put {v!r} on a Windows command line (contains {''.join(sorted(set(bad)))!r})")

    def q(self, p):
        """Quote a PATH for the host's shell. Windows has no safe escape for its metacharacters, so a path holding one is refused."""
        if self.win:
            self._win_ok(p)
            return '"' + p.replace("/", "\\") + '"'
        return shlex.quote(p)

    def psq(self, p):
        self._win_ok(p)
        return "'" + p.replace("/", "\\").replace("'", "''") + "'"

    def run_argv(self, argv, timeout=None):
        """Run a program with an argument list. A local runner uses no shell at all; ssh gets POSIX single-quote escaping."""
        if "system prune" in " ".join(argv).lower():
            raise GuardViolation(f"refusing a bulk command: {argv}")
        go, t = self._budgeted(timeout)
        if not go:
            return TIMEOUT, ""
        cut, timeout = t != timeout, t
        try:
            if self.win and not getattr(self.runner, "shell_free", False):
                for a in argv:
                    self._win_ok(a)
                cmd = " ".join('"' + a + '"' if re.search(r"\s", a) else a for a in argv)
                return self.runner.run(cmd) if timeout is None else self.runner.run(cmd, timeout=timeout)
            return self.runner.run_argv(argv) if timeout is None else self.runner.run_argv(argv, timeout=timeout)
        except subprocess.TimeoutExpired:
            self._over = self._over or cut
            return TIMEOUT, ""

    def run(self, cmd, timeout=None):
        """(rc, out). A command that times out returns (TIMEOUT, "") instead of raising; callers treat it as a failure."""
        low = cmd.lower()
        if "system prune" in low:
            raise GuardViolation(f"refusing a bulk command: {cmd}")
        if re.search(r"rm\s+-\w*r\w*f?\s.*[*?]", low):
            raise ItemRefused(f"refusing a command with a glob in it: {cmd}")
        go, t = self._budgeted(timeout)
        if not go:
            return TIMEOUT, ""
        cut, timeout = t != timeout, t
        try:
            return self.runner.run(cmd) if timeout is None else self.runner.run(cmd, timeout=timeout)
        except subprocess.TimeoutExpired:
            self._over = self._over or cut
            return TIMEOUT, ""

    def age_only(self, p):
        """True when p sits on a disk configured measure = age-only: never sized there (L51)."""
        n = norm(p)
        best = None
        for d in self.host.disks:
            dn = norm(d.path)
            if (n == dn or n.startswith(dn + "/")) and (best is None or len(dn) > len(norm(best.path))):
                best = d
        return best is not None and getattr(best, "measure", "du") == "age-only"

    def join(self, a, b):
        return a.rstrip("/\\") + ("\\" if self.win else "/") + b

    def exists(self, p):
        """True, False, or None when the probe itself failed (unknown is never read as gone)."""
        cmd = f"if exist {self.q(p)} (echo yes) else (echo no)" if self.win else f"test -e {self.q(p)} && echo yes || echo no"
        rc, out = self.run(cmd)
        o = out.strip()
        if rc != 0:
            return None
        return True if o.startswith("yes") else False if o.startswith("no") else None

    def gone(self, p):
        e = self.exists(p)
        if e is None:
            raise GuardViolation(f"cannot tell whether {p} still exists; not assuming it is gone")
        return not e

    def is_link(self, p):
        """True for a symlink or Windows reparse point (junction), False for a real item, None when the probe failed."""
        s = self.link_state(p)
        return True if s == "link" else False if s == "real" else None

    def link_state(self, p):
        """'link', 'real', 'missing' (the item is not there) or 'unknown' (the probe failed)."""
        if self.win:
            rc, out = self.run(f"powershell -NoProfile -Command \"if (-not (Test-Path -LiteralPath {self.psq(p)})) {{'missing'}} elseif ((Get-Item -LiteralPath {self.psq(p)} -Force).Attributes -band 1024) {{'yes'}} else {{'no'}}\"")
        else:
            rc, out = self.run(f"test -L {self.q(p)} && echo yes || {{ test -e {self.q(p)} && echo no || echo missing; }}")
        o = out.strip()
        return "unknown" if rc != 0 else "link" if o.startswith("yes") else "real" if o.startswith("no") else "missing" if o.startswith("missing") else "unknown"

    def toplevel_ok(self, p):
        """True when git, run in p, says p itself is the top of its worktree (a folder with no .git of its own would answer with the repository
        it sits in, and its status would be that repository's)."""
        rc, out = self.git(p, "rev-parse", "--show-toplevel")
        if rc != 0 or not out.strip():
            return False
        a, b = self.rp(out.strip()), self.rp(p)
        return a is not None and a == b

    def link_type(self, p):
        """'dir' or 'file' for a Windows link; anything unclear is a violation (the script must pick rmdir or del)."""
        rc, out = self.run(f"powershell -NoProfile -Command \"(Get-Item -LiteralPath {self.psq(p)} -Force).PSIsContainer\"")
        o = out.strip().lower()
        if rc != 0 or o not in ("true", "false"):
            raise GuardViolation(f"cannot tell whether the link {p} is a folder or a file")
        return "dir" if o == "true" else "file"

    def rp(self, p):
        """The resolved location of p (links followed), normalised; None when it cannot be resolved."""
        cache = self.__dict__.setdefault("_rp", {})
        if p not in cache:
            if self.win:
                rc, out = self.run_argv(["python", "-c", "import os,sys;print(os.path.realpath(sys.argv[1]))", p])
            else:
                rc, out = self.run_argv(["realpath", "-m", "--", p])
                if rc != 0:                      # BSD/macOS realpath has no -m: resolve the nearest existing folder with the shell instead
                    rc, out = self.run_argv(["sh", "-c", RESOLVE_SH, "sh", p])
            o = out.strip()
            cache[p] = norm(o) if rc == 0 and o and "\n" not in o else None
        return cache[p]

    def rpl(self, p):
        """Where the item itself sits: the parent resolved, the last component NOT followed (an item that is a link is judged as the link)."""
        s = p.replace("\\", "/").rstrip("/")
        parent, _, base = s.rpartition("/")
        rpar = self.rp(parent or "/")
        return None if rpar is None else norm(rpar.rstrip("/") + "/" + base)

    def size(self, p):
        """Bytes under p, or None when unknown (timeout, error, age-only disk). Never a fake 0 (L51)."""
        if self.age_only(p):
            return None
        to = self.cfg.size_timeout_s
        if self.win:
            rc, out = self.run(f"powershell -NoProfile -Command \"(Get-ChildItem -LiteralPath {self.psq(p)} -Recurse -Force -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum\"", timeout=to)
            s = out.strip()
            if rc != 0:
                return None
            return int(s) if s.isdigit() else 0 if not s else None
        rc, out = self.run(f"du -sk {self.q(p)}", timeout=to)
        s = out.split()
        return int(s[0]) * 1024 if rc == 0 and s and s[0].isdigit() else None

    def mtime(self, p):
        if self.win:
            rc, out = self.run(f"powershell -NoProfile -Command \"[DateTimeOffset]::new((Get-Item -LiteralPath {self.psq(p)}).LastWriteTimeUtc).ToUnixTimeSeconds()\"")
        else:
            rc, out = self.run(f"stat -c %Y {self.q(p)}")
        s = out.strip()
        return int(s) if s.isdigit() else None

    def listdir(self, p):
        """Names directly in p. A name with a control character (a newline) is refused, never split into two names."""
        if self.win:
            rc, out = self.run(f"dir /B {self.q(p)}")
            if rc != 0 and self.exists(p) is not True:
                raise GuardViolation(f"listing {p} failed (rc {rc}) and the folder cannot be shown to exist; a failed listing is never read as an empty folder")
            names = [l.strip() for l in out.splitlines() if l.strip()] if rc == 0 else []      # rc 1 with the folder there = an empty folder
        else:
            rc, out = self.run(f"find {self.q(p)} -mindepth 1 -maxdepth 1 -print0")
            if rc != 0:
                raise GuardViolation(f"listing {p} failed (rc {rc}); a failed listing is never read as an empty folder")
            names = []
            pre = p.replace("\\", "/").rstrip("/") + "/"
            for r in ([x for x in out.split("\0") if x] if rc == 0 else []):
                if not r.startswith(pre) or len(r) == len(pre):
                    raise GuardViolation(f"find returned {r!r}, which is not directly inside {p}; not trusted")
                names.append(r[len(pre):])      # find prints <path>/<name>
        for nm in names:
            if any(ord(c) < 32 or ord(c) == 127 for c in nm):
                raise GuardViolation(f"{p} holds a name with a control character ({nm!r}); refusing to guess at it")
        return names

    def free_gb(self, p):
        return dr.free_gb(self.host, self.runner, p)

    def recent(self, p, minutes):
        """True when a file inside p changed in the last `minutes` (a bounded walk, `idle_walk_depth`, `size_timeout_s`), False when none did,
        None when that cannot be told (a timeout or an error): unknown is never read as idle."""
        n = max(1, math.ceil(minutes))
        if self.win:
            rc, out = self.run("powershell -NoProfile -Command \"Get-ChildItem -LiteralPath " + self.psq(p) + " -Recurse -Depth " + str(int(self.cfg.idle_walk_depth)) + " -Force -File -ErrorAction SilentlyContinue"
                               " | Where-Object { $_.LastWriteTime -gt (Get-Date).AddMinutes(-" + str(n) + ") } | Select-Object -First 1 -ExpandProperty FullName\"",
                               timeout=self.cfg.size_timeout_s)
        else:
            rc, out = self.run(f"find {self.q(p)} -maxdepth {int(self.cfg.idle_walk_depth)} -type f -mmin -{n} -print -quit", timeout=self.cfg.size_timeout_s)
        if rc != 0:
            return None
        return bool(out.strip())

    def hold_named(self):
        """[(normalised folder, run id)] named by a running hold: the `targets` its lock sidecar (`<lock_file>.info`) lists while it is `held`."""
        lf = getattr(self.host, "lock_file", None)
        if not lf:
            return []
        rc, out = self.run(("type " if self.win else "cat ") + self.q(lf + ".info"))
        if rc != 0 or not out.strip():
            return []
        try:
            j = json.loads(out)
        except ValueError:
            return []
        if not isinstance(j, dict) or j.get("state") != "held":
            return []
        return [(norm(t), str(j.get("run_id", "?"))) for t in j.get("targets", []) if isinstance(t, str)]

    def read_seen(self, trash):
        rc, out = self.run(("type " if self.win else "cat ") + self.q(self.join(trash, SEEN)))
        try:
            j = json.loads(out) if rc == 0 and out.strip() else {}
        except ValueError:
            j = {}
        out = {}
        for k, v in (j.items() if isinstance(j, dict) else []):
            if isinstance(v, (int, float)) and not isinstance(v, bool):
                continue                                           # the older format (a bare time, no folder time to compare): first seen now, and upgraded
            elif isinstance(v, dict) and isinstance(v.get("t"), (int, float)):
                out[k] = {"t": v["t"], "m": v.get("m")}
        return out

    # -- git
    def git(self, repo, *args, timeout=None):
        """git as an argument list: a branch, a path or a file name is data, never part of a command line."""
        return self.run_argv(["git", "-C", repo, *args], timeout)

    def snapshot_main(self):
        m = self.cfg.main_checkout
        if not m:
            return None
        rc, files = self.git(m, "ls-files")
        rc2, st = self.git(m, "status", "--porcelain")
        if rc != 0 or rc2 != 0:
            raise GuardViolation(f"cannot read the main checkout {m}")
        vol = [v.replace("\\", "/").rstrip("/") for v in self.cfg.main_volatile_paths if v.strip("/\\")]

        def volatile(path):
            n = path.strip().replace("\\", "/")
            return any(n == v or n.startswith(v + "/") for v in vol)
        tracked = [l for l in files.splitlines() if l.strip() and not volatile(l)]
        status = [l for l in st.splitlines() if l.strip() and not all(volatile(x) for x in l[3:].split(" -> "))]      # tracked content and untracked-but-not-ignored files, outside the volatile paths
        return len(tracked), "\n".join(status).strip()

    def worktrees(self):
        rc, out = self.git(self.cfg.main_checkout, "worktree", "list", "--porcelain")
        if rc != 0:
            raise GuardViolation("git worktree list failed; an unreadable git is never read as 'no worktrees'")
        res, cur = [], None
        for line in out.splitlines() + [""]:
            if line.startswith("worktree "):
                cur = {"path": line[9:].strip(), "branch": None, "locked": False, "prunable": False}
            elif (line == "prunable" or line.startswith("prunable ")) and cur:
                cur["prunable"] = True
            elif (line == "locked" or line.startswith("locked ")) and cur:
                cur["locked"] = True
            elif line.startswith("branch ") and cur:
                cur["branch"] = line[7:].strip().replace("refs/heads/", "", 1)
            elif not line.strip() and cur:
                res.append(cur)
                cur = None
        return res

    def branches(self):
        rc, out = self.git(self.cfg.main_checkout, "branch", "--list")
        if rc != 0:
            raise GuardViolation("git branch --list failed; an unreadable git is never read as 'no branches'")
        return [l.strip().lstrip("*+ ").strip() for l in out.splitlines() if l.strip()]

    def merged(self, branch, sha=False):
        """True only when merging the branch into main would change nothing: an ancestor, or the merge result's tree equals main's tree
        (git merge-tree --write-tree, git 2.38+). A conflict, an older git or any failure is not merged; an unusable name is unknown."""
        m, mb = self.cfg.main_checkout, self.cfg.main_branch
        if not SAFE_BRANCH.match(branch):
            return None
        ref = branch if sha else "refs/heads/" + branch       # a bare name could be a tag or a path: only a branch or a sha is proved
        mref = "refs/heads/" + mb
        if self.git(m, "merge-base", "--is-ancestor", ref, mref)[0] == 0:
            return True
        rc, tree = self.git(m, "rev-parse", "--verify", mref + "^{tree}")
        if rc != 0 or not tree.strip():
            return None
        rc, out = self.git(m, "merge-tree", "--write-tree", mref, ref)
        lines = out.splitlines()
        return rc == 0 and bool(lines) and lines[0].strip() == tree.strip()

    def dirty(self, path):
        rc, out = self.git(path, "status", "--porcelain")
        return None if rc != 0 else bool(out.strip())

    # -- links (L12, L44)
    def list_links(self, path):
        cmd = f"dir /AL /S /B {self.q(path)} & echo {END}" if self.win else f"find {self.q(path)} -type l; echo {END}"
        rc, out = self.run(cmd, timeout=self.cfg.link_list_timeout_s)
        if rc == TIMEOUT:
            raise ItemRefused(f"the link listing for {path} timed out after {self.cfg.link_list_timeout_s:g}s (link_list_timeout_s); this item is left alone")
        lines = [l.strip() for l in out.splitlines() if l.strip()]
        if END not in lines:
            raise GuardViolation(f"the link listing for {path} did not run (no end marker); refusing to continue")
        self.check_canary()
        res = [l for l in lines if l != END]
        pre = norm(path) + "/"
        for l in res:
            if not norm(l).startswith(pre):
                raise GuardViolation(f"the link listing for {path} returned {l!r}, which is not inside it; not trusted")
        ns = [norm(l) for l in res]
        return [l for l, nl in zip(res, ns) if not any(nl.startswith(o + "/") for o in ns)]      # removing a link removes what hangs below it

    def check_canary(self):
        """A host-wide proof, not a probe of the item that happens to call it first: never cut by that item's budget."""
        d, self._deadline = self._deadline, None
        try:
            return self._check_canary_now()
        finally:
            self._deadline = d

    def _check_canary_now(self):
        c = self.cfg
        if not self.win:
            return
        if not c.link_canary or c.link_canary_count is None:
            raise GuardViolation("a Windows host needs link_canary and link_canary_count: without a listing that is known to find links, an empty result means nothing")
        if self._canary_ok:
            return
        self._canary_ok = True      # set first: the canary listing re-enters list_links
        rc, out = self.run(f"dir /AL /S /B {self.q(c.link_canary)} & echo {END}", timeout=c.link_list_timeout_s)
        if rc == TIMEOUT:
            self._canary_ok = False
            raise GuardViolation(f"the link canary listing of {c.link_canary} timed out after {c.link_list_timeout_s:g}s; the listing command is not trusted")
        n = len([l for l in out.splitlines() if l.strip() and l.strip() != END])
        if n != c.link_canary_count:
            self._canary_ok = False
            raise GuardViolation(f"link listing check: {c.link_canary} returned {n} links, expected {c.link_canary_count}; the listing command is not trusted")

    def remove_link(self, p):
        """Remove ONE link, never anything inside it. Windows: rmdir for a folder link, del for a file link, chosen by type."""
        if self.win:
            self.run(("rmdir " if self.link_type(p) == "dir" else "del ") + self.q(p))
        else:
            self.run(f"unlink {self.q(p)}")

    def unlink_all(self, path):
        links = self.list_links(path)
        if links:
            for l in links:
                if self.is_link(l) is not True:
                    raise GuardViolation(f"{l} was listed as a link but is not one; refusing to unlink it")
            if self.win:
                sd = self.cfg.scratch_dir or getattr(self.runner, "scratch_dir", None)
                if not sd:
                    raise GuardViolation("scratch_dir is needed to write the unlink script on this host")
                script = self.join(sd, "gatetrain-unlink.cmd")
                body = "@echo off\r\n" + "".join(("rmdir " if self.link_type(l) == "dir" else "del ") + self.q(l) + "\r\n" for l in links)
                self.runner.put_file(script, body)
                self.run(self.q(script))
                self.run(f"del {self.q(script)}")
            else:
                self.run(" && ".join(f"unlink {self.q(l)}" for l in links))
        left = self.list_links(path)
        if left:
            raise GuardViolation(f"{len(left)} link(s) remain in {path} after unlinking; not removing")
        return len(links)

    # -- guarded removal
    def kept(self):
        return [k for k in [self.cfg.main_checkout, self.cfg.root_checkout] + list(self.cfg.keep_paths) if k]

    def check_path(self, p):
        safe_path(p)
        if re.search(r"[*?\[]", p):
            raise ItemRefused(f"refusing to remove {p!r}: a glob character in the name")
        n = norm(p)
        for k in self.kept():
            if n == norm(k) or norm(k).startswith(n + "/"):
                raise GuardViolation(f"{p} is on the keep list or contains a kept path ({k})")
        r = self.rpl(p)
        if r is None:
            raise GuardViolation(f"cannot resolve {p}; not removing what cannot be located")
        for k in self.kept():
            rk = self.rp(k)
            if rk is None:
                raise GuardViolation(f"cannot resolve the kept path {k}")
            if r == rk or rk.startswith(r + "/"):
                raise GuardViolation(f"{p} resolves to {r}, which is or contains the kept path {k}")

    def check_trash_entry(self, p):
        """A recycle-bin entry is exactly one component below a bin that resolves to a place of its own."""
        td = self.cfg.trash_dir
        safe_path(p, under=td)
        rt = self.rp(td)
        if rt is None:
            raise GuardViolation(f"cannot resolve the recycle bin {td}")
        for k in self.kept():
            rk = self.rp(k)
            if rk is None or rt == rk or rk.startswith(rt + "/") or rt.startswith(rk + "/"):
                raise GuardViolation(f"the recycle bin {td} resolves to {rt}, which overlaps the kept path {k}")
        par = self.rp(p.replace("\\", "/").rstrip("/").rpartition("/")[0])
        if par != rt:
            raise GuardViolation(f"{p} does not sit directly in the recycle bin {td} (resolves to {par})")

    def remove_dir(self, p, before_rm=None):
        """(status, note). A link is removed as a link and never walked; a timeout is 'stopped', never done. `before_rm` runs after every
        check of the folder's links has passed and before the first change."""
        self.check_path(p)
        before = self.snapshot_main()
        lk = self.is_link(p)
        if lk is None:
            raise GuardViolation(f"cannot tell whether {p} is a link; not removing")
        if lk:
            self.remove_link(p)
            if not self.gone(p):
                raise GuardViolation(f"{p} (a link) still exists after removal")
            self.assert_main(before)
            return "ok", "link removed; its target was not touched"
        rpp = self.rp(p)
        targets = [(l, self.rp(l)) for l in self.list_links(p)]        # where every link inside points, before anything is removed
        if self.win:
            for l, t in targets:                       # a name or a target that can never be put on a Windows command line refuses the item NOW, untouched
                self._win_ok(l)
                if t is not None:
                    self._win_ok(t)
        existed = {l: (t is not None and self.exists(t) is True) for l, t in targets}   # a link whose target was already gone (a dangling junction) has nothing to lose
        if before_rm:
            before_rm()
        self.unlink_all(p)
        rc, _ = self.run(f"rmdir /S /Q {self.q(p)}" if self.win else f"rm -r -- {self.q(p)}", timeout=self.cfg.remove_timeout_s)
        if rc == TIMEOUT:
            e = self.exists(p)
            self.assert_main(before)
            return "stopped", (f"removal did not finish in {self.cfg.remove_timeout_s:g}s; re-probe: "
                               + ("still there" if e else "gone" if e is False else "unknown") + "; not counted as done")
        if not self.gone(p):
            return "failed", f"PARTLY REMOVED: {p} still exists after the removal (a long path, a locked file or a permission); some of it may be gone. Left as it is for a person."
        inside = [norm(p)] + ([rpp] if rpp else [])
        for l, t in targets:
            if t is None or t == norm(l) or any(t == x or t.startswith(x + "/") for x in inside):
                continue                         # a link inside the folder (an npm workspace junction, a relative symlink) goes with it
            if existed.get(l) and self.exists(t) is not True:
                raise GuardViolation(f"the target {t} of the link {l} is gone after removing {p}; stopping")
        self.assert_main(before)
        return "ok", "removed"

    def assert_main(self, before):
        if before is None:
            return
        after = self.snapshot_main()
        if after != before:
            raise GuardViolation(f"the main checkout changed during removal: {before[0]} tracked files before, {after[0]} after; status changed={before[1] != after[1]}")


def norm(p):
    """Compare key. Windows (drive-letter) paths are case-insensitive; POSIX paths keep their case."""
    s = p.replace("\\", "/").rstrip("/")
    return s.lower() if re.match(r"[A-Za-z]:(/|$)", s) else s


# ---------------------------------------------------------------- inventory

@dataclass
class Item:
    cls: str
    kind: str                  # clean | move | remove | rm-container | rm-image | prune-cache | remove-worktree
    key: str                   # exact path or name
    bytes: Optional[int] = 0   # None = unknown
    why: str = ""
    keep: Optional[str] = None  # why it is left alone
    attended_only: bool = False
    extra: dict = field(default_factory=dict)


@dataclass
class Inventory:
    host: str
    disks: list = field(default_factory=list)       # (path, free_gb, min_free_gb)
    targets: list = field(default_factory=list)
    worktrees: list = field(default_factory=list)
    containers: list = field(default_factory=list)
    images: list = field(default_factory=list)
    cache: Optional[dict] = None
    trash: list = field(default_factory=list)
    profiles: list = field(default_factory=list)
    items: List[Item] = field(default_factory=list)
    breaches: List[str] = field(default_factory=list)


def is_cargo(names):
    s = set(names)
    return bool({"debug", "release"} & s) and bool({"tmp", ".rustc_info.json"} & s)


def gather(host, runner, cfg: TidyCfg, now=None) -> Inventory:
    now = now or time.time()
    ops = Ops(host, runner, cfg)
    inv = Inventory(host.name)
    # disks
    for d in host.disks:
        f = ops.free_gb(d.path)
        inv.disks.append((d.path, f, d.min_free_gb))
        if f is not None and f < d.min_free_gb:
            inv.breaches.append(f"disk {d.path}: {f:.0f} GB free < {d.min_free_gb:g}")
    wts, branches = [], []
    if cfg.main_checkout:
        wts, branches = ops.worktrees(), ops.branches()
    kept = [cfg.main_checkout, cfg.root_checkout] + cfg.keep_paths
    kept_n = {norm(k) for k in kept if k}
    if cfg.trash_dir:
        nt = norm(cfg.trash_dir)
        for w in wts:
            nw = norm(w["path"])
            if nw == nt or nw.startswith(nt + "/"):
                raise GuardViolation(f"the recycle bin {cfg.trash_dir} holds the registered worktree {w['path']}; fix trash_dir")
    kept_r = {ops.rp(k) for k in kept if k} - {None} if cfg.main_checkout else set()
    first_n = norm(wts[0]["path"]) if wts else None
    first_r = ops.rp(wts[0]["path"]) if wts else None
    # worktrees
    # the cheap checks (keep lists, merge proof, git status) decide first; only a worktree that would be removed has its links listed, its
    # size measured and its idle time walked - those walk the whole tree (node_modules, build targets) and are slow
    for idx, w in enumerate(wts):
        t0 = time.monotonic()
        ops.start_item()
        n = norm(w["path"])
        wr = ops.rp(w["path"])
        info = {"path": w["path"], "branch": w["branch"], "merged": None, "dirty": None, "links": None, "bytes": None, "keep": None}
        if n in kept_n or (wr is not None and wr in kept_r):
            info["keep"] = "main/root/keep list"
        elif idx == 0 or n == first_n or (wr is not None and wr == first_r):
            info["keep"] = "main worktree"            # the first entry of `git worktree list` is the main checkout, whatever its spelling
        elif w.get("locked"):
            info["keep"] = "locked"
        elif not w["branch"]:
            info["keep"] = "detached head"
        elif not SAFE_BRANCH.match(w["branch"]) or (ops.win and ops.win_unsafe(w["path"])):
            info["keep"] = "unsafe name"
        elif ops.link_state(w["path"]) != "real":
            info["keep"] = {"link": "is a link", "missing": "folder is missing"}.get(ops.link_state(w["path"]), "link state unknown")
        elif not ops.toplevel_ok(w["path"]):
            info["keep"] = "unverifiable (its own git root cannot be confirmed)"
        else:
            info["merged"] = ops.merged(w["branch"]) if w["branch"] != cfg.main_branch else None
            info["dirty"] = ops.dirty(w["path"])
            if w["branch"] == cfg.main_branch:
                info["keep"] = "main branch"
            elif w["branch"] in cfg.keep_branches or lane_of(w["path"], cfg).lower() in {x.lower() for x in cfg.keep_lanes}:
                info["keep"] = "keep list"
            elif info["dirty"] is not False:
                info["keep"] = "uncommitted work" if info["dirty"] else "status unreadable"
            elif info["merged"] is not True:
                info["keep"] = "not merged" if info["merged"] is False else "merge state unknown"
            else:
                try:
                    info["links"] = len(ops.list_links(w["path"]))
                except ItemRefused as e:                  # a listing that timed out, or a name it cannot be trusted with: this one item is kept
                    info["keep"] = f"cannot list its links ({e})"
                info["bytes"] = ops.size(w["path"])
                if not info["keep"] and cfg.worktree_min_idle_days > 0 and ops.recent(w["path"], cfg.worktree_min_idle_days * 1440) is not False:
                    info["keep"] = "used in the last %g days" % cfg.worktree_min_idle_days
        if ops.end_item():
            info["keep"], info["bytes"] = over_budget(cfg), None
        _progress(f"tidy: worktree {idx + 1}/{len(wts)} {w['path']}: {info['keep'] or 'tidy'}"
                  + (f" ({human(info['bytes'])})" if info["bytes"] is not None else "") + _slow(t0))
        inv.worktrees.append(info)
        if not info["keep"]:
            inv.items.append(Item("worktrees", "remove-worktree", w["path"], info["bytes"], f"branch {w['branch']} is on {cfg.main_branch}",
                                  extra={"branch": w["branch"], "links": info["links"] or 0}))
    # targets
    held = ops.hold_named()
    def held_by(path, lane):
        np_ = norm(path)
        for h, run in held:
            if np_ == h or np_.startswith(h + "/") or h.startswith(np_ + "/") or (lane and lane.lower() == h.rsplit("/", 1)[-1].lower()):
                return run
        return None
    entries = [(x["path"], x.get("lane"), x.get("budget_gb")) for x in cfg.targets]
    for root in cfg.target_roots:
        for nm in ops.listdir(root):
            if ops.win and ops.win_unsafe(nm):
                inv.targets.append({"path": ops.join(root, nm), "lane": nm, "bytes": None, "idle_min": None, "cargo": False, "tag": None, "merged": None,
                                    "branch": None, "budget_gb": None, "over": False, "keep": "unsafe name"})
                continue
            entries.append((ops.join(root, nm), nm, None))
    seen = set()
    for ti, (path, lane, budget) in enumerate(entries):
        if norm(path) in seen:
            continue
        e = ops.exists(path)
        if e is None:
            inv.breaches.append(f"cannot tell whether {path} exists; left alone")
            continue
        if not e:
            continue
        seen.add(norm(path))
        t0 = time.monotonic()
        ops.start_item()
        lk = ops.is_link(path)
        if lk is not False:
            inv.targets.append({"path": path, "lane": lane, "bytes": None, "idle_min": None, "cargo": False, "tag": None, "merged": None, "branch": None,
                                "budget_gb": budget, "over": False, "keep": "is a link" if lk else "link state unknown"})
            continue
        names = ops.listdir(path)
        size, mt = ops.size(path), ops.mtime(path)
        cargo = is_cargo(names)
        t = {"path": path, "lane": lane, "bytes": size, "idle_min": None if mt is None else (now - mt) / 60,
             "cargo": cargo, "tag": ("CACHEDIR.TAG" in names) if cargo else None, "merged": None, "branch": None,
             "budget_gb": budget, "over": bool(budget and size is not None and size > budget * GB), "keep": None}
        inv.targets.append(t)
        if t["over"]:
            inv.breaches.append(f"target {path}: {human(size)} over budget {budget:g} GB")
        if lane:
            br = find_branch(lane, wts, branches, cfg)
            t["branch"] = br
            wt = next((w for w in inv.worktrees if w["branch"] == br), None) if br else None
            t["merged"] = ops.merged(br) if br else None
            reason = None
            if norm(path) in kept_n or lane.lower() in {x.lower() for x in cfg.keep_lanes} or (br in cfg.keep_branches):
                reason = "keep list"
            elif held_by(path, lane):
                reason = "named by the running hold " + held_by(path, lane)
            elif wt and wt["dirty"] is not False:
                reason = "its worktree has uncommitted work"
            elif t["idle_min"] is None or t["idle_min"] < cfg.min_idle_min:
                reason = "used in the last %g min" % cfg.min_idle_min
            elif br is None and cfg.orphan_targets != "clean":
                reason = "no lane branch found (orphan_targets = keep)"
            elif br is not None and t["merged"] is not True:
                reason = "lane not merged" if t["merged"] is False else "merge state unknown"
            elif ops.recent(path, cfg.min_idle_min) is not False:
                reason = "a file inside changed in the last %g min (or its activity cannot be read)" % cfg.min_idle_min
            t["keep"] = reason
            if ops.end_item():
                t["keep"], t["bytes"] = over_budget(cfg), None
            elif not reason and (size or size is None):
                inv.items.append(Item("targets", "clean" if cargo else "move", path, size,
                                      f"lane {lane}: " + ("branch content is on main" if br else "orphan"), extra={"cargo": cargo, "tag": t["tag"], "branch": br}))
        elif ops.end_item():
            t["keep"], t["bytes"] = over_budget(cfg), None
        elif t["over"] and norm(path) not in kept_n and not held_by(path, None):
            if cargo or names:
                inv.items.append(Item("targets", "clean" if cargo else "move", path, size, f"over its {budget:g} GB budget",
                                      attended_only=True, extra={"cargo": cargo, "tag": t["tag"], "branch": None}))
        _progress(f"tidy: target {ti + 1}/{len(entries)} {path}: {t['keep'] or 'tidy'}" + (f" ({human(t['bytes'])})" if t["bytes"] is not None else "") + _slow(t0))
    ops.end_item()   # a target that was skipped early leaves no budget running into the host-wide probes below
    # containers
    if cfg.container_prefix:
        rc, out = ops.run('docker ps -a --filter status=exited --format "{{.Names}}"')
        rc2, run_out = ops.run('docker ps --format "{{.Names}}"')
        running = {l.strip() for l in run_out.splitlines()}
        if rc == 0:
            for n in [l.strip() for l in out.splitlines() if l.strip().startswith(cfg.container_prefix)]:
                inv.containers.append(n)
                if n not in running:
                    inv.items.append(Item("containers", "rm-container", n, 0, "exited throwaway container"))
    # images
    if cfg.image_patterns:
        rc, out = ops.run('docker images --format "{{.Repository}}:{{.Tag}}|{{.ID}}|{{.Size}}"')
        rc2, used_out = ops.run('docker ps -a --format "{{.Image}}"')
        used = {l.strip() for l in used_out.splitlines() if l.strip()}
        if rc == 0:
            for line in out.splitlines():
                parts = line.strip().split("|")
                if len(parts) != 3:
                    continue
                name, iid, sz = parts
                if name.endswith(":<none>") or not any(fnmatch.fnmatchcase(name, p) for p in cfg.image_patterns):
                    continue
                in_use = name in used or any(iid.startswith(u) or u.startswith(iid) for u in used)
                inv.images.append({"name": name, "bytes": parse_size(sz), "in_use": in_use})
                if not in_use:
                    inv.items.append(Item("images", "rm-image", name, parse_size(sz), "no container uses it; matches a configured pattern"))
    # build cache
    if cfg.cache_days > 0:
        rc, out = ops.run('docker system df --format "{{.Type}}|{{.Size}}|{{.Reclaimable}}"')
        for line in out.splitlines():
            p = line.strip().split("|")
            if len(p) == 3 and p[0].lower().startswith("build cache"):
                rec = parse_size(p[2].split()[0]) if p[2].split() else 0
                inv.cache = {"size": parse_size(p[1]), "reclaimable": rec}
                inv.items.append(Item("cache", "prune-cache", "build cache", rec, f"older than {cfg.cache_days:g} days (upper bound: all reclaimable)"))
    # recycle bin
    te = ops.exists(cfg.trash_dir) if cfg.trash_dir else False
    if te is None:
        inv.breaches.append(f"cannot tell whether the recycle bin {cfg.trash_dir} exists; left alone")
    if te:
        seen = ops.read_seen(cfg.trash_dir)
        new_seen = {}
        bin_names = [x for x in ops.listdir(cfg.trash_dir) if x != SEEN]
        for bi, nm in enumerate(bin_names):
            p = ops.join(cfg.trash_dir, nm)
            if ops.win and ops.win_unsafe(nm):
                inv.trash.append({"path": p, "bytes": None, "age_days": None, "keep": "unsafe name"})
                continue
            t0 = time.monotonic()
            ops.start_item()
            mt = ops.mtime(p)
            rec = seen.get(nm)
            if rec is None or (rec.get("m") is not None and mt is not None and rec["m"] != mt):
                rec = {"t": now, "m": mt}            # new, or a NEW entry under a reused name (its own folder time differs): first seen now
            new_seen[nm] = rec
            age = max(0.0, (now - rec["t"]) / 86400)  # it counts from when tidy FIRST SAW it, never from a folder time that may be old or reset
            st = STAMP.search(nm)
            if st:                                   # the name says when it was moved here: the LATER of that and the first sighting counts
                try:
                    age = min(age, max(0.0, (now - time.mktime(time.strptime(st.group(1), "%Y%m%d%H%M%S"))) / 86400))
                except (ValueError, OverflowError):
                    pass
            sz = ops.size(p) if age > cfg.trash_days else None   # only an entry old enough to go is sized; a young one shows "?"
            cut = ops.end_item()
            keep = over_budget(cfg) if cut else None
            _progress(f"tidy: recycle bin {bi + 1}/{len(bin_names)} {p}: {age:.0f}d" + ("" if cut or sz is None else f" ({human(sz)})")
                      + (f", {keep}" if keep else "") + _slow(t0))
            inv.trash.append({"path": p, "bytes": None if cut else sz, "age_days": age, **({"keep": keep} if keep else {})})
            if not keep and age is not None and age > cfg.trash_days:
                inv.items.append(Item("trash", "remove", p, sz, f"in the recycle bin {age:.0f} days (> {cfg.trash_days:g})"))
        if new_seen != seen:                         # also drops the records of entries that are gone
            try:
                ops.runner.put_file(ops.join(cfg.trash_dir, SEEN), json.dumps(new_seen, sort_keys=True))
            except Exception as e:
                inv.breaches.append(f"cannot record when the recycle bin entries were first seen ({e}); their age stays 0")
    # profiles in the wrong place (report only)
    for root in cfg.evidence_dirs:
        cmd = (f"dir /S /B {ops.q(root)}" if ops.win else
               f"find {ops.q(root)} -maxdepth 5 \\( -name SingletonLock -o -name Cookies -o -name 'Local State' -o -name lockfile \\)")
        rc, out = ops.run(cmd)
        for l in out.splitlines():
            if re.search(r"(^|[\\/])(SingletonLock|Cookies|Local State|lockfile)$", l.strip()):
                inv.profiles.append(l.strip())
        if inv.profiles:
            inv.breaches.append(f"{len(inv.profiles)} browser-profile file(s) inside {root}")
    names = {lane_of(w["path"], cfg).lower() for w in inv.worktrees} | {str(t["lane"]).lower() for t in inv.targets if t.get("lane")}
    for kl in cfg.keep_lanes:
        if kl.lower() not in names:
            inv.breaches.append(f"keep_lanes entry {kl!r} matches no worktree or target (worktree_prefix {cfg.worktree_prefix!r}); it protects nothing")
    return inv


def lane_of(path, cfg):
    b = norm(path).rsplit("/", 1)[-1]
    p = cfg.worktree_prefix.lower()
    return b[len(p):] if p and b.lower().startswith(p) else b


def find_branch(lane, wts, branches, cfg):
    cands = [lane]
    for w in wts:
        if w["branch"] and lane_of(w["path"], cfg).lower() == lane.lower():
            return w["branch"]
    for b in branches + [w["branch"] for w in wts if w["branch"]]:
        if b == lane or b.rsplit("/", 1)[-1] == lane:
            return b
    return None


# ---------------------------------------------------------------- reports

def total_str(items):
    """12.0 MB, or 12.0 MB + 2 item(s) of unknown size."""
    unk = sum(1 for i in items if i.bytes is None)
    known = human(sum(i.bytes for i in items if i.bytes is not None))
    return known + (f" + {unk} item(s) of unknown size" if unk else "")


def table(rows, head):
    rows = [head] + [tuple(str(c) for c in r) for r in rows]
    w = [max(len(r[i]) for r in rows) for i in range(len(head))]
    return "\n".join("  ".join(r[i].ljust(w[i]) for i in range(len(head))).rstrip() for r in rows)


def yn(v):
    return "-" if v is None else "yes" if v else "no"


def format_check(inv: Inventory) -> str:
    L = [f"== {inv.host}"]
    if inv.disks:
        L += ["disks", table([(p, "unreadable" if f is None else f"{f:.0f} GB", f"{m:g} GB", "LOW" if f is not None and f < m else "ok") for p, f, m in inv.disks],
                             ("path", "free", "min", "state"))]
    if inv.targets:
        L += ["build targets", table([(t["path"], human(t["bytes"]), "-" if t["idle_min"] is None else f"{t['idle_min'] / 60:.0f}h", t["lane"] or "-",
                                       yn(t["merged"]), yn(t["tag"]), "OVER" if t["over"] else "size unknown" if t["budget_gb"] and t["bytes"] is None else "-", t["keep"] or "tidy")
                                      for t in inv.targets], ("path", "size", "idle", "lane", "merged", "tag", "budget", "plan"))]
    if inv.worktrees:
        L += ["worktrees", table([(w["path"], w["branch"] or "-", yn(w["merged"]), yn(w["dirty"]), "-" if w["links"] is None else w["links"], w["keep"] or "tidy") for w in inv.worktrees],
                                 ("path", "branch", "merged", "dirty", "links", "plan"))]
    if inv.containers:
        L += [f"exited containers: {', '.join(inv.containers)}"]
    if inv.images:
        L += ["images", table([(i["name"], human(i["bytes"]), "in use" if i["in_use"] else "unused") for i in inv.images], ("image", "size", "state"))]
    if inv.cache:
        L += [f"build cache: {human(inv.cache['size'])} ({human(inv.cache['reclaimable'])} reclaimable)"]
    if inv.trash:
        L += ["recycle bin", table([(t["path"], human(t["bytes"]), "-" if t["age_days"] is None else f"{t['age_days']:.0f}d") for t in inv.trash], ("entry", "size", "age"))]
    if inv.profiles:
        L += ["browser profiles in the wrong place (never auto-removed)"] + ["  " + p for p in inv.profiles]
    plan = [i for i in inv.items]
    if plan:
        L += ["would tidy", table([(i.cls, i.kind, i.key, "?" if i.bytes is None else human(i.bytes) if i.bytes else "-", "attended" if i.attended_only else "", i.why) for i in plan],
                                  ("class", "action", "item", "bytes", "mode", "why"))]
        L += [f"total reclaimable: {total_str(plan)}"]
    else:
        L += ["would tidy: nothing"]
    L += [("THRESHOLD: " + b) for b in inv.breaches]
    return "\n".join(L)


# ---------------------------------------------------------------- apply

def log_action(path, rec):
    with open(path, "a", encoding="utf-8") as f:     # append-only
        f.write(json.dumps(rec, sort_keys=True) + "\n")


def apply_plan(host, runner, cfg: TidyCfg, inv: Inventory, classes, now=None, log=None, attended=False):
    """(done, failed) of `apply_plan_report`."""
    return apply_plan_report(host, runner, cfg, inv, classes, now, log, attended)[:2]


def apply_plan_report(host, runner, cfg: TidyCfg, inv: Inventory, classes, now=None, log=None, attended=False):
    """Run the plan class by class. Returns (done, failed, reclaimed bytes); raises GuardViolation after logging, stopping everything.
    `reclaimed` is measured: before minus after of every item that went ok and whose two sizes are known.
    Every action writes an `intent` record BEFORE it runs, so a crash mid-action leaves a trace of what was being done."""
    ops = Ops(host, runner, cfg)
    log = log or cfg.log
    done = failed = reclaimed = 0
    for cls in CLASSES:
        if cls not in classes:
            continue
        for it in [i for i in inv.items if i.cls == cls and (attended or not i.attended_only)]:
            rec = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(now or time.time())), "host": host.name,
                   "class": cls, "action": it.kind, "item": it.key, "before_bytes": it.bytes, "after_bytes": None}
            log_action(log, dict(rec, status="intent", note="about to run"))
            fb = ops.free_gb(it.key) if it.kind in ("clean", "move", "remove", "remove-worktree") else None
            try:
                after, status, note = do_item(ops, cfg, it, now)
            except ItemRefused as e:                 # this one item has a bad path or name and nothing was changed: report it, carry on
                rec.update(status="refused", note=str(e))
                log_action(log, rec)
                failed += 1
                continue
            except GuardViolation as e:
                rec.update(status="guard-violation", note=str(e))
                log_action(log, rec)
                raise
            rec.update(after_bytes=after, status=status, note=note)
            if fb is not None:
                rec.update(free_gb_before=round(fb, 2), free_gb_after=ops.free_gb(it.key))
            log_action(log, rec)
            if status == "ok":
                done += 1
                if it.bytes is not None and after is not None:
                    reclaimed += max(0, it.bytes - after)
            else:
                failed += 1
    return done, failed, reclaimed


def cache_hours(cfg):
    return max(1, math.ceil(cfg.cache_days * 24))      # a window under an hour is an hour, never `until=0h` (= everything)


def do_item(ops, cfg, it, now=None):
    k = it.kind
    if k == "clean":
        return clean_target(ops, cfg, it)
    if k == "move":
        if not cfg.trash_dir:
            return it.bytes, "skipped", "not a cargo target and no trash_dir configured; left alone"
        ops.check_path(it.key)
        why = reprove(ops, cfg, it)
        if why:
            return it.bytes, "failed", why
        name = it.key.replace("\\", "/").rstrip("/").rsplit("/", 1)[-1]
        dest = ops.join(cfg.trash_dir, name + "-" + time.strftime("%Y%m%d%H%M%S", time.localtime(now or time.time())))
        safe_path(dest, under=cfg.trash_dir)
        if ops.exists(dest) is not False:
            return it.bytes, "failed", f"{dest} exists or cannot be probed; not moving onto it"
        rc, _ = ops.run(f"move {ops.q(it.key)} {ops.q(dest)}" if ops.win else f"mv -- {ops.q(it.key)} {ops.q(dest)}", timeout=cfg.remove_timeout_s)
        if rc == TIMEOUT:
            return None, "stopped", f"move did not finish in {cfg.remove_timeout_s:g}s; not counted as done"
        src, dst = ops.exists(it.key), ops.exists(dest)
        if rc != 0 or src is not False or dst is not True:
            return it.bytes, "failed", f"move did not complete (rc {rc}; source gone: {src}; destination exists: {dst})"
        return 0, "ok", f"moved to {dest}"
    if k == "remove":
        if not cfg.trash_dir:
            raise GuardViolation(f"{it.key} is not inside a configured recycle bin")
        ops.check_trash_entry(it.key)
        status, note = ops.remove_dir(it.key)
        return (0 if status == "ok" else None), status, note
    if k == "remove-worktree":
        return remove_worktree(ops, cfg, it)
    if k == "rm-container":
        rc, out = ops.run("docker ps --format \"{{.Names}}\"")
        if it.key in out.split():
            raise GuardViolation(f"container {it.key} is running")
        if not SAFE_CONTAINER.match(it.key) or not it.key.startswith(cfg.container_prefix or "\0"):
            raise GuardViolation(f"container {it.key} does not match the prefix")
        rc, _ = ops.run_argv(["docker", "rm", it.key])
        return 0, "ok" if rc == 0 else "failed", "removed by exact name"
    if k == "rm-image":
        if not SAFE_IMAGE.match(it.key) or not any(fnmatch.fnmatchcase(it.key, p) for p in cfg.image_patterns):
            raise GuardViolation(f"image {it.key} matches no configured pattern")
        rc, used = ops.run('docker ps -a --format "{{.Image}}"')
        if it.key in used.split():
            raise GuardViolation(f"image {it.key} is used by a container")
        rc, _ = ops.run_argv(["docker", "rmi", it.key])
        return 0, "ok" if rc == 0 else "failed", "removed by exact name"
    if k == "prune-cache":
        rc, _ = ops.run_argv(["docker", "builder", "prune", "-f", "--filter", f"until={cache_hours(cfg)}h"])
        return 0, "ok" if rc == 0 else "failed", f"build cache older than {cfg.cache_days:g} days"
    raise GuardViolation(f"unknown action {k}")


def reprove(ops, cfg, it):
    """The lane of a target is proved on main again, on the resolved sha, at the moment of action (the plan may be minutes old)."""
    br = (it.extra or {}).get("branch")
    if not br or not cfg.main_checkout:
        return None
    rc, out = ops.git(cfg.main_checkout, "rev-parse", "--verify", "--quiet", f"refs/heads/{br}^{{commit}}")
    sha = out.strip()
    if rc != 0 or not re.fullmatch(r"\S+", sha):
        return f"lane branch {br} cannot be resolved to one commit any more; not touching its target"
    if ops.merged(sha, sha=True) is not True:
        return f"lane branch {br} ({sha}) is not proven on {cfg.main_branch} any more; not touching its target"
    return None


def clean_target(ops, cfg, it):
    p = it.key
    ops.check_path(p)
    lk = ops.is_link(p)
    if lk is not False:
        return it.bytes, "failed", "is a link (or link state unknown); cargo clean not run"
    why = reprove(ops, cfg, it)
    if why:
        return it.bytes, "failed", why
    if not is_cargo(ops.listdir(p)):         # re-check at the moment of action: the plan may be minutes old and sizing may have failed
        return it.bytes, "failed", "no longer looks like a cargo target (no debug/release with tmp); cargo clean not run"
    before = ops.size(p)
    fb = ops.free_gb(p) if before is None else None      # sizing failed: fall back to the disk's free space (L51)
    mp = ["--manifest-path", ops.join(cfg.main_checkout, "Cargo.toml")] if cfg.main_checkout else []
    cmd = ["cargo", "clean", *mp, "--target-dir", p]          # never -q: a quiet clean can refuse and say nothing (L43)
    rc, _ = ops.run_argv(cmd, cfg.remove_timeout_s)
    if rc == TIMEOUT:
        return None, "stopped", f"cargo clean did not finish in {cfg.remove_timeout_s:g}s; not counted as done"
    if rc != 0:
        return ops.size(p), "failed", f"cargo clean exited {rc}"
    after = ops.size(p)
    note = "cargo clean"
    if before is None:
        fa = ops.free_gb(p)
        if fb is None or fa is None:
            return after, "ok", "cargo clean ran; freed: unknown (size and free space could not be measured)"
        d = fa - fb
        note = ""
        if d <= 0.001 and "CACHEDIR.TAG" not in ops.listdir(p):
            ops.runner.put_file(ops.join(p, "CACHEDIR.TAG"), CARGO_TAG)
            rc, _ = ops.run_argv(cmd, cfg.remove_timeout_s)
            if rc == TIMEOUT:
                return None, "stopped", f"the cargo clean rerun did not finish in {cfg.remove_timeout_s:g}s; not counted as done"
            if rc != 0:
                return after, "failed", f"the cargo clean rerun exited {rc}"
            fa = ops.free_gb(p)
            d = (fa - fb) if fa is not None else None
            note = "cargo refused silently (no CACHEDIR.TAG); wrote cargo's standard tag and re-ran; "
        if d is None:
            return after, "ok", note + "cargo clean ran; freed: unknown"
        if d <= 0.001:
            return after, "failed", note + "free space did not grow after cargo clean (df)"
        return after, "ok", note + f"cargo clean; free space grew by about {d:.1f} GB (df)"
    if before and after is None:
        return after, "ok", "cargo clean ran; freed: unknown (size after could not be measured)"
    names = ops.listdir(p) if after else []
    if before and after >= before * 0.99 and is_cargo(names) and "CACHEDIR.TAG" not in names:
        ops.runner.put_file(ops.join(p, "CACHEDIR.TAG"), CARGO_TAG)
        rc, _ = ops.run_argv(cmd, cfg.remove_timeout_s)
        if rc == TIMEOUT:
            return None, "stopped", f"the cargo clean rerun did not finish in {cfg.remove_timeout_s:g}s; not counted as done"
        if rc != 0:
            return after, "failed", f"the cargo clean rerun exited {rc}"
        after = ops.size(p)
        note = "cargo refused silently (no CACHEDIR.TAG); wrote cargo's standard tag and re-ran"
        if after is None:
            return after, "ok", note + "; freed: unknown"
    if before and after >= before * 0.99:
        return after, "failed", "cargo clean freed nothing"
    return after, "ok", note


def remove_worktree(ops, cfg, it):
    """Order: resolve the branch to one sha, archive THAT sha under a tag and verify it, unlink links, remove the worktree, then delete
    the branch only if it still points at that sha. Any step that cannot be proven stops here with the branch intact."""
    p, br = it.key, it.extra["branch"]
    if not cfg.main_checkout:
        raise GuardViolation("worktree removal needs main_checkout in the config")
    if not SAFE_BRANCH.match(br):
        raise ItemRefused(f"branch {br!r} has an unsafe name; not removing")
    m = cfg.main_checkout
    ops.check_path(p)
    if ops.is_link(p) is not False:
        raise GuardViolation(f"{p} is a link (or cannot be probed); not removing a worktree through it")
    if not ops.toplevel_ok(p):
        raise GuardViolation(f"{p} is not the top level of its own git worktree (its .git is missing?); not removing")
    if ops.dirty(p) is not False:
        raise GuardViolation(f"{p} has uncommitted work")
    rc, out = ops.git(m, "rev-parse", "--verify", "--quiet", f"refs/heads/{br}^{{commit}}")
    sha = out.strip()
    if rc != 0 or not re.fullmatch(r"\S+", sha):          # exactly one token
        raise GuardViolation(f"cannot resolve branch {br} to one commit")
    if ops.merged(sha, sha=True) is not True:                         # proved on the very commit that is archived and deleted
        raise GuardViolation(f"branch {br} ({sha}) is not on {cfg.main_branch}")
    before = ops.snapshot_main()
    cell = {"tag": f"archive/{br}"}

    def make_tag():                                  # runs after the link check, before the first change: a refused item leaves no tag behind
        tag = cell["tag"]
        rc, ex = ops.git(m, "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}")
        if rc == 0 and ex.strip() != sha:
            tag = f"{tag}-{sha[:8]}"                 # that name belongs to another commit: never overwrite or reuse it
            rc, ex = ops.git(m, "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}")
        if not (rc == 0 and ex.strip() == sha):
            rc, _ = ops.git(m, "tag", tag, sha)
            if rc != 0:
                raise GuardViolation(f"could not create the archive tag {tag} for {br}; branch and worktree kept")
        rc, got = ops.git(m, "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}")
        if rc != 0 or got.strip() != sha:
            raise GuardViolation(f"archive tag {tag} does not point at {sha}; branch and worktree kept")
        cell["tag"] = tag
        cell["made"] = True
    now_reg = [w for w in ops.worktrees() if norm(w["path"]) == norm(p)]            # re-read just before acting: it may have been locked since the plan
    if not now_reg:
        raise ItemRefused(f"{p} is no longer a registered worktree; not removing")
    if now_reg[0].get("locked"):
        raise ItemRefused(f"{p} is locked now; not removing")
    # the folder goes by a removal that never follows a link (`git worktree remove` may), links first and their targets checked after;
    # git is then told the folder is gone
    status, note = ops.remove_dir(p, before_rm=make_tag)
    tag = cell["tag"]
    if status != "ok":
        return None, status, f"{note}; branch {br} kept" + (f", archived as {tag}" if cell.get("made") else "")
    # `git worktree remove <path>` on the folder that is already gone unregisters exactly that worktree (never the repo-wide `prune`, which
    # also unregisters, and deletes the admin directory of, another worktree whose folder is only away)
    rc, _ = ops.git(m, "worktree", "remove", "--force", p, timeout=cfg.remove_timeout_s)
    if rc != 0:
        return None, "failed", f"STALE REGISTRATION: {p} is removed but `git worktree remove` could not unregister it (rc {rc}); branch {br} kept, archived as {tag}"
    rc, _ = ops.git(m, "update-ref", "-d", f"refs/heads/{br}", sha)
    if rc != 0:
        raise GuardViolation(f"branch {br} no longer points at {sha} (or could not be deleted); it is kept, archived as {tag}")
    ops.assert_main(before)
    return 0, "ok", f"links unlinked, tagged {tag}, worktree and branch removed, main checkout unchanged"


# ---------------------------------------------------------------- CLI

def _hosts(a):
    from . import config as cfgm
    try:
        cfg = cfgm.load(a.config)
    except (cfgm.ConfigError, OSError) as e:
        raise SystemExit(f"config error: {e}")
    hs = [h for h in cfg.hosts if (h.name == a.host if a.host else h.enabled)]
    if a.host and not hs:
        raise SystemExit(f"no host {a.host} in the config")
    return cfg, hs


def _prep(cfg, h):
    r = dr.make_runner(h)
    if r is None:
        return None, None
    try:
        return r, build_cfg(cfg, h)
    except ConfigError as e:
        raise SystemExit(f"config error: {e}")


def cmd_check(a):
    import sys
    cfg, hs = _hosts(a)
    rc = 0
    for h in hs:
        r, t = _prep(cfg, h)
        if r is None:
            print(f"== {h.name}\nskipped: reach {h.reach!r} is not local or ssh")
            continue
        try:
            inv = gather(h, r, t)
        except GuardViolation as e:
            print(f"guard violation on {h.name}: {e}", file=sys.stderr)
            return 3
        print(json.dumps({"host": inv.host, "plan": [asdict(i) for i in inv.items], "breaches": inv.breaches}, indent=2) if a.json else format_check(inv))
        rc = 1 if inv.breaches else rc
    return rc


def cmd_tidy(a):
    import sys
    cfg, hs = _hosts(a)
    wanted = [c.strip() for c in a.cls.split(",")] if a.cls else None
    if wanted and any(c not in CLASSES for c in wanted):
        raise SystemExit(f"unknown class in --class; allowed {list(CLASSES)}")
    rc = 0
    for h in hs:
        r, t = _prep(cfg, h)
        if r is None:
            print(f"== {h.name}\nskipped: reach {h.reach!r} is not local or ssh")
            continue
        classes = [c for c in (wanted or (t.unattended if a.auto else CLASSES))]
        if a.auto:
            classes = [c for c in classes if c in t.unattended]
        try:
            inv = gather(h, r, t)
            plan = [i for i in inv.items if i.cls in classes and not (a.auto and i.attended_only)]
            if not a.apply and not a.auto:
                print(format_check(inv))
                print(f"dry run: {len(plan)} action(s) in classes {','.join(classes)}; add --apply to execute")
                rc = 1 if inv.breaches else rc
                continue
            inv.items = plan
            done, failed, reclaimed = apply_plan_report(h, r, t, inv, classes, attended=not a.auto)
            after = gather(h, r, t)
        except GuardViolation as e:
            print(f"guard violation on {h.name}: {e}; stopped, nothing further touched", file=sys.stderr)
            return 3
        freed = total_str(plan)
        print(f"{h.name}: {len(plan)} planned in {','.join(classes)}; {done} done, {failed} failed")
        print(f"{h.name}: {human(reclaimed)} reclaimed (measured: size before minus size after, per item that went ok; {freed} planned); log {t.log}")
        for d in after.disks:
            print(f"{h.name}: disk {d[0]} {'unreadable' if d[1] is None else format(d[1], '.0f') + ' GB free'} (min {d[2]:g})")
        print(f"{h.name}: {len(after.items)} item(s) still plannable, {len(after.breaches)} threshold(s) crossed")
        for b in after.breaches[:1]:
            print(f"{h.name}: THRESHOLD: {b}")
        if failed or after.breaches:
            rc = 1
    return rc
