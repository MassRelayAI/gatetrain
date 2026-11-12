"""`gatetrain check` (read-only inventory) and `gatetrain tidy` (guarded cleanup). Lessons L12, L31, L43-L50.

Everything goes through a Runner (Local / Ssh / a test fake): run(cmd) -> (rc, out), put_file(path, text).
Nothing here uses a bulk prune, a glob, or a recursive force-remove; every removal names one exact path or object.
"""
import fnmatch
import json
import re
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


class GuardViolation(Exception):
    pass


# ---------------------------------------------------------------- config

@dataclass
class TidyCfg:
    main_checkout: Optional[str] = None
    root_checkout: Optional[str] = None
    main_branch: str = "main"
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


_LISTS = ("keep_paths", "keep_lanes", "keep_branches", "target_roots", "image_patterns", "evidence_dirs", "unattended")
_STRS = ("main_checkout", "root_checkout", "main_branch", "worktree_prefix", "orphan_targets", "trash_dir",
         "container_prefix", "log", "scratch_dir", "link_canary")
_NUMS = ("min_idle_min", "trash_days", "cache_days", "size_timeout_s")


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
    if not t.container_prefix:
        t.container_prefix = host.container_prefix
    return t


# ---------------------------------------------------------------- host operations

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

    # -- primitives
    def q(self, p):
        return '"' + p.replace("/", "\\") + '"' if self.win else dr._q(p, "linux")

    def psq(self, p):
        return "'" + p.replace("/", "\\").replace("'", "''") + "'"

    def run(self, cmd, timeout=None):
        """(rc, out). A command that times out returns (TIMEOUT, "") instead of raising; callers treat it as a failure."""
        low = cmd.lower()
        if "system prune" in low or re.search(r"rm\s+-\w*r\w*f?\s.*[*?]", low):
            raise GuardViolation(f"refusing a bulk command: {cmd}")
        try:
            return self.runner.run(cmd) if timeout is None else self.runner.run(cmd, timeout=timeout)
        except subprocess.TimeoutExpired:
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
        cmd = f"if exist {self.q(p)} (echo yes) else (echo no)" if self.win else f"test -e {self.q(p)} && echo yes || echo no"
        return self.run(cmd)[1].strip().startswith("yes")

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
        rc, out = self.run(f"dir /B {self.q(p)}" if self.win else f"ls -1 {self.q(p)}")
        return [l.strip() for l in out.splitlines() if l.strip()] if rc == 0 else []

    def free_gb(self, p):
        return dr.free_gb(self.host, self.runner, p)

    # -- git
    def git(self, repo, args):
        return self.run(f"git -C {self.q(repo)} {args}")

    def snapshot_main(self):
        m = self.cfg.main_checkout
        if not m:
            return None
        rc, files = self.git(m, "ls-files")
        rc2, st = self.git(m, "status --porcelain")
        if rc != 0 or rc2 != 0:
            raise GuardViolation(f"cannot read the main checkout {m}")
        return len([l for l in files.splitlines() if l.strip()]), st.strip()

    def worktrees(self):
        rc, out = self.git(self.cfg.main_checkout, "worktree list --porcelain")
        res, cur = [], None
        for line in out.splitlines() + [""]:
            if line.startswith("worktree "):
                cur = {"path": line[9:].strip(), "branch": None}
            elif line.startswith("branch ") and cur:
                cur["branch"] = line[7:].strip().replace("refs/heads/", "", 1)
            elif not line.strip() and cur:
                res.append(cur)
                cur = None
        return res

    def branches(self):
        rc, out = self.git(self.cfg.main_checkout, "branch --list")
        return [l.strip().lstrip("*+ ").strip() for l in out.splitlines() if l.strip()] if rc == 0 else []

    def merged(self, branch):
        """True when the branch content is on main: an ancestor, or every added line is present in main's version of the file."""
        m, mb = self.cfg.main_checkout, self.cfg.main_branch
        if self.git(m, f"merge-base --is-ancestor {self.q(branch)} {self.q(mb)}")[0] == 0:
            return True
        rc, out = self.git(m, f"diff --unified=0 --no-color {self.q(mb + '...' + branch)}")
        if rc != 0:
            return None
        if not out.strip():
            return True
        added, f = {}, None
        for line in out.splitlines():
            if line.startswith("+++ "):
                f = line[4:].strip()
                f = f[2:] if f.startswith("b/") else None if f == "/dev/null" else f
                if f:
                    added.setdefault(f, [])
            elif line.startswith("+") and f:
                added[f].append(line[1:].strip())
        todo = [(f, [x for x in ls if x]) for f, ls in added.items()]
        if not any(ls for _, ls in todo):
            return False        # only deletions or renames: content cannot be proven present
        for f, ls in todo:
            rc, txt = self.git(m, f"show {self.q(mb + ':' + f)}")
            if rc != 0:
                return False
            have = {l.strip() for l in txt.splitlines()}
            if any(x not in have for x in ls):
                return False
        return True

    def dirty(self, path):
        rc, out = self.git(path, "status --porcelain")
        return None if rc != 0 else bool(out.strip())

    # -- links (L12, L44)
    def list_links(self, path):
        cmd = f"dir /AL /S /B {self.q(path)} & echo {END}" if self.win else f"find {self.q(path)} -type l; echo {END}"
        rc, out = self.run(cmd)
        lines = [l.strip() for l in out.splitlines() if l.strip()]
        if END not in lines:
            raise GuardViolation(f"the link listing for {path} did not run (no end marker); refusing to continue")
        self.check_canary()
        return [l for l in lines if l != END]

    def check_canary(self):
        c = self.cfg
        if self._canary_ok or not (self.win and c.link_canary):
            return
        self._canary_ok = True      # set first: the canary listing re-enters list_links
        rc, out = self.run(f"dir /AL /S /B {self.q(c.link_canary)} & echo {END}")
        n = len([l for l in out.splitlines() if l.strip() and l.strip() != END])
        if c.link_canary_count is not None and n != c.link_canary_count:
            self._canary_ok = False
            raise GuardViolation(f"link listing check: {c.link_canary} returned {n} links, expected {c.link_canary_count}; the listing command is not trusted")

    def unlink_all(self, path):
        links = self.list_links(path)
        if links:
            if self.win:
                sd = self.cfg.scratch_dir or getattr(self.runner, "scratch_dir", None)
                if not sd:
                    raise GuardViolation("scratch_dir is needed to write the unlink script on this host")
                script = self.join(sd, "gatetrain-unlink.cmd")
                body = "@echo off\r\n" + "".join(f'rmdir "{w}" 2>nul || del "{w}"\r\n' for w in (x.replace("/", "\\") for x in links))
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
    def check_path(self, p):
        n = norm(p)
        if not p or re.search(r"[*?\[]", p) or n in ("", "/") or re.fullmatch(r"[a-z]:", n):
            raise GuardViolation(f"refusing to remove {p!r}")
        for k in [self.cfg.main_checkout, self.cfg.root_checkout] + list(self.cfg.keep_paths):
            if k and (n == norm(k) or norm(k).startswith(n + "/")):
                raise GuardViolation(f"{p} is on the keep list or contains a kept path ({k})")

    def remove_dir(self, p):
        self.check_path(p)
        before = self.snapshot_main()
        self.unlink_all(p)
        self.run(f"rmdir /S /Q {self.q(p)}" if self.win else f"rm -r -- {self.q(p)}")
        if self.exists(p):
            raise GuardViolation(f"{p} still exists after removal")
        self.assert_main(before)

    def assert_main(self, before):
        if before is None:
            return
        after = self.snapshot_main()
        if after != before:
            raise GuardViolation(f"the main checkout changed during removal: {before[0]} tracked files before, {after[0]} after; status changed={before[1] != after[1]}")


def norm(p):
    return p.replace("\\", "/").rstrip("/").lower()


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
    # worktrees
    for w in wts:
        n = norm(w["path"])
        info = {"path": w["path"], "branch": w["branch"], "merged": None, "dirty": None, "links": 0, "bytes": None, "keep": None}
        if n in kept_n:
            info["keep"] = "main/root/keep list"
        elif not w["branch"]:
            info["keep"] = "detached head"
        else:
            info["merged"] = ops.merged(w["branch"]) if w["branch"] != cfg.main_branch else None
            info["dirty"] = ops.dirty(w["path"])
            info["links"] = len(ops.list_links(w["path"]))
            info["bytes"] = ops.size(w["path"])
            if w["branch"] == cfg.main_branch:
                info["keep"] = "main branch"
            elif w["branch"] in cfg.keep_branches or lane_of(w["path"], cfg) in cfg.keep_lanes:
                info["keep"] = "keep list"
            elif info["dirty"] is not False:
                info["keep"] = "uncommitted work" if info["dirty"] else "status unreadable"
            elif info["merged"] is not True:
                info["keep"] = "not merged" if info["merged"] is False else "merge state unknown"
        inv.worktrees.append(info)
        if not info["keep"]:
            inv.items.append(Item("worktrees", "remove-worktree", w["path"], info["bytes"], f"branch {w['branch']} is on {cfg.main_branch}",
                                  extra={"branch": w["branch"], "links": info["links"]}))
    # targets
    entries = [(x["path"], x.get("lane"), x.get("budget_gb")) for x in cfg.targets]
    for root in cfg.target_roots:
        entries += [(ops.join(root, n), n, None) for n in ops.listdir(root)]
    seen = set()
    for path, lane, budget in entries:
        if norm(path) in seen or not ops.exists(path):
            continue
        seen.add(norm(path))
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
            if norm(path) in kept_n or lane in cfg.keep_lanes or (br in cfg.keep_branches):
                reason = "keep list"
            elif wt and wt["dirty"] is not False:
                reason = "its worktree has uncommitted work"
            elif t["idle_min"] is None or t["idle_min"] < cfg.min_idle_min:
                reason = "used in the last %g min" % cfg.min_idle_min
            elif br is None and cfg.orphan_targets != "clean":
                reason = "no lane branch found (orphan_targets = keep)"
            elif br is not None and t["merged"] is not True:
                reason = "lane not merged" if t["merged"] is False else "merge state unknown"
            t["keep"] = reason
            if not reason and (size or size is None):
                inv.items.append(Item("targets", "clean" if cargo else "move", path, size,
                                      f"lane {lane}: " + ("branch content is on main" if br else "orphan"), extra={"cargo": cargo, "tag": t["tag"]}))
        elif t["over"] and norm(path) not in kept_n:
            if cargo or names:
                inv.items.append(Item("targets", "clean" if cargo else "move", path, size, f"over its {budget:g} GB budget",
                                      attended_only=True, extra={"cargo": cargo, "tag": t["tag"]}))
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
    if cfg.trash_dir and ops.exists(cfg.trash_dir):
        for n in ops.listdir(cfg.trash_dir):
            p = ops.join(cfg.trash_dir, n)
            mt = ops.mtime(p)
            age = None if mt is None else (now - mt) / 86400
            sz = ops.size(p)
            inv.trash.append({"path": p, "bytes": sz, "age_days": age})
            if age is not None and age > cfg.trash_days:
                inv.items.append(Item("trash", "remove", p, sz, f"in the recycle bin {age:.0f} days (> {cfg.trash_days:g})"))
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
    return inv


def lane_of(path, cfg):
    b = norm(path).rsplit("/", 1)[-1]
    p = cfg.worktree_prefix.lower()
    return b[len(p):] if p and b.startswith(p) else b


def find_branch(lane, wts, branches, cfg):
    cands = [lane]
    for w in wts:
        if w["branch"] and lane_of(w["path"], cfg) == lane.lower():
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
        L += ["worktrees", table([(w["path"], w["branch"] or "-", yn(w["merged"]), yn(w["dirty"]), w["links"], w["keep"] or "tidy") for w in inv.worktrees],
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
    """Run the plan class by class. Returns (done, failed); raises GuardViolation after logging, stopping everything."""
    ops = Ops(host, runner, cfg)
    log = log or cfg.log
    done = failed = 0
    for cls in CLASSES:
        if cls not in classes:
            continue
        for it in [i for i in inv.items if i.cls == cls and (attended or not i.attended_only)]:
            rec = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S%z", time.localtime(now or time.time())), "host": host.name,
                   "class": cls, "action": it.kind, "item": it.key, "before_bytes": it.bytes, "after_bytes": None}
            fb = ops.free_gb(it.key) if it.kind in ("clean", "move", "remove", "remove-worktree") else None
            try:
                after, status, note = do_item(ops, cfg, it)
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
            else:
                failed += 1
    return done, failed


def do_item(ops, cfg, it):
    k = it.kind
    if k == "clean":
        return clean_target(ops, cfg, it)
    if k == "move":
        if not cfg.trash_dir:
            return it.bytes, "skipped", "not a cargo target and no trash_dir configured; left alone"
        ops.check_path(it.key)
        dest = ops.join(cfg.trash_dir, norm(it.key).rsplit("/", 1)[-1] + "-" + time.strftime("%Y%m%d%H%M%S"))
        ops.run(f"move {ops.q(it.key)} {ops.q(dest)}" if ops.win else f"mv -- {ops.q(it.key)} {ops.q(dest)}")
        return 0, "ok", f"moved to {dest}"
    if k == "remove":
        if not (cfg.trash_dir and norm(it.key).startswith(norm(cfg.trash_dir) + "/")):
            raise GuardViolation(f"{it.key} is not inside the configured recycle bin")
        ops.remove_dir(it.key)
        return 0, "ok", "removed from the recycle bin"
    if k == "remove-worktree":
        return remove_worktree(ops, cfg, it)
    if k == "rm-container":
        rc, out = ops.run("docker ps --format \"{{.Names}}\"")
        if it.key in out.split():
            raise GuardViolation(f"container {it.key} is running")
        if not it.key.startswith(cfg.container_prefix or "\0"):
            raise GuardViolation(f"container {it.key} does not match the prefix")
        rc, _ = ops.run(f"docker rm {dr._q(it.key, 'linux')}")
        return 0, "ok" if rc == 0 else "failed", "removed by exact name"
    if k == "rm-image":
        if not any(fnmatch.fnmatchcase(it.key, p) for p in cfg.image_patterns):
            raise GuardViolation(f"image {it.key} matches no configured pattern")
        rc, used = ops.run('docker ps -a --format "{{.Image}}"')
        if it.key in used.split():
            raise GuardViolation(f"image {it.key} is used by a container")
        rc, _ = ops.run(f"docker rmi {dr._q(it.key, 'linux')}")
        return 0, "ok" if rc == 0 else "failed", "removed by exact name"
    if k == "prune-cache":
        rc, _ = ops.run(f"docker builder prune -f --filter until={int(cfg.cache_days * 24)}h")
        return 0, "ok" if rc == 0 else "failed", f"build cache older than {cfg.cache_days:g} days"
    raise GuardViolation(f"unknown action {k}")


def clean_target(ops, cfg, it):
    p = it.key
    ops.check_path(p)
    before = ops.size(p)
    fb = ops.free_gb(p) if before is None else None      # sizing failed: fall back to the disk's free space (L51)
    mp = f"--manifest-path {ops.q(ops.join(cfg.main_checkout, 'Cargo.toml'))} " if cfg.main_checkout else ""
    cmd = f"cargo clean {mp}--target-dir {ops.q(p)}"          # never -q: a quiet clean can refuse and say nothing (L43)
    ops.run(cmd)
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
            ops.run(cmd)
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
        ops.run(cmd)
        after = ops.size(p)
        note = "cargo refused silently (no CACHEDIR.TAG); wrote cargo's standard tag and re-ran"
        if after is None:
            return after, "ok", note + "; freed: unknown"
    if before and after >= before * 0.99:
        return after, "failed", "cargo clean freed nothing"
    return after, "ok", note


def remove_worktree(ops, cfg, it):
    p, br = it.key, it.extra["branch"]
    if not cfg.main_checkout:
        raise GuardViolation("worktree removal needs main_checkout in the config")
    ops.check_path(p)
    if ops.dirty(p) is not False:
        raise GuardViolation(f"{p} has uncommitted work")
    if ops.merged(br) is not True:
        raise GuardViolation(f"branch {br} is not on {cfg.main_branch}")
    before = ops.snapshot_main()
    ops.unlink_all(p)
    ops.git(cfg.main_checkout, f"tag archive/{br} {ops.q(br)}")
    ops.git(cfg.main_checkout, f"worktree remove {ops.q(p)}")
    if ops.exists(p):
        raise GuardViolation(f"{p} still exists after git worktree remove")
    ops.git(cfg.main_checkout, f"branch -D {ops.q(br)}")
    ops.assert_main(before)
    return 0, "ok", f"links unlinked, tagged archive/{br}, worktree and branch removed, main checkout unchanged"


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
    return r, build_cfg(cfg, h)


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
            done, failed = apply_plan(h, r, t, inv, classes, attended=not a.auto)
            after = gather(h, r, t)
        except GuardViolation as e:
            print(f"guard violation on {h.name}: {e}; stopped, nothing further touched", file=sys.stderr)
            return 3
        freed = total_str(plan)
        print(f"{h.name}: {len(plan)} planned in {','.join(classes)}; {done} done, {failed} failed")
        print(f"{h.name}: about {freed} reclaimed (planned bytes); log {t.log}")
        for d in after.disks:
            print(f"{h.name}: disk {d[0]} {'unreadable' if d[1] is None else format(d[1], '.0f') + ' GB free'} (min {d[2]:g})")
        print(f"{h.name}: {len(after.items)} item(s) still plannable, {len(after.breaches)} threshold(s) crossed")
        for b in after.breaches[:1]:
            print(f"{h.name}: THRESHOLD: {b}")
        if failed or after.breaches:
            rc = 1
    return rc
