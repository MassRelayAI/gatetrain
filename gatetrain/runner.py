"""Runners: run(cmd) -> (returncode, stdout). Tests inject a fake with canned outputs."""
import subprocess


class LocalRunner:
    def run(self, cmd, timeout=60):
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout, errors="replace")
        return p.returncode, p.stdout


class SshRunner:
    """Runs `ssh <reach> <cmd>`; `reach` may be written with or without the leading word ssh."""

    def __init__(self, reach):
        r = reach.strip()
        self.target = r[4:].strip() if r.startswith("ssh ") else r

    def run(self, cmd, timeout=60):
        argv = ["ssh", self.target, "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", cmd]
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout, errors="replace")
        return p.returncode, p.stdout
