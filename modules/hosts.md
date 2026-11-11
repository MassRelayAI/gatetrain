# hosts

The registry of machines and models: role, limits, disks, awake hours, cost; the active profile decides what may run where.

Reads `hosts.toml` (standard library `tomllib`; `config/hosts.example.toml` is the template, the YAML file is documentation only). `gatetrain/config.py` validates it and names the failing field, e.g. `hosts[1].disks[0].min_free_gb`. A host with less than `min_free_gb` on a build disk takes no new run. In `zero-cost` no paid resource is started.

## A disk is a resource

A disk decides what a machine can run: a partial pre-check, a full chain, or just a cache. Each `[[hosts.disks]]` says what it may hold
(`use = ["preflight", "lane-gates", "chain", "cache", "archive"]`), whether it is `fast`, its `min_free_gb`, and whether it is `removable`
(an external SSD that may be absent). The top-level `[needs]` table says what a run kind needs from one disk (`preflight_gb`,
`lane_gates_gb`, `chain_gb`, `chain_needs_fast`). Example: a laptop gets an external SSD with `use = ["preflight"]`, `fast = true`,
`removable = true`, so it can take pre-flights while the SSD is attached and drops back (a WARN, not a failure) when it is not; a server
gets a big internal SSD with `use = ["chain", "cache"]`, `fast = true`, `min_free_gb = 100`, so it can take full chains. A slow disk
never takes a chain while `chain_needs_fast` is true (L19).

`gatetrain doctor` prints per host `can run now: ...`, computed only from disks that are present, allowed for that use, fast enough
and with the free space that run kind needs. A host with the `chain` role and no disk that can take a chain FAILS, unless a removable
disk that is not mounted would enable it (then WARN).

## Doctor: `gatetrain doctor [--config hosts.toml] [--host NAME] [--json]`

One line per check: PASS / WARN / FAIL, the lesson ids, the fix. Exit code 0 when nothing FAILs. Checks (run per host through a runner:
local subprocess or `ssh <reach> <cmd>`): disk free (L18, L30), build disk not fast (L19), capacity (above), leftover exited containers
by exact name, never removed (L31), stray `tail -F` (L24), toolchain (L29), fixed ports vs the ephemeral and reserved ranges and
duplicates (L21, L22), shared build output folder (L17), stale lock (L32), chain running (L26), cache server alive (L20), ssh pollers
(L25). A check not configured for a host is absent; a check that cannot run on the host OS says so as a WARN. Windows hosts check
disks, containers, toolchain, ports and chain; the ps-based checks (watchers, lock, pollers) are skipped there.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
