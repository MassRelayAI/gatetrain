# `gatetrain chain`: the step runner

`gatetrain chain` runs a **steps file**: a list of checks, each with its own exit code, in two tiers. The fast tier runs first, in
parallel. If a fast step is red the slow tier does not start. Every red is reported, never only the first, and each step's full output is
kept in its own file. A green run on a clean git tree writes a **stamp** that says which commit was proven by which steps.

<p align="center"><img src="assets/tiers.svg" alt="The fast tier runs in parallel, a red stops the slow tier, every red is reported" width="100%"></p>

It is built for the cheap run before the expensive one (the pre-flight of [the method](method.md)). The default mode is `preflight`; `--mode` can claim `lane` or `full` (not with `--only` or `--skip`).

## A minimal example

A git repository with this `steps.toml` (the `.gitignore` lists `out/` and `cache/`):

```toml
cwd = "."

[[step]]
name = "lint"
tier = "fast"
run = "echo lint ok"

[[step]]
name = "unit"
tier = "fast"
run = "echo 3 passed"
inputs = ["web/**"]

[[step]]
name = "e2e"
tier = "slow"
needs = ["unit"]
timeout_s = 600
run = "echo e2e ok"
```

Real output, first run (the host name is replaced by `builder-1`; the rest is as printed):

```text
$ gatetrain chain --steps steps.toml --out out --cache cache
branch master at 9e7eba84b7befc39b6bb0492a447707361a30752 on builder-1
CHAIN_MODE preflight
CHAIN_PARALLEL 4
STEP_START lint 2026-11-17T13:06:50.636601+00:00
STEP_START unit 2026-11-17T13:06:50.736785+00:00
STEP_TIME lint 0.1s
STEP_EXIT lint 0
STEP_TIME unit 0.1s
STEP_EXIT unit 0
STEP_START e2e 2026-11-17T13:06:50.792727+00:00
STEP_TIME e2e 0.1s
STEP_EXIT e2e 0
RED_SUMMARY: none
GATES_DONE
chain: green, stamp out\stamp-9e7eba84b7befc39b6bb0492a447707361a30752.json
```

Second run, nothing changed under `web/`: `unit` is a cache hit and runs nothing.

```text
$ gatetrain chain --steps steps.toml --out out --cache cache
...
STEP_START unit 2026-11-17T13:06:51.129989+00:00
STEP_CACHED unit
STEP_TIME unit 0.0s
STEP_EXIT unit 0
...
chain: green, stamp out\stamp-9e7eba84b7befc39b6bb0492a447707361a30752.json
```

The stamp then lists `"cached": ["unit"]`.

A red run. Here `typecheck` and `unit` fail, so `e2e` (slow) is skipped, and the exit code is 1:

```text
$ gatetrain chain --steps bad.toml --out out2
branch master at 9e7eba84b7befc39b6bb0492a447707361a30752 on builder-1
CHAIN_MODE preflight
CHAIN_PARALLEL 4
STEP_START lint 2026-11-17T13:06:58.966214+00:00
STEP_START typecheck 2026-11-17T13:06:58.966286+00:00
STEP_START unit 2026-11-17T13:06:58.966318+00:00
STEP_TIME lint 0.1s
STEP_EXIT lint 0
STEP_TIME unit 0.1s
STEP_EXIT unit 2
RED_NOW unit rc=2 at 2026-11-17T13:06:59.097623+00:00 (full output: unit.full)
STEP_TIME typecheck 0.1s
STEP_EXIT typecheck 1
RED_NOW typecheck rc=1 at 2026-11-17T13:06:59.100061+00:00 (full output: typecheck.full)
STEP_SKIPPED e2e a fast step is red; --keep-going runs slow anyway
RED_SUMMARY: typecheck(rc=1) unit(rc=2)
GATES_DONE
$ cat out2/typecheck.full
error: expected 1 argument
```

Stdout carries [log-format](log-format.md) lines and a last `chain:` summary line the parser ignores, so the output is a chain log that `parse`, `report` and `diagnose` read as it is
(redirect it to a file). A step's own output goes to `<out>/<step>.full`, streamed while it runs.

## The steps file

TOML. Unknown keys are refused (exit 2).

| key | where | meaning |
|---|---|---|
| `cwd` | top level, optional | the working directory of every step. A relative path is relative to the steps file, so the file means the same from wherever it is run. Without it, the caller's directory. |
| `[[step]]` | at least one | one check. |
| `name` | step, required | letters, digits, `.`, `_`, `-` only. Unique. It names the `.full` file. |
| `tier` | step, required | `"fast"` or `"slow"`. |
| `run` | step | a shell string. Give exactly one of `run` and `argv`. |
| `argv` | step | the program and its arguments, run without a shell. Not empty. |
| `needs` | step, optional | names of steps that must be green first. A step whose need is red or skipped is skipped. Unknown names and cycles are refused. A fast step may not need a slow one. |
| `timeout_s` | step, optional | a positive number of seconds. On expiry the whole process tree is killed (Linux and Windows) and the step is red with exit 124; the output so far is kept. |
| `inputs` | step, optional | git pathspecs, relative to `cwd`, that the step reads. With `--cache`, a step whose inputs are unchanged since it last passed is not run again. No `inputs`: never cached. |
| `shards` | step, optional | 1 to 64. Run the command that many times at once and report ONE logical step. `{shard}` and `{shards}` in `run` or `argv` are replaced, **1-based** (shard 1 of N is the first). `shards = 1` is an ordinary single run. `{shard}` without `shards`, or `shards > 1` with no placeholder (unless `shard_env`), is a config error. |
| `shard_check` | step, optional | an argv run once after every shard is green, with `GATETRAIN_SHARD_OUT` naming the shard output files (an OS path list). A non-zero exit makes the step red. Needs `shards`. |
| `shard_env` | step, optional | `true` when the command reads `GATETRAIN_SHARD` and `GATETRAIN_SHARDS` from its environment instead of using placeholders. Needs `shards`. |
| `cache_salt` | step, optional | a string mixed into the cache key. Change it to invalidate the step's entries (a tool version, an environment the inputs do not show). |

On Windows a `run` string goes to the Windows shell, so use `argv` (for example `["sh", "-c", "..."]`) for anything with shell syntax.

## Tiers, order and skips

- The fast tier runs first, up to `--par` steps at once (default 4). The slow tier then runs the same way.
- A tier that has started runs to its end, so every red of the tier is reported. `RED_NOW` is printed the moment a step fails.
- A red fast step skips the slow tier unless `--keep-going`. A skipped step prints `STEP_SKIPPED <step> <reason>`; it counts in no step total.
- `--only a,b` runs just those steps, `--skip c` leaves steps out. A name that is no step is refused. A step that needs a step outside the
  selection does not wait for it.

## Scheduling: longest first, and cores

**Longest first.** When several steps are ready (their `needs` are met and a slot is free), `chain` starts the one expected to take longest.
The expectation is the step's median over the finished runs in the ledger, so it needs `--db`; a step the ledger has not seen counts as
the median of the known ones. `--order longest|file` picks the rule: `longest` is the default when `--db` has history, `file` (the steps
file's order, exactly as before) otherwise. Ties keep file order. The run's log says which rule it used, once: `CHAIN_ORDER longest` or
`CHAIN_ORDER file`. Why: a long step listed last starts last and runs alone at the end.

**Cores.** A step may say `cores = N`. The run has a core budget, `--cores K` (default: this host's core count), and `chain` never starts
a step whose `cores` would exceed what is free; a step without `cores` counts as 1. A step whose `cores` exceeds the whole budget is a
config error (exit 2). A run that declares no `cores` anywhere has no budget gate at all, so existing steps files behave as before. On
Linux with `taskset` installed, a step with `cores` runs under `taskset -c <its reserved CPUs>` (the first contiguous free run), steps
without `cores` are confined to the CPUs no running pinned step holds, and the CPUs are released when the step ends. Elsewhere only the
budget applies. The log gets `CHAIN_CORES <K>` when any step declares `cores`.

```toml
[[step]]
name = "web-e2e"      # a CPU-sensitive suite: give it room, and keep the unpinned steps off its CPUs
tier = "slow"
cores = 4
run = "npm run e2e"
```

```text
$ gatetrain chain --steps steps.toml --out out --db ledger.db --cores 12 --par 12
CHAIN_ORDER longest
CHAIN_CORES 12
```

## Shards

A slow step that is one long suite can be split: `shards = 4` runs the command four times at once, each with its own `{shard}` of
`{shards}`. The shards queue for `--par` slots, one slot each, so with `--par 4` they run side by side. The log still shows one step:

```toml
[[step]]
name = "unit"
tier = "slow"
shards = 3
argv = ["sh", "-c", "echo running shard {shard} of {shards}"]
shard_check = ["sh", "-c", "test -n \"$GATETRAIN_SHARD_OUT\""]
```

Real output (the host name is replaced by `builder-1` and the folder by `/work/out`):

```text
$ gatetrain chain --steps shard.toml --out out --db gatetrain.db --run-id demo-2
branch master at e27a827e0f0734c186099e6295b460a3e206b4bc on builder-1
CHAIN_MODE preflight
CHAIN_PARALLEL 4
CHAIN_OUT /work/out
STEP_START lint 2026-11-17T14:11:28.165081+00:00
STEP_TIME lint 0.1s
STEP_EXIT lint 0
STEP_START unit 2026-11-17T14:11:28.218801+00:00
STEP_SHARD unit 1/3 START 2026-11-17T14:11:28.220419+00:00
STEP_SHARD unit 2/3 START 2026-11-17T14:11:28.220497+00:00
STEP_SHARD unit 3/3 START 2026-11-17T14:11:28.220537+00:00
STEP_SHARD unit 2/3 EXIT 0 0.1s
STEP_SHARD unit 3/3 EXIT 0 0.1s
STEP_SHARD unit 1/3 EXIT 0 0.1s
STEP_TIME unit 0.1s
STEP_EXIT unit 0
RED_SUMMARY: none
GATES_DONE
chain: ledger gatetrain.db: run demo-2
```

(The tree was dirty in that demo, so no stamp was written; the last lines say so.) Each shard's output is `<step>.<i>of<n>.full`, the
`shard_check`'s is `<step>.check.full`. `STEP_TIME` runs from the first shard's start to the last shard's end. The step's `STEP_EXIT` is 0
only when every shard and the check are 0, otherwise the exit code of the lowest-numbered red shard. `RED_NOW` is printed once per
logical step, at its first red shard. A reader that does not know `STEP_SHARD` skips the line.

## The ledger and `diagnose --out`

`--db FILE` writes the run into the ledger at the end: the run, its steps, shards, skips and stamp. `--run-id ID` names it (default
`chain-<UTC time>-<commit>`), `--mode preflight|lane|full` says what the run claims to be (default `preflight`; `full` with `--only` or
`--skip` is refused, exit 2) and `--host NAME` names the host as `hosts.toml` does (default `$GATETRAIN_HOST`, which `hold` sets, then the
environment). The ledger file is created, or migrated forward. **A ledger made before this version (v3) needs `gatetrain ledger migrate`
once**; the current version is 5.

The log carries a `CHAIN_OUT <folder>` line, so `gatetrain diagnose run.log` reads a red step's evidence from its `.full` files (or its red
shards' files) with no banner in the log. For a log copied to another machine, `gatetrain diagnose run.log --out <folder>` names the
folder. Real output, a sharded step with one red shard:

```text
$ gatetrain diagnose red.log
unit: real (rc=1, 1 block(s))
  [real] FAIL: shard 2 of 3: expected 3 to equal 4
    evidence: FAIL: shard 2 of 3: expected 3 to equal 4
    remedy: a real failure until shown otherwise: read the evidence, write a failing test, fix it on the lane
    full rerun needed: after the fix, the lane's defect comes first
1 red steps: 1 real step(s), 0 flake or infrastructure, 0 baseline
1 blocks: 1 real, 0 flake or infrastructure, 0 baseline
```

## The step cache (pre-flight only)

`--cache <dir>` skips a step whose inputs have not changed since it last passed.

- The key is a sha-256 over the step's command, its directory relative to the repository top, its `cache_salt` and `git ls-files -s -- <inputs>`.
  Relative to the top means two worktrees of one commit share an entry.
- **Dirty or untracked inputs** mean the step runs and writes no entry; its `.full` says why. The same holds when no tracked file matches `inputs`.
- **Only a green run writes an entry.** A red is never cached.
- A hit prints `STEP_START`, `STEP_CACHED <step>`, `STEP_TIME <step> 0.0s`, `STEP_EXIT <step> 0`, runs nothing, and counts as green for `needs`.
- `--no-cache-for a,b` forces those steps to run.
- The cache folder is per host and your choice; nothing is shared implicitly. **A proof run must not pass `--cache`**: the batch engine accepts a cached step in a proof only under its own evidence rule (same commit, verified key, stamped source run), otherwise it refuses it.

## The stamp

A green run on a git tree that is clean before and after writes `<out>/stamp-<sha>.json`: the commit, host, time, `mode` (`preflight`),
`par`, total seconds, the `digest` (sha-256 of the steps file), each step with tier, exit code and seconds, and `cached`, the list of steps
that came from the cache. The `<out>` folder itself is not counted as part of the tree. No stamp is written for a red run, a dirty tree or a
directory that is not a git repository; the last line says why. The stamp is what the batch engine's lane-stamp rule reads
(`[lanes] require_stamp`, see [ADR 002](design/adr-002-lane-stamp-and-cause-gated-rerun.md)).

## Flake retry (`--retry-flakes`)

Off by default. With `--retry-flakes` a red step is re-run once, on the same sha, that step only, when every failure block of its output is classed `flake-*` or `infra-*` by `rules/red-classes.toml` (the `diagnose` classifier), or names the test of a live row of `--quarantine <file>` (CSV `test,step,finding,added,expires`, header required, expiry at most 14 days after `added`; a lapsed or malformed row is not honoured). An unclassified red is never re-run; a second red is final; at most 2 steps per run are retried; sharded steps are not. The log gets `STEP_RETRY` and `CHAIN_RETRIES` ([log format](log-format.md)), the first attempt's output stays in `<step>.attempt1.full`, and the stamp lists `retried` so a green retry is never read as a first-try green. Design: [ADR 003](design/adr-003-flake-retry.md).

## Exit codes

| code | meaning |
|---|---|
| 0 | every selected step green (or cached) |
| 1 | at least one step red; `RED_SUMMARY` lists them |
| 2 | configuration error: bad steps file, unknown `--only`, `--skip` or `--no-cache-for` name |

Inside `.full` and `STEP_EXIT`, a timeout is 124 and a step that could not be started is 127.

## Options

`gatetrain chain --help` is the source of truth: `--steps` and `--out` (required), `--par`, `--keep-going`, `--only`, `--skip`, `--cache`,
`--no-cache-for`, `--mode`, `--host`, `--db`, `--run-id`, `--retry-flakes`, `--quarantine`.

## Read the result

```bash
gatetrain chain --steps steps.toml --out out > run.log       # the exit code is the verdict
gatetrain diagnose run.log                                    # causes, then every red step
gatetrain ingest run.log --db gatetrain.db                    # into the ledger
```

`diagnose` reads a step's output between a `=== <step>` banner and the next marker, and, for a `chain` log, from the `.full` files named by
`CHAIN_OUT` (or `--out`). See above.
