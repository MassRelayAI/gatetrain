# Forced, measured and landed work

Everything can be measured, forced or cleaned; if a merge waits too long, land it on main and post-fix after. Five commands, one config.

| command | does |
|---|---|
| `gatetrain measure [--host H] [--json]` | per host, what holds disk, each number with the method that produced it. VM disk images (`ext4.vhdx`) are sized by bytes allocated on disk (`GetCompressedFileSizeW` on Windows, `du -sk` elsewhere), with the logical size beside it |
| `gatetrain branches [--host H] [--json] [--check] [--no-size]` | every branch and worktree on every enabled host and the remote: host, size, ahead/behind main, whether its content is already on main (`git merge-tree --write-tree main <b>` gives main's own tree), uncommitted files, last-commit age, owner, state `live` / `landed` / `archived` / `stale`. Read-only. `--check` exits 1 on a limit breach |
| `gatetrain tidy --force [--keep REGEX] [--apply]` | archive then remove every worktree but the main checkout and the kept ones; clean finished cargo targets (rustc wrapper blanked) and exited throwaway containers. Dry run by default |
| `gatetrain land <branch> [--plan]` | merge onto the remote main tip in a scratch worktree, run the cheap checks, push (fetch, re-merge, retry; never force), record in the decision log, remove the scratch worktree |
| `gatetrain verdict <sha> --by WHO --pass\|--fail` | record an independent read of a commit |
| `gatetrain main-watch [--once] [--deploy-cmd CMD]` | start a full chain for a main commit that has none (through a pushed non-main branch), ingest it, list reds as post-fix items with an owner, run the deploy command only for a green sha; also prints the `[limits]` breaches |

## tidy --force

Per worktree, in order, and any failed step is a STOP (that worktree stays intact, nothing after it is touched):

1. uncommitted work is committed by named paths (`git add -- <paths>`, never `-A`);
2. the tip is pushed as `archive/<worktree>` unless a remote branch already contains it, and read back with `ls-remote`;
3. the folder is removed by the same link-safe removal `tidy` uses (links are unlinked first and never followed; the main checkout's tracked-file count and status are checked after);
4. the branch is kept; the archive ref is the safety net.

The main checkout, `--keep`, `keep_paths`, `protect`, `keep_lanes`, locked worktrees and a missing folder always win.

## land: FAST and SAFE

The mode comes from the path classifier in the config, never from the caller. A file's mode is SAFE when any rule that matches it is SAFE, **or no rule matches it** (fail closed); a change is SAFE when any file is.

* FAST: the cheap checks (`checks`), then push; post-fix on main (`main-watch`).
* SAFE: the cheap checks, the full gates of the matched rules (`gates`, plus `safe_gates`), and a recorded `verdict` pass for the branch tip must exist BEFORE it lands. A missing item is named in the refusal (exit 4).

`land --plan` prints the mode and the matched rule per file and changes nothing. Exit codes: 0 landed or planned, 2 cannot read, 4 refused, 5 conflict, 6 a check is red, 7 the push never went through.

## Config (top-level tables of hosts.toml)

```toml
[land]
main = "main"
remote = "origin"
scratch_dir = "F:/repos/.land"          # default: next to the repository
checks = ["npm run regen", "npm test -- {files}"]   # {files} {branch} {sha}
safe_gates = []                          # for every SAFE change
retries = 3
[[land.rules]]
glob = "docs/**"
mode = "fast"
[[land.rules]]
glob = "**/migrations/**"
mode = "safe"
gates = ["npm run verify"]

[mainwatch]
chain_cmd = "cockpit/scripts/merge/chain-start.sh {branch} --no-stamp"   # {branch} {sha} {short}
branch_prefix = "chain/main-"            # never main
log_template = "F:/build/chain-logs/{short}.log"
[[mainwatch.owners]]
step = "lint*"
owner = "alice"

[limits]            # checked by `branches --check` and on every `main-watch` pass
max_worktrees = 10
max_idle_days = 2
max_worktree_gb = 20
max_unlanded_hours = 24

[measure]
vm_disks = ["C:/Users/me/AppData/Local/wsl/ext4.vhdx", "buildbox=/vm/disk.img"]   # "host=path" limits one host
```

Landings, verdicts, chain starts, post-fix items and deploys are rows of the hash-chained decision log (`kind` = `land`, `verdict`, `chain-start`, `post-fix`, `deploy`) in the ledger given by `--db`.
