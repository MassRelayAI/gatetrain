import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def sh(cwd, *args):
    r = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *args], cwd=cwd,
                       capture_output=True, text=True, encoding="utf-8")
    assert r.returncode == 0, (args, r.stderr)
    return r.stdout.strip()


class Repo:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory(ignore_cleanup_errors=True)
        self._keep = self.tmp
        self.p = self.tmp.name
        sh(self.p, "init", "-q", "-b", "main")

    def commit_files(self, msg, files):
        for k, v in files.items():
            f = Path(self.p, k)
            f.parent.mkdir(parents=True, exist_ok=True)
            f.write_text(v, encoding="utf-8", newline="\n")
        sh(self.p, "add", "-A")
        sh(self.p, "commit", "-q", "-m", msg)

    def branch(self, name, start="main"):
        sh(self.p, "checkout", "-q", "-B", name, start)


def _run(repo, refs, extra):
    r = subprocess.run([sys.executable, "-m", "gatetrain", "lanes", "--repo", repo.p, "--main", "main", *refs, *extra],
                       cwd=ROOT, capture_output=True, text=True, encoding="utf-8")
    assert r.returncode == 0, r.stderr
    return r.stdout


def lanes(repo, *refs, extra=()):
    return json.loads(_run(repo, refs, ["--json", *extra]))


def text(repo, *refs, extra=()):
    return _run(repo, refs, list(extra))


def base_repo(files):
    r = Repo()
    r.commit_files("init", files)
    return r


def kinds(rep):
    return {(h["kind"], h["file"]) for h in rep["hotspots"]}


class LanesTest(unittest.TestCase):
    def tearDown(self):
        import gc
        gc.collect()

    def test_a_contained(self):
        r = base_repo({"a.txt": "1\n"})
        r.branch("A"); r.commit_files("a", {"a1.txt": "x\n"})
        r.branch("B", "A"); r.commit_files("b", {"b1.txt": "x\n"})
        rep = lanes(r, "A", "B")
        self.assertEqual(rep["contained"], [{"lane": "A", "in": "B"}])
        self.assertIn("contained in B: list only B", text(r, "A", "B"))

    def test_b_both_carry(self):
        r = base_repo({"a.txt": "1\n"})
        r.branch("E"); r.commit_files("e1", {"e1.txt": "x\n"}); r.commit_files("e2", {"e2.txt": "x\n"})
        etip = sh(r.p, "rev-parse", "E")
        r.branch("C", "E"); r.commit_files("c", {"c.txt": "x\n"})
        r.branch("D", "E"); r.commit_files("d", {"d.txt": "x\n"})
        rep = lanes(r, "C", "D")
        self.assertEqual(len(rep["carried"]), 1)
        c = rep["carried"][0]
        self.assertEqual(sorted(c["lanes"]), ["C", "D"])
        self.assertEqual(c["tip"], etip)
        self.assertEqual(c["commits"], 2)
        self.assertEqual(len(rep["passes"]), 2)
        self.assertIn("stack with that tip as lane base", text(r, "C", "D"))
        self.assertEqual(rep["hotspots"], [])  # own diff is measured from the carry tip

    def test_c_same_lines_and_doubled(self):
        r = base_repo({"m.js": "import a from 'a'\nimport b from 'b'\nimport c from 'c'\n"})
        r.branch("L1"); r.commit_files("1", {"m.js": "import a from 'a'\nimport b, {x} from 'b'\nimport c from 'c'\n"})
        r.branch("L2"); r.commit_files("2", {"m.js": "import a from 'a'\nimport b, {y} from 'b'\nimport c from 'c'\n"})
        rep = lanes(r, "L1", "L2")
        self.assertIn(("SAME-LINES", "m.js"), kinds(rep))
        self.assertTrue(rep["doubled_line_risk"])
        self.assertEqual(rep["summary"]["certain_conflicts"], 1)
        self.assertIn("likely doubled line after stacking", text(r, "L1", "L2"))

    def test_d_shared_hub(self):
        r = base_repo({"package.json": '{\n"test": "a.test.js"\n}\n'})
        r.branch("L1"); r.commit_files("1", {"package.json": '{\n"test": "a.test.js b.test.js"\n}\n'})
        r.branch("L2"); r.commit_files("2", {"package.json": '{\n"test": "a.test.js c.test.js"\n}\n'})
        rep = lanes(r, "L1", "L2")
        self.assertEqual(kinds(rep), {("SHARED-HUB", "package.json")})
        self.assertEqual(rep["summary"]["certain_conflicts"], 0)
        self.assertEqual(rep["summary"]["shared_hubs"], 1)

    def test_e_code_both(self):
        body = "".join(f"line{i}\n" for i in range(1, 41))
        r = base_repo({"code.py": body})
        r.branch("L1"); r.commit_files("1", {"code.py": body.replace("line3\n", "LINE3\n")})
        r.branch("L2"); r.commit_files("2", {"code.py": body.replace("line30\n", "LINE30\n")})
        rep = lanes(r, "L1", "L2")
        self.assertEqual(kinds(rep), {("CODE-BOTH", "code.py")})
        self.assertEqual(rep["summary"]["independent_groups"], 2)

    def test_delete_vs_edit(self):
        r = base_repo({"x.py": "1\n2\n"})
        r.branch("L1"); sh(r.p, "rm", "-q", "x.py"); sh(r.p, "commit", "-q", "-m", "rm")
        r.branch("L2"); r.commit_files("2", {"x.py": "1\n3\n"})
        rep = lanes(r, "L1", "L2")
        self.assertIn(("DELETE-EDIT", "x.py"), kinds(rep))

    def test_f_resource_clash(self):
        r = base_repo({"a.txt": "1\n"})
        r.branch("L1"); r.commit_files("1", {"gate_one.mjs": "const port = 57901\n"})
        r.branch("L2"); r.commit_files("2", {"gate_two.mjs": "run({port: 57901})\n"})
        rep = lanes(r, "L1", "L2")
        self.assertEqual(len(rep["resource_clashes"]), 1)
        c = rep["resource_clashes"][0]
        self.assertEqual((c["name"], c["value"], sorted(c["lanes"])), ("port", "57901", ["L1", "L2"]))
        self.assertEqual(rep["summary"]["resource_clashes"], 1)

    def test_g_paired_count(self):
        r = base_repo({"gate.mjs": "const EXPECTED = 13\n", "gate.test.mjs": "assert.equal(EXPECTED, 13)\n"})
        r.branch("L1"); r.commit_files("1", {"gate.mjs": "const EXPECTED = 12\n"})
        rep = lanes(r, "L1")
        self.assertEqual(len(rep["paired_counts"]), 1)
        p = rep["paired_counts"][0]
        self.assertEqual((p["lane"], p["old"], p["new"], p["file"], p["stale_in"]), ("L1", "13", "12", "gate.mjs", "gate.test.mjs"))
        self.assertIn("count changed here but its check still says 13", text(r, "L1"))

    def test_g2_paired_count_updated_is_quiet(self):
        r = base_repo({"gate.mjs": "const EXPECTED = 13\n", "gate.test.mjs": "assert.equal(EXPECTED, 13)\n"})
        r.branch("L1"); r.commit_files("1", {"gate.mjs": "const EXPECTED = 12\n", "gate.test.mjs": "assert.equal(EXPECTED, 12)\n"})
        self.assertEqual(lanes(r, "L1")["paired_counts"], [])

    def test_h_independent(self):
        r = base_repo({"a.txt": "1\n"})
        for n in "XYZ":
            r.branch(n); r.commit_files(n, {f"{n}.py": "x\n"})
        rep = lanes(r, "X", "Y", "Z")
        self.assertEqual(rep["hotspots"], [])
        self.assertEqual(len(rep["groups"]), 1)
        self.assertEqual(sorted(rep["groups"][0]), ["X", "Y", "Z"])
        self.assertEqual(rep["summary_line"], "3 lanes: 1 independent groups; 0 certain conflicts; 0 shared hubs; 0 resource clashes")

    # ---- drift against main, carried files, noise
    def test_i_drift_same_lines_vs_main(self):
        r = base_repo({"m.js": "import a from 'a'\nimport b from 'b'\nimport c from 'c'\n"})
        r.branch("L1"); r.commit_files("1", {"m.js": "import a from 'a'\nimport b, {x} from 'b'\nimport c from 'c'\n"})
        sh(r.p, "checkout", "-q", "main"); r.commit_files("m", {"m.js": "import a from 'a'\nimport b, {y} from 'b'\nimport c from 'c'\n"})
        rep = lanes(r, "L1")
        h = [x for x in rep["hotspots"] if x["file"] == "m.js"]
        self.assertEqual([(x["kind"], x["lanes"]) for x in h], [("SAME-LINES", ["L1", "main"])])
        self.assertTrue(rep["doubled_line_risk"])
        self.assertIn("L1 x main", text(r, "L1"))

    def test_j_drift_code_both_and_hub_vs_main(self):
        body = "".join(f"line{i}\n" for i in range(1, 41))
        r = base_repo({"code.py": body, "package.json": '{\n"t": "a"\n}\n'})
        r.branch("L1"); r.commit_files("1", {"code.py": body.replace("line3\n", "LINE3\n"), "package.json": '{\n"t": "a b"\n}\n'})
        sh(r.p, "checkout", "-q", "main"); r.commit_files("m", {"code.py": body.replace("line30\n", "LINE30\n"), "package.json": '{\n"t": "a c"\n}\n'})
        rep = lanes(r, "L1")
        self.assertEqual({(x["kind"], x["file"], tuple(x["lanes"])) for x in rep["hotspots"]},
                         {("CODE-BOTH", "code.py", ("L1", "main")), ("SHARED-HUB", "package.json", ("L1", "main"))})

    def test_k_carried_files_collide(self):
        r = base_repo({"main.rs": "a\nb\nc\n"})
        r.branch("S"); r.commit_files("s", {"main.rs": "a\nB1\nc\n"})
        r.branch("A", "S"); r.commit_files("a", {"a.txt": "x\n"})
        r.branch("C", "S"); r.commit_files("c", {"c.txt": "x\n"})
        r.branch("B", "main"); r.commit_files("b", {"main.rs": "a\nB2\nc\n"})
        rep = lanes(r, "A", "C", "B")
        h = [x for x in rep["hotspots"] if x["kind"] == "SAME-LINES"]
        self.assertEqual(len(h), 1)
        self.assertEqual(h[0]["file"], "main.rs")
        self.assertTrue(h[0]["lanes"][0].startswith("carried "))
        self.assertIn("B", h[0]["lanes"])
        self.assertIn("carried ", text(r, "A", "C", "B"))

    def test_l_carried_already_on_main(self):
        r = base_repo({"a.txt": "1\n"})
        r.branch("S"); r.commit_files("s", {"s.txt": "side\n"})
        sha = sh(r.p, "rev-parse", "S")
        r.branch("A", "S"); r.commit_files("a", {"a1.txt": "x\n"})
        r.branch("C", "S"); r.commit_files("c", {"c1.txt": "x\n"})
        sh(r.p, "checkout", "-q", "main"); r.commit_files("other", {"o.txt": "o\n"}); sh(r.p, "cherry-pick", sha)
        rep = lanes(r, "A", "C")
        self.assertTrue(rep["carried"][0]["on_main"])
        t = text(r, "A", "C")
        self.assertIn("already on main (by patch)", t)
        self.assertIn("do NOT re-apply", t)
        self.assertNotIn("stack the carried commits", t)

    def test_l2_carried_not_on_main_flag_false(self):
        r = base_repo({"a.txt": "1\n"})
        r.branch("S"); r.commit_files("s", {"s.txt": "side\n"})
        r.branch("A", "S"); r.commit_files("a", {"a1.txt": "x\n"})
        r.branch("C", "S"); r.commit_files("c", {"c1.txt": "x\n"})
        self.assertFalse(lanes(r, "A", "C")["carried"][0]["on_main"])

    def test_m_locale_is_hub(self):
        r = base_repo({"web/locales/en.json": '{\n"a": 1\n}\n', "w.py": "x\n"})
        r.branch("L1"); r.commit_files("1", {"web/locales/en.json": '{\n"a": 1,\n"b": 2\n}\n'})
        r.branch("L2"); r.commit_files("2", {"web/locales/en.json": '{\n"a": 1,\n"c": 3\n}\n'})
        self.assertEqual(kinds(lanes(r, "L1", "L2")), {("SHARED-HUB", "web/locales/en.json")})

    def test_n_paired_counts_only_in_tests(self):
        r = base_repo({"gate.mjs": "const EXPECTED = 13\n", "docs/notes.md": "EXPECTED = 13\n",
                       "_archive/old.txt": "EXPECTED = 13\n", "gate.test.mjs": "assert.equal(EXPECTED, 13)\n"})
        r.branch("L1"); r.commit_files("1", {"gate.mjs": "const EXPECTED = 12\n"})
        got = [p["stale_in"] for p in lanes(r, "L1")["paired_counts"]]
        self.assertEqual(got, ["gate.test.mjs"])

    def test_o_code_both_grouped_by_file(self):
        body = "".join(f"line{i}\n" for i in range(1, 61))
        r = base_repo({"lib.rs": body})
        for n, ln in (("L1", 3), ("L2", 25), ("L3", 50)):
            r.branch(n, "main"); r.commit_files(n, {"lib.rs": body.replace(f"line{ln}\n", f"X{ln}\n")})
        t = text(r, "L1", "L2", "L3")
        self.assertIn("lib.rs: lanes L1, L2, L3", t)
        self.assertEqual(t.count("CODE-BOTH"), 1)

    def test_bad_ref_fails(self):
        r = base_repo({"a.txt": "1\n"})
        p = subprocess.run([sys.executable, "-m", "gatetrain", "lanes", "--repo", r.p, "--main", "main", "nope"],
                           cwd=ROOT, capture_output=True, text=True)
        self.assertNotEqual(p.returncode, 0)


if __name__ == "__main__":
    unittest.main()
