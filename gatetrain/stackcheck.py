"""gatetrain generated: list UNTRACKED files under the configured generated directories (L53).
A stack or finish step that commits only tracked changes (`git status --untracked-files=no`) leaves a NEW generated file behind,
and a later "disk equals build" test goes red. Run this after regenerating, before the chain."""
import subprocess
import sys


def untracked_generated(repo, dirs):
    out = []
    for d in dirs:
        r = subprocess.run(["git", "-c", "core.quotepath=off", "ls-files", "--others", "--exclude-standard", "--", d],
                           cwd=repo, capture_output=True, text=True, encoding="utf-8", errors="replace")
        if r.returncode != 0:
            raise RuntimeError(f"git ls-files failed in {repo}: {r.stderr.strip()}")
        out += [ln for ln in r.stdout.splitlines() if ln]
    return sorted(set(out))


def cmd_generated(a):
    from . import config as cfgm
    dirs = list(a.dir or [])
    if a.config:
        try:
            dirs += cfgm.load(a.config).stack.get("generated_dirs", [])
        except (cfgm.ConfigError, OSError) as e:
            print(f"config error: {e}", file=sys.stderr)
            return 2
    if not dirs:
        print("no generated directories: give --dir or set [stack] generated_dirs in the config", file=sys.stderr)
        return 2
    try:
        files = untracked_generated(a.repo, dirs)
    except RuntimeError as e:
        print(e, file=sys.stderr)
        return 2
    if not files:
        print(f"ok: no untracked files under {', '.join(dirs)}")
        return 0
    print(f"{len(files)} untracked generated file(s) (a later disk-equals-build test will go red; git add them by name):")
    for f in files:
        print("  " + f)
    return 1
