# stack

Combine finished lanes into one batch and prove it cheaply before the expensive run.

Base detection and passes per base (L8); shared files rebuilt; known artefacts repaired and nothing else (L5, L6); a pre-flight on a free host before the chain (L9).

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.

## Plugin points the engine reads (`gatetrain.toml`, lesson L69)

```toml
[stack]
command   = ["tools/stack"]    # optional: the repository's own stack script builds the batch branch (checked out for it);
                               # it gets GATETRAIN_BRANCH, GATETRAIN_LANES, GATETRAIN_LANE_TIPS, GATETRAIN_BASE
hubs      = [{ glob = "**/routes.json", rebuild = ["tools/merge-json", "{base}", "{ours}", "{theirs}", "{path}"] }]
resolvers = [{ glob = "tests/**", take = "lane" }]   # lane = theirs, stack = ours, manual = stop
```

Without `command` the engine cherry-picks each lane's own commits. A conflict is resolved only by a hub rebuild (it writes the result
to `{path}`) or a resolver; the result is guarded against conflict markers and staged by name. Anything else stops the stack with the
checkout clean. After either path the engine checks that the checkout is on the batch branch, clean, and that every lane's tip subject is in
the stack. Settle (regen to a fixpoint) is the last commit: `settle.tip-moved` refuses a preflight or a chain on a branch that moved since,
and `floor.clean-tree` refuses a chain on a dirty tree.
