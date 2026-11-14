"""GT-DIAG: failure-class diagnosis, random-port scan, untracked generated files, baseline radar. All fixtures are synthetic."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_lanes import base_repo, lanes  # noqa: E402
from test_ops import FakeRunner, host, status_of  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent


def mklog(steps, par=None):
    """steps: (name, start second, duration, rc, [output lines]); a synthetic chain log in docs/log-format.md style."""
    out = ["branch batch-a at 0a1b2c3d4 on builder-1", "CHAIN_MODE full"]
    if par:
        out.append(f"CHAIN_PARALLEL {par}")
    reds = []
    for name, st, dur, rc, lines in steps:
        h, m, s = st // 3600, st % 3600 // 60, st % 60
        out += [f"STEP_START {name} 2026-11-11T{9 + h:02d}:{m:02d}:{s:02d}+07:00", f"=== {name}", *lines,
                f"STEP_TIME {name} {dur}s", f"STEP_EXIT {name} {rc}"]
        if rc:
            reds.append(f"{name}(rc={rc})")
    out += ["RED_SUMMARY: " + (" ".join(reds) or "none"), "GATES_DONE"]
    return "\n".join(out) + "\n"


def normal(n=5):
    return [(f"step-{i:03d}", i * 20, 20, 0, []) for i in range(n)]


def one_red(lines, dur=20, rc=1):
    return mklog(normal() + [("db-gate-001", 100, dur, rc, lines)])


def diag(text, **kw):
    from gatetrain.diagnose import diagnose_text
    return diagnose_text(text, **kw)


def format_text_of(findings):
    from gatetrain.diagnose import format_findings
    return format_findings(findings)


def only(findings):
    assert len(findings) == 1, findings
    return findings[0]


HOOK = ["Error: Hook timed out in 10000ms.", " Test Files  1 failed | 9 passed (10)", "      Tests  0 failed | 120 passed (120)"]
ASSERT = ["AssertionError: expected 3 to be 4", " Test Files  1 failed | 9 passed (10)", "      Tests  1 failed | 119 passed (120)"]


def gt(*a):
    return subprocess.run([sys.executable, "-m", "gatetrain", *a], cwd=ROOT, capture_output=True, text=True, encoding="utf-8")


class TestClasses(unittest.TestCase):
    def test_load_flake(self):
        f = only(diag(one_red(HOOK, dur=400)))
        self.assertEqual(f.cls, "flake-load")
        self.assertIn("Hook timed out", f.evidence)
        self.assertIn("timeout", f.remedy)
        self.assertTrue(f.rerun)

    def test_load_flake_negative(self):
        self.assertEqual(only(diag(one_red(ASSERT, dur=400))).cls, "real")  # a real assertion on a slow step
        self.assertEqual(only(diag(one_red(HOOK, dur=20))).cls, "real")     # a timeout with no load evidence

    def test_infra_port(self):
        f = only(diag(one_red(["docker: Error response from daemon: failed to bind host port 0.0.0.0:5501/tcp: address already in use"])))
        self.assertEqual(f.cls, "infra-port")
        self.assertIn("ephemeral", f.remedy)
        self.assertEqual(only(diag(one_red(ASSERT))).cls, "real")

    def test_generated_uncommitted(self):
        f = only(diag(one_red(["FAIL gen-sync: on-disk output differs from the build", "  missing: gen/new-file.json"])))
        self.assertEqual(f.cls, "generated-uncommitted")
        self.assertIn("git add", f.remedy)
        self.assertEqual(only(diag(one_red(["SyntaxError: missing: ) after argument list"]))).cls, "real")

    def test_baseline_shrink(self):
        for line in ("expected length 15 got 14", "item-a is now witnessed, delete the entry", "route-b now reaches the gate, delete it"):
            f = only(diag(one_red([line])))
            self.assertEqual(f.cls, "baseline-shrink", line)
            self.assertIn("same commit", f.remedy)
        self.assertEqual(only(diag(one_red(["expected length 15 got 16"]))).cls, "real")  # growth is not a shrink

    def test_baseline_missing(self):
        for line in ("NEW route /widgets not in the allow-list", "table gadgets: nobody classified it"):
            self.assertEqual(only(diag(one_red([line]))).cls, "baseline-missing", line)

    def test_platform_windows(self):
        f = only(diag(one_red(["The command line is too long."])))
        self.assertEqual(f.cls, "platform-windows")
        self.assertIn("Linux", f.remedy)
        f = only(diag(one_red(["allow-list: expected 'src/a/b.ts' received 'src\\a\\b.ts'"])))
        self.assertEqual(f.cls, "platform-windows")
        self.assertEqual(only(diag(one_red(["TypeError: x is not a function"]))).cls, "real")

    def test_unknown_red_is_real(self):
        f = only(diag(one_red(["something strange"])))
        self.assertEqual(f.cls, "real")
        self.assertFalse(f.rerun)

    def test_green_has_no_findings(self):
        self.assertEqual(diag(mklog(normal())), [])

    def test_extra_rules_as_data(self):
        from gatetrain.diagnose import RULES
        extra = [{"id": "x", "cls": "infra-quota", "pattern": "quota exceeded", "remedy": "wait", "rerun": True}]
        self.assertEqual(only(diag(one_red(["quota exceeded for key"]), rules=extra + RULES)).cls, "infra-quota")

    def test_rules_table_is_data(self):
        from gatetrain.diagnose import RULES
        self.assertTrue(all({"cls", "pattern", "remedy", "rerun"} <= set(r) for r in RULES))


class TestParallelism(unittest.TestCase):
    def serial(self):
        return mklog([(f"s{i}", i * 10, 10, 0, []) for i in range(4)], par=2)

    def test_note(self):
        from gatetrain.diagnose import parallelism_note
        from gatetrain.parse import parse_log
        p = parse_log(self.serial())
        self.assertIn("parallelism 2 is below the configured 4", parallelism_note(p, 4) or "")
        self.assertIsNone(parallelism_note(p, 2))
        self.assertIsNone(parallelism_note(p, None))

    def test_cli(self):
        with tempfile.TemporaryDirectory() as d:
            lg = Path(d, "run-x.log")
            lg.write_text(self.serial())
            for sub in ("diagnose", "report", "eta"):
                r = gt(sub, str(lg), "--parallel", "4", *(["--db", ":memory:"] if sub == "eta" else []))
                self.assertIn("parallelism 2 is below the configured 4", r.stdout, sub + r.stderr)


class TestCli(unittest.TestCase):
    def test_diagnose_prints_class_evidence_remedy_rerun(self):
        with tempfile.TemporaryDirectory() as d:
            lg = Path(d, "run-y.log")
            lg.write_text(one_red(HOOK, dur=400))
            r = gt("diagnose", str(lg))
            self.assertEqual(r.returncode, 0, r.stderr)
            for want in ("db-gate-001", "flake-load", "Hook timed out", "remedy:", "full rerun needed: yes", "zero-red"):
                self.assertIn(want, r.stdout)

    def test_report_includes_diagnosis(self):
        with tempfile.TemporaryDirectory() as d:
            lg = Path(d, "run-z.log")
            lg.write_text(one_red(ASSERT))
            r = gt("report", str(lg))
            self.assertIn("diagnosis:", r.stdout)
            self.assertIn("real", r.stdout)


class TestRandomPorts(unittest.TestCase):
    JS = "const port = 5000 + ((process.pid * 7 + Date.now()) % 60000);\n"
    SAFE = "const port = 20000 + ((process.pid * 7 + Date.now()) % 12000);\n"

    def test_scan(self):
        from gatetrain.portscan import scan_random_ports
        rng = lambda s: [(a, b) for _, a, b in scan_random_ports(s)]
        self.assertEqual(rng(self.JS), [(5000, 64999)])
        self.assertEqual(rng(self.SAFE), [(20000, 31999)])
        self.assertEqual(rng("port = random.randint(40000, 50000)\n"), [(40000, 50000)])
        self.assertEqual(rng("P=$((30000 + RANDOM % 10000))\n"), [(30000, 39999)])
        self.assertEqual(rng("const timeout = 1000 + (n % 7);\n"), [])

    def _check(self, script, rules, **kw):
        from gatetrain.doctor import check_random_ports
        h = host(gate_scripts=["gates/db.mjs"], **kw)
        return check_random_ports(h, FakeRunner([("@@FILE", 0, "@@FILE gates/db.mjs\n" + script)] + rules))

    def test_doctor_fails_when_range_reaches_ephemeral(self):
        rng = [("ip_local_port_range", 0, "32768\t60999\n"), ("ip_local_reserved_ports", 0, "\n")]
        r = self._check(self.JS, rng)
        self.assertEqual(status_of(r), ["FAIL"])
        self.assertIn("20000-31999", r[0].fix)
        self.assertEqual(status_of(self._check(self.SAFE, rng)), ["PASS"])

    def test_doctor_kernel_reserved_covers_range(self):
        rng = [("ip_local_port_range", 0, "32768\t60999\n"), ("ip_local_reserved_ports", 0, "40000-50000\n")]
        script = "const port = 40000 + ((process.pid * 3 + Date.now()) % 10001);\n"
        self.assertEqual(status_of(self._check(script, rng)), ["PASS"])

    def test_doctor_windows_dynamic_range(self):
        from gatetrain.doctor import check_random_ports
        h = host(os="windows", gate_scripts=["g.mjs"])
        w = FakeRunner([("show dynamicport tcp", 0, "Start Port      : 49152\nNumber of Ports : 16384\n"),
                        ("show excludedportrange", 0, ""),
                        ("@@FILE", 0, "@@FILE g.mjs\nport = 45000 + ((process.pid + Date.now()) % 10000)\n")])
        self.assertEqual(status_of(check_random_ports(h, w)), ["FAIL"])

    def test_no_scripts_no_result(self):
        from gatetrain.doctor import check_random_ports
        self.assertEqual(check_random_ports(host(), FakeRunner()), [])


class TestUntrackedGenerated(unittest.TestCase):
    def test_lists_new_generated_file(self):
        from gatetrain.stackcheck import untracked_generated
        r = base_repo({"gen/a.json": "{}", "src/x.txt": "1"})
        Path(r.p, "gen/new.json").write_text("{}")      # new generated file, never added
        Path(r.p, "scratch.txt").write_text("x")        # untracked but outside the generated dirs
        self.assertEqual(untracked_generated(r.p, ["gen"]), ["gen/new.json"])
        self.assertEqual(untracked_generated(r.p, ["other"]), [])

    def test_cli_and_config(self):
        r = base_repo({"gen/a.json": "{}"})
        Path(r.p, "gen/new.json").write_text("{}")
        with tempfile.TemporaryDirectory() as d:
            cfg = Path(d, "hosts.toml")
            cfg.write_text('[stack]\ngenerated_dirs = ["gen"]\n')
            run = gt("generated", "--repo", r.p, "--config", str(cfg))
            self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
            self.assertIn("gen/new.json", run.stdout)
            subprocess.run(["git", "add", "gen/new.json"], cwd=r.p, check=True)
            run = gt("generated", "--repo", r.p, "--config", str(cfg))
            self.assertEqual(run.returncode, 0, run.stdout + run.stderr)

    def test_config_parses(self):
        from gatetrain.config import ConfigError, parse
        self.assertEqual(parse({"stack": {"generated_dirs": ["a", "b"]}}).stack["generated_dirs"], ["a", "b"])
        with self.assertRaises(ConfigError):
            parse({"stack": {"generated_dirs": "a"}})


class TestBaselineRadar(unittest.TestCase):
    FILES = {"api/routes.ts": "app.get('/a', h)\n", "tests/known-routes.txt": "/a\n"}

    def _lane(self, touch_baseline):
        r = base_repo(self.FILES)
        r.branch("lane-a")
        new = {"api/routes.ts": "app.get('/a', h)\napp.get('/b', h)\n"}
        if touch_baseline:
            new["tests/known-routes.txt"] = "/a\n/b\n"
        r.commit_files("add route", new)
        return r

    def test_warns_when_route_added_without_baseline(self):
        w = lanes(self._lane(False), "lane-a", extra=["--baseline-files", "tests/known-*.txt"])["baseline_warnings"]
        self.assertEqual([x["lane"] for x in w], ["lane-a"])
        self.assertIn("app.get('/b'", w[0]["adds"][0])

    def test_quiet_when_baseline_touched_or_unconfigured(self):
        self.assertEqual(lanes(self._lane(True), "lane-a", extra=["--baseline-files", "tests/known-*.txt"])["baseline_warnings"], [])
        self.assertEqual(lanes(self._lane(False), "lane-a")["baseline_warnings"], [])


if __name__ == "__main__":
    unittest.main()
