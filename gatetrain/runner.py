"""Runners: run(cmd) -> (returncode, stdout). Tests inject a fake with canned outputs."""
import shlex
import subprocess
import tempfile


class LocalRunner:
    def run(self, cmd, timeout=60):
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout, errors="replace")
        return p.returncode, p.stdout

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
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, errors="replace")
        return p.returncode, p.stdout

    scratch_dir = "/tmp"

    def put_file(self, path, text):
        subprocess.run(["ssh", self.target, "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "cat > " + shlex.quote(path)],
                       input=text, text=True, capture_output=True, timeout=60, check=True)
