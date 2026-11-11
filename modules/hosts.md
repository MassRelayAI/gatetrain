# hosts

The registry of machines and models: role, limits, disks, awake hours, cost; the active profile decides what may run where.

Reads config/hosts.yaml. A host with less than `min_free_gb` on a build disk takes no new run. In `zero-cost` no paid resource is started.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
