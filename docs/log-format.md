# Log format gatetrain reads

A chain prints plain text lines; gatetrain looks only at lines that start with one of these markers. Everything else (test output) is ignored.
Steps may run in parallel and their lines may interleave: lines are matched to a step by its name, never by position.

| line | meaning |
|---|---|
| `branch <name> at <sha> on <host>` | once, first: what is being proven, the commit, the machine |
| `CHAIN_MODE <mode>` | once: for example `full` |
| `CHAIN_PARALLEL <n>` | optional, once: how many steps the run was allowed in parallel; `report`, `eta` and `diagnose` compare it with the host's configured parallelism (L56) |
| `STEP_START <step> <ISO 8601 time with zone>` | a step began |
| `=== <step>` | optional banner before the step's own output |
| `STEP_TIME <step> <N>s` | the step's wall time in seconds; end time = `STEP_START` + `STEP_TIME` |
| `STEP_EXIT <step> <rc>` | the step's exit code; `0` is green |
| `STEP_CACHED <step>` | optional: the result came from the step cache, nothing ran |
| `RED_NOW <step> rc=<n> at <ISO time> (<note>)` | printed the moment a step fails, so a watcher can react before the run ends |
| `RED_SUMMARY: <step>(rc=<n>) ... \| none` | once, at the end: every red, or `none` |
| `GATES_DONE` | once, last: the run finished (green or red) |

Rules gatetrain applies:

- **green** = `GATES_DONE` and `RED_SUMMARY: none` and no non-zero `STEP_EXIT`; **red** = `GATES_DONE` with at least one red;
  **running** = no `GATES_DONE` and the latest timestamp in the log is younger than `--stale-min` (default 30) at `--now`;
  **stopped** = no `GATES_DONE` and the log went quiet.
- A step with `STEP_TIME` but no `STEP_START` keeps an **unknown start**; it counts in totals and durations but is left out of the trace and the concurrency figures. Nothing is invented.
- A step with `STEP_START` and no `STEP_EXIT` is still running.
- Use a timestamp with a zone (`2026-11-11T09:00:48+07:00` or `Z`) so runs from different machines compare.
