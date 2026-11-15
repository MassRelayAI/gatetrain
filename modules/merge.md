# merge

Merge only on proof.

Zero red, full mode, the tested commit equal to the branch tip; then deploy; then wait for a health signal on the new build. Never a production push without its owner's word.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.

## Plugin points the engine reads (`gatetrain.toml`, lesson L69)

```toml
[chain]
cache_verify_command = ["tools/cache-verify"]   # exit 0 vouches for ONE cached step (GATETRAIN_STEP, GATETRAIN_SHA) on this host

[postmerge]
command = ["tools/install-and-build"]           # after the merge, before the first deploy; dir = "..." to run elsewhere

[deploy.staging]
command = ["tools/deploy", "{sha}"]
health  = ["tools/health", "--expect", "{sha}"]
health_timeout_min = 30                         # 0 or absent: wait for ever
```

A proof with a cached step and no verifier is refused (`proof.cached-unverified`). A protected branch that is checked out somewhere is
fast-forwarded together with its working tree; a dirty one refuses the merge (`merge.checkout-dirty`). Health is asked about the merged sha
until it answers; past `health_timeout_min` the batch is refused (`deploy.health-timeout`, logged once) and still goes live if the target
recovers later.

## The host seams and the generated-delta (lesson L72)

```toml
[chain]
target_dirs = ["/srv/build/target"]   # the disk budget is checked against these before a preflight or a chain
repo_root   = "/srv/repo"             # the chain must run in this repository top level (exit 78 otherwise)

[batch]
doctor = true                         # default: with --hosts, the host must pass `doctor` first (exit 79)
```

`gatetrain batch advance` refuses with the exit codes of `gatetrain hold`: 75 the lock is busy, 76 the chain does not fit the memory (`step_default_gb`
is required for a step with no history; unknown memory is not unlimited), 77 the disk would drop under `min_free_gb`, 78 the working directory is
not the repository top level, 79 `doctor` fails. After the chain, only generated files may differ from the settled tip; they are committed by name at
the merge after the generators run again to a fixpoint (`chain.tree-differs` otherwise).
