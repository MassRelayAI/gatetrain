# `gatetrain batch precheck`: every cheap check before stacking, at once

A batch that is restarted again and again is usually stopped by one problem found late and one at a time: two lanes editing one file,
a lane's own gate red, the local base behind the remote, an untracked file where a lane adds one, stale built packages. `precheck` runs all
of these checks in parallel and prints **every** finding, one line each, then a summary.

```
gatetrain batch precheck --config gatetrain.toml --base main lane/a lane/b lane/c [--repo .] [--par N]
gatetrain batch precheck --self-test
```

Exit 0 only when nothing is red; exit 1 when anything is red; exit 2 for a config error. A warning does not change the exit code.
`--par` bounds the checks running at once (default: the host's cores). It is read-only: no ref, index, worktree or file of the repository
is changed (`git merge-tree --write-tree` adds only unreferenced objects; lane checkouts are temporary `git archive` extracts, removed
afterwards). The configured commands are the operator's.

## The checks

| check | red when |
|---|---|
| `pair-conflict` | `git merge-tree --write-tree --name-only a b` reports a conflict for any two lanes (the files are named) |
| `base-conflict` | the same for each lane against `--base` |
| `lane-command` | a `[precheck] lane_commands` entry exits non-zero in a temporary checkout of the lane (env `GATETRAIN_LANE`); the output tail is shown |
| `host-base` | the local base ref differs from the same branch on `[precheck] remote` (read with `git ls-remote`; default `origin`, `""` turns it off) |
| `untracked-collision` | a file a lane adds (`git diff --diff-filter=A base lane`) already sits untracked in the main checkout |
| `host-command` | a `[precheck] host_commands` entry (run once, in the main checkout) exits non-zero |
| `known-red` | a `[[precheck.known_reds]]` `detect` command exits non-zero; the `hint` is printed |

A command that exits 0 with no output is a **warning**: a check that prints nothing may not have run. An unresolvable lane or base ref
is a red `refs` finding and stops the other checks. `--self-test` builds a fixture repository with a known conflicting pair and fails
if the conflict check does not report it.

## Example config block

```toml
[precheck]
remote = "origin"
lane_commands = ["npm run check:docs", "npm run check:tracker"]
host_commands = ["node tools/check-packages-fresh.mjs"]
known_reds = [
  { name = "stale-packages", detect = "node tools/check-packages-fresh.mjs", hint = "run npm run build:packages" },
]
```

## Example output

```
RED  pair-conflict: lane/a x lane/b: tests/api.test.ts
RED  base-conflict: lane/b x main: tests/api.test.ts
RED  lane-command: lane/c: `npm run check:docs` exited 1: docs/x.md: missing front matter
RED  untracked-collision: lane/c adds docs/new.md, which sits untracked in the main checkout
WARN host-command: host: `true` exited 0 with no output (a check with no output may not have run)
precheck: 9 checks, 4 red, 1 warning -> RED
```
