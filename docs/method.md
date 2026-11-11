# The method

## Roles

- **Orchestrator** — plans, decides, reviews diffs and reports, merges. Never delegates a read that decides shape.
- **Worker** — implements one lane from a written brief in its own worktree; stops and reports when the brief is wrong.
- **Reviewer** — reads a risky lane's diff independently (given the code, not the orchestrator's conclusion) and tries to break it.
- **Hosts** — machines with roles: `builder` (the long chain, merges), `preflight` (cheap checks, lane gates), `deploy` (staging).

## A lane

One branch, one worktree, one owner, a list of owned files that no concurrent lane touches. A lane is ready when:

1. its tests were seen to fail before the change (red first) and pass after, with the counts pasted;
2. its own gates pass on a preflight host;
3. risky lanes (money, permission, data leak, network egress) passed an independent review;
4. worker and reviewer citations were spot-checked.

## A batch

1. **Bases.** For every lane, find the commit it branched from and what it already contains. Lanes that contain other lanes: list only the
   outer one. Lanes on different bases: stack in passes, each pass on the previous one.
2. **Stack.** Apply each lane's own commits. Shared files (manifests, lists, tables) are rebuilt, not textually merged.
3. **Repair known artefacts** automatically and only those: byte-identical duplicate rows, a row added then edited (keep the last), test-list
   entries lost in a merged string. Anything else stays red for a person.
4. **Pre-flight** on a free host: every check under ~10 minutes. Only when green does the expensive chain start.
5. **Chain** on the builder host. Reds do not stop the chain (collect them all), except where an early stop saves hours on a red that is
   certain.
6. **Merge** only when the chain is green on exactly the tested commit; then deploy, then wait for health.
7. **Tidy** what the merge finished.

## The tick

A short prompt or script on a timer. Each tick: chain state, merge state, preflight queues, reviews due, new branches, disk and leftover
containers, recurring failures (fix the tool once and record a lesson), record one dated line per change, report 3-6 lines. A tick that
finds nothing says so in one line.

## Measurement

Every claim carries its number and the command that produced it. "Tests pass" is not a report; "4211 passed, 1 failed (spawn-parity)" is.
A check that passes while producing no output has not run.
