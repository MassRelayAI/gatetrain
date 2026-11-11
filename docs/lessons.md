# Lessons

Each lesson: what happened, the rule, the test or script that enforces it.

| # | what happened | rule | enforced by |
|---|---|---|---|
| L1 | A stop killed the chain but a follow-up merger, queued with a 5-minute delay, started later and attached to the next run. | A stop cancels queued follow-ups too (`atq`/scheduler), not only running processes. | `chain` stop script |
| L2 | Killing the chain root left parallel branches that started new steps. | Repeat the kill over every chain process until none is left; reds after a stop are artefacts. | `chain` stop script |
| L3 | A watcher grepped for "FAIL" and fired on passing test names that contained the word. | Watch anchored markers (`^RED_NOW`, `^GATES_DONE`), never free words. | `watch` filter defaults |
| L4 | A watcher on a remote log reported old lines after a restart. | Start tails at the end (`tail -n 0 -F`) and bound the remote side with a timeout. | `watch` |
| L5 | Combining lanes duplicated a row that one lane added and a later commit edited. | Repair "same id, several rows" by keeping the last; anything else stays red. | `stack` repair |
| L6 | Combining lanes lost test files from a merged one-line test list. | Every test file must be reachable from the full run; re-add lost ones automatically. | `stack` repair + a "tests listed" check |
| L7 | Combining lanes doubled a line two lanes edited differently (an import list, a config matcher). | The type check or a config check names it; merge by hand. Pre-flight catches it in minutes. | pre-flight |
| L8 | Lanes that already carried another merged lane's commits re-applied them and spliced code files. | Use the carried lane's tip as the base; stack in passes per base. | `stack` base detection |
| L9 | Three long runs were stopped by cheap reds that a 10-minute check would have found. | Pre-flight every stacked batch on a free host before the expensive chain. | `stack` + `hosts` |
| L10 | Database tests ran as a superuser and passed while the real application role was denied by row-level security. | Every database lane runs one step as the application role. | review checklist |
| L11 | A failed step skipped the suite and an old log was read as green. | Delete the log before the run; decide on the exit code, never on a log line. | `chain` |
| L12 | Removing a worktree that held a link to a shared dependency folder deleted files in the main checkout. | Unlink links first; check the main checkout after any tidy. | `tidy` |
| L13 | A test flaked once on a random id that looked like a card number. | A red that passes on the same commit is flaky: rerun once, then fix the cause in the next batch. | `ledger` flaky detection |
| L14 | A disk image grew until the drive was full and the build host went read-only. | Free-space check before every run; sparse images; prune build outputs after a merge. | `tick` + `tidy` |
| L15 | A background agent's summary cited code that did not exist; its direct hand-back was accurate. | Trust direct hand-backs, spot-check every citation. | `review` |
