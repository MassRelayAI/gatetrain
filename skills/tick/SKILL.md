---
name: tick
description: The recurring check across every host, chain, lane, review and disk. Acts only where a written rule covers it; reports in 3-6 lines (one line when nothing moved).
---

# tick

Run this on a timer (every 20-40 minutes) or when asked "run the tick". Read logs; do not start heavy work while a chain runs.

1. **Chains.** For the current batch: steps done, `RED_NOW` lines, `GATES_DONE`, the red summary. A red: read its step block. If the fix is
   certain and cheap to prove, stop the run early (see `preflight-and-chain`), fix on the batch, rerun; otherwise let the run finish so all
   reds arrive together.
2. **Merges.** Only the merger merges: zero red, full mode, tested commit equal to the branch tip, then deploy, then a health wait. If health
   is not confirmed, read the service status and logs and fix forward.
3. **Lane checks.** Queues on pre-flight hosts: alive? A status file with no counts means the check did not run.
4. **Reviews due.** Every money, permission, data-leak or network-egress lane gets an independent review before it is stacked
   (`review-lane`).
5. **Branches.** New branches from other people or agents since the last tick.
6. **Resources.** Free disk on every build drive against its minimum; leftover throwaway containers (remove by exact name); idle hosts while
   work queues.
7. **Recurring failures.** The same failure twice: fix the tool once, prove the fix red then green, write the lesson.
8. **Record** one dated line per change in the handover file.
9. **Report** in 3-6 lines: moved / next / waits on a person. Nothing moved: one line.

Never: force-push the main branch, skip hooks, stage everything blindly, delete without an archive, or run production steps without their
owner's word.
