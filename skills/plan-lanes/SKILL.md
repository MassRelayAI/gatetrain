---
name: plan-lanes
description: Split a piece of work into independent lanes with owned files, find hidden coupling, and decide what runs in parallel and where.
---

# plan-lanes

1. **Map before cutting.** Read the code the work touches (callers, shared functions, data tables) and write the map with `file:line`.
   A worker verifies the map; it never re-maps.
2. **Cut lanes.** One lane = one branch, one worktree, one owner, a list of **owned files**. Two lanes may run at once only when their owned
   files do not overlap.
3. **Name the shared hubs** (manifests, route tables, test lists, trackers): lanes may each add to them; the stack rebuilds them.
4. **Find hidden coupling:** a lane built on another lane's branch is not independent of it; a change to a shared function is a dependency
   even when the files differ; ports, container names, build folders and caches are per lane.
5. **Size the risk.** Money, permission, data-leak and network-egress lanes are marked for independent review.
6. **Schedule.** Longest lane first; spread lanes over hosts with the `lane-gates` role up to their slots; prefer free local machines in
   `zero-cost`.
7. **Write each brief** (see `review-lane`): goal, owned files, what to read first, red-first proof, how to verify, what not to touch.
