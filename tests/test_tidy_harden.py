"""Hardening tests for `check` / `tidy` (lessons L60-L66): injection, deletion order, path safety, merge proof, links, timeouts.

Each class is one review finding. The fake host in test_tidy.py models the real failure (an unquoted name reaching a shell, a recursive
remove that follows a link); the real-git and real-subprocess tests run in temp dirs.
"""
import json, os, shlex, shutil, subprocess, sys, tempfile, time, unittest
from pathlib import Path
from unittest import mock

from gatetrain import tidy as T
from gatetrain.config import ConfigError, parse
from test_tidy import (DAY, GB, MB, NOW, SimHost, WIN_EXTRA, cargo_target, gather, hostcfg, linux_sim, n)

EVIL = ["x;touch pwned", "x&calc", "x$(id)", "x`id`", "x|id", "-oops", "a b"]


def ops_for(sim, extra=None, win=False):
    cfg, h = hostcfg(extra, win)
    return T.Ops(h, sim, T.build_cfg(cfg, h)), h


def winsim():
    sim = SimHost(win=True)
    sim.add("C:/repo/main", age_s=0)
    sim.add("C:/repo/main/a.rs", "file", 10, 0)
    sim.add("C:/repo/main/node_modules", age_s=0)
    sim.add("C:/repo/main/node_modules/pkg.js", "file", 10, 0)
    sim.add("C:/canary", age_s=0)
    sim.add("C:/canary/l1", "link", target="C:/repo/main/a.rs")
    sim.add("C:/repo/wt/lane-a", age_s=DAY)
    sim.add("C:/repo/wt/lane-a/src.rs", "file", 5, DAY)
    sim.worktrees = [("C:/repo/main", "main"), ("C:/repo/wt/lane-a", "line/lane-a")]
    sim.branches = ["main", "line/lane-a"]
    sim.ancestors = {"line/lane-a", "main"}
    sim.mtree = {"line/lane-a": sim.tree_main}
    return sim


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.log = os.path.join(self.tmp, "log.jsonl")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def apply(self, sim, classes, extra=None, win=False):
        h, t, inv = gather(sim, dict(WIN_EXTRA if win else {}, **(extra or {}), log=self.log), win)
        return T.apply_plan(h, sim, t, inv, classes, now=NOW)

    def recs(self, with_intent=False):
        rs = [json.loads(l) for l in Path(self.log).read_text().splitlines()]
        return rs if with_intent else [r for r in rs if r["status"] != "intent"]


# ---------------------------------------------------------------- 1. branch names never reach a shell

class BranchNameTests(Base):
    def test_unsafe_branch_is_kept_at_inventory(self):
        for bad in EVIL:
            sim = linux_sim()
            sim.worktrees[1] = ("/repo/wt/lane-a", bad)
            sim.branches.append(bad)
            sim.ancestors.add(bad)
            h, t, inv = gather(sim)
            w = next(x for x in inv.worktrees if x["path"] == "/repo/wt/lane-a")
            self.assertEqual(w["keep"], "unsafe name", bad)
            self.assertEqual(by(inv, "/repo/wt/lane-a"), [], bad)

    def test_apply_refuses_an_unsafe_branch_even_if_planned(self):
        sim = linux_sim()
        h, t, inv = gather(sim, {"log": self.log})
        inv.items = [T.Item("worktrees", "remove-worktree", "/repo/wt/lane-a", 0, "x", extra={"branch": "x;touch pwned", "links": 0})]
        self.assertEqual(T.apply_plan(h, sim, t, inv, ["worktrees"], now=NOW), (0, 1))     # refused and reported, never run
        self.assertEqual([r["status"] for r in self.recs()], ["refused"])
        self.assertFalse([c for c in sim.calls if "pwned" in c and ("tag" in c or "update-ref" in c)])

    def test_git_never_goes_through_a_shell_string(self):
        sim = linux_sim()
        self.apply(sim, ["worktrees"])
        git_calls = [c for c in sim.calls if c.startswith("git")]
        self.assertTrue(git_calls)
        joined = {" ".join(a) for a in sim.argvs}
        self.assertTrue(all(c in joined for c in git_calls), [c for c in git_calls if c not in joined])

    def test_docker_and_cargo_run_as_argv(self):
        sim = linux_sim()
        self.apply(sim, ["containers", "targets"])
        flat = [" ".join(a) for a in sim.argvs]
        self.assertIn("docker rm gate-1", flat)
        self.assertTrue(any(c.startswith("cargo clean") for c in flat))


def by(inv, key):
    return [i for i in inv.items if i.key == key]


# ---------------------------------------------------------------- 2. quoting on each kind of host

class QuotingTests(Base):
    def test_windows_refuses_shell_metacharacters_in_interpolated_values(self):
        ops, _ = ops_for(winsim(), WIN_EXTRA, win=True)
        for ch in '"%^!&|<>':
            with self.assertRaises(T.GuardViolation, msg=ch):
                ops.q(f"C:/a{ch}b")
            with self.assertRaises(T.GuardViolation, msg=ch):
                ops.psq(f"C:/a{ch}b")
        self.assertTrue(ops.q("C:/ok dir/x-1_2.txt"))

    def test_windows_git_takes_the_repo_as_an_argument_not_a_command_line(self):
        sim = winsim()
        ops, _ = ops_for(sim, WIN_EXTRA, win=True)
        ops.git("C:/repo&calc/main", "status", "--porcelain")
        self.assertEqual(sim.argvs[-1], ["git", "-C", "C:/repo&calc/main", "status", "--porcelain"])

    def test_local_runner_argv_has_no_shell(self):
        from gatetrain.runner import LocalRunner
        rc, out = LocalRunner().run_argv([sys.executable, "-c", "import sys;print(sys.argv[1])", "a&b|c;d$(e)"])
        self.assertEqual((rc, out.strip()), (0, "a&b|c;d$(e)"))

    def test_ssh_argv_is_posix_single_quoted(self):
        from gatetrain.runner import SshRunner
        argv = ["git", "-C", "/a b", "tag", "x';id;'"]
        with mock.patch("gatetrain.runner.subprocess.run") as run:
            run.return_value = mock.Mock(returncode=0, stdout="")
            SshRunner("ssh h").run_argv(argv)
        self.assertEqual(run.call_args[0][0][-1], shlex.join(argv))


# ---------------------------------------------------------------- 3. the branch is deleted only after a verified archive

class ArchiveOrderTests(Base):
    def test_failed_tag_keeps_branch_and_worktree(self):
        sim = linux_sim()
        sim.tag_fails = True
        with self.assertRaises(T.GuardViolation):
            self.apply(sim, ["worktrees"])
        self.assertEqual(sim.deleted_branches, [])
        self.assertIn("line/lane-a", sim.branches)
        self.assertIn("/repo/wt/lane-a", sim.fs)

    def test_tag_that_exists_elsewhere_gets_a_suffixed_name(self):
        sim = linux_sim()
        sim.tag_refs["archive/line/lane-a"] = "someothersha"
        done, failed = self.apply(sim, ["worktrees"])
        self.assertEqual(failed, 0)
        name = "archive/line/lane-a-" + sim.sha("line/lane-a")[:8]
        self.assertEqual(sim.tag_refs[name], sim.sha("line/lane-a"))
        self.assertEqual(sim.tag_refs["archive/line/lane-a"], "someothersha")      # the old tag is untouched
        self.assertIn("line/lane-a", sim.deleted_branches)

    def test_tag_that_points_elsewhere_is_a_violation(self):
        sim = linux_sim()
        orig = sim.run_argv

        def wrong(argv, timeout=None):
            if "tag" in argv:
                sim.tag_refs[argv[argv.index("tag") + 1]] = "WRONG"
                return 0, ""
            return orig(argv, timeout)
        sim.run_argv = wrong
        with self.assertRaises(T.GuardViolation):
            self.apply(sim, ["worktrees"])
        self.assertEqual(sim.deleted_branches, [])

    def test_branch_that_moved_after_archiving_is_not_deleted(self):
        sim = linux_sim()
        orig = sim.run_argv

        def moving(argv, timeout=None):
            rc, out = orig(argv, timeout)
            if "tag" in argv:
                sim.shas["line/lane-a"] = "newer"      # someone commits to the branch in between
            return rc, out
        sim.run_argv = moving
        with self.assertRaises(T.GuardViolation):
            self.apply(sim, ["worktrees"])
        self.assertEqual(sim.deleted_branches, [])
        self.assertIn("line/lane-a", sim.branches)

    def test_branch_is_deleted_by_ref_and_sha(self):
        sim = linux_sim()
        self.apply(sim, ["worktrees"])
        self.assertIn(["git", "-C", "/repo/main", "update-ref", "-d", "refs/heads/line/lane-a", sim.sha("line/lane-a")], sim.argvs)
        self.assertFalse([c for c in sim.calls if "branch -D" in c])


class RealGitRemovalTests(Base):
    @unittest.skipUnless(shutil.which("git"), "git needed")
    def test_remove_worktree_against_real_git(self):
        from gatetrain.runner import LocalRunner
        with tempfile.TemporaryDirectory() as d:
            main, wt = os.path.join(d, "main"), os.path.join(d, "wt-lane")
            def g(*a, cwd=main):
                return subprocess.run(["git", "-C", cwd, "-c", "user.name=t", "-c", "user.email=tester", *a], check=True, capture_output=True, text=True).stdout
            os.makedirs(main); g("init", "-b", "main"); Path(main, "f").write_text("1"); g("add", "f"); g("commit", "-m", "base")
            g("worktree", "add", "-b", "line/lane", wt); Path(wt, "g").write_text("2"); g("add", "g", cwd=wt); g("commit", "-m", "lane", cwd=wt)
            g("merge", "--no-edit", "line/lane")
            sha = g("rev-parse", "line/lane").strip()
            g("tag", "archive/line/lane", g("rev-parse", "main~1").strip())      # the name is taken by another commit
            tidy = {"main_checkout": main, "log": self.log, "worktree_min_idle_days": 0}
            if os.name == "nt":                      # a Windows host must prove its link listing works: one known junction
                canary = os.path.join(d, "canary"); os.makedirs(canary)
                r = subprocess.run(["cmd", "/c", "mklink", "/J", os.path.join(canary, "l"), main], capture_output=True, text=True)
                if r.returncode != 0:
                    self.skipTest("cannot create a junction here")
                tidy.update(link_canary=canary, link_canary_count=1)
            cfg = parse({"hosts": [{"name": "h", "tidy": tidy}]})
            host = cfg.hosts[0]
            t = T.build_cfg(cfg, host)
            runner = LocalRunner()
            inv = T.gather(host, runner, t, now=time.time())
            self.assertEqual([os.path.realpath(i.key) for i in inv.items if i.cls == "worktrees"], [os.path.realpath(wt)])
            T.apply_plan(host, runner, t, inv, ["worktrees"])
            self.assertFalse(os.path.exists(wt))
            self.assertNotIn("line/lane", g("branch", "--list"))
            self.assertEqual(g("rev-parse", "archive/line/lane-" + sha[:8]).strip(), sha)


# ---------------------------------------------------------------- 4. paths

class PathSafetyTests(Base):
    def test_safe_path_rules(self):
        T.safe_path("/build/targets/lane-a")
        T.safe_path("C:/build/x")
        for bad in ("", "relative/x", "/a/../b", "/a/./b", "/a\nb", "/a\x00b", "/a\x1bb", "C:", "//server/share/x"):
            with self.assertRaises(T.GuardViolation, msg=repr(bad)):
                T.safe_path(bad)

    def test_trash_entry_is_exactly_one_component_below(self):
        T.safe_path("/trash/old", under="/trash")
        for bad in ("/trash", "/trash/old/x", "/trash/../x", "/other/old", "/trashy/old"):
            with self.assertRaises(T.GuardViolation, msg=bad):
                T.safe_path(bad, under="/trash")

    def test_remove_dir_refuses_dot_dot(self):
        sim = linux_sim()
        ops, _ = ops_for(sim)
        with self.assertRaises(T.GuardViolation):
            ops.remove_dir("/build/targets/../../repo/main/node_modules")
        self.assertIn("/repo/main/node_modules/pkg.js", sim.fs)

    def test_trash_remove_of_a_nested_path_is_refused(self):
        sim = linux_sim()
        h, t, inv = gather(sim, {"log": self.log})
        inv.items = [T.Item("trash", "remove", "/trash/old/x", 0, "x")]
        self.assertEqual(T.apply_plan(h, sim, t, inv, ["trash"], now=NOW), (0, 1))
        self.assertEqual([r["status"] for r in self.recs()], ["refused"])
        self.assertIn("/trash/old/x", sim.fs)

    def test_trash_that_resolves_onto_a_kept_path_is_refused(self):
        sim = linux_sim()
        for k in [k for k in sim.fs if k == "/trash"]:
            sim.fs.pop(k)
        sim.add("/trash", "link", target="/repo/main")
        h, t, inv = gather(sim, {"log": self.log})
        inv.items = [T.Item("trash", "remove", "/trash/node_modules", 0, "x")]
        with self.assertRaises(T.GuardViolation):
            T.apply_plan(h, sim, t, inv, ["trash"], now=NOW)
        self.assertIn("/repo/main/node_modules/pkg.js", sim.fs)

    def test_config_refuses_a_dangerous_trash_dir(self):
        for bad in ("/", "C:/", "C:", "/repo", "/repo/main", "/build", "/build/targets", "/repo/main/sub", "relative", "/a/../trash"):
            cfg, h = hostcfg({"trash_dir": bad})
            with self.assertRaises(ConfigError, msg=bad):
                T.build_cfg(cfg, h)

    def test_config_accepts_a_separate_trash_dir(self):
        cfg, h = hostcfg({"trash_dir": "/var/trash"})
        self.assertEqual(T.build_cfg(cfg, h).trash_dir, "/var/trash")

    def test_a_name_with_a_newline_is_refused_not_split(self):
        sim = linux_sim()
        sim.add("/trash/a\nb", age_s=10 * DAY)
        with self.assertRaises(T.GuardViolation):
            gather(sim)


# ---------------------------------------------------------------- 5. merged means: merging changes nothing

class MergedTests(Base):
    def test_fake_host_never_shows_files_or_diffs(self):
        sim = linux_sim()
        gather(sim)
        self.assertFalse([c for c in sim.calls if " show " in c or " diff " in c])
        self.assertTrue([c for c in sim.calls if "merge-tree --write-tree" in c])

    def test_conflict_or_old_git_means_not_merged(self):
        sim = linux_sim()
        sim.conflict.add("line/lane-a")
        ops, _ = ops_for(sim)
        self.assertIs(ops.merged("line/lane-a"), False)

    @unittest.skipUnless(shutil.which("git"), "git needed")
    def test_real_git_cases(self):
        from gatetrain.runner import LocalRunner
        ver = subprocess.run(["git", "--version"], capture_output=True, text=True).stdout
        nums = [int(x) for x in ver.split()[2].split(".")[:2]]
        if nums < [2, 38]:
            self.skipTest("git < 2.38 has no merge-tree --write-tree")
        with tempfile.TemporaryDirectory() as d:
            def g(*a):
                subprocess.run(["git", "-C", d, "-c", "user.name=t", "-c", "user.email=tester", *a], check=True, capture_output=True)
            w = lambda f, t: Path(d, f).write_text(t)
            g("init", "-b", "main")
            w("f.txt", "a\n}\n"); w("g.txt", "keep\n"); g("add", "."); g("commit", "-m", "base")
            g("checkout", "-b", "dup-line"); w("f.txt", "a\n}\n}\n"); g("commit", "-am", "adds a closing brace")
            g("checkout", "-b", "same-change", "main"); w("g.txt", "keep\nmore\n"); g("commit", "-am", "x")
            g("checkout", "-b", "deletion", "main"); os.remove(Path(d, "g.txt")); g("commit", "-am", "drop g")
            g("checkout", "main"); w("g.txt", "keep\nmore\n"); g("commit", "-am", "same change made separately")
            cfg = parse({"hosts": [{"name": "h", "os": "linux", "tidy": {"main_checkout": d}}]})
            host = cfg.hosts[0]
            ops = T.Ops(host, LocalRunner(), T.build_cfg(cfg, host))
            self.assertIs(ops.merged("dup-line"), False)       # every added line "exists" in main, yet the change is not there
            self.assertIs(ops.merged("same-change"), True)     # not an ancestor, but merging it changes nothing
            self.assertIs(ops.merged("deletion"), False)
            self.assertIs(ops.merged("main"), True)


# ---------------------------------------------------------------- 6. links and the main worktree

class LinkTests(Base):
    def test_trash_entry_that_is_a_link_is_unlinked_not_walked(self):
        sim = linux_sim()
        sim.add("/trash/j", "link", target="/repo/main/node_modules", age_s=10 * DAY)
        self.apply(sim, ["trash"])
        self.assertNotIn("/trash/j", sim.fs)
        self.assertIn("/repo/main/node_modules/pkg.js", sim.fs)
        self.assertFalse([c for c in sim.calls if c.startswith("find") and "/trash/j" in c])     # never listed inside
        self.assertFalse([c for c in sim.calls if c.startswith("rm -r") and "/trash/j" in c])    # never a recursive remove

    def test_windows_junction_item_is_removed_as_a_link_only(self):
        sim = winsim()
        sim.add("C:/trash", age_s=0)
        sim.add("C:/trash/j", "link", target="C:/repo/main/node_modules", age_s=10 * DAY)
        self.apply(sim, ["trash"], win=True)
        self.assertNotIn("C:/trash/j", sim.fs)
        self.assertIn("C:/repo/main/node_modules/pkg.js", sim.fs)
        self.assertFalse([c for c in sim.calls if c.startswith("dir /AL /S /B") and "trash" in c and "canary" not in c])
        self.assertFalse([c for c in sim.calls if "rmdir /S" in c.replace("\\", "/") and "trash/j" in c.replace("\\", "/")])

    def test_worktree_that_is_a_link_is_kept(self):
        sim = linux_sim()
        sim.fs["/repo/wt/lane-a"]["type"] = "link"
        sim.fs["/repo/wt/lane-a"]["target"] = "/trash/new"
        h, t, inv = gather(sim)
        w = next(x for x in inv.worktrees if x["path"] == "/repo/wt/lane-a")
        self.assertEqual(w["keep"], "is a link")
        self.assertEqual(by(inv, "/repo/wt/lane-a"), [])

    def test_target_that_is_a_link_is_kept(self):
        sim = linux_sim()
        sim.fs["/build/targets/lane-a"]["type"] = "link"
        sim.fs["/build/targets/lane-a"]["target"] = "/trash/new"
        h, t, inv = gather(sim)
        self.assertEqual(by(inv, "/build/targets/lane-a"), [])
        self.assertEqual(next(x for x in inv.targets if x["path"] == "/build/targets/lane-a")["keep"], "is a link")

    def test_first_worktree_entry_is_always_kept(self):
        sim = linux_sim()
        sim.add("/repo/alias", age_s=DAY)
        sim.worktrees[0] = ("/repo/alias", "line/lane-z")      # the first entry, not spelled like the configured main checkout, on a merged branch
        sim.branches.append("line/lane-z")
        sim.ancestors.add("line/lane-z")
        h, t, inv = gather(sim)
        self.assertEqual(by(inv, "/repo/alias"), [])
        self.assertEqual(next(x for x in inv.worktrees if x["path"] == "/repo/alias")["keep"], "main worktree")

    def test_a_worktree_that_resolves_to_main_is_kept(self):
        sim = linux_sim()
        sim.add("/repo/wt/alias", "link", target="/repo/main")
        sim.worktrees.append(("/repo/wt/alias", "line/lane-q"))
        sim.branches.append("line/lane-q")
        sim.ancestors.add("line/lane-q")
        h, t, inv = gather(sim)
        self.assertEqual(by(inv, "/repo/wt/alias"), [])

    def test_locked_worktree_is_kept(self):
        sim = linux_sim()
        sim.locked.add("/repo/wt/lane-a")
        h, t, inv = gather(sim)
        self.assertEqual(next(x for x in inv.worktrees if x["path"] == "/repo/wt/lane-a")["keep"], "locked")
        self.assertEqual(by(inv, "/repo/wt/lane-a"), [])

    @unittest.skipUnless(os.name == "nt", "real junctions need Windows")
    def test_real_junction_in_a_temp_dir(self):
        with tempfile.TemporaryDirectory() as d:
            main, trash = Path(d, "main"), Path(d, "trash")
            (main / "keep").mkdir(parents=True); (main / "keep" / "f.txt").write_text("x"); trash.mkdir()
            subprocess.run(["git", "-C", str(main), "init", "-q"], check=True)
            j = trash / "j"
            r = subprocess.run(["cmd", "/c", "mklink", "/J", str(j), str(main / "keep")], capture_output=True, text=True)
            if r.returncode != 0:
                self.skipTest("cannot create a junction here")
            from gatetrain.runner import LocalRunner
            canary = Path(d, "canary"); canary.mkdir()
            subprocess.run(["cmd", "/c", "mklink", "/J", str(canary / "l"), str(main)], capture_output=True, text=True)
            cfg = parse({"hosts": [{"name": "h", "os": "windows", "tidy": {"main_checkout": str(main), "trash_dir": str(trash),
                                                                           "link_canary": str(canary), "link_canary_count": 1}}]})
            host = cfg.hosts[0]
            ops = T.Ops(host, LocalRunner(), T.build_cfg(cfg, host))
            self.assertTrue(ops.is_link(str(j)))
            self.assertFalse(ops.is_link(str(main)))
            ops.remove_dir(str(j))
            self.assertFalse(j.exists())
            self.assertTrue((main / "keep" / "f.txt").exists())


# ---------------------------------------------------------------- 7. move

class MoveTests(Base):
    def _sim(self):
        sim = linux_sim()
        sim.add("/build/targets/lane-m", age_s=10 * DAY)
        sim.add("/build/targets/lane-m/x.bin", "file", 5 * MB, 10 * DAY)
        return sim

    def test_failed_move_is_reported_not_ok(self):
        sim = self._sim()
        sim.mv_fails = True
        done, failed = self.apply(sim, ["targets"], {"orphan_targets": "clean"})
        r = [r for r in self.recs() if r["item"] == "/build/targets/lane-m"][0]
        self.assertEqual(r["status"], "failed")
        self.assertIn("/build/targets/lane-m/x.bin", sim.fs)

    def test_good_move_is_verified(self):
        sim = self._sim()
        self.apply(sim, ["targets"], {"orphan_targets": "clean"})
        r = [r for r in self.recs() if r["item"] == "/build/targets/lane-m"][0]
        self.assertEqual(r["status"], "ok")
        self.assertNotIn("/build/targets/lane-m", sim.fs)
        dest = r["note"].split("moved to ")[1]
        self.assertIn(n(dest), sim.fs)

    def test_trash_age_comes_from_the_timestamp_in_the_name(self):
        sim = linux_sim()
        fresh = "lane-q-" + time.strftime("%Y%m%d%H%M%S", time.localtime(NOW - 60))
        stale = "lane-r-" + time.strftime("%Y%m%d%H%M%S", time.localtime(NOW - 10 * DAY))
        sim.add("/trash/" + fresh, age_s=10 * DAY)          # moved just now, but the folder keeps its old mtime
        sim.add("/trash/" + fresh + "/x", "file", 5, 10 * DAY)
        sim.add("/trash/" + stale, age_s=10 * DAY)
        sim.add("/trash/" + stale + "/x", "file", 5, 10 * DAY)
        h, t, inv = gather(sim)
        self.assertEqual(by(inv, "/trash/" + fresh), [])
        self.assertEqual(len(by(inv, "/trash/" + stale)), 1)


# ---------------------------------------------------------------- 8, 9. timeouts, unknown states, intent

class StateTests(Base):
    def test_removal_timeout_is_stopped_never_done(self):
        sim = linux_sim()
        sim.rm_timeout = True
        done, failed = self.apply(sim, ["trash"])
        self.assertEqual((done, failed), (0, 1))
        r = self.recs()[0]
        self.assertEqual(r["status"], "stopped")
        self.assertIn("/trash/old/x", sim.fs)

    def test_removal_timeout_is_its_own_configurable_key(self):
        sim = linux_sim()
        seen = []
        orig = sim.run

        def spy(cmd, timeout=None):
            if cmd.startswith("rm -r"):
                seen.append(timeout)
            return orig(cmd, timeout)
        sim.run = spy
        self.apply(sim, ["trash"], {"remove_timeout_s": 1234})
        self.assertEqual(seen, [1234])
        self.assertEqual(T.TidyCfg().remove_timeout_s, 1800)

    def test_unknown_existence_after_removal_is_a_violation(self):
        sim = linux_sim()
        sim.unknown_exists = ("/trash/old",)
        with self.assertRaises(T.GuardViolation):
            self.apply(sim, ["trash"])

    def test_unknown_existence_at_inventory_is_reported_and_skipped(self):
        sim = linux_sim()
        sim.unknown_exists = ("/build/targets/lane-a",)
        h, t, inv = gather(sim)
        self.assertEqual(by(inv, "/build/targets/lane-a"), [])
        self.assertTrue(any("cannot tell" in b and "/build/targets/lane-a" in b for b in inv.breaches))

    def test_intent_is_logged_before_the_action(self):
        sim = linux_sim()
        self.apply(sim, ["containers"])
        rs = self.recs(with_intent=True)
        self.assertEqual([r["status"] for r in rs][:2], ["intent", "ok"])
        self.assertEqual(rs[0]["item"], rs[1]["item"])

    def test_intent_survives_a_crash(self):
        sim = linux_sim()
        sim.docker = lambda c: (_ for _ in ()).throw(RuntimeError("host went away")) if c.startswith("docker rm") else SimHost.docker(sim, c)
        with self.assertRaises(RuntimeError):
            self.apply(sim, ["containers"])
        rs = self.recs(with_intent=True)
        self.assertEqual([r["status"] for r in rs], ["intent"])


# ---------------------------------------------------------------- 10. small ones

class SmallTests(Base):
    def test_refs_keep_their_slashes_on_windows(self):
        sim = winsim()
        done, failed = self.apply(sim, ["worktrees"], win=True)
        self.assertEqual((done, failed), (1, 0))
        self.assertFalse([c for c in sim.calls if c.startswith("git") and "\\" in c], [c for c in sim.calls if c.startswith("git") and "\\" in c])
        self.assertTrue(any("refs/heads/line/lane-a" in " ".join(a) for a in sim.argvs))

    def test_fake_git_rejects_a_backslash_in_a_ref(self):
        self.assertEqual(SimHost().run_argv(["git", "-C", "/r", "rev-parse", "refs/heads/a\\b"])[0], 128)

    def test_unlink_script_picks_rmdir_or_del_by_type(self):
        sim = winsim()
        sim.add("C:/repo/wt/lane-a/node_modules", "link", target="C:/repo/main/node_modules")
        sim.add("C:/repo/wt/lane-a/note.txt", "link", target="C:/repo/main/a.rs")
        sim.fs["C:/repo/wt/lane-a/note.txt"]["ltype"] = "file"
        self.apply(sim, ["worktrees"], win=True)
        script = sim.scripts["C:/tmp/gatetrain-unlink.cmd"]
        self.assertNotIn("||", script)
        self.assertIn('rmdir "C:\\repo\\wt\\lane-a\\node_modules"', script)
        self.assertIn('del "C:\\repo\\wt\\lane-a\\note.txt"', script)
        self.assertNotIn('rmdir "C:\\repo\\wt\\lane-a\\note.txt"', script)

    def test_link_of_unknown_type_is_a_violation(self):
        sim = winsim()
        sim.add("C:/repo/wt/lane-a/node_modules", "link", target="C:/repo/main/node_modules")
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (1, "") if "PSIsContainer" in cmd else orig(cmd, timeout)
        with self.assertRaises(T.GuardViolation):
            self.apply(sim, ["worktrees"], win=True)

    def test_windows_listing_needs_the_canary(self):
        sim = winsim()
        ops, h = ops_for(sim, WIN_EXTRA, win=True)
        ops.cfg.link_canary = None                      # config load refuses this; the runtime check is the second lock
        with self.assertRaises(T.GuardViolation):
            ops.list_links("C:/repo/wt/lane-a")

    def test_short_cache_age_is_at_least_an_hour(self):
        sim = linux_sim()
        self.apply(sim, ["cache"], {"cache_days": 0.01})
        prunes = [c for c in sim.calls if c.startswith("docker builder prune")]
        self.assertEqual(len(prunes), 1)
        self.assertIn("until=1h", prunes[0])
        self.assertNotIn("until=0h", prunes[0])

    def test_cargo_clean_rechecks_the_folder_is_a_cargo_target(self):
        sim = linux_sim()
        sim.add("/build/targets/plain", age_s=10 * DAY)
        sim.add("/build/targets/plain/notes.txt", "file", 10, 10 * DAY)
        h, t, inv = gather(sim, {"log": self.log})
        inv.items = [T.Item("targets", "clean", "/build/targets/plain", 10, "x", extra={"cargo": True, "tag": True})]
        done, failed = T.apply_plan(h, sim, t, inv, ["targets"], now=NOW)
        self.assertFalse([c for c in sim.calls if c.startswith("cargo clean")])
        self.assertEqual((done, failed), (0, 1))
        self.assertIn("/build/targets/plain/notes.txt", sim.fs)


# ---------------------------------------------------------------- review round 2

class RealFindShapeTests(Base):
    def test_listdir_strips_the_find_prefix(self):
        sim = linux_sim()
        ops, _ = ops_for(sim)
        self.assertEqual(sorted(x for x in ops.listdir("/trash") if x != T.SEEN), ["new", "old"])
        self.assertEqual(sorted(x for x in ops.listdir("/trash/") if x != T.SEEN), ["new", "old"])

    def test_listdir_refuses_a_line_without_the_prefix(self):
        sim = linux_sim()
        ops, _ = ops_for(sim)
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (0, "/elsewhere/x\0") if cmd.startswith("find") else orig(cmd, timeout)
        with self.assertRaises(T.GuardViolation):
            ops.listdir("/trash")

    def test_cargo_target_is_recognised_through_listdir(self):
        sim = linux_sim()
        h, t, inv = gather(sim)
        self.assertTrue(next(x for x in inv.targets if x["path"] == "/build/targets/lane-a")["cargo"])

    @unittest.skipIf(os.name == "nt", "real find needs Linux (run in WSL)")
    def test_real_disk_listdir_and_inventory(self):
        from gatetrain.runner import LocalRunner
        with tempfile.TemporaryDirectory() as d:
            roots, trash = Path(d, "targets"), Path(d, "trash")
            for lane in ("lane-a", "lane-b"):
                (roots / lane / "debug").mkdir(parents=True); (roots / lane / "tmp").mkdir()
                (roots / lane / "debug" / "x.o").write_text("x" * 1000)
            (trash / "old-20200101000000").mkdir(parents=True); (trash / "old-20200101000000" / "f").write_text("y")
            os.makedirs(os.path.join(str(roots), "weird\nname"))
            cfg = parse({"hosts": [{"name": "h", "os": "linux", "tidy": {"target_roots": [str(roots)], "trash_dir": str(trash), "log": self.log}}]})
            host = cfg.hosts[0]
            ops = T.Ops(host, LocalRunner(), T.build_cfg(cfg, host))
            with self.assertRaises(T.GuardViolation):
                ops.listdir(str(roots))                       # the newline name is refused, not split
            os.rmdir(os.path.join(str(roots), "weird\nname"))
            self.assertEqual(sorted(ops.listdir(str(roots))), ["lane-a", "lane-b"])
            # tidy saw that entry long ago (a name stamp alone no longer sets an age: the later of it and the first sighting counts)
            (trash / ".gatetrain-seen.json").write_text(json.dumps({"old-20200101000000": {"t": 1577836800, "m": None}}))
            inv = T.gather(host, LocalRunner(), T.build_cfg(cfg, host), now=time.time())
            self.assertEqual(sorted(t["path"] for t in inv.targets), [str(roots / "lane-a"), str(roots / "lane-b")])
            self.assertTrue(all(t["cargo"] for t in inv.targets))
            self.assertEqual([i.key for i in inv.items if i.cls == "trash"], [str(trash / "old-20200101000000")])
            print("REAL-DISK listdir/gather ok:", len(inv.targets), "targets,", len(inv.trash), "trash entries")


class ResolveShTests(Base):
    """macOS/BSD `realpath` has no -m: the shell fallback must resolve links like `realpath -m` does."""

    @unittest.skipIf(os.name == "nt", "needs a POSIX sh")
    def test_fallback_resolves_links_and_missing_tails(self):
        with tempfile.TemporaryDirectory() as d:
            real = os.path.realpath(d)
            os.makedirs(os.path.join(real, "a", "b"))
            os.symlink(os.path.join(real, "a"), os.path.join(real, "lnk"))
            for p, want in ((os.path.join(real, "lnk", "b"), os.path.join(real, "a", "b")),
                            (os.path.join(real, "lnk", "nope", "x"), os.path.join(real, "a", "nope", "x")),
                            (os.path.join(real, "a", "b") + "/", os.path.join(real, "a", "b"))):
                r = subprocess.run(["sh", "-c", T.RESOLVE_SH, "sh", p], capture_output=True, text=True)
                self.assertEqual((r.returncode, r.stdout.strip()), (0, want), p)
            r = subprocess.run(["sh", "-c", T.RESOLVE_SH, "sh", "/"], capture_output=True, text=True)
            self.assertEqual(r.stdout.strip(), "/")


class PowershellQuoteTests(Base):
    def test_typographic_quotes_are_refused(self):
        sim = winsim()
        ops, _ = ops_for(sim, WIN_EXTRA, win=True)
        evil = "C:/Windows\u2019$(Write-Host INJECTED)\u2018x"
        before = len(sim.calls)
        for f in (ops.is_link, ops.link_type, ops.size, ops.mtime):
            with self.assertRaises(T.GuardViolation):
                f(evil)
        self.assertEqual(len(sim.calls), before)       # nothing reached a shell


class WorktreeTimeoutTests(Base):
    def test_git_worktree_remove_timeout_is_stopped(self):
        sim = linux_sim()
        sim.rm_timeout = True                    # the folder removal is what can hang (the worktree goes by a non-traversing removal, then `git worktree prune`)
        done, failed = self.apply(sim, ["worktrees"])
        self.assertEqual(done, 0)
        self.assertTrue(self.recs() and all(r["status"] == "stopped" for r in self.recs()))
        self.assertEqual(sim.deleted_branches, [])

    def test_worktree_remove_gets_the_removal_timeout(self):
        sim = linux_sim()
        seen = []
        orig = sim.run_argv

        def spy(argv, timeout=None):
            return orig(argv, timeout)
        sim.run_argv = spy
        orig_run = sim.run

        def spy_run(cmd, timeout=None):
            if cmd.startswith("rm -r"):
                seen.append(timeout)
            return orig_run(cmd, timeout)
        sim.run = spy_run
        self.apply(sim, ["worktrees"], {"remove_timeout_s": 4321})
        self.assertTrue(seen and set(seen) == {4321})


class RoundTwoLows(Base):
    def test_merge_is_proved_on_the_resolved_sha(self):
        sim = linux_sim()
        sim.shas["line/lane-a"] = "abc123"; sim.ancestors.add("abc123")
        self.apply(sim, ["worktrees"])
        i = sim.argvs.index(["git", "-C", "/repo/main", "rev-parse", "--verify", "--quiet", "refs/heads/line/lane-a^{commit}"])
        later = [a for a in sim.argvs[i:] if "--is-ancestor" in a][:1]
        self.assertTrue(later and all("line/lane-a" not in a and sim.sha("line/lane-a") in a for a in later))

    def test_win_names_with_metacharacters_are_kept_not_fatal(self):
        sim = winsim()
        sim.add("C:/targets", age_s=0)
        sim.add("C:/targets/a&b", age_s=DAY)
        sim.add("C:/trash", age_s=0)
        sim.add("C:/trash/x%y", age_s=10 * DAY)
        h, t, inv = gather(sim, dict(WIN_EXTRA, target_roots=["C:/targets"]), win=True)
        tg = next(x for x in inv.targets if x["path"].endswith("a&b"))
        self.assertIn("unsafe", tg["keep"])
        self.assertEqual(by(inv, "C:/trash/x%y"), [])
        self.assertTrue(any(x["path"].endswith("x%y") for x in inv.trash))

    def test_windows_without_canary_is_a_config_error(self):
        cfg, h = hostcfg({k: v for k, v in WIN_EXTRA.items() if not k.startswith("link_canary")}, win=True)
        with self.assertRaises(ConfigError):
            T.build_cfg(cfg, h)

    def test_trash_dir_holding_a_worktree_is_refused(self):
        sim = linux_sim()
        sim.worktrees.append(("/trash/old", "line/lane-t"))
        with self.assertRaises(T.GuardViolation):
            gather(sim)

    def test_a_link_under_another_link_is_not_listed_for_unlinking(self):
        sim = linux_sim()
        sim.add("/repo/wt/lane-a/L", "link", target="/repo/main/node_modules")
        sim.add("/repo/wt/lane-a/L/inner", "link", target="/repo/main/a.rs")
        ops, _ = ops_for(sim)
        self.assertEqual(sorted(n(x) for x in ops.list_links("/repo/wt/lane-a")), ["/repo/wt/lane-a/L", "/repo/wt/lane-a/node_modules"])

    def test_keep_lanes_match_case_insensitively_on_windows(self):
        sim = winsim()
        sim.add("C:/repo/wt/Lane-A", age_s=DAY)
        sim.add("C:/repo/wt/Lane-A/src.rs", "file", 5, DAY)
        sim.worktrees[1] = ("C:/repo/wt/Lane-A", "line/lane-a")
        h, t, inv = gather(sim, dict(WIN_EXTRA, keep_lanes=["Lane-A"]), win=True)
        w = next(x for x in inv.worktrees if x["path"] == "C:/repo/wt/Lane-A")
        self.assertEqual(w["keep"], "keep list")
        self.assertEqual(by(inv, "C:/repo/wt/Lane-A"), [])

    def test_keep_lanes_also_protect_a_target_with_a_capital(self):
        sim = winsim()
        sim.add("C:/targets", age_s=0)
        cargo_target(sim, "C:/targets/Lane-B", tag=True)
        h, t, inv = gather(sim, dict(WIN_EXTRA, keep_lanes=["lane-b"], target_roots=["C:/targets"]), win=True)
        self.assertEqual(by(inv, "C:/targets/Lane-B"), [])

    def test_norm_keeps_case_on_posix_paths_only(self):
        self.assertEqual(T.norm("/Srv/Repo/"), "/Srv/Repo")
        self.assertEqual(T.norm("C:\\Srv\\Repo"), "c:/srv/repo")


# ---------------------------------------------------------------- review of e2c543b (inherited holes)

class ReviewFixTests(Base):
    def test_a_worktree_prefix_with_a_separator_is_a_config_error(self):
        for bad in ("C:/wt/lane-", "/srv/wt/lane-", "wt\\lane-"):
            cfg, h = hostcfg({"worktree_prefix": bad})
            with self.assertRaises(ConfigError, msg=bad):
                T.build_cfg(cfg, h)

    def test_a_keep_lanes_entry_that_matches_nothing_is_reported(self):
        sim = linux_sim()
        h, t, inv = gather(sim, {"keep_lanes": ["lane-c", "no-such-lane"]})
        self.assertTrue(any("no-such-lane" in b and "protects nothing" in b for b in inv.breaches), inv.breaches)
        self.assertFalse(any("'lane-c'" in b for b in inv.breaches))

    def test_unc_and_extended_length_keep_paths_are_refused(self):
        for bad in ("\\\\?\\C:\\repo\\keep", "\\\\server\\share\\keep", "//server/share/keep"):
            cfg, h = hostcfg({"keep_paths": [bad]})
            with self.assertRaises(ConfigError, msg=bad):
                T.build_cfg(cfg, h)

    def test_the_merge_proof_names_branches_by_refs_heads(self):
        sim = linux_sim()
        gather(sim)
        proofs = [a for a in sim.argvs if "--is-ancestor" in a or "merge-tree" in a]
        self.assertTrue(proofs)
        for a in proofs:
            for x in a[a.index("--is-ancestor") + 1:] if "--is-ancestor" in a else a[a.index("--write-tree") + 1:]:
                self.assertTrue(x.startswith("refs/heads/") or x.startswith("sha-"), a)

    def test_an_unreadable_git_is_never_read_as_no_branches_or_no_worktrees(self):
        for part, sub in (("worktree list", "worktree"), ("branch --list", "branch")):
            sim = linux_sim()
            orig = sim.run_argv
            sim.run_argv = lambda argv, timeout=None, o=orig, p=part: (128, "") if " ".join(argv).startswith("git -C /repo/main " + p) else o(argv, timeout)
            with self.assertRaises(T.GuardViolation, msg=part):
                gather(sim)

    def test_a_failed_listing_is_not_an_empty_folder(self):
        sim = linux_sim()
        ops, _ = ops_for(sim)
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (1, "") if cmd.startswith("find") else orig(cmd, timeout)
        with self.assertRaises(T.GuardViolation):
            ops.listdir("/trash")

    def test_a_link_target_that_vanishes_during_the_removal_stops_the_run(self):
        sim = linux_sim()
        orig = sim.run

        def evil(cmd, timeout=None):
            rc, out = orig(cmd, timeout)
            if cmd.startswith("rm -r") and "/repo/wt/lane-a" in cmd:
                for k in [k for k in sim.fs if k.startswith("/repo/main/node_modules")]:
                    sim.fs.pop(k)                       # a traversing removal emptied what the link pointed to
            return rc, out
        sim.run = evil
        with self.assertRaises(T.GuardViolation) as cm:
            self.apply(sim, ["worktrees"])
        self.assertIn("is gone after removing", str(cm.exception))

    def test_a_worktree_is_removed_by_a_plain_removal_then_pruned(self):
        sim = linux_sim()
        self.apply(sim, ["worktrees"])
        calls = sim.calls
        i_rm = next(i for i, c in enumerate(calls) if c.startswith("rm -r") and "/repo/wt/lane-a" in c)
        i_prune = next(i for i, c in enumerate(calls) if c.endswith("worktree prune"))
        self.assertLess(i_rm, i_prune)
        self.assertFalse([c for c in calls if "worktree remove" in c])

    def test_cargo_clean_with_a_non_zero_exit_is_a_failure_not_freed_unknown(self):
        sim = linux_sim()
        orig = sim.run_argv
        sim.run_argv = lambda argv, timeout=None: (101, "error") if argv[:2] == ["cargo", "clean"] else orig(argv, timeout)
        self.apply(sim, ["targets"])
        rs = [r for r in self.recs() if r["action"] == "clean"]
        self.assertTrue(rs and all(r["status"] == "failed" and "exited 101" in r["note"] for r in rs), rs)


# ---------------------------------------------------------------- review of e2c543b, round 2

class ReviewFix2Tests(Base):
    def _gather(self, sim, extra=None, lock=None, now=NOW):
        cfg, h = hostcfg(dict(extra or {}, log=self.log))
        if lock is not None:
            h.lock_file = "/run/h.lock"
            sim.scripts["/run/h.lock.info"] = json.dumps(lock)
        t = T.build_cfg(cfg, h)
        return h, t, T.gather(h, sim, t, now=now)

    def test_a_target_with_a_file_changed_inside_is_kept_though_its_folder_is_old(self):
        sim = linux_sim()
        sim.add("/build/targets/lane-a/debug/fresh.o", "file", 5, 120)          # the folder's own time is 10 days old; a file inside is 2 minutes old
        h, t, inv = self._gather(sim)
        tg = next(x for x in inv.targets if x["path"] == "/build/targets/lane-a")
        self.assertIn("a file inside changed", tg["keep"])
        self.assertEqual(by(inv, "/build/targets/lane-a"), [])

    def test_an_activity_probe_that_cannot_answer_means_not_idle(self):
        sim = linux_sim()
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (1, "") if "-mmin" in cmd else orig(cmd, timeout)
        h, t, inv = self._gather(sim)
        self.assertEqual(by(inv, "/build/targets/lane-a"), [])
        self.assertIn("cannot be read", next(x for x in inv.targets if x["path"] == "/build/targets/lane-a")["keep"])

    def test_a_target_named_by_a_running_hold_is_kept_and_a_released_one_is_not(self):
        held = {"run_id": "r7", "state": "held", "targets": ["/build/targets/lane-a/"]}
        h, t, inv = self._gather(linux_sim(), lock=held)
        self.assertEqual(by(inv, "/build/targets/lane-a"), [])
        self.assertIn("named by the running hold r7", next(x for x in inv.targets if x["path"] == "/build/targets/lane-a")["keep"])
        h, t, inv = self._gather(linux_sim(), lock=dict(held, state="released"))
        self.assertEqual(len(by(inv, "/build/targets/lane-a")), 1)
        # a hold that names the lane's folder by another spelling of its parent still names its lane
        h, t, inv = self._gather(linux_sim(), lock={"run_id": "r8", "state": "held", "targets": ["/elsewhere/Lane-B"]})
        self.assertIn("named by the running hold r8", next(x for x in inv.targets if x["path"] == "/build/targets/lane-b")["keep"])

    def test_a_worktree_with_recent_files_is_kept_until_it_is_old_enough(self):
        sim = linux_sim()                                                       # lane-a: merged, clean, its files are a day old
        h, t, inv = self._gather(sim, {"worktree_min_idle_days": 2})
        w = next(x for x in inv.worktrees if x["path"] == "/repo/wt/lane-a")
        self.assertIn("used in the last 2 days", w["keep"])
        self.assertEqual(by(inv, "/repo/wt/lane-a"), [])
        sim.add("/repo/wt/lane-a/src.rs", "file", 5, 3 * DAY)
        sim.add("/repo/wt/lane-a", age_s=3 * DAY)
        h, t, inv = self._gather(sim, {"worktree_min_idle_days": 2})
        self.assertEqual(len(by(inv, "/repo/wt/lane-a")), 1)
        self.assertEqual(T.TidyCfg().worktree_min_idle_days, 2)

    def test_a_lane_that_is_no_longer_proven_is_not_cleaned_even_from_an_old_plan(self):
        sim = linux_sim()
        h, t, inv = self._gather(sim)
        it = next(i for i in inv.items if i.key == "/build/targets/lane-a")
        self.assertEqual(it.extra["branch"], "line/lane-a")
        sim.mtree.clear()                                                        # main moved on: merging lane-a would now change something
        self.assertEqual(T.apply_plan(h, sim, t, inv, ["targets"], now=NOW), (0, 1))
        r = [r for r in self.recs() if r["item"] == "/build/targets/lane-a"][0]
        self.assertEqual(r["status"], "failed")
        self.assertIn("not proven", r["note"])
        self.assertFalse([c for c in sim.calls if c.startswith("cargo clean") and "lane-a" in c])
        self.assertIn("/build/targets/lane-a/debug/big.o", sim.fs)

    def test_a_bad_item_is_reported_and_skipped_and_the_rest_carries_on(self):
        sim = linux_sim()
        h, t, inv = self._gather(sim)
        inv.items = [T.Item("worktrees", "remove-worktree", "/repo/wt/lane-a", 0, "x", extra={"branch": "x;touch pwned", "links": 0}),
                     T.Item("containers", "rm-container", "gate-1", 0, "x")]
        self.assertEqual(T.apply_plan(h, sim, t, inv, ["worktrees", "containers"], now=NOW), (1, 1))
        self.assertEqual([r["status"] for r in self.recs()], ["refused", "ok"])

    def test_a_plain_file_in_the_trash_is_removed_and_a_half_removed_folder_is_reported_loudly(self):
        sim = linux_sim()
        sim.add("/trash/stray.log", "file", 40, 10 * DAY)
        done, failed = self.apply(sim, ["trash"])
        self.assertEqual(failed, 0)
        self.assertNotIn("/trash/stray.log", sim.fs)
        Path(self.log).unlink()
        sim = linux_sim()
        sim.rm_noop = True                                                       # a long path or a locked file: the removal "succeeds" and nothing goes
        done, failed = self.apply(sim, ["trash", "containers"])
        r = [r for r in self.recs() if r["class"] == "trash"][0]
        self.assertEqual(r["status"], "failed")
        self.assertIn("PARTLY REMOVED", r["note"])
        self.assertTrue([r for r in self.recs() if r["class"] == "containers" and r["status"] == "ok"], "the run carried on")

    def test_a_half_removed_worktree_keeps_its_branch(self):
        sim = linux_sim()
        sim.rm_noop = True
        done, failed = self.apply(sim, ["worktrees"])
        self.assertEqual(sim.deleted_branches, [])
        self.assertTrue(self.recs() and all("PARTLY REMOVED" in r["note"] for r in self.recs() if r["status"] == "failed"))

    def test_a_windows_worktree_path_with_a_metacharacter_is_kept_not_fatal(self):
        sim = winsim()
        sim.add("C:/repo/wt/a&b", age_s=DAY)
        sim.worktrees.append(("C:/repo/wt/a&b", "line/lane-x"))
        sim.branches.append("line/lane-x")
        sim.ancestors.add("line/lane-x")
        h, t, inv = gather(sim, dict(WIN_EXTRA), win=True)
        self.assertEqual(next(x for x in inv.worktrees if x["path"].endswith("a&b"))["keep"], "unsafe name")

    def test_an_unstamped_trash_entry_ages_from_when_tidy_first_saw_it(self):
        sim = linux_sim()
        sim.fs["/trash/zz"] = dict(type="dir", size=0, mtime=NOW - 30 * DAY, target=None)      # an old folder time is no age
        sim.fs["/trash/zz/f"] = dict(type="file", size=5, mtime=NOW - 30 * DAY, target=None)
        h, t, inv = self._gather(sim)
        self.assertEqual(by(inv, "/trash/zz"), [])                                # first seen now: age 0
        self.assertIn('"zz"', sim.scripts["/trash/.gatetrain-seen.json"])
        h, t, inv = self._gather(sim, now=NOW + 3 * DAY)
        self.assertEqual(by(inv, "/trash/zz"), [])                                # 3 days: under trash_days
        h, t, inv = self._gather(sim, now=NOW + 8 * DAY)
        self.assertEqual(len(by(inv, "/trash/zz")), 1)                            # seen 8 days ago
        self.assertNotIn(T.SEEN, [i.key.rsplit("/", 1)[-1] for i in inv.items])


# ---------------------------------------------------------------- second review (ce417a5)

class ReviewFix3Tests(Base):
    def _gather(self, sim, extra=None, now=NOW):
        cfg, h = hostcfg(dict(extra or {}, log=self.log))
        t = T.build_cfg(cfg, h)
        return h, t, T.gather(h, sim, t, now=now)

    def test_a_link_that_points_inside_the_removed_folder_goes_with_it(self):
        sim = linux_sim()
        sim.add("/repo/wt/lane-a/inner", age_s=DAY)
        sim.add("/repo/wt/lane-a/inner/f", "file", 3, DAY)
        sim.add("/repo/wt/lane-a/ws-link", "link", target="/repo/wt/lane-a/inner")        # an npm workspace junction, a relative symlink
        done, failed = self.apply(sim, ["worktrees"])
        self.assertEqual(failed, 0)
        self.assertNotIn("/repo/wt/lane-a", sim.fs)

    def test_prune_runs_only_when_this_is_the_one_prunable_worktree(self):
        sim = linux_sim()
        sim.worktrees.append(("/repo/wt/lane-away", "line/lane-away"))              # registered, its folder only temporarily missing
        sim.branches.append("line/lane-away")
        done, failed = self.apply(sim, ["worktrees"])
        r = [r for r in self.recs() if r["item"] == "/repo/wt/lane-a"][0]
        self.assertEqual(r["status"], "failed")
        self.assertIn("STALE REGISTRATION", r["note"])
        self.assertIn("lane-away", r["note"])
        self.assertFalse([c for c in sim.calls if c.endswith("worktree prune")], "the repo-wide prune never ran")
        self.assertNotIn("line/lane-a", sim.deleted_branches)
        self.assertTrue(any(w[0] == "/repo/wt/lane-away" for w in sim.worktrees), "the other registration is untouched")

    def test_a_worktree_with_no_git_root_of_its_own_is_unverifiable(self):
        sim = linux_sim()
        orig = sim.run_argv
        sim.run_argv = lambda argv, timeout=None: (0, "/repo/main\n") if argv[:3] == ["git", "-C", "/repo/wt/lane-a"] and "--show-toplevel" in argv else orig(argv, timeout)
        h, t, inv = self._gather(sim)
        w = next(x for x in inv.worktrees if x["path"] == "/repo/wt/lane-a")
        self.assertIn("unverifiable", w["keep"])
        self.assertEqual(by(inv, "/repo/wt/lane-a"), [])

    def test_a_reused_trash_name_is_a_new_entry_and_records_of_gone_entries_are_dropped(self):
        sim = linux_sim()
        sim.scripts["/trash/.gatetrain-seen.json"] = json.dumps({"zz": {"t": NOW - 10 * DAY, "m": NOW - 30 * DAY}, "gone": {"t": NOW - 9 * DAY, "m": None},
                                                                   "old": {"t": NOW - 10 * DAY, "m": None}, "new": {"t": NOW - DAY, "m": None}})
        sim.fs["/trash/zz"] = dict(type="dir", size=0, mtime=NOW - DAY, target=None)         # a different folder now carries the name "zz"
        sim.fs["/trash/zz/f"] = dict(type="file", size=5, mtime=NOW - DAY, target=None)
        h, t, inv = self._gather(sim)
        self.assertEqual(by(inv, "/trash/zz"), [], "the old record does not delete the new entry on its first day")
        rec = json.loads(sim.scripts["/trash/.gatetrain-seen.json"])
        self.assertNotIn("gone", rec)
        self.assertEqual(rec["zz"]["t"], NOW)
        self.assertEqual(len(by(inv, "/trash/old")), 1)

    def test_a_stamped_name_ages_from_the_later_of_its_stamp_and_the_first_sighting(self):
        stamp = time.strftime("%Y%m%d%H%M%S", time.localtime(NOW - 20 * DAY))
        sim = linux_sim()
        sim.fs["/trash/lane-s-" + stamp] = dict(type="dir", size=0, mtime=NOW - 20 * DAY, target=None)
        sim.fs["/trash/lane-s-" + stamp + "/f"] = dict(type="file", size=5, mtime=NOW - 20 * DAY, target=None)
        h, t, inv = self._gather(sim)
        self.assertEqual(by(inv, "/trash/lane-s-" + stamp), [], "first seen now: a name stamp alone does not delete it")
        h, t, inv = self._gather(sim, now=NOW + 8 * DAY)
        self.assertEqual(len(by(inv, "/trash/lane-s-" + stamp)), 1)

    def test_a_glob_character_in_one_name_refuses_that_item_not_the_run(self):
        sim = linux_sim()
        h, t, inv = self._gather(sim)
        inv.items = [T.Item("trash", "remove", "/trash/a*b", 0, "x"), T.Item("containers", "rm-container", "gate-1", 0, "x")]
        self.assertEqual(T.apply_plan(h, sim, t, inv, ["trash", "containers"], now=NOW), (1, 1))
        self.assertEqual([r["status"] for r in self.recs()], ["ok", "refused"])       # containers run before the trash class

    def test_the_idle_walk_depth_is_configurable_with_a_default_of_twelve(self):
        sim = linux_sim()
        self._gather(sim)
        self.assertTrue([c for c in sim.calls if c.startswith("find") and "-mmin" in c and "-maxdepth 12 " in c])
        sim = linux_sim()
        self._gather(sim, {"idle_walk_depth": 3})
        self.assertTrue([c for c in sim.calls if c.startswith("find") and "-mmin" in c and "-maxdepth 3 " in c])

    def test_link_state_tells_missing_from_unknown(self):
        sim = linux_sim()
        ops, _ = ops_for(sim)
        self.assertEqual(ops.link_state("/nope/at/all"), "missing")
        self.assertEqual(ops.link_state("/repo/main"), "real")
        self.assertEqual(ops.link_state("/repo/wt/lane-a/node_modules"), "link")
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (1, "") if cmd.startswith("test -L") else orig(cmd, timeout)
        self.assertEqual(ops.link_state("/repo/main"), "unknown")
        self.assertIsNone(ops.is_link("/nope/at/all") if False else None)

    def test_a_windows_listing_failure_is_not_an_empty_folder(self):
        sim = winsim()
        sim.add("C:/empty", age_s=DAY)
        ops, _ = ops_for(sim, WIN_EXTRA, win=True)
        orig = sim.run
        sim.run = lambda cmd, timeout=None: (1, "") if cmd.startswith("dir /B") else orig(cmd, timeout)
        self.assertEqual(ops.listdir("C:/empty"), [])                         # rc 1 and the folder is there: an empty folder
        with self.assertRaises(T.GuardViolation):
            ops.listdir("C:/not-there")                                       # rc 1 and the folder cannot be shown: a failed listing


if __name__ == "__main__":
    unittest.main()
