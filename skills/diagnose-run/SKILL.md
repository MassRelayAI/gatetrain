---
name: diagnose-run
description: Use when a chain, a batch, a lane gate or an agent hand-back went wrong, or when a run is slower or costlier than expected. Finds the cause with evidence and turns a repeat failure into a lesson.
---

# diagnose-run

1. **Collect the evidence first.** The run's own log and per-step logs, its exit codes, the commit it tested, the host, the time; for an
   agent, its brief and its hand-back. Quote lines, never paraphrase them.
2. **Classify the red.**
   - *Artefact of stacking* (a duplicated line, a dropped list entry, a replayed row, re-applied commits) -> fix on the batch; teach the
     stack to repair it if it is the second time.
   - *Flaky* (the same step green on the same commit elsewhere) -> rerun once; fix the cause in the next batch.
   - *Real defect* in a lane -> back to the lane, with a failing test.
   - *Environment* (disk full, ports taken, a leftover container, a missing setting) -> fix the host; add the check to the tick.
   - *Process* (a stop that missed a queued follow-up, a stale log read as green) -> fix the script.
3. **Slowness or cost.** From the ledger: the critical path, waiting time per step, idle hosts, cost per batch; name the one change that
   saves the most.
4. **Make it stick.** Anything seen twice becomes a lesson: the rule, plus a test or script that enforces it, written to `docs/lessons.md`.
5. **Report** the cause, the evidence lines, the fix, and the lesson - in a few lines.
