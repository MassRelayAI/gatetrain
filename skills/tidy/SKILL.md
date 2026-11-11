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
