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
        with self.assertRaises(T.GuardViolation):
            T.apply_plan(h, sim, t, inv, ["worktrees"], now=NOW)
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
        self.assertEqual((done, failed), (1, 0))
        name = "archive/line/lane-a-" + sim.sha("line/lane-a")[:8]
        self.assertEqual(sim.tag_refs[name], sim.sha("line/lane-a"))
        self.assertEqual(sim.tag_refs["archive/line/lane-a"], "someothersha")      # the old tag is untouched
        self.assertEqual(sim.deleted_branches, ["line/lane-a"])

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
        with self.assertRaises(T.GuardViolation):
            T.apply_plan(h, sim, t, inv, ["trash"], now=NOW)
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
            cfg = parse({"hosts": [{"name": "h", "tidy": {"main_checkout": d}}]})
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
        sim.fs["/repo/wt/lane-a"]["target"] = "/repo/main"
        h, t, inv = gather(sim)
        w = next(x for x in inv.worktrees if x["path"] == "/repo/wt/lane-a")
        self.assertEqual(w["keep"], "is a link")
        self.assertEqual(by(inv, "/repo/wt/lane-a"), [])

    def test_target_that_is_a_link_is_kept(self):
        sim = linux_sim()
        sim.fs["/build/targets/lane-a"]["type"] = "link"
        sim.fs["/build/targets/lane-a"]["target"] = "/repo/main"
        h, t, inv = gather(sim)
        self.assertEqual(by(inv, "/build/targets/lane-a"), [])
        self.assertEqual(next(x for x in inv.targets if x["path"] == "/build/targets/lane-a")["keep"], "is a link")

    def test_first_worktree_entry_is_always_kept(self):
        sim = linux_sim()
        sim.add("/repo/alias", "link", target="/repo/main")
        sim.worktrees[0] = ("/repo/alias", "line/lane-z")      # the main worktree under another spelling, on a merged branch
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
            j = trash / "j"
            r = subprocess.run(["cmd", "/c", "mklink", "/J", str(j), str(main / "keep")], capture_output=True, text=True)
            if r.returncode != 0:
                self.skipTest("cannot create a junction here")
            from gatetrain.runner import LocalRunner
            cfg = parse({"hosts": [{"name": "h", "os": "windows", "tidy": {"main_checkout": str(main), "trash_dir": str(trash)}}]})
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
        sim.add("/trash/" + stale, age_s=0)
        sim.add("/trash/" + stale + "/x", "file", 5, 0)
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
        with self.assertRaises(T.GuardViolation):
            gather(sim, {k: v for k, v in WIN_EXTRA.items() if not k.startswith("link_canary")}, win=True)

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


if __name__ == "__main__":
    unittest.main()
