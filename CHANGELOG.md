# Changelog

The version is `workspace.package.version` in `Cargo.toml`; one entry per released version, newest first.

## 0.2.0

- `gatetrain metrics`: the minutes-lost view over the last N batches (docs/metrics.md). Text, `--json`, `--csv-dir`.
- tidy: the build-cache prune reports what `docker builder prune` itself printed (`Total:` / `Total reclaimed space:`); a prune that ran is no longer reported as `0 B` when `docker system df` did not move.
- tidy: detached-HEAD chain checkouts are a `worktrees` class. `merged-detached-checkout` (commit on main) and, with `chain_checkout_max_age_days`, `aged-detached-checkout` (archived under a tag first). Never when dirty, locked, a link, or held by a running process.
- tidy: a `keep_lanes` entry that matches nothing is reported once per config change by `tidy` (state in `<log>.notified`); `check` always shows it.
- tidy: worktree removal is link-aware and pinned by tests: links out of a worktree are unlinked first and never followed; links inside go with it.
- New tidy config key `chain_checkout_max_age_days` (default 0: only merged detached checkouts go).

## 0.1.0

The Rust port of the gate train: chain, ledger, decision log, batch state machine, tidy, doctor, watch, tick.
