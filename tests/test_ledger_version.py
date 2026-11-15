import sqlite3, tempfile, unittest
from pathlib import Path

from gatetrain import db


class TestKnownVersion(unittest.TestCase):
    def test_known_version_is_three_and_a_v3_file_opens(self):
        self.assertEqual(db.KNOWN_SCHEMA_VERSION, 3)
        with tempfile.TemporaryDirectory() as d:
            p = str(Path(d) / "l.db")
            c = sqlite3.connect(p)
            c.execute("PRAGMA user_version=3")
            c.commit()
            c.close()
            db.connect(p)
            db.close_all()

    def test_a_v4_file_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            p = str(Path(d) / "l.db")
            c = sqlite3.connect(p)
            c.execute("PRAGMA user_version=4")
            c.commit()
            c.close()
            with self.assertRaises(RuntimeError):
                db.connect(p)
            db.close_all()


if __name__ == "__main__":
    unittest.main()
