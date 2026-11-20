# ADR 003: a flake is re-run once, under a strict rule of its own, logged and never counted as a first-try green

Date: 2026-11-20. Status: proposed (D446, lane GT-1; amended after the adversarial review). Builds on [ADR 002](adr-002-lane-stamp-and-cause-gated-rerun.md).

## Principle

Retry eligibility is its **own strict rule** (`crates/gatetrain-core/src/retry.rs`). It never reads the `diagnose` rule table: those classes
(`flake-*`, `infra-*`) were written to advise a person, and a person can doubt them; a re-run cannot. **When in doubt, never retry.**

## Decision

`gatetrain chain --retry-flakes [--quarantine-ref <ref>:<path>]` re-runs a red step **once, on the same sha, that step only**, when one of
these holds, and nothing else:

1. **Quarantine.** Every failing test id in the step's output (vitest `FAIL <file> > <name>`, cargo `test <name> ... FAILED`, node
   `not ok N - <name>`; whitespace normalised) is the **exact** `test` of a live row for that step, and there is at least one id. Never a
   substring: row `a.test.ts > pays` does not cover `a.test.ts > pays twice`. One unquarantined failure makes the red mixed: no retry.
2. **Infra.** The output shows an infra signature (docker daemon unreachable, port already in use, DNS temporary failure) AND nothing from
   the deny set, **anywhere in the output, in or outside a block**: assertion wording (`AssertionError`, `ERR_ASSERTION`, `assertion`,
   `expected ... to`), a panic (`panicked at`), a failing-test line (`FAIL`, `FAILED`, `not ok`), a compile error (`error[E`,
   `could not compile`, `error TS`, `SyntaxError`) or any timeout wording. The signature and deny tables are `INFRA` and `DENY` in
   `retry.rs`, tested one by one. There is no tail window and no block heuristic: an output with no signature is unclassified, and
   unclassified is not retried.

Never retried, whatever the output says: a step killed by its `timeout_s` (rc 124; a timeout is never a flake), a step whose worker died
(125) or that could not start (127), an output over 4 MiB (it cannot be judged whole), a sharded step, a third retried step in one run
(`STEP_RETRY_REFUSED`; at most 2), and a second red (final). `flake-load` and the `slow` rule are not used, so no step history decides
anything. (`expected_from` in `chaincmd.rs` still gives unknown steps a median for START ORDER only; eligibility never reads it.)

Without `--retry-flakes` the run is byte for byte what it was.

## The quarantine file

CSV `test,step,finding,added,expires`; the **header row is required** (a file without it is a configuration error, exit 2). A row is honoured
only when it has five fields, a finding id without spaces, real calendar dates (2026-02-31 is refused), **`added <= today <= expires`**
and `expires - added <= 14` days. Dropped rows are named on stderr with their line number.

**Ownership.** The file is read ONLY from a trusted ref the caller passes, `--quarantine-ref origin/main:<path>`, with `git show`: never
from the working tree under test, so the change under test cannot quarantine its own failure. The flag requires `--retry-flakes`.

## Accounting (a green retry is never a first-try green)

- `STEP_RETRY <step> attempt=1 rc=<rc> class=<cls> via=<infra:CLASS or quarantine:FINDING> test=<ids joined by " ;; ">` at the retry; every
  cause is named. The step's own `STEP_EXIT` is the second attempt's; its `STEP_TIME` is the sum of both.
- `CHAIN_RETRIES <step>=green|red ...` before the summary, and **`RED_SUMMARY: none; retried: <step>(<class>)`** instead of a bare `none`.
  The parser keeps `red_summary` as the reds alone and reads the suffix as retries. Exit code stays 0 for a retried green.
- The stamp lists `retried: [{name, first_rc, final_rc, class, via, test}]`; the first attempt's output stays in `<step>.attempt1.full`.
- `ParsedLog::to_json` carries `retries` and `retry_summary` (only when present); `gatetrain parse <log>` prints them with a per-test count.
- **Ledger.** A retried green is recorded with `log_ref = <step>.full#retried` and `history_from` excludes such rows, so a summed-duration
  green never feeds a median. The schema is untouched (the ledger version belongs to the metrics lane); a column replaces the marker there.
- A retried green writes no step-cache entry.

## Limits

- **Isolation.** The second attempt reuses the same working directory, ports, caches and environment as the first. A step that leaves
  state behind (a bound port, a half-written cache, a container) can make the retry meaningless; there is no per-step `retry_prep` hook yet.
- Quarantine ids are matched as parsed from vitest, cargo and node output only; any other reporter yields no ids and so no quarantine retry.

## Consumers

**Do not enable `--retry-flakes` in a consumer until its merger reads `CHAIN_RETRIES` (and the `retried:` suffix of `RED_SUMMARY`) and its
ledger ingest records retries.** A reader that matches the bare line `RED_SUMMARY: none` will, by design, no longer see a retried green as a
clean one.
