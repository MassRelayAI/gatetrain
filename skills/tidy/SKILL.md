---
name: tidy
description: Clean up finished lanes, build outputs and containers without ever touching real work.
---

# tidy

Allowed after a merge:
- a lane's worktree and branch **once its work is on main** - tag `archive/<branch>` first;
- the lane's build output folder;
- throwaway containers that have exited, removed **by exact name**;
- a recycle bin older than its keep window.

Order for a worktree: check it has no uncommitted work -> unlink any linked folders inside it (dependency folders linked into the main
checkout) -> remove the worktree -> delete the branch -> check the main checkout is unchanged.

Never: the main checkout, docs (archive them instead), a live lane, anything with uncommitted work, a running container, a glob.


## Commands

- `gatetrain check` - read-only inventory and a "would tidy" plan; exit 1 when a disk, budget or profile threshold is crossed.
- `gatetrain tidy` - dry run; `--apply [--class targets,worktrees,containers,images,cache,trash]` executes; `--auto` is for a scheduler and runs only the `unattended` classes (default: targets of merged lanes, exited prefixed containers, recycle bin over N days).
- Every action is appended to a jsonl log; the first guard violation stops the run. Read the log, not the terminal, to see what happened.
- Never run `cargo clean -q`, never `docker system prune`; if a clean frees nothing, the tool writes `CACHEDIR.TAG` and retries. On Windows it unlinks links inside a folder before removing it and re-checks the main checkout.
- "Merged" means the branch content is on main (ancestor or every added line present), not the branch name.
