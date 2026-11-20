# ADR 003: a flake is re-run once, only through the quarantine, logged and never counted as a first-try green

Date: 2026-11-20. Status: proposed (D446, lane GT-1; amended after two adversarial review rounds). Builds on [ADR 002](adr-002-lane-stamp-and-cause-gated-rerun.md).

## Why the only path is the quarantine

The first version retried on the advisory `diagnose` classes (`flake-*`, `infra-*`). Review round 1 showed those rules were written to advise a
person, not to decide a re-run, and replaced them with a strict rule of their own: an infra signature with a deny list of "real failure"
wording. **Review round 2 broke that too**: eight probes with wording the list did not name went green (playwright `✘`, pytest `ERROR` and
`E   ` lines, Go `--- FAIL`, `TypeError`, `npm ERR!`, a segfault at rc 139, an OOM kill at rc 137, a cargo SIGABRT, ANSI-coloured `FAIL`), and
a long log with the real error above the tail still retried. Two rounds failing on the same seam, the deny list, means the shape is wrong:
a rule that must recognise every way a run can go wrong loses to the next runner.

**Decision:** the infra auto-retry is removed entirely (no infra signatures, no deny-gated eligibility). **No retry happens without a live
quarantine row.** The reasons: every real flake seen so far was a test-level timing or mock race, which an infra rule would never catch; and an
infrastructure outage is the consumer's driver's job (it waits for a fix or reruns), not gatetrain's guess.

## The rule, by construction

`gatetrain chain --retry-flakes --quarantine-ref <ref>:<path>` re-runs a red step **once, on the same sha, that step only** when ALL hold
(`crates/gatetrain-core/src/retry.rs`):

1. The step declares its test runner in the steps file, `runner = "vitest" | "node" | "cargo" | "playwright" | "pytest"` (anything else is a
   configuration error). An undeclared runner is never retried.
2. ANSI escape sequences are stripped first; then the runner's own result lines are parsed, per runner, for the failing test ids and for the
   runner's own failed count (vitest `Tests  N failed`; cargo `test result: FAILED. x passed; N failed`, summed over binaries; node `# fail N`;
   playwright `N failed`; pytest `N failed` in the final line).
3. **The failed count equals the number of failing ids parsed.** A missing count, a different count, a failure with no id, an unhandled error
   (vitest "Unhandled Errors"), a suite or collection failure (an id that is not `file > test`), a pytest `ERROR`, a playwright interrupted or
   did-not-run test: no retry. The proof is the run's own arithmetic, not wording.
4. **Every parsed id is the exact `test` of a live quarantine row for this step.** Never a substring.
5. The exit code is the runner's documented test-failure code: 1 (cargo: 101). 0, every other code, anything 128 or above (a signal: segfault, OOM
   kill, abort) and the timeout codes 124, 125, 127 never retry.
6. As an extra belt (no longer the main gate): no compile error (`error[E`, `could not compile`, `error TS`, `SyntaxError`) and no gatetrain
   timeout note in the output.

Also never retried: a sharded step, an output over 4 MiB, a third retried step in one run (`STEP_RETRY_REFUSED`; at most 2), and a second red
(final). Without `--retry-flakes` the run is byte for byte what it was.

## The quarantine file

CSV `test,step,finding,added,expires`; the **header row is required** (a file without it is a configuration error, exit 2). A row is honoured
only when it has five fields, a finding id without spaces, real calendar dates (2026-02-31 is refused), **`added <= today <= expires`** and
`expires - added <= 14` days. Dropped rows are named on stderr with their line number.

**Ownership and hygiene.** The file is read ONLY from a trusted ref the caller passes, `--quarantine-ref <ref>:<path>`, with `git show`: never
from the working tree under test. The ref must name a commit **other than the checkout's own HEAD**, and the index form `:path` is refused,
each with a clear error; otherwise the change under test could quarantine its own failure. **The consumer must pass a remote-tracking ref pinned
to its trusted main** (for example `origin/main:<path>`). `--retry-flakes` and `--quarantine-ref` require each other.

## Accounting (a green retry is never a first-try green)

- `STEP_RETRY <step> attempt=1 rc=<rc> class=quarantine via=quarantine:<finding> test=<ids joined by " ;; ">` at the retry; every cause is named. The
  step's own `STEP_EXIT` is the second attempt's; its `STEP_TIME` is the sum of both.
- `CHAIN_RETRIES <step>=green|red ...` before the summary, and **`RED_SUMMARY: none; retried: <step>(quarantine)`** instead of a bare `none`. The
  parser keeps `red_summary` as the reds alone and reads the suffix as retries. The exit code stays 0 for a retried green.
- The stamp lists `retried: [{name, first_rc, final_rc, class, via, test}]`; the first attempt's output stays in `<step>.attempt1.full`.
- `ParsedLog::to_json` carries `retries` and `retry_summary` (only when present); `gatetrain parse <log>` prints them with a per-test count.
- **Ledger.** A retried green is recorded with `log_ref = <step>.full#retried` and `history_from` excludes such rows, so a summed-duration green
  never feeds a median. The schema is untouched (the ledger version belongs to the metrics lane); a column replaces the marker there.
- A retried green writes no step-cache entry.
- **Lane stamp.** A retried green does write a stamp, so it satisfies ADR 002's `require_stamp` (the stamp's `retried` list says so). That is
  deliberate: the stamp is a filter in front of the batch chain, and **the chain re-proves the sha** from scratch; a retried green cannot merge
  untested code.

## Limits

- **Isolation.** The second attempt reuses the same working directory, ports, caches and environment as the first. A step that leaves state
  behind can make the retry meaningless; there is no per-step `retry_prep` hook yet.
- Test ids are parsed from the five supported runners' default output only; any other reporter format yields no ids, hence no retry.

## Consumers

**Do not enable `--retry-flakes` in a consumer until its merger reads `CHAIN_RETRIES` (and the `retried:` suffix of `RED_SUMMARY`) and its
ledger ingest records retries.** A reader that matches the bare line `RED_SUMMARY: none` will, by design, no longer see a retried green as a
clean one.
