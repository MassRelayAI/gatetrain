"""The local runner's timeout ends the call even when the command's own child keeps the output pipe open.

On Windows, subprocess.run(shell=True, timeout=...) kills only cmd.exe and then waits on the pipes, which a grandchild (the
PowerShell that sizes a folder) still holds: the timeout never fires and a dry run waits for the whole recursive walk. This is
why `tidy` on a workstation with large worktrees printed nothing for 300 s.
"""
import subprocess
import sys
import time
import unittest

from gatetrain.runner import LocalRunner

# a grandchild of the shell that outlives the timeout and holds stdout: python started by the shell, then the shell's own exit
SLEEPER = f'"{sys.executable}" -c "import time; time.sleep(8)"'
if sys.platform != "win32":
    SLEEPER = f"{SLEEPER}; true"  # the trailing command keeps sh from exec-ing python in place


class RunnerTimeout(unittest.TestCase):
    def test_a_shell_command_whose_child_holds_the_pipe_times_out_on_time(self):
        t0 = time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            LocalRunner().run(SLEEPER, timeout=1)
        took = time.monotonic() - t0
        self.assertLess(took, 5, f"the 1 s timeout took {took:.1f} s: the runner waited for the grandchild")

    def test_an_argv_command_times_out_on_time(self):
        t0 = time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            LocalRunner().run_argv([sys.executable, "-c", "import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(8)']); time.sleep(8)"], timeout=1)
        self.assertLess(time.monotonic() - t0, 5)

    def test_output_and_returncode_unchanged(self):
        rc, out = LocalRunner().run(f'"{sys.executable}" -c "print(42)"', timeout=30)
        self.assertEqual((rc, out.strip()), (0, "42"))
        rc, out = LocalRunner().run_argv([sys.executable, "-c", "import sys; print('x'); sys.exit(3)"], timeout=30)
        self.assertEqual((rc, out.strip()), (3, "x"))


if __name__ == "__main__":
    unittest.main()
