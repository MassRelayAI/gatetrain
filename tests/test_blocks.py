"""GT-DIAG blocks: rules match inside FAILURE BLOCKS only. Synthetic vitest-shaped logs; nothing here is a real log."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_diag import diag, format_text_of, mklog, normal  # noqa: E402

NOISE = [f"stderr | src/mod{i}.test.ts > case {i}\nwarn: missing: optional.json, stray handle on disk vs build, ignored" for i in range(40)]
NOISE = [ln for blob in NOISE for ln in blob.split("\n")] + [" ✓ src/ok.test.ts (12 tests) 31ms"] * 200

GOLDEN = ["FAIL  src/golden.test.ts > golden > matches the stored output",
          "AssertionError: expected { a: 1 } to deeply equal { a: 2 }",
          "  - Expected", "  + Received", " ❯ src/golden.test.ts:14:20"]
SHRINK = ["FAIL  src/frozen.test.ts > frozen > the list only shrinks",
          "AssertionError: these routes now reach the gate - delete their FROZEN entries: expected [ 'PATCH /x' ] to deeply equal []",
          " ❯ src/frozen.test.ts:9:3"]
SUMMARY = [" Test Files  2 failed | 80 passed (82)", "      Tests  2 failed | 900 passed (902)"]
TIMEOUT = ["FAIL  src/db.test.ts [ src/db.test.ts ]", "Error: Hook timed out in 30000ms.", " ❯ src/db.test.ts:3:1"]
TSUMMARY = [" Test Files  1 failed | 81 passed (82)", "      Tests  0 failed | 900 passed (900)"]


def red(lines, dur=20):
    return mklog(normal() + [("unit-001", 100, dur, 1, lines)])


class TestBlocks(unittest.TestCase):
    def classes(self, text, **kw):
        return sorted(f.cls for f in diag(text, **kw))

    def test_two_real_failures_amid_noise(self):
        fs = diag(red(NOISE + ["⎯⎯⎯⎯ Failed Tests 2 ⎯⎯⎯⎯"] + GOLDEN + ["⎯⎯⎯⎯[1/2]⎯"] + SHRINK + ["⎯⎯⎯⎯[2/2]⎯"] + SUMMARY))
        self.assertEqual(sorted(f.cls for f in fs), ["baseline-shrink", "real"])
        self.assertNotIn("generated-uncommitted", [f.cls for f in fs])
        self.assertTrue(all("stderr" not in f.evidence and "missing" not in f.evidence for f in fs))
        txt = format_text_of(fs)
        self.assertIn("unit-001: real", txt)  # the step verdict
        self.assertNotIn("0 real", txt)
        self.assertIn("1 real", txt)

    def test_timeout_only_is_flake_load(self):
        fs = diag(red(NOISE + TIMEOUT + TSUMMARY, dur=400))
        self.assertEqual([f.cls for f in fs], ["flake-load"])
        self.assertIn("Hook timed out", fs[0].evidence)

    def test_timeout_next_to_an_assertion_is_real(self):
        fs = diag(red(TIMEOUT + GOLDEN + SUMMARY, dur=400))
        self.assertNotIn("flake-load", [f.cls for f in fs])
        self.assertIn("real", [f.cls for f in fs])

    def test_noise_alone_gives_nothing(self):
        fs = diag(red(NOISE + GOLDEN + SUMMARY))
        self.assertEqual([f.cls for f in fs], ["real"])

    def test_rule_needs_pattern_and_also_in_one_block(self):
        a = ["FAIL  src/a.test.ts > a", "Error: missing: gen/x.json", "FAIL  src/b.test.ts > b", "Error: on-disk output differs"]
        self.assertEqual(self.classes(red(a)), ["real", "real"])
        ok = ["FAIL  src/a.test.ts > a", "Error: on-disk output differs from the build", "  missing: gen/x.json"]
        self.assertEqual(self.classes(red(ok)), ["generated-uncommitted"])

    def test_one_block_per_runner(self):
        node = ["✖ widgets > renders (3ms)", "  AssertionError [ERR_ASSERTION]: NEW route /w not in the allow-list", "✔ other (1ms)"]
        self.assertEqual(self.classes(red(node)), ["baseline-missing"])
        tap = ["ok 1 - fine", "not ok 2 - frozen", "  ---", "  message: now witnessed, delete the entry", "  ..."]
        self.assertEqual(self.classes(red(tap)), ["baseline-shrink"])
        cargo = ["running 2 tests", "test a ... ok", "test b ... FAILED", "failures:", "", "---- b stdout ----",
                 "thread 'b' panicked at src/lib.rs:9:5:", "assertion `left == right` failed", "  left: 1", " right: 2", "",
                 "failures:", "    b", "", "test result: FAILED. 1 passed; 1 failed"]
        self.assertEqual(self.classes(red(cargo)), ["real"])
        lst = ["FAIL  2 problem(s):", "  - route /a: nobody classified it", "  - golden gen/b.json differs"]
        self.assertEqual(self.classes(red(lst)), ["baseline-missing", "real"])

    def test_summary_counts_blocks_and_steps(self):
        fs = diag(red(GOLDEN + SHRINK + SUMMARY))
        txt = format_text_of(fs)
        self.assertIn("1 real step", txt)
        self.assertIn("2 blocks: 1 real, 0 flake or infrastructure, 1 baseline", txt)


if __name__ == "__main__":
    unittest.main()
