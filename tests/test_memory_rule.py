"""GT-RS2: the infra-memory red class is a [[rule]] of rules/red-classes.toml (the one table, read by Python and Rust). Synthetic logs."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_diag import ASSERT, diag, one_red, only  # noqa: E402


class TestMemoryRule(unittest.TestCase):
    def rules(self):
        from gatetrain.diagnose import RULES
        return RULES

    def test_oom_lines_are_infra_memory(self):
        for lines in (["Killed"], ["error: process exited with code 137"], ["FATAL ERROR: JavaScript heap out of memory"],
                      ["Cannot allocate memory (OOM killer)"], ["Windows stopped because the system is running low on memory"]):
            f = only(diag(one_red(lines, rc=137), rules=self.rules()))
            self.assertEqual(f.cls, "infra-memory", lines)
            self.assertTrue(f.rerun)

    def test_a_normal_assertion_failure_is_not_memory(self):
        f = only(diag(one_red(ASSERT), rules=self.rules()))
        self.assertEqual(f.cls, "real")

    def test_the_word_killed_inside_a_sentence_is_not_memory(self):
        f = only(diag(one_red(["the test killed the mock server as designed", "AssertionError: x"]), rules=self.rules()))
        self.assertEqual(f.cls, "real")

    def test_the_memory_rule_is_a_row_of_the_one_table(self):
        self.assertTrue(any(r["cls"] == "infra-memory" for r in self.rules()))
        self.assertFalse((Path(__file__).resolve().parent.parent / "rules" / "red-classes-extra.json").exists())

    def test_every_row_has_the_keys_diagnose_needs(self):
        for r in self.rules():
            self.assertTrue({"cls", "pattern", "remedy", "rerun"} <= set(r))


if __name__ == "__main__":
    unittest.main()
