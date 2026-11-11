"""Pre-run checks per host. Each check takes (host, runner) and returns a list of Result; the runner is injected."""
import posixpath
import re
import shlex
import sys
from dataclasses import asdict, dataclass
from typing import List, Optional

from .config import Config, Host, Needs
from .runner import LocalRunner, SshRunner

BUILD_USES = ("preflight", "lane-gates", "chain")
WIN_PS = ('powershell -NoProfile -Command "Get-CimInstance Win32_Process | ForEach-Object '
          '{ $_.ProcessId.ToString() + \' \' + $_.CommandLine }"')


@dataclass
class Result:
    host: str
    check: str
    status: str          # PASS | WARN | FAIL
    lessons: List[str]
    message: str
    fix: str = ""

    def as_dict(self):
        return asdict(self)


def host_os(h: Host) -> str:
    if h.os != "auto":
        return h.os
    if h.reach == "local":
        return "windows" if sys.platform == "win32" else "linux"
    return "linux"


def _q(s: str, os_: str) -> str:
    return '"' + s + '"' if os_ == "windows" else shlex.quote(s)


def _skip(h, check, lessons, why):
    return [Result(h.name, check, "WARN", lessons, f"skipped on {host_os(h)}: {why}", "run this check on a host where it is supported")]


def free_gb(h: Host, runner, path: str) -> Optional[float]:
    """Free space in GB at `path`, or None when the path cannot be read (missing, not mounted)."""
    if host_os(h) == "windows":
        rc, out = runner.run('python -c "import shutil,sys;print(shutil.disk_usage(sys.argv[1]).free)" ' + _q(path, "windows"))
        s = out.strip()
        return int(s) / 1024 ** 3 if rc == 0 and s.isdigit() else None
    rc, out = runner.run(f"df -Pk {_q(path, 'linux')}")
    lines = [l for l in out.splitlines() if l.strip()]
    if rc != 0 or len(lines) < 2:
        return None
    cols = lines[-1].split()
    return int(cols[3]) / 1024 ** 2 if len(cols) >= 4 and cols[3].isdigit() else None


def check_disks(h, runner):
    out = []
    for d in h.disks:
        f = free_gb(h, runner, d.path)
        if f is None:
            if d.removable:
                out.append(Result(h.name, "disk", "WARN", ["L18"], f"{d.path}: removable disk is not mounted",
                                  f"attach and mount {d.path}; what it enables is dropped meanwhile"))
            else:
                out.append(Result(h.name, "disk", "FAIL", ["L18", "L30"], f"{d.path}: cannot read free space (missing or not mounted)",
                                  f"mount {d.path} or correct the path in the config"))
            continue
        if f < d.min_free_gb:
            out.append(Result(h.name, "disk", "FAIL", ["L18", "L30"], f"{d.path}: {f:.0f} GB free, need {d.min_free_gb:g}",
                              f"free space on {d.path} (prune build outputs of merged lanes) before starting a run"))
        else:
            out.append(Result(h.name, "disk", "PASS", ["L18", "L30"], f"{d.path}: {f:.0f} GB free >= {d.min_free_gb:g}"))
        if not d.fast and any(u in BUILD_USES for u in d.use):
            out.append(Result(h.name, "disk-speed", "WARN", ["L19"], f"{d.path}: allowed for {[u for u in d.use if u in BUILD_USES]} but not marked fast",
                              f"put build outputs on a fast disk, or limit {d.path} to use = [\"archive\"]"))
    return out


def check_capacity(h, runner, needs: Needs = None):
    needs = needs or Needs()
    kinds = [k for k in BUILD_USES if k in h.roles]
    if not kinds:
        return []
    need = {"preflight": needs.preflight_gb, "lane-gates": needs.lane_gates_gb, "chain": needs.chain_gb}
    free = {d.path: free_gb(h, runner, d.path) for d in h.disks}

    def fits(d, k, present_only=True):
        if k not in d.use or (k == "chain" and needs.chain_needs_fast and not d.fast):
            return False
        f = free[d.path]
        return f is not None and f >= max(need[k], d.min_free_gb)

    ok, no, hint = [], [], []
    for k in kinds:
        if any(fits(d, k) for d in h.disks):
            ok.append(k)
            continue
        no.append(k)
        for d in h.disks:
            if d.removable and free[d.path] is None and k in d.use and (k != "chain" or d.fast or not needs.chain_needs_fast):
                hint.append(f"{d.path} (not mounted) would enable {k}")
    msg = "can run now: " + (", ".join(ok) or "nothing")
    if no:
        msg += "; cannot: " + ", ".join(f"{k} (needs {need[k]:g} GB free on an allowed" + (" fast" if k == "chain" and needs.chain_needs_fast else "") + " disk)" for k in no)
    if hint:
        msg += "; " + "; ".join(hint)
    chain_lost = "chain" in no
    if chain_lost and not any("would enable chain" in x for x in hint):
        status = "FAIL"
    elif no:
        status = "WARN"
    else:
        status = "PASS"
    fix = "" if status == "PASS" else "add or mount a disk with use including the missing kind, enough free space and fast = true for chains"
    return [Result(h.name, "capacity", status, ["L18", "L19"], msg, fix)]


def check_containers(h, runner):
    if not h.container_prefix:
        return []
    rc, out = runner.run('docker ps -a --filter status=exited --format "{{.Names}}"')
    if rc != 0:
        return [Result(h.name, "containers", "WARN", ["L31"], "docker not available; leftover containers not checked", "install or start docker, or drop container_prefix")]
    names = [n.strip() for n in out.splitlines() if n.strip().startswith(h.container_prefix)]
    if not names:
        return [Result(h.name, "containers", "PASS", ["L31"], f"no exited container with prefix {h.container_prefix}")]
    return [Result(h.name, "containers", "FAIL", ["L31"], f"{len(names)} leftover exited container(s): " + ", ".join(names),
                   "docker rm " + " ".join(names) + "  (doctor never removes; exact names only)")]


def check_watchers(h, runner):
    if h.watcher_max_age_min is None:
        return []
    if host_os(h) == "windows":
        return _skip(h, "watchers", ["L24"], "needs ps with elapsed time")
    rc, out = runner.run("ps -eo pid=,etimes=,args=")
    if rc != 0:
        return [Result(h.name, "watchers", "WARN", ["L24"], "cannot list processes", "check ps on the host")]
    stray = []
    for line in out.splitlines():
        p = line.split(None, 2)
        if len(p) < 3 or not p[0].isdigit() or not p[1].isdigit():
            continue
        words = p[2].split()
        if words and words[0].rsplit("/", 1)[-1] == "tail" and ("-F" in words or "--follow=name" in words) and int(p[1]) > h.watcher_max_age_min * 60:
            stray.append((p[0], int(p[1]) // 60, p[2]))
    if not stray:
        return [Result(h.name, "watchers", "PASS", ["L24"], f"no tail -F older than {h.watcher_max_age_min:g} min")]
    return [Result(h.name, "watchers", "FAIL", ["L24"], "; ".join(f"pid {a} up {m} min: {c}" for a, m, c in stray),
                   "kill " + " ".join(a for a, _, _ in stray) + "  (by exact pid)")]


def check_tools(h, runner):
    out = []
    for t in h.tools:
        rc, o = runner.run(f"{t} --version")
        first = (o.strip().splitlines() or [""])[0]
        if rc == 0 and first:
            out.append(Result(h.name, "toolchain", "PASS", ["L29"], f"{t}: {first}"))
        else:
            out.append(Result(h.name, "toolchain", "FAIL", ["L29"], f"{t}: does not resolve or prints no version",
                              f"install {t} or fix the PATH or version file the chain uses"))
    return out


def _ranges_linux(runner):
    rc, o = runner.run("cat /proc/sys/net/ipv4/ip_local_port_range")
    n = re.findall(r"\d+", o)
    return [(int(n[0]), int(n[1]), "ephemeral range")] if rc == 0 and len(n) >= 2 else None


def _ranges_windows(runner):
    rc, o = runner.run("netsh int ipv4 show dynamicport tcp")
    s = re.search(r"Start Port\s*:\s*(\d+)", o)
    c = re.search(r"Number of Ports\s*:\s*(\d+)", o)
    if rc != 0 or not (s and c):
        return None
    out = [(int(s[1]), int(s[1]) + int(c[1]) - 1, "dynamic range")]
    rc, o = runner.run("netsh int ipv4 show excludedportrange protocol=tcp")
    for m in re.finditer(r"^\s*(\d+)\s+(\d+)", o, re.M):
        out.append((int(m[1]), int(m[2]), "OS-excluded range"))
    return out


def check_ports(h, runner):
    if not h.ports:
        return []
    ranges = _ranges_windows(runner) if host_os(h) == "windows" else _ranges_linux(runner)
    res = []
    extra = []
    for r in h.reserved_port_ranges:
        m = re.fullmatch(r"\s*(\d+)\s*-\s*(\d+)\s*", r)
        if m:
            extra.append((int(m[1]), int(m[2]), "reserved range"))
    out = []
    if ranges is None:
        out.append(Result(h.name, "ports", "WARN", ["L21"], "cannot read the OS port ranges; only reserved ranges and duplicates checked",
                          "check the ephemeral and excluded ranges by hand"))
        ranges = []
    for p in h.ports:
        hit = next(((a, b, w) for a, b, w in ranges + extra if a <= p <= b), None)
        if hit:
            out.append(Result(h.name, "ports", "FAIL", ["L21"], f"port {p} is inside the {hit[2]} {hit[0]}-{hit[1]}",
                              f"move port {p} outside {hit[0]}-{hit[1]}"))
        else:
            out.append(Result(h.name, "ports", "PASS", ["L21"], f"port {p} is outside every ephemeral or reserved range"))
    dup = sorted({p for p in h.ports if h.ports.count(p) > 1})
    if dup:
        out.append(Result(h.name, "ports", "FAIL", ["L22"], "duplicate port(s): " + ", ".join(map(str, dup)), "give every gate its own port"))
    return out


def _norm(path, os_):
    p = posixpath.normpath(path.replace("\\", "/"))
    return p.lower() if os_ == "windows" else p


def check_shared_build(h, runner):
    if not h.worktrees:
        return []
    groups = {}
    for w in h.worktrees:
        key = _norm(w.build_dir, host_os(h))
        if host_os(h) != "windows":
            rc, o = runner.run(f"realpath {shlex.quote(w.build_dir)}")
            if rc == 0 and o.strip():
                key = o.strip()
        groups.setdefault(key, []).append(w.name)
    shared = {k: v for k, v in groups.items() if len(v) > 1}
    if not shared:
        return [Result(h.name, "shared-build", "PASS", ["L17"], f"{len(h.worktrees)} worktree(s), each with its own build output folder")]
    return [Result(h.name, "shared-build", "FAIL", ["L17"], "; ".join(f"{', '.join(v)} share {k}" for k, v in shared.items()),
                   "point each worktree at its own build output folder")]


def check_lock(h, runner):
    if not h.lock_file:
        return []
    if host_os(h) == "windows":
        return _skip(h, "lock", ["L32"], "needs stat and date")
    q = shlex.quote(h.lock_file)
    rc, m = runner.run(f"stat -c %Y {q}")
    if rc != 0:
        return [Result(h.name, "lock", "PASS", ["L32"], f"{h.lock_file}: no lock file")]
    rc2, n = runner.run("date +%s")
    try:
        age_h = (int(n.strip()) - int(m.strip())) / 3600
    except ValueError:
        return [Result(h.name, "lock", "WARN", ["L32"], "cannot read the lock age", "check stat and date on the host")]
    if age_h <= h.lock_max_age_hours:
        return [Result(h.name, "lock", "PASS", ["L32"], f"{h.lock_file}: {age_h:.1f} h old, within {h.lock_max_age_hours:g} h")]
    _, c = runner.run(f"grep -c released {q}")
    if c.strip().isdigit() and int(c.strip()) > 0:
        return [Result(h.name, "lock", "PASS", ["L32"], f"{h.lock_file}: {age_h:.1f} h old but has a released line")]
    return [Result(h.name, "lock", "FAIL", ["L32"], f"{h.lock_file}: {age_h:.1f} h old with no released line (holder died?)",
                   "find the holder, then remove the lock by hand after checking no chain is running")]


def check_chain(h, runner):
    if not h.chain_pattern:
        return []
    win = host_os(h) == "windows"
    rc, o = runner.run(WIN_PS if win else "ps -eo pid=,args=")
    if rc != 0:
        return [Result(h.name, "chain", "WARN", ["L26"], "cannot list processes", "check ps on the host")]
    hits = []
    for line in o.splitlines():
        p = line.strip().split(None, 1)
        if len(p) == 2 and p[0].isdigit() and re.search(h.chain_pattern, p[1]) and "ps -eo" not in p[1] and "Get-CimInstance" not in p[1]:
            hits.append(p[0])
    if hits:
        return [Result(h.name, "chain", "WARN", ["L26"], f"a chain is running (pid {', '.join(hits)}): logs and light reads only",
                       "wait for the chain to finish before heavy work on this host")]
    return [Result(h.name, "chain", "PASS", ["L26"], "no chain running")]


def check_cache(h, runner):
    if not h.cache_server_check:
        return []
    rc, _ = runner.run(h.cache_server_check)
    if rc == 0:
        return [Result(h.name, "cache", "PASS", ["L20"], "shared cache server answers")]
    return [Result(h.name, "cache", "FAIL", ["L20"], f"cache server check failed (rc={rc})",
                   "restart the server with its own environment and no idle timeout")]


def check_pollers(h, runner):
    if h.max_pollers is None:
        return []
    if host_os(h) == "windows":
        return _skip(h, "pollers", ["L25"], "counts sshd sessions with ps")
    rc, o = runner.run("ps -eo args=")
    n = sum(1 for l in o.splitlines() if re.match(r"^sshd: \S+@", l.strip()))
    if n > h.max_pollers:
        return [Result(h.name, "pollers", "FAIL", ["L25"], f"{n} ssh sessions > max_pollers {h.max_pollers}",
                       "stop extra pollers; use one watcher per log")]
    return [Result(h.name, "pollers", "PASS", ["L25"], f"{n} ssh sessions <= max_pollers {h.max_pollers}")]


CHECKS = [check_disks, check_capacity, check_containers, check_watchers, check_tools, check_ports,
          check_shared_build, check_lock, check_chain, check_cache, check_pollers]


def run_checks(h, runner, needs: Needs = None) -> List[Result]:
    out = []
    for fn in CHECKS:
        try:
            out += fn(h, runner, needs) if fn is check_capacity else fn(h, runner)
        except Exception as e:  # a check that cannot run is a failure, never a silent pass
            out.append(Result(h.name, fn.__name__.replace("check_", ""), "FAIL", [], f"check could not run: {e}", "fix the connection or the command"))
    return out


def make_runner(h: Host):
    if h.reach == "local":
        return LocalRunner()
    if h.reach.startswith("ssh "):
        return SshRunner(h.reach)
    return None


def run_doctor(cfg: Config, only: str = None) -> List[Result]:
    out = []
    hosts = [h for h in cfg.hosts if (h.name == only if only else True)]
    if only and not hosts:
        raise SystemExit(f"no host {only} in the config")
    for h in hosts:
        if not h.enabled and not only:
            continue
        r = make_runner(h)
        if r is None:
            out.append(Result(h.name, "reach", "WARN", [], f"reach {h.reach!r} is not local or ssh; nothing checked", "use reach = \"local\" or \"ssh <name>\" to check this host"))
            continue
        out += run_checks(h, r, cfg.needs)
    return out


def format_text(results: List[Result]) -> str:
    L = []
    for r in results:
        line = f"{r.status:<4}  {','.join(r.lessons) or '-':<7}  {r.host} {r.check}: {r.message}"
        if r.status != "PASS" and r.fix:
            line += f"  -> fix: {r.fix}"
        L.append(line)
    return "\n".join(L)


def exit_code(results: List[Result]) -> int:
    return 1 if any(r.status == "FAIL" for r in results) else 0
