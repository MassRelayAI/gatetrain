# Incremental re-run: `chain --since`

After a red, a fix commit usually touches a few files, yet the whole tier runs again. `gatetrain chain --since <sha>` starts from the
previous run and runs only what that fix can have changed.

```text
gatetrain chain --steps steps.toml --out out --since 9e7eba8 [--since-max-age 5] [--full] \
  [--toolchain-probe "rustc -V" ...] [--env-key CI,NODE_ENV]
```

Every `chain` run writes `out/result-<sha>-<tiers>.json` (green or red): per step its exit code, a hash of its definition, whether it ran
`fresh` or was `reused`, and where it really ran (`origin_run`, `origin_sha`, `age`). `--since` takes a commit (the file is looked up in
`--out`) or the path of a result file. No previous result is not an error: `CHAIN_SINCE refused <why>` and every step runs.

## When a step runs again

Run level (any one re-runs everything):

- the previous run covered other tiers or had another `--mode`;
- the previous run was partial (`--only`/`--skip`) or on a dirty tree;
- the toolchain fingerprint (`--toolchain-probe`, platform, gatetrain version) or the environment fingerprint (`--env-key`, hashed) differs;
- a lockfile or toolchain pin changed between the two commits (`Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `uv.lock`,
  `poetry.lock`, `go.sum`, `rust-toolchain*`, `.node-version`, `.nvmrc`, `.tool-versions`, ...);
- git cannot compare the commits; `--full` was given.

Step level:

- it declares no `inputs` (always runs);
- it was red, lost, skipped or absent in the previous run;
- its definition (command, needs, inputs, salt, shards, runner, tier, directory) changed;
- a declared input differs between the two commits, or is dirty;
- its result was carried through more than `--since-max-age` runs (default 5);
- a step it `needs` runs.

Anything else is reused. Each decision is logged: `STEP_RERUN <step> <why>`, `STEP_REUSED <step> <run-id> <sha>`, `CHAIN_SINCE <sha> reused=N run=M`.

## Never silently green

A reused step prints `STEP_START`, `STEP_REUSED`, `STEP_TIME 0.0s`, `STEP_EXIT 0`; its `.full` says `reused-from <run-id> <sha>`; its ledger row has
`cached = 1` and `cached_from = "reused-from <run-id> <sha>"`. The stamp lists, per step, `freshness` (`fresh`, `reused` or `cached`) and
`reused_from {run, sha, age}`, plus top-level `reused` and `fresh` lists (see Consumers below for how a stamp with reused steps is marked partial).

## What `inputs` must be

Reuse is only as sound as `inputs`: it must be the step's whole read set (sources, configs, fixtures, scripts it calls). git cannot see a file
it ignores, or the target of a symlink, so these also re-run a step: inputs that match no tracked file; a tracked symlink in the inputs that
points outside them; ignored, untracked or modified files under the inputs now, or when the step last ran (a step whose inputs hold a
`node_modules` or `target` folder is therefore always re-run; list the sources instead). Listing too little is the declarer's error.

## Fingerprints

With no `--toolchain-probe` the toolchain fingerprint covers `rustc -Vv`, `cargo -V`, `node -v`, `git --version`, the platform and the gatetrain
version; with no `--env-key` the environment fingerprint covers `RUSTFLAGS`, `RUSTUP_TOOLCHAIN`, `CARGO_BUILD_TARGET`,
`CARGO_ENCODED_RUSTFLAGS`, `NODE_ENV`, `NODE_OPTIONS`, `CC`, `CXX`, `CI` (values hashed). Lockfile and toolchain-pin changes are caught by diff.

## The result file is checked

A result file is refused (`CHAIN_SINCE refused <why>`, every step runs) when it names another commit than `--since` resolved to, when its
`seal` (sha-256 over its content and the run's log digest) does not verify, or, with `--db`, when the ledger has no row for its run or the row's
commit, log digest or a step's exit code disagrees.

## Consumers

A stamp with reused steps is `"v": 2`, `"partial": true`, `"partial_reason": "reused"` with a non-empty `"reused"` list, so every consumer that
refuses partial stamps (lane stamp, pre-flight stamp, evidence) refuses it. `[preflight] accept_reused = true` and `[lanes] accept_reused = true`
accept exactly that stamp (never a `--only`/`--skip` one). A run with nothing reused writes the ordinary v1 stamp.
Not available with `--hosts`.

## Tiers: `chain --tier`

`gatetrain chain --tier fast` (repeatable or `--tier fast,slow`) runs every step of those tiers as a COMPLETE run of them: it is not partial, so `--since`
can reuse from an earlier run of the same commit (results are taken step by step, whatever the tier set). It cannot be combined with `--only`,
`--skip`, `--hosts` or `--mode full`. The steps it needs from another tier run in the same run (the closure; see below).
The stamp names the tiers it covers (`"tiers"`, and `"plan_tiers"` when the run covers fewer tiers than the steps file has) and, for a subset of the plan's
tiers, is `stamp-<sha>-<tiers>.json`, so a fast and a slow run on one commit never overwrite each other. The pre-flight gate accepts a single
full stamp, or the union of tier stamps on one commit and one steps-file digest that together cover every tier of the steps file (not for
`legacy_stamp_command`, which needs the single stamp).

### Closure, self-contained stamps, invalidation

A tier run executes the closure of its steps: every step a selected step needs, from any tier, runs in the same run (or is reused under the normal `--since` rules
for the same commit: same inputs, same definition, same fingerprints, so its outputs are equivalent). Only the selected tier's steps are that stamp's proof (`steps`);
the pulled-in steps are listed as `ran_for_closure`. Each tier stamp therefore stands alone: nothing on disk from another run is trusted. `--since <sha>` reads every
verified `result-<sha>-*.json` of that commit and takes each step from the freshest green one, so a slow run after a fast run on one commit reuses the fast steps it needs.
A tier stamp is `"v": 2, "partial": true, "partial_reason": "tier"`, so every consumer that does not do the union check (lane stamp, single pre-flight stamp, evidence)
refuses it. The pre-flight gate accepts a set of tier stamps on one commit and steps-file digest that together cover every tier. A tier stamp built with reused steps is
marked `reused` and needs `accept_reused`. A red run deletes the older green stamp of the same commit and tiers.
