# watch

## `gatetrain watch`: the lanes of a repository and what to do next

```
gatetrain watch --config watch.toml        # read-only; any TOML with a [watch] table (it may be hosts.toml)
```

```toml
[watch]
repo = "/srv/repo/main"                 # the repository; paths live in config, never in code
main_branch = "main"
lane_prefix = "line/"                   # lanes are the branches under this prefix
lanes_csv = "docs/lanes.csv"            # optional lane ledger, relative to repo: columns id, batch, branch, worktree, status
plans_index = "docs/plans/INDEX.csv"    # optional plan index: columns tracker_id, kind, status, plan_file, last_change
stale_days = 3                          # no commit for longer: stale
idle_hours = 24                         # no commit for longer: idle
ready_status = ["built", "ready"]       # ledger statuses that mean "finished, waiting for the stack"
blocked_status = ["blocked", "red"]
done_status = ["merged", "abandoned", "dropped"]
suggest_status = ["open"]               # plan statuses that make a plan a candidate for a new lane
merged_plan_status = ["merged"]         # plan statuses that mean the plan's lane is merged
max_suggestions = 8
git_timeout_s = 60
regenerable = []                        # globs of files the stacker rebuilds (`*` also matches "/"): a lane that conflicts only there is "stackable"
```

It prints a summary line, a numbered `next:` list (stack the ready lanes; unblock; rebase the lanes that conflict with main; check by
hand the lanes the ledger calls merged but git cannot prove; tidy the merged ones; start or drop dispatched lanes with no branch;
assign the open plans no lane carries, written plans first, newest first; review the stale ones), then the tables:

- **live lanes**: branch, worktree, last commit age, dirty, commits ahead (`git cherry`), ledger status, state and why;
- **merged, awaiting tidy**: merged as an ancestor, by patch (`git cherry`), or by content (every path the lane changed is the same on
  main), with the worktree still on disk;
- **ledger rows whose branch does not exist** (a `stacked` row is carried by its batch and is not listed).

A live lane is `stackable` when it conflicts with main only in files that match `regenerable` (the stacker takes them whole and rebuilds them); the next list says "ready for the stacker", not "rebase". With `regenerable` set, a lane that also conflicts in other files stays `blocked` and its why names up to 3 of them.

A live lane is `blocked` when it conflicts with main (three-way `git merge-tree <base> <main> <lane>`, conflict markers), when the
ledger says so, or when the ledger says it is ready while its worktree has uncommitted work; `ready` when the ledger says ready, the
worktree is clean and it merges cleanly; `unproven` when the ledger or the plan index calls it merged and git cannot show it.

It never writes to the repository, not even a timestamp (L77): `--no-optional-locks` (no index refresh) and no command that stores an
object. `merge-tree --write-tree` is not used: git "freshens" (re-dates) an object that already exists even when the new objects go to
a throwaway object directory, and an earlier merge proof (tidy's) leaves those objects behind. A test snapshots every file of the
repository and its worktrees around a run, with the merge objects already present.

## Log watchers

Watchers on local or remote logs that cannot go quietly wrong.

Anchored markers only (lessons L3, L4); the remote side bounded by a timeout; a watch that expires is re-armed; every terminal state (green, red, stopped, crashed) produces an event.

`gatetrain doctor` already reports stray watchers (`tail -F` older than `watcher_max_age_min`) by exact pid (L24); the watcher itself is not built yet.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
