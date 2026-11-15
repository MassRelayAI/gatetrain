"""The deny-list check: the repository is clean, and the check goes red when it should (a guard never seen to fail is not known to work)."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import privacy_check as pc  # noqa: E402

SCRIPT = str(Path(pc.ROOT) / "tools" / "privacy_check.py")


def repo_with(files: dict, words):
    """A throwaway git repository holding `files`, with a deny-list of `words` (hashes only)."""
    d = tempfile.TemporaryDirectory()
    root = Path(d.name)
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    (root / pc.DENYLIST).write_text("".join(pc.sha(w) + "\n" for w in words), encoding="utf-8")
    for rel, text in files.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(text, encoding="utf-8")
    return d, root


class TestPrivacy(unittest.TestCase):
    def test_the_repository_is_clean(self):
        self.assertEqual(pc.check(), [])

    def test_the_denylist_holds_hashes_only(self):
        lines = [ln for ln in (pc.ROOT / pc.DENYLIST).read_text(encoding="utf-8").splitlines() if ln and not ln.startswith("#")]
        self.assertGreaterEqual(len(lines), 3)
        for ln in lines:
            self.assertRegex(ln, r"^[0-9a-f]{64}$")

    def test_a_word_in_a_file_is_found_with_its_line_and_never_printed(self):
        d, root = repo_with({"a/b.txt": "fine\nhost secretname here\n"}, ["secretname"])
        with d:
            hits = pc.check(root)
            self.assertEqual(hits, ["a/b.txt:2"])
            self.assertNotIn("secretname", "".join(hits))

    def test_case_and_punctuation_do_not_hide_a_word(self):
        files = {"x.log": "ssh SecretName-01\n", "y.toml": 'name = "SECRETNAME"\n', "z.txt": "secretnames and xsecretname\n"}
        d, root = repo_with(files, ["secretname"])
        with d:
            # words are runs of [a-z0-9]: a hyphen splits, case is folded, a longer word is a different word
            self.assertEqual(sorted(pc.check(root)), ["x.log:1", "y.toml:1"])

    def test_a_deny_listed_word_in_a_path_is_found(self):
        d, root = repo_with({"dir-secretname/x.txt": "clean"}, ["secretname"])
        with d:
            self.assertEqual(pc.check(root), ["dir-secretname/x.txt (path)"])

    def test_a_clean_tree_passes(self):
        d, root = repo_with({"a.txt": "builder-1 host-a /srv/repos"}, ["secretname"])
        with d:
            self.assertEqual(pc.check(root), [])

    def test_the_cli_exits_nonzero_on_a_hit_and_zero_when_clean(self):
        d, root = repo_with({"a.txt": "secretname"}, ["secretname"])
        with d:
            r = subprocess.run([sys.executable, SCRIPT, "--root", str(root)], capture_output=True, text=True)
            self.assertEqual(r.returncode, 1)
            self.assertIn("a.txt:1", r.stdout)
            self.assertNotIn("secretname", r.stdout)
        d, root = repo_with({"a.txt": "fine"}, ["secretname"])
        with d:
            self.assertEqual(subprocess.run([sys.executable, SCRIPT, "--root", str(root)], capture_output=True).returncode, 0)


if __name__ == "__main__":
    unittest.main()
