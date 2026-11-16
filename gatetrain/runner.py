"""Runners: run(cmd) -> (returncode, stdout). Tests inject a fake with canned outputs."""
import os
import shlex
import signal
import subprocess
import tempfile


def _kill_tree(p):
    """Kill the process and every child it started (the shell's powershell, du, find): a timeout must end the whole walk."""
    try:
        if os.name == "nt":
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(p.pid)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)
        else:
            os.killpg(p.pid, signal.SIGKILL)
    except (OSError, subprocess.SubprocessError):
        pass
    try:
        p.kill()
    except OSError:
        pass


def _run(args, timeout, shell=False):
    """(returncode, stdout). On a timeout the process TREE is killed and TimeoutExpired raised at once: subprocess.run would kill only
    the shell and then wait on pipes a grandchild still holds, so a 20 s size probe could run for as long as the walk takes."""
    kw = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == "nt" else {"start_new_session": True}
    p = subprocess.Popen(args, shell=shell, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                         errors="replace", **kw)
    try:
        out, _ = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        _kill_tree(p)
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        raise subprocess.TimeoutExpired(args, timeout) from None
    return p.returncode, out


class LocalRunner:
    shell_free = True        # run_argv never involves a shell

    def run_argv(self, argv, timeout=60):
        """Run an argument list directly (no shell): every element reaches the program as one argument."""
        return _run(list(argv), timeout)

    def run(self, cmd, timeout=60):
        return _run(cmd, timeout, shell=True)

    scratch_dir = tempfile.gettempdir()

    def put_file(self, path, text):
        """Write a generated script as a file (no shell escaping of paths; L43)."""
        with open(path, "w", encoding="utf-8", newline="") as f:
            f.write(text)


class SshRunner:
    """Runs `ssh <reach> <cmd>`; `reach` may be written with or without the leading word ssh."""

    def __init__(self, reach):
        r = reach.strip()
        self.target = r[4:].strip() if r.startswith("ssh ") else r

    def run(self, cmd, timeout=60):
        argv = ["ssh", self.target, "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", cmd]
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, errors="replace")   # ssh is a direct child: no local shell holds the pipe
        return p.returncode, p.stdout

    def run_argv(self, argv, timeout=60):
        """The remote side is a POSIX shell: join with strict single-quote escaping."""
        return self.run(shlex.join(list(argv)), timeout)

    scratch_dir = "/tmp"

    def put_file(self, path, text):
        subprocess.run(["ssh", self.target, "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "cat > " + shlex.quote(path)],
                       input=text, text=True, capture_output=True, timeout=60, check=True)
