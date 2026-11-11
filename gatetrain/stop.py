"""Stop a chain safely: queued followers first (L1), then the processes, repeated until none match (L2). Linux only."""
import os
import re
import sys
import time

ROUNDS = 4


def _procs(runner, pattern, exclude):
    rc, o = runner.run("ps -eo pid=,args=")
    out = []
    for line in o.splitlines():
        p = line.strip().split(None, 1)
        if len(p) < 2 or not p[0].isdigit() or int(p[0]) in exclude or "ps -eo" in p[1]:
            continue
        if re.search(pattern, p[1]):
            out.append((p[0], p[1]))
    return out


def _at_jobs(runner, followers):
    rc, o = runner.run("atq")
    jobs = []
    for line in o.splitlines():
        t = line.split(None, 1)
        if not t or not t[0].isdigit():
            continue
        _, body = runner.run(f"at -c {t[0]}")
        if any(re.search(f, body) for f in followers):
            jobs.append(t[0])
    return jobs


def _timers(runner, followers):
    rc, o = runner.run("systemctl --user list-timers --all --no-legend")
    found = []
    for line in o.splitlines():
        for tok in line.split():
            if tok.endswith(".timer") and tok not in found:
                svc = tok[:-6] + ".service"
                _, body = runner.run(f"systemctl --user cat {tok} {svc}")
                if any(re.search(f, body) for f in followers):
                    found.append(tok)
    return found


def _scan(runner, pattern, followers, exclude):
    return (_procs(runner, pattern, exclude), _at_jobs(runner, followers) if followers else [],
            _timers(runner, followers) if followers else [])


def _show(out, procs, jobs, timers):
    for pid, args in procs:
        print(f"  process pid {pid}: {args}", file=out)
    for j in jobs:
        print(f"  queued follower: at job {j}", file=out)
    for t in timers:
        print(f"  queued follower: systemd timer {t}", file=out)


def stop(runner, pattern, followers=(), yes=False, out=None, sleep=time.sleep, exclude_pids=None):
    out = out or sys.stdout
    exclude = {os.getpid(), os.getppid()} if exclude_pids is None else set(exclude_pids)
    followers = list(followers)
    procs, jobs, timers = _scan(runner, pattern, followers, exclude)
    if not yes:
        if not (procs or jobs or timers):
            print("nothing matches", file=out)
        else:
            print("would stop (dry run; pass --yes to act):", file=out)
            _show(out, procs, jobs, timers)
        return 0
    for rnd in range(1, ROUNDS + 1):
        if rnd > 1:
            procs, jobs, timers = _scan(runner, pattern, followers, exclude)
        if not (procs or jobs or timers):
            print("nothing left", file=out)
            return 0
        print(f"round {rnd}:", file=out)
        _show(out, procs, jobs, timers)
        for j in jobs:
            runner.run(f"atrm {j}")
        for t in timers:
            runner.run(f"systemctl --user stop {t}")
        if procs:
            sig = "-TERM" if rnd <= 2 else "-9"
            runner.run(f"kill {sig} " + " ".join(p for p, _ in procs))
        sleep(2)
    procs, jobs, timers = _scan(runner, pattern, followers, exclude)
    if not (procs or jobs or timers):
        print("nothing left", file=out)
        return 0
    print("left after 4 rounds:", file=out)
    _show(out, procs, jobs, timers)
    return 1
