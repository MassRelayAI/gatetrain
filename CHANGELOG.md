# Changelog

The version is `workspace.package.version` in `Cargo.toml`; one entry per released version, newest first.

## 0.5.2

The archive's secret scan, widened (round 2 of the independent read of 0.5.0).

- `tidy --force`: secret FILE names are `*.env`, `.env*`, `*.env.*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.age`, `id_*`, `credentials*`, `secrets*`, `.npmrc`, `.netrc`, `token*` (so `stag.env` and `prod.env` are caught). Content is scanned with the full pattern list of the project's own commit hook (AWS, generic `api_key|secret|password|token` assignments, private-key blocks, JWTs, GitHub, the DOKU shapes, base64 private-key headers, the project's env keys with a non-placeholder value) plus OpenAI, Anthropic, Stripe, Google, Hugging Face, Slack and `scheme://user:pass@` shapes.
- The scan covers the whole archive: untracked files, modified tracked files (the staged diff; the index is put back on a hit) and the unpushed commits already on the branch (`git log -p` of what the push would send). Content is read from git's output, so an ssh host is scanned too, not only by name. A hit STOPs the run with nothing pushed and names `file:line (kind)`, never the value.
- Ignored build output (`node_modules`, `target`, `dist`, `build`, `out` ...) is still skipped, but the plan now prints what was skipped.
- `land`: `{files}` reaches a check after `--`, so a name that starts with a dash is not read as an option.

## 0.5.1

Fixes from the independent read of 0.5.0 (docs/force.md).

- `tidy --force`: the archive proof is the live remote (`fetch --prune` and `ls-remote`), not remote-tracking refs a deleted branch leaves behind; a detached head also gets a local `archive/detached-<sha12>` tag. Ignored files that are not build output are listed and stop the run unless `--include-ignored`. Untracked files that look like secrets (by name or content) are never committed or pushed in an archive. A `*.stackdump` is neither committed nor blocking. The real destructive path now has tests on a throwaway repo with real git and real junctions (`tests/force_real.rs`).
- `land`: both sides of a rename are classified (a migration moved into `docs/` is SAFE); a file or branch name with shell syntax is refused when a check uses `{files}` or `{branch}`; the scratch worktree is removed by tidy's link-safe removal, never by `git worktree remove` on a folder that may hold a junction.
- `main-watch`: only a run whose recorded `CHAIN_MODE` is exactly `full`, for the exact sha, counts as the full chain; `runs::ingest` records the mode (table `run_modes`, created on first use).

## 0.5.0

Forced, measured and landed work (docs/force.md). Everything can be measured, forced or cleaned; a merge that waits too long can be landed on main and post-fixed.

- `gatetrain measure [--host H] [--json]`: what holds disk per host (worktrees, cargo targets, docker images, containers, volumes and cache, VM disk images, the recycle bin), each number with its method; a VM disk is sized by bytes allocated on disk, not its logical size.
- `gatetrain tidy --force [--keep REGEX] [--apply]`: archive (uncommitted work committed by named paths, tip pushed as `archive/<worktree>`, STOP when that cannot be proven), then link-safe removal with the main-checkout guard, for every worktree but the main checkout and the kept ones; then finished cargo targets (rustc wrapper blanked) and exited throwaway containers. Dry run by default.
- `gatetrain land <branch> [--plan]` and `gatetrain verdict <sha> --by WHO --pass|--fail`: merge onto the remote main in a scratch worktree, cheap checks, push (never force), record. FAST or SAFE by the `[land]` path rules; an unknown path is SAFE; SAFE needs the full gates and a recorded independent-read verdict first.
- `gatetrain main-watch [--once] [--deploy-cmd CMD]`: a full chain for each main commit that has none (through a pushed non-main branch), steps ingested, each red a post-fix item with an owner, deploy only for a green sha.
- `gatetrain branches [--json] [--check]`: every branch and worktree on every host and the remote with size, ahead/behind, content-on-main, uncommitted, age, owner and state; `[limits]` (`max_worktrees`, `max_idle_days`, `max_worktree_gb`, `max_unlanded_hours`) are checked by `--check` and on every `main-watch` pass, each breach with its fix.

## 0.4.4

- `tidy`: a naming report. `[tidy.naming]` (`roots`, `exempt`) makes a dry run and `--apply` end with a block that flags worktrees not named
  `<repo>-wt-<lane>`, folders with a bare `wt-`, `kk-`, `tmp-` or `tmp_` prefix and main clones whose folder differs from their remote's name. Flag only:
  nothing is renamed and the exit code does not change.
- Tests on a real fixture for the live-lane and uncommitted-work keeps (`tests/tidy_hosts.rs`).

## 0.4.3

- The admission guard names the LINE of a `[guard]` key or value that does not parse (`[guard] line 7: unknown field ...`), so a launcher that refuses
  to start on a broken config can say where to fix it. `max_hold_s` is at most 86400 (a day): `gatetrain guard wait` sits outside any step timeout.

## 0.4.2

- The admission guard (docs/admission.md): `gatetrain chain --guard-config <hosts.toml>` (and `--min-free-disk PATH=GB`, `--probe URL`,
  `--probe-max-ms`, `--probe-fails`, `--max-psi-mem`, `--max-psi-io`, `--max-psi-cpu`, `--max-load`, `--max-hold`) HOLDS new steps while a disk
  is below its floor, a protected service is slow, 5xx or silent N probes in a row, or a set pressure/load threshold is passed; every reading is
  a `GUARD_MONITOR` line. A hold is never a red; after `max_hold_s` the run goes on at par 1. The `[guard]` table of the hosts file is re-read
  before every check (`GUARD_CONFIG changed: <diff>`). `gatetrain guard wait|monitor|status|env` for shell launchers; `[guard.caps]` holds
  opt-in limits a launcher applies. Without a `[guard]` table or a flag nothing changes.

## 0.4.1

- `gatetrain chain --tier <fast|slow>`: a complete run of a tier. It executes the closure of its steps (every step they need, from any tier, runs in the same run; those are listed as `ran_for_closure`), so each tier stamp stands alone. The stamp is `v2`, `partial`, reason `tier`, named `stamp-<sha>-<tiers>.json`, and names the tiers it covers; the pre-flight gate accepts a set of tier stamps that covers every tier on one commit and steps-file digest. A red run deletes the older green stamp of the same commit and tiers. Not with `--only`, `--skip`, `--hosts` or `--mode full` (docs/since.md).
- `--since` fixes: it reads every verified result of the commit and takes each step from the newest run in the LEDGER's order, so an older green (or a replayed, re-sealed one) never overrides a newer red; a step is never reused while a step that needs it runs.

## 0.4.0

- `gatetrain chain --since <sha>` (a stamp with reused steps is v2 and partial, accepted only under `accept_reused`; with `--since-max-age`, `--full`, `--toolchain-probe`, `--env-key`): incremental re-run that carries a previous green result only under strict rules, labelled in the log, ledger and stamp (docs/since.md). Every run now also writes `result-<sha>-<tiers>.json` in `--out`; stamps gain per-step `freshness` and `reused`/`fresh` lists.

## 0.3.0

- **Behaviour change:** a stamp written by a run with `--only` or `--skip` is now marked `"partial": true` and is no longer accepted
  as a lane stamp, a pre-flight stamp or evidence. Run the whole steps file to stamp a lane or a pre-flight.
- Multi-host chains (docs/multi-host.md): `gatetrain chain --hosts hosts.toml` spreads one run over the coordinator and every enabled
  `[[hosts]]` entry with `roles = ["worker"]`, over ssh, with no daemon, no open port and no shared filesystem. One log, one ledger
  run, one stamp.
  - Steps declare `requires = [tags]` (default: the coordinator's platform tag); hosts declare `tags`, `slots`, `cores`.
  - A host counts only on the coordinator's exact commit and gatetrain version, and the exact answers to `[multihost] fingerprint`;
    `[multihost.tag_fingerprint]` checks a tag's own tools (a tag whose tools differ is dropped, the host keeps the rest). Checked at
    hello and again in every result; a host whose tools change mid-run is removed.
  - The commit is pushed by the coordinator into a bare repository on the worker (`repo`, `push_url`, `sync = "push"`, the default) and
    checked out in a worktree (`workdir`); `sync = "fetch"` fetches instead. `prepare` (keyed by `prepare_key` files) runs before the
    host's first step and refuses the host when it fails. `command_prefix` wraps each step (for example `caffeinate -i`).
  - Longest-first packing by the ledger's medians, honouring slots; work stealing for an idle host.
  - A lost step (drop, timeout, no valid result) is re-run on another qualified host, never counted green; a host that loses
    `max_host_failures` steps is removed; a step lost `max_step_attempts` times is red (125), one no host qualifies for is red (126); a
    red step is never tried elsewhere.
  - `gatetrain agent hello | run | clean`: the worker side; `GATETRAIN_SSH` replaces the ssh program.
  - The stamp keeps every field its consumers read and adds each step's `host` and `fingerprint`, `hosts` and `multi_host`.
  - New `[[hosts]]` keys: `tags`, `slots`, `workdir`, `repo`, `push_url`, `sync`, `gatetrain_bin`, `prepare`, `prepare_key`,
    `command_prefix`; new top-level `[multihost]` table. `config dump` of a file without them is unchanged.
  - `chain --tags`: a local run refuses a step whose `requires` this host lacks (exit 2).
- Ledger version 9: `steps.host`, `steps.host_fingerprint`, `run_hosts`, `host_events` (append-only). Run `gatetrain ledger migrate`.
- Review fixes before release:
  - `agent run` never writes a stamp, and the coordinator removes `stamp-<sha>.json` from `--out` before a multi-host run: a red run
    can no longer leave a stamp that passes the lane-stamp or pre-flight rule.
  - A stamp of a local run with `--only` or `--skip` carries `"partial": true`; the lane-stamp and pre-flight rules refuse it. A
    multi-host run with them writes no stamp and leaves an existing full one alone.
  - Each worker slot has its own checkout (`<workdir>-slot<k>`). A step that leaves its checkout dirty is red (rc 120); a checkout not
    clean before a step makes the step lost (re-run elsewhere) and retires that checkout, without removing the host at once.
  - Results still in flight on a host removed for a commit or tool change are lost, never counted.
  - Only the step's own output file names are copied back; a fingerprint answer is stdout then stderr, captured from the process
    (`stdout:<command>` for stdout only), and an empty one refuses the host; a worker named like the coordinator (or twice) is refused; ssh gets `-- <target>` and a `reach` starting with `-` is refused.
  - The `advise` loop test no longer depends on today's date.

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
