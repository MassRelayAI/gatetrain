# plan

Detect which work can run in parallel, and schedule it across the hosts.

## Built now: `gatetrain lanes`

`gatetrain lanes --repo PATH --main REF LANE_REF... [--json] [--shared-files PATTERNS] [--resource-regex NAME=REGEX ...]` is a read-only radar
over git (it shells out to `git` only; exit 0 unless git fails). It reports: bases and containment (a lane whose tip is inside another's is
listed once; lanes sharing unmerged commits are told to stack from that tip; stacking passes per base), collision hotspots per file
(SAME-LINES, DELETE-EDIT, CODE-BOTH, SHARED-HUB, ranked), doubled-line risk, resource clashes (default: a 5xxxx port added in a file whose
path contains `gate`), paired counts, and a parallel plan (lanes grouped so no group has a conflict edge; the summary line).

Beyond lane against lane, it also reports **drift** (`lane x main`: the lane's files that main changed since the lane's merge-base,
classified the same way), **carried files** (a commit range two lanes share takes part as `carried <sha>` against the other lanes and main,
so a collision inside the carried part is visible), and whether the carried commits are **already on main by patch** (`git cherry`; then use
the tip as base and do not re-apply). Locale catalogues (`locales/*.json`, `i18n/*.json`, `*.po`) are shared hubs by default; paired counts
only look in test files and never in `docs/`, `_archive/` or `*.md`; CODE-BOTH is printed once per file with all its lanes.

Heuristic, not proof: SAME-LINES compares old-side line ranges of two diffs taken from possibly different bases; hub patterns are globs;
doubled-line risk is "one-line hunks at the same line"; paired counts only pair a removed and an added line in one hunk and look for the old
number next to the same name elsewhere at the lane tip; the groups are a greedy colouring, not an optimal schedule. A quiet report is not a
guarantee of a clean stack (drift and carried hunks are compared in old-side coordinates from different bases, so they can miss or invent an overlap; the on-main check needs every carried commit to have a patch-equal twin on main); the pre-flight remains the check.

Still planned (not built): declared owned files per lane and the out-of-set warning, hidden coupling through a code graph, host scheduling,
critical path from the ledger, per-lane build folders and container names as resources, and the Measure section.

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
