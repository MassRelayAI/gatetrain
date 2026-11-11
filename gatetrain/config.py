"""Host configuration from TOML (standard library tomllib). Errors name the field, e.g. hosts[1].disks[0].min_free_gb."""
import tomllib
from dataclasses import dataclass, field
from typing import List, Optional

PROFILES = ("zero-cost", "budget", "speed", "mixed")
KINDS = ("local", "server", "cloud")
OSES = ("auto", "linux", "windows")
USES = ("preflight", "lane-gates", "chain", "cache", "archive")


class ConfigError(ValueError):
    pass


@dataclass
class Disk:
    path: str
    min_free_gb: float = 0
    fast: bool = False
    use: List[str] = field(default_factory=lambda: list(USES))
    removable: bool = False


@dataclass
class Needs:
    preflight_gb: float = 20
    lane_gates_gb: float = 40
    chain_gb: float = 120
    chain_needs_fast: bool = True


@dataclass
class Worktree:
    name: str
    build_dir: str


@dataclass
class Host:
    name: str
    kind: str = "server"
    roles: List[str] = field(default_factory=list)
    reach: str = "local"
    os: str = "auto"
    cores: Optional[int] = None
    max_parallel_steps: Optional[int] = None
    disks: List[Disk] = field(default_factory=list)
    cost_per_hour_usd: float = 0
    max_pollers: Optional[int] = None
    awake_hours: Optional[str] = None
    enabled: bool = True
    tools: List[str] = field(default_factory=list)
    ports: List[int] = field(default_factory=list)
    reserved_port_ranges: List[str] = field(default_factory=list)
    container_prefix: Optional[str] = None
    watcher_max_age_min: Optional[float] = None
    worktrees: List[Worktree] = field(default_factory=list)
    lock_file: Optional[str] = None
    lock_max_age_hours: float = 6
    chain_pattern: Optional[str] = None
    cache_server_check: Optional[str] = None


@dataclass
class Model:
    name: str
    kind: str = "local"
    cost_per_mtok_usd: float = 0


@dataclass
class Config:
    profile: str = "mixed"
    budget: dict = field(default_factory=dict)
    needs: Needs = field(default_factory=Needs)
    hosts: List[Host] = field(default_factory=list)
    models: List[Model] = field(default_factory=list)


def _num(d, key, where, default):
    v = d.get(key, default)
    if isinstance(v, bool) or not isinstance(v, (int, float)):
        raise ConfigError(f"{where}.{key}: must be a number, got {v!r}")
    return v


def _bool(d, key, where, default):
    v = d.get(key, default)
    if not isinstance(v, bool):
        raise ConfigError(f"{where}.{key}: must be true or false, got {v!r}")
    return v


def _str(d, key, where, default=None, required=False):
    if key not in d:
        if required:
            raise ConfigError(f"{where}.{key}: required")
        return default
    v = d[key]
    if not isinstance(v, str) or not v:
        raise ConfigError(f"{where}.{key}: must be a non-empty string, got {v!r}")
    return v


def _list(d, key, where, kind, default=()):
    v = d.get(key, list(default))
    if not isinstance(v, list) or not all(isinstance(x, kind) and not isinstance(x, bool) for x in v):
        raise ConfigError(f"{where}.{key}: must be a list of {kind.__name__}, got {v!r}")
    return list(v)


def _opt_int(d, key, where):
    return int(_num(d, key, where, 0)) if key in d else None


def _disk(d, where):
    if not isinstance(d, dict):
        raise ConfigError(f"{where}: must be a table")
    use = _list(d, "use", where, str, USES)
    bad = [u for u in use if u not in USES]
    if bad:
        raise ConfigError(f"{where}.use: unknown {bad}; allowed {list(USES)}")
    return Disk(_str(d, "path", where, required=True), _num(d, "min_free_gb", where, 0), _bool(d, "fast", where, False),
                use, _bool(d, "removable", where, False))


def _host(d, i):
    w = f"hosts[{i}]"
    if not isinstance(d, dict):
        raise ConfigError(f"{w}: must be a table")
    kind = _str(d, "kind", w, "server")
    if kind not in KINDS:
        raise ConfigError(f"{w}.kind: must be one of {list(KINDS)}, got {kind!r}")
    os_ = _str(d, "os", w, "auto")
    if os_ not in OSES:
        raise ConfigError(f"{w}.os: must be one of {list(OSES)}, got {os_!r}")
    disks = [_disk(x, f"{w}.disks[{j}]") for j, x in enumerate(d.get("disks", []))]
    wts = []
    for j, x in enumerate(d.get("worktrees", [])):
        ww = f"{w}.worktrees[{j}]"
        if not isinstance(x, dict):
            raise ConfigError(f"{ww}: must be a table")
        wts.append(Worktree(_str(x, "name", ww, required=True), _str(x, "build_dir", ww, required=True)))
    return Host(
        name=_str(d, "name", w, required=True), kind=kind, roles=_list(d, "roles", w, str),
        reach=_str(d, "reach", w, "local"), os=os_, cores=_opt_int(d, "cores", w),
        max_parallel_steps=_opt_int(d, "max_parallel_steps", w), disks=disks,
        cost_per_hour_usd=_num(d, "cost_per_hour_usd", w, 0), max_pollers=_opt_int(d, "max_pollers", w),
        awake_hours=_str(d, "awake_hours", w), enabled=_bool(d, "enabled", w, True),
        tools=_list(d, "tools", w, str), ports=_list(d, "ports", w, int),
        reserved_port_ranges=_list(d, "reserved_port_ranges", w, str),
        container_prefix=_str(d, "container_prefix", w), watcher_max_age_min=_num(d, "watcher_max_age_min", w, 0) if "watcher_max_age_min" in d else None,
        worktrees=wts, lock_file=_str(d, "lock_file", w), lock_max_age_hours=_num(d, "lock_max_age_hours", w, 6),
        chain_pattern=_str(d, "chain_pattern", w), cache_server_check=_str(d, "cache_server_check", w))


def parse(data: dict) -> Config:
    profile = data.get("profile", "mixed")
    if profile not in PROFILES:
        raise ConfigError(f"profile: must be one of {list(PROFILES)}, got {profile!r}")
    n = data.get("needs", {})
    if not isinstance(n, dict):
        raise ConfigError("needs: must be a table")
    needs = Needs(_num(n, "preflight_gb", "needs", 20), _num(n, "lane_gates_gb", "needs", 40),
                  _num(n, "chain_gb", "needs", 120), _bool(n, "chain_needs_fast", "needs", True))
    hosts = [_host(h, i) for i, h in enumerate(data.get("hosts", []))]
    names = [h.name for h in hosts]
    for i, nm in enumerate(names):
        if names.index(nm) != i:
            raise ConfigError(f"hosts[{i}].name: duplicate {nm!r}")
    models = []
    for i, m in enumerate(data.get("models", [])):
        w = f"models[{i}]"
        models.append(Model(_str(m, "name", w, required=True), _str(m, "kind", w, "local"),
                            _num(m, "cost_per_mtok_usd", w, 0)))
    b = data.get("budget", {})
    if not isinstance(b, dict):
        raise ConfigError("budget: must be a table")
    return Config(profile, b, needs, hosts, models)


def load(path: str) -> Config:
    try:
        with open(path, "rb") as f:
            return parse(tomllib.load(f))
    except tomllib.TOMLDecodeError as e:
        raise ConfigError(f"{path}: not valid TOML: {e}") from None
