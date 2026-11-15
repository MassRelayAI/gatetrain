"""The committed doctor corpus (tests/corpus/portb/doctor) is what the Python doctor prints now; the Rust port is compared with these files."""
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CASES = sorted((ROOT / "tests/corpus/portb/doctor").iterdir())


def dump(case, *flags):
    r = subprocess.run([sys.executable, str(ROOT / "tests/corpus/portb_dump.py"), "doctor", str(case), *flags],
                       capture_output=True, cwd=ROOT)
    return r.returncode, r.stdout.decode("utf-8")


class TestDoctorCorpus(unittest.TestCase):
    def test_cases_exist(self):
        self.assertGreaterEqual(len(CASES), 2)

    def test_expected_files_are_current(self):
        for c in CASES:
            rc, out = dump(c)
            self.assertEqual(out, (c / "expected-stdout").read_text(encoding="utf-8"), c.name)
            self.assertEqual(str(rc), (c / "expected-exit").read_text().strip(), c.name)
            self.assertEqual(dump(c, "--json")[1], (c / "expected-json").read_text(encoding="utf-8"), c.name)


if __name__ == "__main__":
    unittest.main()
