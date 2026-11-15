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
