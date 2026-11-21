# Changelog

The version is `workspace.package.version` in `Cargo.toml`; one entry per released version, newest first.

## 0.2.2

- The process scan skips unreadable own session daemons by exact name (`systemd`, `(sd-pam)`, `ssh-agent`, `gpg-agent`, `dbus-daemon`) so a real host is not always unknown; docker installed but unreachable is unknown. Test on the real `/proc`.

## 0.2.1

Review fixes of 0.2.0 (detached chain checkouts):
- The process scan is conservative: a process of this user whose working directory cannot be read makes the answer unknown (the checkout is kept); executables and open files inside the folder count; a running container whose mount source is the folder, inside it or a parent of it counts. Processes of other users are covered only by the idle check.
- Windows: a detached checkout is never removed (no handle check exists; command lines miss shells and relative paths).
- At apply time the idle rule is re-checked for merged checkouts too; the host lock, and a rebase, merge, cherry-pick or bisect in progress, keep a checkout.
- The archive tag is skipped only when the commit is an ancestor of main (a content-equal merge is archived).
- Docker sizes are read in decimal units; `after_bytes` is never negative.

## 0.2.0

- `gatetrain metrics`: the minutes-lost view over the last N batches (docs/metrics.md). Text, `--json`, `--csv-dir`.
- tidy: the build-cache prune reports what `docker builder prune` itself printed (`Total:` / `Total reclaimed space:`); a prune that ran is no longer reported as `0 B` when `docker system df` did not move.
- tidy: detached-HEAD chain checkouts are a `worktrees` class. `merged-detached-checkout` (commit on main) and, with `chain_checkout_max_age_days`, `aged-detached-checkout` (archived under a tag first). Never when dirty, locked, a link, or held by a running process.
- tidy: a `keep_lanes` entry that matches nothing is reported once per config change by `tidy` (state in `<log>.notified`); `check` always shows it.
- tidy: worktree removal is link-aware and pinned by tests: links out of a worktree are unlinked first and never followed; links inside go with it.
- New tidy config key `chain_checkout_max_age_days` (default 0: only merged detached checkouts go).

## 0.1.0

The Rust port of the gate train: chain, ledger, decision log, batch state machine, tidy, doctor, watch, tick.
