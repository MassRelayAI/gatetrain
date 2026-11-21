# gatetrain metrics: the minutes-lost view

`gatetrain metrics` answers one question from the ledger: **where did the last N batches spend time that a faster chain would not have spent?** The next speed-up is the largest line of its cause table.

```
gatetrain metrics [--db gatetrain.db] [--last 10] [--json] [--csv-dir DIR]
```

It is read-only (the ledger is never migrated or written) and reads `runs`, `steps`, `step_shards` and `batch_events`. `--last 0` is every batch. A ledger older than this binary is refused with the same message as every other command; run `gatetrain ledger migrate` first.

## What it prints

- **Minutes lost**, in three kinds, each attributed to a cause:
  - `red`: the time inside chains that ended red (the whole chain is thrown away);
  - `restart`: the time inside chains that were stopped or errored;
  - `wait`: from the end of such a chain to the start of the next chain of the same batch (the fix-to-next-chain wait).
- **Causes**, largest first. The cause of a red chain is its first red step (by start time); of a stopped chain, the step still open when it stopped (`<step> (open at stop)`), else its recorded stop reason.
- **Batches**: chains, how many red, green and other, `restart events` (finished `T6a`, `retry`, `restack` and `abandon` rows of `batch_events`), minutes lost, and the batch's main cause.
- **Steps**, longest first: how many runs ran and how many were cached (`steps.cached` or a `cached_from`), how many went red, median and maximum duration of the runs that ran (a cached run's duration is not a duration), and how often the step was the **critical path** of a green chain: the step whose duration is closest to the chain's span, which is the longest step. Where `step_shards` rows exist: the average shard count, the slowest shard against the mean shard, and the skew (slowest over mean; 1.00 is perfectly even).

A step with no recorded duration is left out of the medians; nothing is invented.

## Sample

```
minutes lost: 90.0 min in 3 red/stopped chain(s) of 5 chain(s) over 2 batch(es): red 40.0, restart 10.0, wait 40.0

causes (minutes lost, largest first)
     40.0 min  unit                         1 chain(s): red 30.0, restart 0.0, wait 10.0
     35.0 min  e2e                          1 chain(s): red 10.0, restart 0.0, wait 25.0
     15.0 min  e2e (open at stop)           1 chain(s): red 0.0, restart 10.0, wait 5.0

batches
  b1                       chains 2 (red 1, green 1, other 0), restart events 1, lost 40.0 min, mostly unit
  b2                       chains 3 (red 1, green 1, other 1), restart events 0, lost 50.0 min, mostly e2e

steps (longest first)
  unit                     ran 3 cached 0 red 1 median 18m20s max 19m00s critical 2 | shards 3.0: slowest 5m00s vs mean 3m20s (skew 1.50)
  e2e                      ran 4 cached 0 red 2 median 9m00s max 10m00s critical 0
  lint                     ran 1 cached 1 red 0 median 2m00s max 2m00s critical 0
```

## Export

`--json` prints everything as one JSON object (`chains`, `red_chains`, `lost`, `batches`, `steps`, `causes`). `--csv-dir DIR` also writes `batches.csv`, `steps.csv` and `causes.csv` (a header row each; times in minutes or seconds as named in the column).
