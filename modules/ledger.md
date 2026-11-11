# ledger

Append-only rows for runs, steps, agent hand-backs and host samples (ledger/schema.sql).

Answers: critical path and waiting time per batch, flaky steps (L13), cost per batch and per accepted lane, agent first-try-green rate by model and task kind.

## Commands (Python 3 standard library only; `python -m gatetrain <command>` or `bin/gatetrain`)

| command | what it does |
|---|---|
| `ingest <log> [--db gatetrain.db] [--run-id ID] [--host H] [--stale-min 30] [--now ISO]` | parse a chain log ([format](../docs/log-format.md)) into `runs` and `steps`. Append-only: the same run id is refused; `--replace` is allowed only for a run still `running` or `stopped`. Run id defaults to the log file name. |
| `report [log] [--run ID]` | totals, wall and busy time, reds, the 10 slowest steps, cached steps, and a critical-path approximation (steps overlap, so: the busiest stretch and the steps that end last) |
| `trace <log> \| --run ID` | Chrome Trace Event JSON on stdout; open it in ui.perfetto.dev or chrome://tracing. Overlapping steps get different `tid` lanes (greedy packing); reds `terrible`, others `good` |
| `eta <log> [--now ISO]` | prediction for a running chain, see [tick](tick.md) |
| `next-tick [--log] [--min 5] [--max 60]` | when to look next, see [tick](tick.md) |
| `flaky` | steps red in one finished run and green in another on the same commit (view `flaky_steps`) |

Notes: a run is `green` only when `GATES_DONE` printed and no step is red. `steps.started_at` is NULL when the log never printed
`STEP_START` (unknown, not invented); `steps.duration_sec` holds the logged time. The `flaky_steps` view was corrected to ignore
unfinished steps and to count green and red runs explicitly.

Status: `ingest`, `report`, `trace`, `flaky` built; hand-backs and host samples are tables only. See docs/method.md and docs/lessons.md for the rules this module carries.
