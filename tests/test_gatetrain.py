import contextlib, io, json, os, tempfile, unittest
from pathlib import Path

FIX = Path(__file__).parent / "fixtures"
GREEN, PARTIAL, STOPPED = (str(FIX / n) for n in ("run-one-red.log", "run-partial-red.log", "run-stopped.log"))
REDS = ["lint-deps", "test-web", "lint-web", "check-tracker", "cargo-test", "test-api"]


def run(*argv):
    from gatetrain.cli import main
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        rc = main(list(argv))
    return rc, buf.getvalue()


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)  # registered first, so it runs after other cleanups
        self.db = os.path.join(self.tmp.name, "t.db")


class TestParse(unittest.TestCase):
    def parse(self, p):
        from gatetrain.parse import parse_log
        return parse_log(Path(p).read_text())

    def test_green_counts(self):
        r = self.parse(GREEN)
        self.assertEqual(len(r.steps), 466)
        self.assertTrue(r.gates_done)
        self.assertEqual(r.commit, "0a1b2c3d4")
        self.assertEqual(r.host, "builder-1")
        self.assertEqual(r.mode, "full")
        self.assertEqual([s.name for s in r.steps if s.exit_code], ["e2e-001"])

    def test_partial(self):
        r = self.parse(PARTIAL)
        self.assertEqual(len(r.steps), 250)
        self.assertFalse(r.gates_done)
        self.assertEqual([x[0] for x in r.red_now], REDS)

    def test_stopped_count(self):
        self.assertEqual(len(self.parse(STOPPED).steps), 31)

    def test_end_is_start_plus_time(self):
        r = self.parse(GREEN)
        s = next(s for s in r.steps if s.name == "build-packages")
        self.assertEqual((s.end - s.start).total_seconds(), 31)

    def test_missing_start_is_unknown(self):
        from gatetrain.parse import parse_log
        r = parse_log("branch b at abc on h\n=== x\nSTEP_TIME x 5s\nSTEP_EXIT x 0\n")
        self.assertIsNone(r.steps[0].start)
        self.assertIsNone(r.steps[0].end)
        self.assertEqual(r.steps[0].duration, 5)


class TestIngest(Base):
    def test_outcomes(self):
        self.assertEqual(run("ingest", GREEN, "--db", self.db)[0], 0)
        run("ingest", PARTIAL, "--db", self.db, "--now", "2026-11-11T18:20:00+07:00")
        run("ingest", STOPPED, "--db", self.db, "--now", "2026-11-12T20:00:00+07:00")
        import sqlite3
        c = sqlite3.connect(self.db)
        self.addCleanup(c.close)
        got = dict(c.execute("select id, outcome from runs"))
        self.assertEqual(got["run-one-red"], "red")  # fixture carries one real red: e2e-001
        self.assertEqual(got["run-partial-red"], "running")
        self.assertEqual(got["run-stopped"], "stopped")
        self.assertEqual(c.execute("select count(*) from steps where run_id='run-one-red'").fetchone()[0], 466)

    def test_all_zero_exits_is_green(self):
        p = os.path.join(self.tmp.name, "g.log")
        Path(p).write_text("branch b at abc on h\nCHAIN_MODE full\nSTEP_START a 2026-01-01T00:00:00+00:00\n=== a\n"
                           "STEP_TIME a 5s\nSTEP_EXIT a 0\nRED_SUMMARY: none\nGATES_DONE\n")
        run("ingest", p, "--db", self.db)
        import sqlite3
        c = sqlite3.connect(self.db)
        self.addCleanup(c.close)
        self.assertEqual(c.execute("select outcome from runs").fetchone()[0], "green")

    def test_twice_refused(self):
        self.assertEqual(run("ingest", GREEN, "--db", self.db)[0], 0)
        rc, _ = run("ingest", GREEN, "--db", self.db)
        self.assertNotEqual(rc, 0)
        # a completed run cannot be replaced; a still-running one can
        self.assertNotEqual(run("ingest", GREEN, "--db", self.db, "--replace")[0], 0)
        now = "2026-11-11T18:20:00+07:00"
        self.assertEqual(run("ingest", PARTIAL, "--db", self.db, "--now", now)[0], 0)
        self.assertNotEqual(run("ingest", PARTIAL, "--db", self.db, "--now", now)[0], 0)
        self.assertEqual(run("ingest", PARTIAL, "--db", self.db, "--now", now, "--replace")[0], 0)


class TestTrace(Base):
    def events(self, p):
        rc, out = run("trace", p)
        self.assertEqual(rc, 0)
        return json.loads(out)

    def test_trace_valid(self):
        for p, n in ((GREEN, 466), (PARTIAL, 250)):
            ev = self.events(p)
            self.assertEqual(len(ev), n)
            lanes = {}
            for e in ev:
                for k in ("name", "ph", "ts", "dur", "pid", "tid", "args"):
                    self.assertIn(k, e)
                lanes.setdefault(e["tid"], []).append(e)
            for es in lanes.values():
                es.sort(key=lambda e: e["ts"])
                for a, b in zip(es, es[1:]):
                    self.assertLessEqual(a["ts"] + a["dur"], b["ts"])
        red = [e for e in self.events(GREEN) if e["name"] == "e2e-001"][0]
        self.assertEqual(red["cname"], "terrible")
        self.assertEqual([e for e in self.events(GREEN) if e["name"] == "build-packages"][0]["cname"], "good")

    def test_trace_from_run_id(self):
        run("ingest", GREEN, "--db", self.db)
        rc, out = run("trace", "--run", "run-one-red", "--db", self.db)
        self.assertEqual(len(json.loads(out)), 466)


class TestEtaTick(Base):
    NOW = "2026-11-11T18:20:00+07:00"

    def last_ts(self):
        from gatetrain.parse import parse_log
        return parse_log(Path(PARTIAL).read_text()).last_ts

    def test_eta_with_history(self):
        from datetime import datetime, timedelta
        run("ingest", GREEN, "--db", self.db)
        rc, out = run("eta", PARTIAL, "--db", self.db, "--now", self.NOW)
        self.assertEqual(rc, 0)
        line = [l for l in out.splitlines() if l.startswith("eta ")][0]
        eta = datetime.fromisoformat(line.split()[1])
        last = self.last_ts()
        self.assertGreater(eta, last)
        self.assertLess(eta, last + timedelta(hours=12))
        self.assertIn("dominant", out)
        self.assertIn("band", out)

    def test_eta_no_history(self):
        rc, out = run("eta", PARTIAL, "--db", self.db, "--now", self.NOW)
        self.assertIn("no history - rough estimate", out)

    def test_next_tick_min_after_fresh_red(self):
        from datetime import timedelta
        run("ingest", GREEN, "--db", self.db)
        red_at = "2026-11-11T18:14:37+07:00"
        rc, out = run("next-tick", "--log", PARTIAL, "--db", self.db, "--now", "2026-11-11T18:16:00+07:00")
        self.assertEqual(out.split()[0], "5")

    def test_next_tick_longer_mid_run(self):
        run("ingest", GREEN, "--db", self.db)
        lines = Path(PARTIAL).read_text().splitlines()[:78]  # before the first RED_NOW
        p = os.path.join(self.tmp.name, "mid.log")
        Path(p).write_text("\n".join(lines) + "\n")
        from gatetrain.parse import parse_log
        last = parse_log("\n".join(lines)).last_ts
        from datetime import timedelta
        now = (last + timedelta(minutes=1)).isoformat()
        rc, out = run("next-tick", "--log", p, "--db", self.db, "--now", now)
        self.assertGreater(int(out.split()[0]), 5)
        self.assertLessEqual(int(out.split()[0]), 60)


class TestFlaky(Base):
    def test_flaky(self):
        mk = lambda rc: ("branch b at abc1 on h\nSTEP_START s 2026-01-01T00:00:00+00:00\n=== s\nSTEP_TIME s 5s\n"
                         f"STEP_EXIT s {rc}\nGATES_DONE\n")
        for name, rc in (("a", 1), ("b", 0)):
            p = os.path.join(self.tmp.name, name + ".log")
            Path(p).write_text(mk(rc))
            run("ingest", p, "--db", self.db)
        rc, out = run("flaky", "--db", self.db)
        self.assertIn("s", out.split())
        self.assertIn("abc1", out)


if __name__ == "__main__":
    unittest.main()


class TrueGreen(unittest.TestCase):
    def test_a_complete_green_run_is_green_with_no_red(self):
        import tempfile, sqlite3, os
        d = tempfile.mkdtemp(); db = os.path.join(d, "g.db")
        rc, out = run("ingest", str(FIX / "run-green.log"), "--db", db)
        self.assertEqual(rc, 0, out)
        c = sqlite3.connect(db)
        self.assertEqual(c.execute("select outcome from runs where id='run-green'").fetchone()[0], "green")
        self.assertEqual(c.execute("select count(*) from steps where run_id='run-green'").fetchone()[0], 466)
        self.assertEqual(c.execute("select count(*) from steps where run_id='run-green' and exit_code<>0").fetchone()[0], 0)

