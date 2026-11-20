# ADR 003: a classified flake is re-run once, logged, and never counted as a first-try green

Date: 2026-11-20. Status: proposed (D446 of the massrelay tooling-upgrade plan, lane GT-1). Builds on [ADR 002](adr-002-lane-stamp-and-cause-gated-rerun.md).

## Decision

`gatetrain chain --retry-flakes [--quarantine <file>]` re-runs a red step **once, on the same sha, that step only** when, and only when:

1. every failure block of the step's own output is classed `flake-*` or `infra-*` by `rules/red-classes.toml` (the same classifier as `diagnose`; first match wins; no match is `real`), or
2. the failure block names the test of a **live** row of the quarantine file for that step. Format `test,step,finding,added,expires` (CSV, header required). A row needs a finding id and an expiry 0 to 14 days after `added`; a lapsed, malformed or longer-lived row is not honoured and is named on stderr.

A mixed red (one block explained, another `real`) is not retried. An unclassified red is never re-run ("it passed the second time" is not a class). At most 2 steps per run are retried (`STEP_RETRY_REFUSED` for a third, which stays red). Sharded steps are not retried. A second red is final.

Without `--retry-flakes` the run is unchanged, byte for byte.

## Visibility (a green retry is never a first-try green)

- `STEP_RETRY <step> attempt=1 rc=<rc> class=<cls> via=<diagnose:<rule>|quarantine:<finding>> test=<text>` at the moment of the retry; the step's own `STEP_EXIT` is the second attempt's, its `STEP_TIME` the sum of both.
- `CHAIN_RETRIES <step>=green|red ...` before `RED_SUMMARY`.
- The stamp gains `retried: [{name, first_rc, final_rc, class, via, test}]` (only when something was retried).
- The first attempt's output stays in `<step>.attempt1.full`.
- `ParsedLog::retries_by_test()` gives the per-test count. A retried-green step writes no step-cache entry.

## Provisional choices

- The quarantine path is the caller's flag; the plan puts massrelay's file at `cockpit/scripts/merge/flake-quarantine.csv`.
- The `slow` rule (`flake-load`) needs a step history: `chain` feeds it the ledger medians (`--db`), so without a ledger it cannot match and such a red stays `real`.
- Not in this lane: ledger columns for attempts (GT-2 owns the ledger schema), the `diagnose` rule for the p1 `findBy` flake (decided on that log's evidence).
