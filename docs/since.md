# Incremental re-run: `chain --since`

After a red, a fix commit usually touches a few files, yet the whole tier runs again. `gatetrain chain --since <sha>` starts from the
previous run and runs only what that fix can have changed.

```text
gatetrain chain --steps steps.toml --out out --since 9e7eba8 [--since-max-age 5] [--full] \
  [--toolchain-probe "rustc -V" ...] [--env-key CI,NODE_ENV]
```

Every `chain` run writes `out/result-<sha>-<tiers>.json` (green or red): per step its exit code, a hash of its definition, whether it ran
`fresh` or was `reused`, and where it really ran (`origin_run`, `origin_sha`, `age`). `--since` takes a commit (the file is looked up in
`--out`) or the path of a result file. No previous result is not an error: `CHAIN_SINCE none <why>` and every step runs.

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
`reused_from {run, sha, age}`, plus top-level `reused` and `fresh` lists, so a consumer that wants every step fresh refuses a stamp whose
`reused` list is not empty. Stamp rules are unchanged: a partial or dirty run writes none. Not available with `--hosts`.
