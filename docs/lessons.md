# Lessons

Each lesson: what happened, the rule, the test or script that enforces it.

| # | what happened | rule | enforced by |
|---|---|---|---|
| L1 | A stop killed the chain but a follow-up merger, queued with a 5-minute delay, started later and attached to the next run. | A stop cancels queued follow-ups too (`atq`/scheduler), not only running processes. | `gatetrain stop` |
| L2 | Killing the chain root left parallel branches that started new steps. | Repeat the kill over every chain process until none is left; reds after a stop are artefacts. | `gatetrain stop` |
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
| L16 | A background chain's wrapper ended with an `echo`, so it always exited 0 and a red run looked green. | Read each step's own exit code; a wrapper's exit status proves nothing. | `chain` |
| L17 | Two worktrees shared one build output folder; one lane's build overwrote the other's and produced errors that were not in either lane. | One build output folder per worktree and per chain. | `doctor` (shared target) |
| L18 | Build output folders grew to 100-180 GB each; a few lanes filled the build disk. | A disk budget per lane; prune a lane's outputs when it merges; check free space before every run. | `doctor` (disk), `tidy` |
| L19 | Builds on a spinning disk saturated it and every step slowed. | Build outputs on the fast disk; slow disks for archives only. | `hosts` (`fast: true`), `doctor` |
| L20 | A build cache server started from inside one job inherited that job's temporary folder and died when the job cleaned it. | Start shared servers with their own environment and no idle timeout; check they are alive before a run. | `doctor` (cache server) |
| L21 | After a reboot the OS reserved a different range of ports, and test databases bound to fixed ports never started. | Fixed test ports live outside the OS ephemeral and reserved ranges; check reservations first when a bind fails. | `doctor` (ports) |
| L22 | Two lanes picked the same fixed test port and their gates collided. | Every gate's default port is unique; a test enforces it. | a "ports unique" test |
| L23 | Jobs started detached inside a subsystem shell died when the last shell exited. | Run long jobs attached (or under a real supervisor), never as orphans of a short-lived shell. | `chain` |
| L24 | A remote watcher outlived its local timeout and left `tail` processes on the build host. | Bound the remote side with its own timeout; sweep leftovers by exact process id. | `watch`, `doctor` (stray watchers) |
| L25 | Too many agents polling the build host over ssh at once exhausted it; the host stopped accepting connections and was rebooted. | Cap concurrent remote pollers; prefer one watcher per log over many polls. | `hosts` (max pollers), `doctor` |
| L26 | Heavy work started on the build host while a chain ran and slowed the chain for everyone. | While a chain runs: logs and light reads only; heavy jobs wait. | `tick`, `doctor` (chain running) |
| L27 | A test log from an earlier run was read after a failed step skipped the suite, and the old green was committed. | Delete the log before the run; decide on this run's exit code. | `chain` |
| L28 | Gate scripts printed only a summary; the failure detail was gone when it was needed. | Keep the full output of every step next to its summary. | `chain` |
| L29 | The toolchain path in a shared environment file pointed at a version that had been removed; gates went red for no code reason. | Check that every tool the chain names exists at the version it names before starting. | `doctor` (toolchain) |
| L30 | A disk image of a Linux subsystem grew until the host drive was full. | Watch the host drive, not only the build folder inside the image; sparse images; reclaim after cleanup. | `doctor` (disk), `tidy` |
| L31 | A throwaway database container leaked from a killed gate and held a port. | Remove exited throwaway containers by exact name; never a bulk prune on a shared host. | `doctor` (leftover containers), `tidy` |
| L32 | A "lock holder" process exited early and the chain ran without the lock it thought it held. | The holder writes "holds the lock" and "released" lines; the runner waits for the first and the tick checks them. | `chain`, `doctor` (lock age) |
| L33 | A rebase over a merge commit replayed the wrong side and a batch silently lost a lane. | Stack lanes from their own commits on a proven base; check every lane's tip subject is in the batch. | `stack` |
| L34 | Tests passed because they ran on old code: the build had not picked up the change. | Print the tested commit at the top of every run; the merger compares it with the branch tip. | `chain`, `merge` |
| L35 | A worker reported a fix "typecheck clean"; the deploy build on the server still failed. | Re-run the checks fresh on the stacked batch before merging; the server build is the final gate. | `stack` (pre-flight) |
| L36 | A guard was "proven" by switching it off; nothing showed it would go red on a real violation. | Prove a guard by breaking what it watches, or by a test fixture that violates it, never by disabling it. | `review` |
| L37 | A worker's research cited files and lines that did not exist. | Spot-check citations after every map or report; a citation that does not resolve is a finding. | `review` |
| L38 | An audit of "what is built" looked only at the main branch and missed work sitting on unmerged branches. | Status audits sweep unmerged branches too. | `tick` (branches) |
| L39 | A code index on one machine held a small fraction of the files and answered "no callers" wrongly. | Check an index's coverage before trusting an absence. | `review` |
| L40 | A deploy change worked in the editor and broke on the real host. | Prove deploy changes with a real bring-up on the host, isolated from the live stack. | `merge` |
| L41 | A watcher filter matched the word "FAIL" inside passing test names and raised false alarms. | Filters match anchored markers only. | `watch` |
| L42 | A step was killed by a hard timeout while it was still making progress, then reported as a red. | Time-outs per step class from measured durations, not one global number; a killed step is "stopped", not "red". | `chain`, `ledger` |
