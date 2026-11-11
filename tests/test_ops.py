"""Tests for config, doctor, status, multi-log next-tick and stop. Runner is always a FakeRunner."""
import contextlib, io, json, os, tempfile, unittest
from pathlib import Path
from unittest import mock

FIX = Path(__file__).parent / "fixtures"
GREEN, PARTIAL, STOPPED = (str(FIX / n) for n in ("run-one-red.log", "run-partial-red.log", "run-stopped.log"))
NOW = "2026-11-11T18:15:00+07:00"
ROOT = Path(__file__).resolve().parent.parent


class FakeRunner:
    """rules: list of (substring, rc, out); out may be a list consumed one per call (last one repeats)."""

    def __init__(self, rules=()):
        self.rules = [[s, rc, list(o) if isinstance(o, list) else o] for s, rc, o in rules]
        self.calls = []

    def run(self, cmd):
        self.calls.append(cmd)
        for r in self.rules:
            if r[0] in cmd:
                out = r[2]
                if isinstance(out, list):
                    out = out.pop(0) if len(out) > 1 else out[0]
                return r[1], out
        return 127, ""


def host(**kw):
    from gatetrain.config import parse
    d = {"name": "h1", "kind": "server", "reach": "local", "os": "linux"}
    d.update(kw)
    return parse({"hosts": [d]}).hosts[0]


def cli(*argv):
    from gatetrain.cli import main
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        rc = main(list(argv))
    return rc, buf.getvalue()


def status_of(results, check=None):
    return [r.status for r in results if check is None or r.check == check]


def df(gb):
    return f"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/a 9 9 {gb * 1024 * 1024} 1% /m\n"


class TestConfig(unittest.TestCase):
    def test_example_toml_loads(self):
        from gatetrain.config import load
        cfg = load(str(ROOT / "config" / "hosts.example.toml"))
        self.assertEqual(cfg.profile, "mixed")
        self.assertEqual([h.name for h in cfg.hosts], ["builder-1", "workstation-1", "cloud-runner"])
        self.assertEqual(cfg.hosts[0].disks[0].min_free_gb, 100)
        self.assertTrue(cfg.hosts[0].disks[0].fast)
        self.assertEqual(cfg.hosts[0].disks[0].use, ["chain", "cache"])
        self.assertTrue(cfg.hosts[1].disks[1].removable)
        self.assertEqual(cfg.needs.chain_gb, 120)
        self.assertEqual(len(cfg.models), 3)
        self.assertFalse(cfg.hosts[2].enabled)

    def test_error_names_the_field(self):
        from gatetrain.config import ConfigError, parse
        with self.assertRaises(ConfigError) as cm:
            parse({"hosts": [{"name": "a", "kind": "server"}, {"name": "b", "kind": "server",
                                                                 "disks": [{"path": "/x", "min_free_gb": "lots"}]}]})
        self.assertIn("hosts[1].disks[0].min_free_gb", str(cm.exception))
        with self.assertRaises(ConfigError) as cm:
            parse({"hosts": [{"name": "a", "kind": "toaster"}]})
        self.assertIn("hosts[0].kind", str(cm.exception))
        with self.assertRaises(ConfigError) as cm:
            parse({"profile": "free"})
        self.assertIn("profile", str(cm.exception))
        with self.assertRaises(ConfigError) as cm:
            parse({"hosts": [{"name": "a", "kind": "server", "disks": [{"path": "/x", "use": ["chain", "toys"]}]}]})
        self.assertIn("hosts[0].disks[0].use", str(cm.exception))
        with self.assertRaises(ConfigError) as cm:
            parse({"needs": {"chain_gb": "big"}})
        self.assertIn("needs.chain_gb", str(cm.exception))


class TestDoctorChecks(unittest.TestCase):
    def test_disk(self):
        from gatetrain.doctor import check_disks
        h = host(disks=[{"path": "/srv/build", "min_free_gb": 100, "fast": True}])
        r = check_disks(h, FakeRunner([("df -Pk", 0, df(50))]))
        self.assertEqual(status_of(r), ["FAIL"])
        self.assertIn("L18", r[0].lessons)
        self.assertEqual(status_of(check_disks(h, FakeRunner([("df -Pk", 0, df(200))]))), ["PASS"])

    def test_disk_not_fast(self):
        from gatetrain.doctor import check_disks
        h = host(disks=[{"path": "/srv/build", "min_free_gb": 10, "fast": False}])
        ok = FakeRunner([("df -Pk", 0, df(200))])
        r = check_disks(h, ok)
        self.assertIn("WARN", status_of(r))
        self.assertTrue(any("L19" in x.lessons for x in r))
        h2 = host(disks=[{"path": "/srv/archive", "min_free_gb": 10, "fast": False, "use": ["archive"]}])
        self.assertNotIn("WARN", status_of(check_disks(h2, ok)))

    def test_removable_disk_absent_is_warn_not_fail(self):
        from gatetrain.doctor import check_disks
        h = host(disks=[{"path": "/mnt/ext", "min_free_gb": 10, "fast": True, "removable": True}])
        self.assertEqual(status_of(check_disks(h, FakeRunner([("df -Pk", 1, "")]))), ["WARN"])
        h2 = host(disks=[{"path": "/mnt/ext", "min_free_gb": 10, "fast": True}])
        self.assertEqual(status_of(check_disks(h2, FakeRunner([("df -Pk", 1, "")]))), ["FAIL"])

    def test_capacity_ssd_enables_chain(self):
        from gatetrain.doctor import check_capacity
        h = host(roles=["chain"], disks=[{"path": "/srv/ssd", "min_free_gb": 50, "fast": True, "use": ["chain"]}])
        r = check_capacity(h, FakeRunner([("df -Pk", 0, df(150))]))
        self.assertEqual(status_of(r), ["PASS"])
        self.assertIn("can run now: chain", r[0].message)

    def test_capacity_unmounted_ssd_drops_chain_to_warn(self):
        from gatetrain.doctor import check_capacity
        h = host(roles=["chain"], disks=[{"path": "/mnt/ext", "min_free_gb": 50, "fast": True, "use": ["chain"], "removable": True}])
        r = check_capacity(h, FakeRunner([("df -Pk", 1, "")]))
        self.assertEqual(status_of(r), ["WARN"])
        self.assertIn("chain", r[0].message)
        self.assertIn("/mnt/ext", r[0].message)

    def test_capacity_slow_disk_does_not_enable_chain(self):
        from gatetrain.doctor import check_capacity
        h = host(roles=["chain"], disks=[{"path": "/srv/hdd", "min_free_gb": 50, "fast": False, "use": ["chain"]}])
        r = check_capacity(h, FakeRunner([("df -Pk", 0, df(500))]))
        self.assertEqual(status_of(r), ["FAIL"])
        from gatetrain.config import Needs
        r2 = check_capacity(h, FakeRunner([("df -Pk", 0, df(500))]), Needs(chain_needs_fast=False))
        self.assertEqual(status_of(r2), ["PASS"])

    def test_capacity_lists_kinds_by_space_and_use(self):
        from gatetrain.doctor import check_capacity
        h = host(roles=["preflight", "lane-gates", "chain"],
                 disks=[{"path": "/mnt/a", "min_free_gb": 5, "fast": True, "use": ["preflight", "lane-gates"]}])
        r = check_capacity(h, FakeRunner([("df -Pk", 0, df(30))]))
        self.assertIn("can run now: preflight", r[0].message)
        self.assertNotIn("lane-gates", r[0].message.split("can run now:")[1].split(";")[0])  # 30 GB < 40 GB
        self.assertEqual(status_of(r), ["FAIL"])  # chain role, nothing takes a chain

    def test_containers_listed_never_removed(self):
        from gatetrain.doctor import check_containers
        h = host(container_prefix="mr-")
        two = FakeRunner([("docker ps -a --filter status=exited", 0, "mr-gate-a\nmr-gate-b\nother-db\n")])
        r = check_containers(h, two)
        self.assertEqual(status_of(r), ["FAIL"])
        self.assertIn("mr-gate-a", r[0].message)
        self.assertIn("mr-gate-b", r[0].message)
        self.assertNotIn("other-db", r[0].message)
        self.assertIn("docker rm mr-gate-a", r[0].fix)
        self.assertFalse(any("docker rm" in c for c in two.calls))
        none = FakeRunner([("docker ps -a", 0, "other-db\n")])
        self.assertEqual(status_of(check_containers(h, none)), ["PASS"])

    def test_stray_watchers(self):
        from gatetrain.doctor import check_watchers
        h = host(watcher_max_age_min=30)
        old = FakeRunner([("ps -eo pid=,etimes=,args=", 0, "200 7200 tail -n 0 -F /var/log/chain.log\n201 10 tail -F /x\n202 9000 vim a\n")])
        r = check_watchers(h, old)
        self.assertEqual(status_of(r), ["FAIL"])
        self.assertIn("200", r[0].message)
        self.assertNotIn("201", r[0].message)
        self.assertNotIn("202", r[0].message)
        new = FakeRunner([("ps -eo pid=,etimes=,args=", 0, "201 10 tail -F /x\n")])
        self.assertEqual(status_of(check_watchers(h, new)), ["PASS"])

    def test_toolchain(self):
        from gatetrain.doctor import check_tools
        h = host(tools=["node", "cargo"])
        r = check_tools(h, FakeRunner([("node --version", 0, "v22.1.0\n"), ("cargo --version", 127, "")]))
        self.assertEqual(status_of(r), ["PASS", "FAIL"])
        self.assertIn("v22.1.0", r[0].message)
        self.assertTrue(all("L29" in x.lessons for x in r))

    def test_ports(self):
        from gatetrain.doctor import check_ports
        h = host(ports=[5500, 40000])
        rng = FakeRunner([("ip_local_port_range", 0, "32768\t60999\n")])
        r = check_ports(h, rng)
        self.assertEqual(status_of(r), ["PASS", "FAIL"])
        self.assertIn("40000", " ".join(x.message for x in r if x.status == "FAIL"))
        self.assertEqual(status_of(check_ports(host(ports=[5500, 5501]), rng)), ["PASS", "PASS"])

    def test_ports_duplicates(self):
        from gatetrain.doctor import check_ports
        rng = FakeRunner([("ip_local_port_range", 0, "32768\t60999\n")])
        r = check_ports(host(ports=[5500, 5501, 5500]), rng)
        self.assertIn("FAIL", status_of(r))
        self.assertTrue(any("duplicate" in x.message and "5500" in x.message for x in r))

    def test_ports_reserved_range(self):
        from gatetrain.doctor import check_ports
        rng = FakeRunner([("ip_local_port_range", 0, "32768\t60999\n")])
        r = check_ports(host(ports=[5500, 6001], reserved_port_ranges=["6000-6100"]), rng)
        self.assertEqual(status_of(r), ["PASS", "FAIL"])

    def test_ports_kernel_reserved_inside_ephemeral(self):
        # a fixed gate port inside the Linux ephemeral range is safe when the kernel reserves it (found on a real build host)
        from gatetrain.doctor import check_ports
        rng = FakeRunner([("ip_local_port_range", 0, "32768\t60999\n"), ("ip_local_reserved_ports", 0, "55400-58399,6000\n")])
        r = check_ports(host(ports=[57901, 40000]), rng)
        self.assertEqual(status_of(r), ["PASS", "FAIL"])
        self.assertIn("reserved by the kernel", r[0].message)

    def test_ports_windows(self):
        from gatetrain.doctor import check_ports
        w = FakeRunner([("show dynamicport tcp", 0, "Protocol tcp Dynamic Port Range\n---\nStart Port      : 49152\nNumber of Ports : 16384\n"),
                        ("show excludedportrange protocol=tcp", 0, "Start Port    End Port\n----------    --------\n      5600        5699\n     50000       50059     *\n")])
        r = check_ports(host(os="windows", ports=[5500, 5650, 49200]), w)
        self.assertEqual(status_of(r), ["PASS", "FAIL", "FAIL"])

    def test_shared_build_output(self):
        from gatetrain.doctor import check_shared_build
        h = host(worktrees=[{"name": "a", "build_dir": "/w/a/target"}, {"name": "b", "build_dir": "/w/b/../a/target/"}])
        r = check_shared_build(h, FakeRunner())
        self.assertEqual(status_of(r), ["FAIL"])
        self.assertIn("L17", r[0].lessons)
        h2 = host(worktrees=[{"name": "a", "build_dir": "/w/a/target"}, {"name": "b", "build_dir": "/w/b/target"}])
        self.assertEqual(status_of(check_shared_build(h2, FakeRunner())), ["PASS"])

    def test_shared_build_output_through_a_link(self):
        from gatetrain.doctor import check_shared_build
        h = host(worktrees=[{"name": "a", "build_dir": "/w/a/target"}, {"name": "b", "build_dir": "/w/b/target"}])
        rp = FakeRunner([("realpath /w/a/target", 0, "/real/t\n"), ("realpath /w/b/target", 0, "/real/t\n")])
        self.assertEqual(status_of(check_shared_build(h, rp)), ["FAIL"])

    def test_lock(self):
        from gatetrain.doctor import check_lock
        h = host(lock_file="/run/chain.lock", lock_max_age_hours=6)
        late = str(1000 + 10 * 3600) + "\n"
        old = FakeRunner([("stat -c %Y", 0, "1000\n"), ("date +%s", 0, late), ("grep -c released", 1, "0\n")])
        self.assertEqual(status_of(check_lock(h, old)), ["FAIL"])
        released = FakeRunner([("stat -c %Y", 0, "1000\n"), ("date +%s", 0, late), ("grep -c released", 0, "1\n")])
        self.assertEqual(status_of(check_lock(h, released)), ["PASS"])
        young = FakeRunner([("stat -c %Y", 0, "1000\n"), ("date +%s", 0, "2000\n"), ("grep -c released", 1, "0\n")])
        self.assertEqual(status_of(check_lock(h, young)), ["PASS"])
        self.assertEqual(status_of(check_lock(h, FakeRunner([("stat -c %Y", 1, "")]))), ["PASS"])

    def test_chain_running_warns(self):
        from gatetrain.doctor import check_chain
        h = host(chain_pattern="run-chain")
        up = FakeRunner([("ps -eo pid=,args=", 0, "300 /bin/sh run-chain.sh b9\n301 sleep 1\n")])
        r = check_chain(h, up)
        self.assertEqual(status_of(r), ["WARN"])
        self.assertIn("300", r[0].message)
        self.assertIn("L26", r[0].lessons)
        self.assertEqual(status_of(check_chain(h, FakeRunner([("ps -eo pid=,args=", 0, "301 sleep 1\n")]))), ["PASS"])

    def test_cache_server(self):
        from gatetrain.doctor import check_cache
        h = host(cache_server_check="cachectl --show-stats")
        self.assertEqual(status_of(check_cache(h, FakeRunner([("cachectl --show-stats", 0, "ok")]))), ["PASS"])
        self.assertEqual(status_of(check_cache(h, FakeRunner([("cachectl --show-stats", 1, "")]))), ["FAIL"])

    def test_pollers(self):
        from gatetrain.doctor import check_pollers
        h = host(max_pollers=2)
        many = FakeRunner([("ps -eo args=", 0, "sshd: me@notty\nsshd: me@notty\nsshd: me@pts/0\n/usr/sbin/sshd -D\n")])
        self.assertEqual(status_of(check_pollers(h, many)), ["FAIL"])
        few = FakeRunner([("ps -eo args=", 0, "sshd: me@pts/0\n/usr/sbin/sshd -D\n")])
        self.assertEqual(status_of(check_pollers(h, few)), ["PASS"])

    def test_unconfigured_checks_are_absent(self):
        from gatetrain.doctor import run_checks
        self.assertEqual(run_checks(host(), FakeRunner()), [])

    def test_runner_error_is_a_fail(self):
        from gatetrain.doctor import run_checks

        class Boom:
            def run(self, cmd):
                raise OSError("no ssh")
        self.assertEqual(status_of(run_checks(host(tools=["node"]), Boom())), ["FAIL"])


class TestDoctorCli(unittest.TestCase):
    def cfg(self, tmp):
        p = os.path.join(tmp, "h.toml")
        Path(p).write_text('profile = "zero-cost"\n[[hosts]]\nname = "h1"\nkind = "local"\nreach = "local"\ntools = ["node"]\n'
                           '[[hosts]]\nname = "h2"\nkind = "server"\nreach = "ssh h2"\ntools = ["node"]\n')
        return p

    def test_text_json_exit(self):
        import gatetrain.doctor as d
        with tempfile.TemporaryDirectory() as t:
            p = self.cfg(t)
            good = FakeRunner([("node --version", 0, "v22\n")])
            with mock.patch.object(d, "make_runner", lambda h: good):
                rc, out = cli("doctor", "--config", p)
            self.assertEqual(rc, 0)
            self.assertIn("PASS", out)
            self.assertIn("L29", out)
            bad = FakeRunner([("node --version", 127, "")])
            with mock.patch.object(d, "make_runner", lambda h: bad):
                rc, out = cli("doctor", "--config", p, "--host", "h2", "--json")
            self.assertEqual(rc, 1)
            rows = json.loads(out)
            self.assertEqual([r["host"] for r in rows], ["h2"])
            self.assertEqual(rows[0]["status"], "FAIL")
            self.assertIn("fix", rows[0])

    def test_ssh_runner_builds_the_command(self):
        from gatetrain.runner import SshRunner
        with mock.patch("subprocess.run") as sp:
            sp.return_value = mock.Mock(returncode=0, stdout="x")
            SshRunner("ssh builder-1").run("df -Pk /srv")
            args = sp.call_args[0][0]
            self.assertEqual(args[:2], ["ssh", "builder-1"])
            self.assertEqual(args[-1], "df -Pk /srv")


class TestStatusAndNextTick(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = os.path.join(self.tmp.name, "t.db")

    def test_status_rows_and_earliest(self):
        rc, out = cli("status", "--log", GREEN, "--log", PARTIAL, "--log", STOPPED, "--now", NOW)
        self.assertEqual(rc, 0)
        for rid in ("run-one-red", "run-partial-red", "run-stopped"):
            self.assertEqual(sum(1 for l in out.splitlines() if l.startswith(rid)), 1, out)
        self.assertIn("next look: 5 min (run-partial-red:", out)

    def test_status_uses_a_given_ledger_and_replaces_running(self):
        cli("status", "--log", PARTIAL, "--db", self.db, "--now", NOW)
        rc, out = cli("status", "--log", PARTIAL, "--db", self.db, "--now", NOW)
        self.assertEqual(rc, 0)
        self.assertEqual(sum(1 for l in out.splitlines() if l.startswith("run-partial-red")), 1)

    def test_next_tick_single_unchanged(self):
        rc, out = cli("next-tick", "--log", PARTIAL, "--db", self.db, "--now", NOW)
        self.assertEqual(out.strip(), "5 min - a red appeared in the last 5 minutes: look again at the minimum")

    def test_next_tick_multi_picks_the_minimum(self):
        singles = []
        for lg in (STOPPED, PARTIAL):
            _, o = cli("next-tick", "--log", lg, "--db", self.db, "--now", NOW)
            singles.append((int(o.split()[0]), o.strip().split(" - ", 1)[1], Path(lg).stem))
        best = min(singles, key=lambda s: s[0])
        rc, out = cli("next-tick", "--log", STOPPED, "--log", PARTIAL, "--db", self.db, "--now", NOW)
        self.assertEqual(out.strip(), f"{best[0]} min - {best[1]} (decided by {best[2]})")


class TestStop(unittest.TestCase):
    PS = "  1 /sbin/init\n100 /bin/sh run-chain.sh batch-9\n101 sleep 5\n102 python other.py\n103 run-chain.sh step a\n"
    ATQ = "7\tWed Nov 11 18:20:00 2026 a me\n8\tWed Nov 11 18:30:00 2026 a me\n"

    def rules(self, ps, atq):
        return [("ps -eo pid=,args=", 0, ps), ("atq", 0, atq),
                ("at -c 7", 0, "#!/bin/sh\nexec merge-follow.sh batch-9\n"),
                ("at -c 8", 0, "#!/bin/sh\necho unrelated\n"),
                ("systemctl --user list-timers", 0, ""), ("kill", 0, ""), ("atrm", 0, "")]

    def test_dry_run_lists_only_matches(self):
        from gatetrain.stop import stop
        fr = FakeRunner(self.rules(self.PS, self.ATQ))
        out = io.StringIO()
        rc = stop(fr, "run-chain", ["merge-follow"], yes=False, out=out, sleep=lambda s: None)
        txt = out.getvalue()
        self.assertEqual(rc, 0)
        self.assertIn("100", txt); self.assertIn("103", txt)
        for no in ("101", "102", "/sbin/init", "other.py", "unrelated"):
            self.assertNotIn(no, txt)
        self.assertIn("at job 7", txt)
        self.assertNotIn("at job 8", txt)
        self.assertFalse(any(c.startswith("kill") or c.startswith("atrm") for c in fr.calls))

    def test_yes_removes_followers_before_killing(self):
        from gatetrain.stop import stop
        fr = FakeRunner(self.rules([self.PS, ""], [self.ATQ, "8\tWed Nov 11 18:30:00 2026 a me\n"]))
        out = io.StringIO()
        rc = stop(fr, "run-chain", ["merge-follow"], yes=True, out=out, sleep=lambda s: None)
        self.assertEqual(rc, 0)
        i_at = next(i for i, c in enumerate(fr.calls) if c.startswith("atrm 7"))
        i_kill = next(i for i, c in enumerate(fr.calls) if c.startswith("kill"))
        self.assertLess(i_at, i_kill)
        self.assertFalse(any(c.startswith("atrm 8") for c in fr.calls))
        killed = [c for c in fr.calls if c.startswith("kill")]
        self.assertTrue(all("101" not in c and "102" not in c for c in killed))
        self.assertFalse(any("-9" in c for c in killed))
        self.assertIn("nothing left", out.getvalue())

    def test_stubborn_process_escalates_after_listing_and_reports_left(self):
        from gatetrain.stop import stop
        fr = FakeRunner(self.rules(self.PS, ""))
        out = io.StringIO()
        rc = stop(fr, "run-chain", [], yes=True, out=out, sleep=lambda s: None)
        self.assertEqual(rc, 1)
        self.assertTrue(any("-9" in c for c in fr.calls if c.startswith("kill")))
        self.assertIn("left", out.getvalue())
        self.assertLessEqual(sum(1 for c in fr.calls if c.startswith("ps -eo")), 5)

    def test_systemd_timer_follower(self):
        from gatetrain.stop import stop
        rules = [("ps -eo pid=,args=", 0, ""), ("atq", 0, ""),
                 ("systemctl --user list-timers", 0, "Wed 18:20 5min merge-batch9.timer merge-batch9.service\n"),
                 ("systemctl --user cat merge-batch9.timer", 0, "ExecStart=merge-follow.sh\n"),
                 ("systemctl --user stop merge-batch9.timer", 0, "")]
        fr = FakeRunner(rules)
        out = io.StringIO()
        stop(fr, "run-chain", ["merge-follow"], yes=True, out=out, sleep=lambda s: None)
        self.assertIn("merge-batch9.timer", out.getvalue())
        self.assertTrue(any(c.startswith("systemctl --user stop merge-batch9.timer") for c in fr.calls))

    def test_not_supported_off_linux(self):
        with mock.patch("sys.platform", "win32"):
            rc, out = cli("stop", "run-chain")
        self.assertEqual(rc, 2)
        self.assertIn("not supported", out)


if __name__ == "__main__":
    unittest.main()
