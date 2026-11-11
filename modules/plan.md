# plan

Detect which work can run in parallel, and schedule it across the hosts.

## Detect

- **Owned files.** Every lane declares the files it owns. Two lanes may run at once only when their owned sets are disjoint; a lane whose
  diff touches a file outside its set is flagged before it is stacked.
- **Shared hubs.** Files many lanes must touch (manifests, route tables, test lists, trackers, registries) are listed once. Lanes may each
  add to them; the stack rebuilds them instead of text-merging (see `stack`).
- **Hidden coupling.** Before dispatch, check each lane's base and what it already contains (a lane that carries another lane's commits is
  not independent of it), and the code graph for callers across the planned file sets (a change to a shared function is a dependency even
  when the files differ).
- **Resource conflicts.** Database ports, container names, build output folders, caches and locks are per lane, so parallel gates never share
  them.

## Schedule

- **Lanes** run in parallel on whatever host has a free slot and the role (`lane-gates`), up to each host's `max_parallel_steps`.
- **Steps** of a chain run in parallel by class (compile once, then the database gates and test suites side by side), with a per-step cap so
  slow end-to-end walks are not starved.
- **Batches** are one at a time per builder host; the next batch is stacked and pre-flighted on a free host while the current chain runs.
- **Critical path first.** The ledger knows how long each step and lane took last time; the longest chain of dependent work starts first.
- **Cost-aware.** In `budget` or `speed` profiles a queued run may start on a paid host when the measured wait passes the limit.

## Measure

Per batch: how much ran in parallel, how long each lane and step waited for a slot, a lock or a port, and which host was idle while work
queued. A parallel plan that keeps colliding (same files, same ports) is the signal to split the hub or the lane, not to retry.
