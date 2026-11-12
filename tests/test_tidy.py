"""Tests for `check` and `tidy` on synthetic hosts. A SimHost is a runner over a virtual file system, git state and docker state."""
import contextlib, io, json, os, re, shutil, subprocess, tempfile, unittest
from pathlib import Path
from unittest import mock

from gatetrain import tidy as T
from gatetrain.config import parse, ConfigError

NOW = 1_800_000_000
DAY = 86400
GB = 1024 ** 3
MB = 1024 ** 2


def n(p):
    return p.replace("\\", "/").rstrip("/")


class SimHost:
    """A runner for the command vocabulary tidy uses. `fs` maps path -> dict(type dir|file|link, size, mtime, target)."""

    def __init__(self, win=False, free_gb=500):
        self.win, self.free = win, free_gb
        self.fs, self.calls, self.scripts = {}, [], {}
        self.scratch_dir = "C:/tmp" if win else "/tmp"
        self.worktrees, self.branches, self.diffs, self.shown, self.ancestors = [], [], {}, {}, set()
        self.dirty = set()
        self.containers_exited, self.containers_running, self.images, self.image_users = [], [], [], []
        self.cache_row = None
        self.broken_links = False        # the link listing never runs
        self.unlink_noop = False         # unlinking silently does nothing
        self.clean_needs_tag = True      # cargo refuses, quietly, without CACHEDIR.TAG
        self.tags, self.deleted_branches = [], []
        self.main = "C:/repo/main" if win else "/repo/main"
        self.slow = ()                   # path prefixes whose du never finishes; ("",) = every path

    def slow_du(self, cmd):
        c = n(cmd.replace('"', "").replace("'", ""))
        return any(x in c for x in self.slow)

    def add(self, path, type="dir", size=0, age_s=10 * DAY, target=None):
        self.fs[n(path)] = dict(type=type, size=size, mtime=NOW - age_s, target=n(target) if target else None)

    def under(self, p):
        p = n(p)
        return [k for k in self.fs if k == p or k.startswith(p + "/")]

    def total(self, p):
        return sum(self.fs[k]["size"] for k in self.under(p) if self.fs[k]["type"] == "file")

    def tracked(self):
        return sorted(k for k in self.under(self.main) if self.fs[k]["type"] == "file")

    def put_file(self, path, text):
        self.add(path, "file", size=len(text), age_s=0)
        self.scripts[n(path)] = text

    def children(self, p):
        p = n(p)
        return sorted({k[len(p) + 1:].split("/")[0] for k in self.fs if k.startswith(p + "/")})

    def rm_tree(self, p, follow_links):
        for k in self.under(p):
            e = self.fs.get(k)
            if e and e["type"] == "link" and follow_links:
                for t in self.under(e["target"]):       # a recursive remove that follows a link empties its target
                    self.fs.pop(t, None)
            self.fs.pop(k, None)

    def run(self, cmd, timeout=None):
        self.calls.append(cmd)
        if re.match(r"(du -sk|powershell .*Measure-Object)", cmd) and self.slow_du(cmd):
            raise subprocess.TimeoutExpired(cmd, timeout or 60)
        c = cmd.replace("\\", "/").replace('"', "").replace("'", "")
        if c.startswith("df -Pk"):
            return 0, f"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/a 9 9 {int(self.free * 1024 * 1024)} 1% /m\n"
        if c.startswith("python -c") and "shutil" in c:
            return 0, str(int(self.free * GB)) + "\n"
        m = re.match(r"du -sk (.+)$", c)
        if m:
            return 0, f"{self.total(m.group(1)) // 1024}\t{m.group(1)}\n"
        if c.startswith("powershell") and "Measure-Object" in c:
            return 0, f"{self.total(re.search(r'-LiteralPath (.+?) -Recurse', c).group(1))}\n"
        m = re.match(r"stat -c %Y (.+)$", c) or re.search(r"Get-Item -LiteralPath (.+?)\)\.LastWrite", c)
        if m:
            e = self.fs.get(n(m.group(1)))
            return (0, f"{e['mtime']}\n") if e else (1, "")
        m = re.match(r"(?:ls -1|dir /B) (.+)$", c)
        if m:
            return 0, "\n".join(self.children(m.group(1))) + "\n"
        m = re.match(r"test -e (.+?) &&", c) or re.match(r"if exist (.+?) \(echo", c)
        if m:
            return 0, "yes\n" if n(m.group(1)) in self.fs else "no\n"
        m = re.match(r"find (.+?) -type l; echo (\w+)", c) or re.match(r"dir /AL /S /B (.+?) & echo (\w+)", c)
        if m:
            if self.broken_links:
                return 1, ""
            links = [k for k in self.under(m.group(1)) if self.fs[k]["type"] == "link"]
            return 0, "\n".join(links + [m.group(2)]) + "\n"
        if c.startswith("unlink "):
            if not self.unlink_noop:
                for p in c.split(" && "):
                    self.fs.pop(n(p[7:]), None)
            return 0, ""
        if c.startswith("del "):
            self.fs.pop(n(c[4:]), None)
            return 0, ""
        if c.endswith("gatetrain-unlink.cmd"):
            if not self.unlink_noop:
                for l in self.scripts[n(c)].splitlines():
                    m = re.match(r"rmdir (.+?) 2>nul", l.replace("\\", "/").replace('"', ""))
                    if m:
                        self.fs.pop(n(m.group(1)), None)       # rmdir on a link removes only the link
            return 0, ""
        m = re.match(r"(?:rmdir /S /Q|rm -r --) (.+)$", c)
        if m:
            self.rm_tree(m.group(1), follow_links=True)     # the dangerous kind: follows what is still linked
            return 0, ""
        m = re.match(r"(?:mv --|move) (.+?) (.+)$", c)
        if m:
            for k in self.under(m.group(1)):
                self.fs[n(m.group(2)) + k[len(n(m.group(1))):]] = self.fs.pop(k)
            return 0, ""
        m = re.search(r"cargo clean .*--target-dir (.+)$", c)
        if m:
            p = n(m.group(1))
            if self.clean_needs_tag and p + "/CACHEDIR.TAG" not in self.fs:
                return 0, ""        # refuses, says nothing
            for k in self.under(p):
                if k != p and not k.endswith("CACHEDIR.TAG"):
                    self.fs.pop(k)
            return 0, ""
        if c.startswith("git -C"):
            return self.git(c)
        if c.startswith("docker"):
            return self.docker(c)
        return 127, ""

    def git(self, c):
        m = re.match(r"git -C (\S+) (.+)$", c)
        repo, a = n(m.group(1)), m.group(2)
        if a == "ls-files":
            return 0, "\n".join(self.tracked()) + "\n"
        if a == "status --porcelain":
            return 0, ("M x\n" if repo in self.dirty else "")
        if a.startswith("worktree list"):
            return 0, "\n".join(f"worktree {w[0]}\nHEAD abc\nbranch refs/heads/{w[1]}\n" for w in self.worktrees)
        if a == "branch --list":
            return 0, "\n".join("  " + b for b in self.branches) + "\n"
        m = re.match(r"merge-base --is-ancestor (\S+) (\S+)", a)
        if m:
            return (0 if m.group(1) in self.ancestors else 1), ""
        m = re.match(r"diff --unified=0 --no-color (\S+)\.\.\.(\S+)", a)
        if m:
            return 0, self.diffs.get(m.group(2), "")
        m = re.match(r"show (\S+?):(\S+)$", a)
        if m:
            return (0, self.shown[m.group(2)]) if m.group(2) in self.shown else (128, "")
        m = re.match(r"worktree remove (\S+)", a)
        if m:
            self.rm_tree(m.group(1), follow_links=True)
            self.worktrees = [w for w in self.worktrees if n(w[0]) != n(m.group(1))]
            return 0, ""
        if a.startswith("tag "):
            self.tags.append(a.split()[1])
            return 0, ""
        m = re.match(r"branch -D (\S+)", a)
        if m:
            self.deleted_branches.append(m.group(1))
            return 0, ""
        return 1, ""

    def docker(self, c):
        if "ps -a --filter status=exited" in c:
            return 0, "\n".join(self.containers_exited) + "\n"
        if c.startswith("docker ps --format"):
            return 0, "\n".join(self.containers_running) + "\n"
        if "ps -a --format" in c:
            return 0, "\n".join(self.image_users) + "\n"
        if c.startswith("docker images"):
            return 0, "\n".join(self.images) + "\n"
        if c.startswith("docker system df"):
            return 0, self.cache_row or ""
        if c.startswith("docker rm "):
            nm = c.split()[2]
            self.containers_exited = [x for x in self.containers_exited if x != nm]
            return 0, ""
        if c.startswith("docker rmi "):
            nm = c.split()[2]
            self.images = [x for x in self.images if not x.startswith(nm + "|")]
            return 0, ""
        if c.startswith("docker builder prune"):
            self.cache_row = None
            return 0, ""
        return 127, ""


def cargo_target(sim, path, size=100 * MB, tag=False, age_s=10 * DAY):
    sim.add(path, age_s=age_s)
    sim.add(path + "/debug", age_s=age_s)
    sim.add(path + "/tmp", age_s=age_s)
    sim.add(path + "/debug/big.o", "file", size, age_s)
    if tag:
        sim.add(path + "/CACHEDIR.TAG", "file", 100, age_s)


def linux_sim():
    s = SimHost()
    s.add("/repo/main", age_s=0)
    for f in ("a.rs", "b.rs", "c.rs"):
        s.add("/repo/main/" + f, "file", 10, 0)
    s.add("/repo/main/node_modules", age_s=0)
    s.add("/repo/main/node_modules/pkg.js", "file", 10, 0)
    s.add("/repo/wt", age_s=0)
    s.add("/build", age_s=0)
    s.add("/build/targets", age_s=0)
    cargo_target(s, "/build/targets/lane-a")                 # merged by content, no CACHEDIR.TAG
    cargo_target(s, "/build/targets/lane-b", tag=True)       # not merged
    cargo_target(s, "/build/targets/lane-c", tag=True)       # merged but on the keep list
    cargo_target(s, "/build/targets/lane-new", tag=True, age_s=60)   # merged but used a minute ago
    cargo_target(s, "/build/chain", size=3 * GB, tag=True)  # shared, over budget
    s.worktrees = [("/repo/main", "main"), ("/repo/wt/lane-a", "line/lane-a"), ("/repo/wt/lane-b", "line/lane-b"),
                   ("/repo/wt/lane-d", "line/lane-d"), ("/repo/wt/lane-c", "line/lane-c"), ("/repo/wt/lane-new", "line/lane-new")]
    s.branches = ["main", "line/lane-a", "line/lane-b", "line/lane-c", "line/lane-d", "line/lane-new"]
    for l in ("a", "b", "c", "d", "new"):
        s.add(f"/repo/wt/lane-{l}", age_s=DAY)
        s.add(f"/repo/wt/lane-{l}/src.rs", "file", 5, DAY)
    s.add("/repo/wt/lane-a/node_modules", "link", target="/repo/main/node_modules")
    s.dirty.add("/repo/wt/lane-d")
    s.shown = {"f.rs": "let x = 1;\nlet y = 2;\n"}
    s.diffs = {"line/lane-a": "--- a/f.rs\n+++ b/f.rs\n@@ -0,0 +1,2 @@\n+let x = 1;\n+let y = 2;\n",
               "line/lane-b": "--- a/f.rs\n+++ b/f.rs\n@@ -0,0 +1 @@\n+let z = 3;\n"}
    s.ancestors = {"line/lane-c", "line/lane-d", "line/lane-new", "main"}
    s.add("/trash", age_s=0)
    s.add("/trash/old", age_s=10 * DAY); s.add("/trash/old/x", "file", 50 * MB, 10 * DAY)
    s.add("/trash/new", age_s=DAY); s.add("/trash/new/x", "file", 5 * MB, DAY)
    s.containers_exited = ["gate-1", "gate-9", "other-1"]
    s.containers_running = ["gate-9"]
    s.images = ["gate-img:1|aaa111|2.0GB", "gate-img:2|bbb222|1.0GB", "keepme:1|ccc333|500MB", "gate-img:<none>|ddd444|1GB"]
    s.image_users = ["gate-img:2"]
    s.cache_row = "Build Cache|8GB|6GB\nImages|3GB|1GB\n"
    s.free = 5
    return s


BASE = {"main_checkout": "/repo/main", "root_checkout": "/repo/root", "trash_dir": "/trash",
        "container_prefix": "gate-", "image_patterns": ["gate-img:*"], "cache_days": 3, "keep_lanes": ["lane-c"],
        "target_roots": ["/build/targets"], "targets": [{"path": "/build/chain", "budget_gb": 1}]}

WIN_EXTRA = {"main_checkout": "C:/repo/main", "root_checkout": "C:/repo/root", "scratch_dir": "C:/tmp", "target_roots": [], "targets": [],
             "trash_dir": "C:/trash", "container_prefix": "x-", "image_patterns": [], "cache_days": 0, "keep_lanes": []}


def hostcfg(extra=None, win=False, mins=50):
    t = dict(BASE); t.update(extra or {})
    h = {"name": "h1", "reach": "local", "os": "windows" if win else "linux",
         "disks": [{"path": "C:/build" if win else "/build", "min_free_gb": mins}], "tidy": t}
    cfg = parse({"hosts": [h]})
    return cfg, cfg.hosts[0]


def gather(sim, extra=None, win=False, mins=50):
    cfg, h = hostcfg(extra, win, mins)
    t = T.build_cfg(cfg, h)
    return h, t, T.gather(h, sim, t, now=NOW)


def by_key(inv, key):
    return [i for i in inv.items if i.key == key]


def toml_val(v):
    if isinstance(v, dict):
        return "{ " + ", ".join(f"{k} = {toml_val(x)}" for k, x in v.items()) + " }"
    if isinstance(v, list):
        return "[" + ", ".join(toml_val(x) for x in v) + "]"
    return json.dumps(v)


def write_conf(d, extra, disk=True):
    p = Path(d, "hosts.toml")
    head = '[[hosts]]\nname="h1"\nreach="local"\nos="linux"\n'
    if disk:
        head += '[[hosts.disks]]\npath="/build"\nmin_free_gb=50\n'
    t = dict(BASE); t.update(extra)
    p.write_text(head + "[tidy]\n" + "\n".join(f"{k} = {toml_val(v)}" for k, v in t.items()) + "\n")
    return str(p)


def cli(sim, *argv):
    from gatetrain.cli import main
    buf = io.StringIO()
    with mock.patch("gatetrain.tidy.dr.make_runner", return_value=sim), mock.patch("gatetrain.tidy.time.time", return_value=NOW), contextlib.redirect_stdout(buf):
        rc = main(list(argv))
    return rc, buf.getvalue()


class UnknownSizeTests(unittest.TestCase):
    def test_du_timeout_does_not_crash_check_and_shows_question_mark(self):
        sim = linux_sim()
        sim.slow = ("/trash/old",)
        h, t, inv = gather(sim)
        old = next(x for x in inv.trash if x["path"] == "/trash/old")
        self.assertIsNone(old["bytes"])                                  # unknown, never a fake 0
        self.assertEqual(len(by_key(inv, "/trash/old")), 1)               # age still decides removal
        self.assertIsNone(by_key(inv, "/trash/old")[0].bytes)
        out = T.format_check(inv)
        self.assertIn("?", next(l for l in out.splitlines() if l.startswith("/trash/old")))
        self.assertIn("item(s) of unknown size", out)

    def test_ops_size_is_none_on_timeout_and_error(self):
        sim = linux_sim()
        sim.slow = ("/trash/old",)
        cfg, h = hostcfg()
        ops = T.Ops(h, sim, T.build_cfg(cfg, h))
        self.assertIsNone(ops.size("/trash/old"))
        self.assertEqual(ops.size("/trash/new"), 5 * MB)
        self.assertEqual(T.human(None), "?")

    def test_everything_slow_check_still_completes(self):
        sim = linux_sim()
        sim.slow = ("",)
        h, t, inv = gather(sim)
        self.assertTrue(all(w["bytes"] is None for w in inv.worktrees if not w["keep"] or w["bytes"] is not None))
        self.assertTrue(T.format_check(inv))

    def test_age_only_disk_is_never_sized(self):
        sim = linux_sim()
        cfg, h = hostcfg()
        h.disks.append(__import__("gatetrain.config", fromlist=["Disk"]).Disk("/trash", 0, measure="age-only"))
        inv = T.gather(h, sim, T.build_cfg(cfg, h), now=NOW)
        self.assertEqual([c for c in sim.calls if c.startswith("du -sk") and "/trash" in c], [])
        self.assertTrue(any(c.startswith("df -Pk") and "/trash" in c for c in sim.calls))
        old = next(x for x in inv.trash if x["path"] == "/trash/old")
        self.assertIsNone(old["bytes"])
        self.assertEqual(len(by_key(inv, "/trash/old")), 1)

    def test_measure_config_is_validated(self):
        base = {"name": "h", "disks": [{"path": "/x", "measure": "age-only"}]}
        self.assertEqual(parse({"hosts": [base]}).hosts[0].disks[0].measure, "age-only")
        with self.assertRaises(ConfigError):
            parse({"hosts": [{"name": "h", "disks": [{"path": "/x", "measure": "guess"}]}]})

    def test_size_timeout_is_configurable_and_passed_to_the_runner(self):
        seen = []
        sim = linux_sim()
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (seen.append(timeout) if cmd.startswith("du -sk") else None, orig(cmd, timeout))[1]
        gather(sim, {"size_timeout_s": 7})
        self.assertTrue(seen and set(seen) == {7})
        cfg, h = hostcfg()
        self.assertEqual(T.build_cfg(cfg, h).size_timeout_s, 30)

    def test_unknown_size_target_over_budget_is_not_judged_nor_cleaned(self):
        sim = linux_sim()
        sim.slow = ("/build/chain",)
        h, t, inv = gather(sim)
        chain = next(x for x in inv.targets if x["path"] == "/build/chain")
        self.assertIsNone(chain["bytes"])
        self.assertFalse(chain["over"])
        self.assertEqual(by_key(inv, "/build/chain"), [])                 # never planned on budget grounds
        self.assertFalse(any("/build/chain" in b for b in inv.breaches))
        self.assertIn("size unknown", T.format_check(inv))

    def test_clean_with_unknown_before_uses_df_delta(self):
        sim = linux_sim()
        sim.clean_needs_tag = False
        sim.slow = ("/build/targets/lane-a",)
        orig = sim.run
        def run(cmd, timeout=None):
            if cmd.startswith("cargo clean"):
                sim.free += 10
            return orig(cmd, timeout)
        sim.run = run
        h, t, inv = gather(sim, {"log": os.path.join(tempfile.mkdtemp(), "l.jsonl")})
        it = by_key(inv, "/build/targets/lane-a")[0]
        self.assertIsNone(it.bytes)
        ops = T.Ops(h, sim, t)
        after, status, note = T.clean_target(ops, t, it)
        self.assertEqual(status, "ok")
        self.assertIn("df", note)

    def test_clean_unmeasurable_never_claims_freed_nothing(self):
        sim = linux_sim()
        sim.clean_needs_tag = False
        sim.slow = ("/build/targets/lane-a",)
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (1, "") if cmd.startswith("df -Pk") else orig(cmd, timeout)
        h, t, inv = gather(sim)
        it = by_key(inv, "/build/targets/lane-a")[0]
        after, status, note = T.clean_target(T.Ops(h, sim, t), t, it)
        self.assertEqual(status, "ok")
        self.assertIn("freed: unknown", note)
        self.assertNotIn("nothing", note)


class CheckTests(unittest.TestCase):
    def test_inventory_and_plan(self):
        h, t, inv = gather(linux_sim())
        tg = {x["path"]: x for x in inv.targets}
        a = tg["/build/targets/lane-a"]
        self.assertIs(a["tag"], False)                       # missing CACHEDIR.TAG is detected
        self.assertIs(a["merged"], True)                     # squash-merged: added lines are present in main
        self.assertIs(tg["/build/targets/lane-b"]["merged"], False)
        self.assertEqual(len(by_key(inv, "/build/targets/lane-a")), 1)
        self.assertEqual(by_key(inv, "/build/targets/lane-b"), [])        # unmerged lane is kept
        self.assertEqual(by_key(inv, "/build/targets/lane-c"), [])        # keep list
        self.assertEqual(by_key(inv, "/build/targets/lane-new"), [])      # used a minute ago
        self.assertIn("not merged", tg["/build/targets/lane-b"]["keep"])
        chain = by_key(inv, "/build/chain")[0]
        self.assertTrue(chain.attended_only)                                # over budget: attended only
        self.assertTrue(any("over budget" in b for b in inv.breaches))

    def test_worktrees_containers_images_trash(self):
        h, t, inv = gather(linux_sim())
        wt = {w["path"]: w for w in inv.worktrees}
        self.assertEqual(wt["/repo/wt/lane-a"]["links"], 1)
        self.assertEqual(wt["/repo/wt/lane-d"]["keep"], "uncommitted work")
        self.assertEqual(wt["/repo/wt/lane-b"]["keep"], "not merged")
        self.assertEqual(wt["/repo/wt/lane-c"]["keep"], "keep list")
        self.assertEqual(wt["/repo/main"]["keep"], "main/root/keep list")
        self.assertEqual([i.key for i in inv.items if i.cls == "worktrees"], ["/repo/wt/lane-a", "/repo/wt/lane-new"])
        self.assertEqual([i.key for i in inv.items if i.cls == "containers"], ["gate-1"])   # running and other-prefix kept
        self.assertEqual([i.key for i in inv.items if i.cls == "images"], ["gate-img:1"])    # in use, unmatched, <none> kept
        self.assertEqual([i.key for i in inv.items if i.cls == "trash"], ["/trash/old"])

    def test_threshold_exit_code_and_table(self):
        sim = linux_sim()
        with tempfile.TemporaryDirectory() as d:
            p = write_conf(d, {})
            rc, out = cli(sim, "check", "--config", p)
            self.assertEqual(rc, 1)
            self.assertIn("would tidy", out)
            self.assertIn("THRESHOLD: disk /build: 5 GB free < 50", out)
            sim.free = 500
            sim.fs["/build/chain/debug/big.o"]["size"] = 1 * MB
            rc, out = cli(sim, "check", "--config", p)
            self.assertEqual(rc, 0, out)

    def test_profile_in_evidence_dir_is_a_breach(self):
        sim = linux_sim()
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (0, "/evidence/profile/SingletonLock\n") if cmd.startswith("find /evidence") else orig(cmd, timeout)
        h, t, inv = gather(sim, {"evidence_dirs": ["/evidence"]}, mins=1)
        self.assertEqual(inv.profiles, ["/evidence/profile/SingletonLock"])
        self.assertTrue(any("browser-profile" in b for b in inv.breaches))
        self.assertFalse([i for i in inv.items if "Singleton" in i.key])      # reported, never removed


class ApplyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.log = os.path.join(self.tmp, "log.jsonl")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_apply(self, sim, classes, extra=None, win=False):
        h, t, inv = gather(sim, dict(extra or {}, log=self.log), win)
        return T.apply_plan(h, sim, t, inv, classes, now=NOW), t

    def records(self):
        return [json.loads(l) for l in Path(self.log).read_text().splitlines()]

    def test_quiet_clean_refusal_is_detected_and_fixed(self):
        sim = linux_sim()
        before = sim.total("/build/targets/lane-a")
        (done, failed), _ = self.run_apply(sim, ["targets"])
        self.assertEqual((done, failed), (1, 0))
        self.assertEqual(sim.total("/build/targets/lane-a"), len(T.CARGO_TAG))
        self.assertIn("/build/targets/lane-a/CACHEDIR.TAG", sim.fs)
        self.assertIn(T.CARGO_TAG_SIG, sim.scripts["/build/targets/lane-a/CACHEDIR.TAG"])
        cleans = [c for c in sim.calls if c.startswith("cargo clean")]
        self.assertEqual(len(cleans), 2)                       # refused once, tagged, re-run
        self.assertFalse(any(re.search(r"(^| )(-q|--quiet)( |$)", c) for c in cleans))
        r = self.records()[0]
        self.assertEqual((r["before_bytes"], r["after_bytes"]), (before, 0))
        self.assertIn("refused silently", r["note"])

    def test_clean_that_frees_nothing_is_reported_failed(self):
        sim = linux_sim()
        sim.clean_needs_tag = False
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (0, "") if cmd.startswith("cargo clean") else orig(cmd, timeout)     # a clean that does nothing, with a tag present
        sim.add("/build/targets/lane-a/CACHEDIR.TAG", "file", 100, 10 * DAY)
        (done, failed), _ = self.run_apply(sim, ["targets"])
        self.assertEqual((done, failed), (0, 1))
        self.assertEqual(self.records()[0]["status"], "failed")

    def test_untouched_targets_stay(self):
        sim = linux_sim()
        keep = {k: dict(v) for k, v in sim.fs.items() if k.startswith(("/build/targets/lane-b", "/build/targets/lane-c", "/build/targets/lane-new", "/build/chain"))}
        self.run_apply(sim, ["targets"])
        for k, v in keep.items():
            self.assertEqual(sim.fs.get(k), v, k)

    def _win_sim(self):
        sim = SimHost(win=True)
        sim.add("C:/repo/main", age_s=0)
        for f in ("a.rs", "b.rs"):
            sim.add("C:/repo/main/" + f, "file", 10, 0)
        sim.add("C:/repo/main/node_modules", age_s=0)
        sim.add("C:/repo/main/node_modules/pkg.js", "file", 10, 0)
        sim.add("C:/repo/wt/lane-a", age_s=DAY)
        sim.add("C:/repo/wt/lane-a/src.rs", "file", 5, DAY)
        sim.add("C:/repo/wt/lane-a/node_modules", "link", target="C:/repo/main/node_modules")
        sim.worktrees = [("C:/repo/main", "main"), ("C:/repo/wt/lane-a", "line/lane-a")]
        sim.branches = ["main", "line/lane-a"]
        sim.ancestors = {"line/lane-a", "main"}
        return sim

    def test_windows_worktree_links_unlinked_and_main_untouched(self):
        sim = self._win_sim()
        naive = SimHost(win=True)                    # the danger is real in the simulation: a naive removal empties main
        naive.fs = {k: dict(v) for k, v in sim.fs.items()}
        naive.run('rmdir /S /Q "C:\\repo\\wt\\lane-a"')
        self.assertNotIn("C:/repo/main/node_modules/pkg.js", naive.fs)
        main_before = {k: dict(v) for k, v in sim.fs.items() if k.startswith("C:/repo/main")}
        h, t, inv = gather(sim, dict(WIN_EXTRA, log=self.log), win=True)
        done, failed = T.apply_plan(h, sim, t, inv, ["worktrees"], now=NOW)
        self.assertEqual((done, failed), (1, 0))
        self.assertEqual({k: v for k, v in sim.fs.items() if k.startswith("C:/repo/main")}, main_before)
        self.assertNotIn("C:/repo/wt/lane-a", sim.fs)
        calls = sim.calls
        i_unlink = next(i for i, c in enumerate(calls) if c.endswith('gatetrain-unlink.cmd"'))
        i_remove = next(i for i, c in enumerate(calls) if "worktree remove" in c)
        self.assertLess(i_unlink, i_remove)
        self.assertTrue(any(c.startswith("dir /AL /S /B") for c in calls))
        self.assertIn("archive/line/lane-a", sim.tags)
        self.assertIn("line/lane-a", sim.deleted_branches)
        script = sim.scripts["C:/tmp/gatetrain-unlink.cmd"]
        self.assertIn('rmdir "C:\\repo\\wt\\lane-a\\node_modules"', script)       # written as a file, backslashes intact
        self.assertFalse(any("echo" in c and "rmdir" in c for c in calls))        # never an inline echo/printf script

    def test_stuck_link_stops_before_anything_is_removed(self):
        sim = self._win_sim()
        sim.unlink_noop = True
        h, t, inv = gather(sim, dict(WIN_EXTRA, log=self.log), win=True)
        with self.assertRaises(T.GuardViolation):
            T.apply_plan(h, sim, t, inv, ["worktrees"], now=NOW)
        self.assertIn("C:/repo/main/node_modules/pkg.js", sim.fs)
        self.assertFalse(any("worktree remove" in c for c in sim.calls))
        self.assertEqual(self.records()[-1]["status"], "guard-violation")

    def test_listing_that_did_not_run_is_not_trusted(self):
        sim = self._win_sim()
        sim.broken_links = True
        with self.assertRaises(T.GuardViolation):
            gather(sim, dict(WIN_EXTRA, log=self.log), win=True)

    def test_canary_must_return_expected_count(self):
        sim = self._win_sim()
        sim.add("C:/canary", age_s=0)
        sim.add("C:/canary/l1", "link", target="C:/repo/main/a.rs")
        with self.assertRaises(T.GuardViolation):
            gather(sim, dict(WIN_EXTRA, link_canary="C:/canary", link_canary_count=2), win=True)
        gather(sim, dict(WIN_EXTRA, link_canary="C:/canary", link_canary_count=1), win=True)

    def test_main_checkout_change_is_caught(self):
        sim = self._win_sim()
        orig = sim.run

        def evil(cmd, timeout=None):
            rc, out = orig(cmd, timeout)
            if "worktree remove" in cmd:
                sim.fs.pop("C:/repo/main/a.rs", None)       # something deletes a tracked file meanwhile
            return rc, out
        sim.run = evil
        h, t, inv = gather(sim, dict(WIN_EXTRA, log=self.log), win=True)
        with self.assertRaises(T.GuardViolation) as cm:
            T.apply_plan(h, sim, t, inv, ["worktrees"], now=NOW)
        self.assertIn("main checkout changed", str(cm.exception))

    def test_keep_list_path_is_never_removed(self):
        sim = linux_sim()
        h, t, inv = gather(sim)
        ops = T.Ops(h, sim, t)
        for p in ("/repo/main", "/repo/root", "/repo", "/", "/build/*"):
            with self.assertRaises(T.GuardViolation, msg=p):
                ops.remove_dir(p)
        self.assertIn("/repo/main/a.rs", sim.fs)

    def test_containers_images_cache_trash_exact_and_safe(self):
        sim = linux_sim()
        (done, failed), t = self.run_apply(sim, ["containers", "images", "cache", "trash"])
        self.assertEqual(failed, 0)
        self.assertEqual(sim.containers_exited, ["gate-9", "other-1"])     # running gate-9 and other-prefix stay
        self.assertEqual([i.split("|")[0] for i in sim.images], ["gate-img:2", "keepme:1", "gate-img:<none>"])
        self.assertNotIn("/trash/old", sim.fs)
        self.assertIn("/trash/new", sim.fs)
        self.assertIsNone(sim.cache_row)
        self.assertTrue(any("until=72h" in c for c in sim.calls))
        self.assertFalse(any("system prune" in c for c in sim.calls))
        self.assertFalse(any(re.search(r"rm -r\w* .*[*?]", c) for c in sim.calls))

    def test_bulk_prune_is_refused_by_the_wrapper(self):
        sim = linux_sim()
        h, t, inv = gather(sim)
        with self.assertRaises(T.GuardViolation):
            T.Ops(h, sim, t).run("docker system prune -af")

    def test_action_log_is_append_only_jsonl(self):
        sim = linux_sim()
        self.run_apply(sim, ["containers"])
        first = Path(self.log).read_text()
        self.run_apply(sim, ["trash"])
        text = Path(self.log).read_text()
        self.assertTrue(text.startswith(first))                              # appended, never rewritten
        recs = [json.loads(l) for l in text.splitlines()]
        self.assertEqual([r["class"] for r in recs], ["containers", "trash"])
        for r in recs:
            self.assertTrue({"ts", "host", "action", "item", "before_bytes", "after_bytes", "status"} <= set(r))
        self.assertEqual(recs[1]["before_bytes"], 50 * MB)
        self.assertEqual(recs[1]["after_bytes"], 0)

    def test_first_violation_stops_the_run(self):
        sim = linux_sim()
        h, t, inv = gather(sim, dict(log=self.log))
        inv.items.insert(0, T.Item("targets", "clean", "/repo/main", 1, "poisoned"))
        with self.assertRaises(T.GuardViolation):
            T.apply_plan(h, sim, t, inv, ["targets", "containers"], now=NOW)
        self.assertEqual(sim.containers_exited, ["gate-1", "gate-9", "other-1"])    # nothing after it ran
        self.assertEqual(len(self.records()), 1)


class AutoTests(unittest.TestCase):
    def test_auto_runs_only_unattended_classes_and_summarises(self):
        sim = linux_sim()
        sim.free = 500
        sim.fs["/build/chain/debug/big.o"]["size"] = 1 * MB
        with tempfile.TemporaryDirectory() as d:
            p = write_conf(d, {"log": os.path.join(d, "l.jsonl")})
            rc, out = cli(sim, "tidy", "--auto", "--config", p)
            self.assertEqual(rc, 0, out)
            self.assertLessEqual(len(out.strip().splitlines()), 5)
            self.assertEqual(sim.total("/build/targets/lane-a"), len(T.CARGO_TAG))
            self.assertEqual(sim.containers_exited, ["gate-9", "other-1"])
            self.assertNotIn("/trash/old", sim.fs)
            self.assertEqual(len(sim.images), 4)                    # images not unattended
            self.assertIn("/repo/wt/lane-a", sim.fs)                # worktrees not unattended
            self.assertEqual(sim.deleted_branches, [])
            self.assertIsNotNone(sim.cache_row)                     # cache not unattended

    def test_dry_run_changes_nothing(self):
        sim = linux_sim()
        snap = ({k: dict(v) for k, v in sim.fs.items()}, list(sim.containers_exited), list(sim.images))
        with tempfile.TemporaryDirectory() as d:
            p = write_conf(d, {"log": os.path.join(d, "l")}, disk=False)
            rc, out = cli(sim, "tidy", "--config", p)
            self.assertIn("dry run", out)
            self.assertFalse(os.path.exists(os.path.join(d, "l")))
        self.assertEqual(snap, (sim.fs, sim.containers_exited, sim.images))
        self.assertFalse(any(c.startswith(("cargo", "docker rm", "unlink", "rm ")) for c in sim.calls))


class ConfigAndGitTests(unittest.TestCase):
    def test_unknown_key_and_bad_class(self):
        cfg, h = hostcfg({"nope": 1})
        with self.assertRaises(ConfigError):
            T.build_cfg(cfg, h)
        cfg, h = hostcfg({"unattended": ["everything"]})
        with self.assertRaises(ConfigError):
            T.build_cfg(cfg, h)

    def test_defaults_are_conservative(self):
        self.assertEqual(T.TidyCfg().unattended, ["targets", "containers", "trash"])
        self.assertEqual(T.TidyCfg().cache_days, 0)

    @unittest.skipUnless(shutil.which("git"), "git needed")
    def test_merged_by_content_against_real_git(self):
        from gatetrain.runner import LocalRunner
        with tempfile.TemporaryDirectory() as d:
            def g(*a):
                subprocess.run(["git", "-C", d, "-c", "user.name=t", "-c", "user.email=tester", *a], check=True, capture_output=True)
            g("init", "-b", "main")
            Path(d, "f.txt").write_text("one\n"); g("add", "f.txt"); g("commit", "-m", "base")
            g("checkout", "-b", "lane-x"); Path(d, "f.txt").write_text("one\ntwo\nthree\n"); g("commit", "-am", "x")
            g("checkout", "-b", "lane-y", "main"); Path(d, "f.txt").write_text("one\nfour\n"); g("commit", "-am", "y")
            g("checkout", "main"); Path(d, "f.txt").write_text("one\ntwo\nthree\nfive\n"); g("commit", "-am", "squash of x, plus more")
            cfg = parse({"hosts": [{"name": "h", "tidy": {"main_checkout": d}}]})
            host = cfg.hosts[0]
            ops = T.Ops(host, LocalRunner(), T.build_cfg(cfg, host))
            self.assertTrue(ops.merged("lane-x"))        # not an ancestor, but every added line is on main
            self.assertFalse(ops.merged("lane-y"))
            self.assertTrue(ops.merged("main"))


if __name__ == "__main__":
    unittest.main()
