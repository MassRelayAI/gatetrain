# tidy

Cleanup that cannot destroy work.

Archive tag (verified to point at the branch's sha) before a branch is deleted, by sha, never by name alone; links unlinked before a worktree is removed (L12); containers removed by exact name; never the main checkout, never anything with uncommitted work; check the main checkout afterwards.

`gatetrain doctor` lists leftover exited throwaway containers (configurable name prefix) by exact name with the `docker rm` command, and never removes them (L31). `gatetrain check` and `gatetrain tidy` (dry run, `--apply`, `--auto`) are built: see the README section "Tidy and check".

Hardened by lessons L60-L65 (no shell for local commands, one `safe_path`, merge proof by tree, links tested first, verified moves, `stopped` on timeout, an `intent` line before each action).

A lane that was squashed into main and then edited again is now "not merged" and is kept: `merged` means an ancestor, or merging the branch into main changes nothing (`git merge-tree --write-tree`). Review such a lane by hand and remove it yourself.

Status: built. See docs/method.md and docs/lessons.md for the rules this module carries.

## Lane names

`worktree_prefix` is a prefix of the worktree **folder name**, not a path. `/srv/wt/wt-lane-x` with `worktree_prefix = "wt-"` is the lane
`lane-x`, which is what `keep_lanes` and the folders under `target_roots` are matched against (case-insensitively). A prefix with a path
separator is a config error, and `check` reports a `keep_lanes` entry that matches no worktree and no target, because it protects nothing.
