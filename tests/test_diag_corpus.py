"""GT-RS4D: the diagnose corpus (tests/corpus/diagnose) and the invariants the Rust port must share. Synthetic logs only."""
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / "tests" / "corpus" / "diagnose"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_diag import diag, format_text_of, mklog, normal  # noqa: E402


def dump(case):
    return subprocess.run([sys.executable, str(ROOT / "tests" / "corpus" / "diagnose_dump.py"), str(case)], capture_output=True, cwd=ROOT).stdout


class TestCorpus(unittest.TestCase):
    def test_every_case_matches_its_committed_output(self):
        cases = sorted(p for p in CORPUS.iterdir() if p.is_dir())
        self.assertGreaterEqual(len(cases), 30)
        for c in cases:
            self.assertEqual(dump(c), (c / "expected-stdout").read_bytes(), c.name)

    def test_rule_file_is_the_single_source(self):
        from gatetrain.diagnose import RULES, RULES_FILE, load_toml_rules
        self.assertEqual(RULES, load_toml_rules(RULES_FILE))
        ids = [r["id"] for r in RULES]
        self.assertIn("load-flake", ids)
        # the seven environment classes (audit rows 41, 66, 68, 69, 70, 96, 106) sit behind the original eight
        self.assertEqual(ids[8:], ["build-folder-missing", "service-never-came-up", "cache-server-died", "packages-not-built", "windows-linker", "generated-stale", "browser-timeout"])

    def test_shrink_wording_now_reach_gates(self):
        fs = diag(mklog(normal() + [("u", 100, 20, 1, ["FAIL  a > b", "AssertionError: these now reach gates - delete their FROZEN entries"])]))
        self.assertEqual([f.cls for f in fs], ["baseline-shrink"])


class TestInvariants(unittest.TestCase):
    def test_an_unmatched_assertion_makes_the_step_real(self):
        for extra in ([], ["FAIL  s > t", "AssertionError: expected length 5, got 4"]):
            lines = ["FAIL  g > h", "AssertionError: expected 1 to be 2"] + extra
            fs = diag(mklog(normal() + [("u", 100, 20, 1, lines)]))
            txt = format_text_of(fs)
            self.assertIn("u: real (", txt)
            self.assertIn("1 real step(s)", txt)
            self.assertNotIn(" 0 real,", txt.splitlines()[-1])


if __name__ == "__main__":
    unittest.main()
