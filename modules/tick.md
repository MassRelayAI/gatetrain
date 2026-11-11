# tick

The recurring check: one cheap pass over every host, chain, lane, review and disk, acting only on written rules.

Order: chains, merges, preflight queues, reviews due, new branches, tidy, recurring failures, record, report (3-6 lines; one line when nothing moved).

## Prediction: `gatetrain eta <log>`

History = successful, uncached steps of completed runs (`green` or `red`) already in the ledger, excluding the run being predicted.
For every step not yet finished (started but no exit, or known from history but not yet seen):

1. its own median duration; else the median of known steps with the same name prefix (e.g. `db-gate-`); else the overall median;
2. remaining wall time = `max(longest remaining step, sum of remaining / p)` where `p` is this run's observed average parallelism
   (busy time / elapsed time, at least 1; the maximum concurrency seen is printed too);
3. `eta` = last timestamp in the log + that wall time; the band scales it by the p25 and p75 of the per-step medians relative to their median.

With no completed run in the ledger it prints `no history - rough estimate` and uses only this run's own median step time and parallelism.
It also prints steps finished per minute and the five dominant remaining steps. It is a prediction, not a promise: steps that
wait on a lock, a slow disk or a red that retries are not modelled.

## Cadence: `gatetrain next-tick`

Prints `<minutes> min - <reason>`. Rules in order, result clamped to `--min`..`--max` (default 5..60):

1. a `RED_NOW` within the last 5 minutes: the minimum;
2. fewer than 10 minutes since the run started: 5 (catch early reds);
3. predicted time left (needs history) of 15 minutes or less: the minimum;
4. otherwise a third of the predicted time left (a third of the elapsed time when there is no history), capped at 30 once any red exists.

## Several runs: `gatetrain next-tick --log A --log B` and `gatetrain status`

`next-tick` accepts `--log` more than once, returns the earliest suggestion and names the run that decided it
(`<min> min - <reason> (decided by <run>)`); a finished run never decides while another is live. With one log the output is unchanged.

`gatetrain status --log A --log B [--db LEDGER] [--now T]` ingests each log (replacing a running or stopped run, never a finished one)
into an in-memory or the given ledger and prints one row per run: run, host, steps done / known, reds so far, state, eta, band, next look.
Then `next look: <min> min (<run>: <reason>)` for the earliest. The other finished runs in the set serve as history for the predictions.

Status: `eta`, `next-tick` and `status` built; the tick as a whole is still the written order above. See docs/method.md and docs/lessons.md for the rules this module carries.
