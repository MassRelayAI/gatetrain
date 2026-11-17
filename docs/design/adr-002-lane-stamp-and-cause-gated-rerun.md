# ADR 002: a lane is stacked only with a green lane stamp, and a red proof is rerun only when its causes are addressed

Date: 2026-11-17. Status: accepted. Slice: batch governance, `engine-and-governance.md` sections 3.2 (T1, T4, T6a), 5.3 and 6.7a.

## Decision

1. **Lane stamp (T1).** `gatetrain.toml` may set `[lanes] require_stamp = true` and `stamp_dir`. With it on, stacking is refused (`stack.lane-stamp`) unless every lane tip `<sha>` has `<stamp_dir>/stamp-<sha>.json`, the file `gatetrain chain` writes only after a green run on a clean tree (JSON with `sha`, `steps`, `digest`). The file must parse and its `sha` must equal the lane tip. The refusal names each lane without one. Off by default.
2. **Cause-gated rerun (T6a).** A batch in `red` can be retried (`Engine::retry`). The retry is refused (`retry.causes-open`) while the tip still equals the tested sha of the red run, unless a person recorded the red of that run as a `flake` or `infra` with a reason (`Engine::note_cause`). Any new commit on the batch branch addresses the causes. A retry is also refused when the full-run budget is spent (`budget.full-runs`, `[chain] max_full_runs_per_batch`).

## Why

- A lane that never went green on its own tip makes the batch chain the first test of it. The chain then pays for a red that a lane-sized run would have found. The stamp already exists and is bound to a sha, so the check is a file lookup.
- A rerun on an unchanged tip with no stated cause is a lottery ticket: if it comes up green, the red is hidden (design 5.4). Requiring a new commit, or a written flake or infra reason, keeps "it passed the second time" from being a classification.

## Choices

- **No new store.** The cause note is a decision-log entry (`kind = override`, `rule = retry.cause-noted`, evidence `{run_id, class, tested_sha}`), so it is hash-chained and visible to `decisions`. It covers one run id. The red run's commit and id come from the batch facts (`tested_sha`, `run_id`) written by T4.
- **Budget.** The design names `full_chains_per_batch` (policy file, not loaded yet); the code had no budget. The toml key `max_full_runs_per_batch` is added; absent means unlimited, so existing configs are unchanged. It is checked at T4 and T6a. A launch counts unless it was later `stopped` (T5s): stopped runs are infrastructure and are charged elsewhere (design 3.4).
- **Where the stamp lives.** The engine's own preflight stamp (`<state_dir>/stamps/<sha>.stamp`) is a different file and is untouched. The lane stamp is read from `stamp_dir`, relative to the batch checkout when not absolute. `require_stamp = true` without `stamp_dir` is a configuration error (exit 2), not a refusal.
- **A moved tip means any other commit than the tested sha.** Descent is not checked: a restacked branch is a moved tip.
- **T6a is a full rerun.** The code had no `red` -> `proving` path at all (a red batch only refused `floor.zero-red`). T6a is added as the same launch as T4 with the cause and budget gates in front. The T4 guards still apply, so a moved tip needs a preflight stamp for the new tip. T4 and T6a share one run sequence (`<batch>-p<n>`) so a log is never reused (L11, L27). The classifier-driven red-only retry (5.2, 5.4 rule 7) stays a later slice, after `diagnose`.

## Consequences

- Existing configs behave as before: no `[lanes]` table and no `max_full_runs_per_batch` means no new refusal. The run id of a first launch is still `<batch>-p1`.
- `advance` on a `red` batch still refuses `floor.zero-red`; the retry is its own entry point. The CLI verbs for `retry` and the cause note are not wired in this slice (the CLI crate was outside its files).
- `diagnose` output is not consumed here. When it lands, its class (`infra`, `flake`) can write the same `retry.cause-noted` entry as the engine instead of a person.
