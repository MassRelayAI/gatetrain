# The admission guard

A build chain that shares a host with something that matters (a production service, a database, a person's desktop) can starve it:
a dozen parallel steps, each starting more containers and compilers, push the disk to full or the memory into stalls, and the other
service slows down or stops answering. The admission guard sits in front of every NEW step and holds it while the host or a protected
service is in trouble. It never kills, throttles or fails anything: running steps keep running, and a hold is a buffer, not a red.

It is **monitor first**. Every reading is printed; only the thresholds you set hold.

## What it reads, and what holds

Before a new step starts (and every `every_s` while holding) the guard reads:

| reading | source | holds when (only if set) |
|---|---|---|
| free disk per path | `df -Pk <path>` (Windows: `GetDiskFreeSpaceExW`) | `[[guard.disk]] min_free_gb`: free GB below the floor |
| a protected service | `curl` GET, every `probe_every_s` | `[[guard.probe]]`: slower than `max_ms`, a 5xx, or no answer, `fails` probes in a row |
| memory pressure | `/proc/pressure/memory` `full avg10` | `max_psi_mem_full` |
| IO pressure | `/proc/pressure/io` `full avg10` | `max_psi_io_full` |
| CPU pressure | `/proc/pressure/cpu` `some avg10` | `max_psi_cpu_some` |
| load | `/proc/loadavg`, 1 minute | `max_load` |

A reading that cannot be taken (no `/proc/pressure`, no `df`, no `curl`) prints `?` and never holds. A 4xx answer is an answer (the
service is up); only 5xx, a timeout or a refused connection count as bad. One good probe resets the count.

High load and high CPU or IO pressure are normal on a build host: by default they are only monitored. Set a `max_*` to turn one into a hold.

After `max_hold_s` of continuous holding the run goes on at **one step at a time** (`GUARD_HOLD_TIMEOUT`) until the trouble clears, so a
stuck probe can slow a run down but never wedge it.

## The settings: `[guard]` in the hosts file

The guard reads the `[guard]` table of a hosts file, the same file `gatetrain tidy --config` reads (tidy ignores the table). The file is
re-read before every check: **an edit takes effect at the next step**, without a restart, and is printed as
`GUARD_CONFIG changed: <key>: <old> -> <new>; ...`. An edit that does not parse keeps the previous settings and prints `GUARD_CONFIG invalid
(...)` once. A file that does not parse at the start is a config error (exit 2) naming the line (`[guard] line 7: unknown field ...`): a typo never
passes silently (unknown keys are refused). `max_hold_s` is at most 86400.

```toml
[guard]
every_s = 5              # seconds between checks while holding
max_hold_s = 1800        # then go on at par 1
monitor_every_s = 300    # a GUARD_MONITOR line every 5 minutes
probe_every_s = 30       # one probe per service per 30 s, whatever the number of steps
max_psi_mem_full = 20    # hold while memory `full avg10` > 20 %
# max_psi_io_full = 60   # unset: monitor only
# max_psi_cpu_some = 90
# max_load = 80

[[guard.disk]]
path = "/"
min_free_gb = 10         # near full: hold below 10 GB free

[[guard.probe]]
url = "https://service.example/"
max_ms = 2000            # slower than 2 s is bad
fails = 3                # 3 bad probes in a row hold

[guard.caps]             # opt-in limits a LAUNCHER applies; gatetrain only prints them (`gatetrain guard env`)
scope = false            # systemd-run --user --scope with the limits below
# cpu_quota = "2400%"
# memory_max = "64G"
# memory_high = "56G"
# nice = 15
# ionice_idle = true
# container_cpus = "2"   # docker run --cpus for throwaway containers
# container_memory = "4g"
```

Every key is optional. With no `[guard]` table (and no flag) the guard is off and the run is what it was before.

## Flags (they override the file)

On `gatetrain chain` and every `gatetrain guard` command:

| flag | overrides |
|---|---|
| `--guard-config <hosts.toml>` | the file to read (re-read before every check) |
| `--min-free-disk PATH=GB` (repeatable) | the floor of that path, or adds one |
| `--probe URL` (repeatable), `--probe-max-ms N`, `--probe-fails N` | adds a probe; sets `max_ms` / `fails` of every probe |
| `--max-psi-mem`, `--max-psi-io`, `--max-psi-cpu`, `--max-load` | the four `max_*` |
| `--guard-every`, `--max-hold`, `--monitor-every`, `--probe-every` | the four intervals, in seconds |

A flag keeps winning after a reload. The guard covers a local run; with `--hosts` it is refused (run `gatetrain guard wait` on each host).

## For launchers that are not `gatetrain chain`

- `gatetrain guard wait [--label NAME] [--state FILE]`: returns at once on a quiet host and prints nothing; while held prints
  `NAME GUARD_HOLD ...` and waits; after `max_hold_s` returns. Exit 0 always (2 on a config error): put it before each step of a shell
  loop. `--state` carries the probe counts, the last probe time and the hold start from one wait to the next, so `fails` in a row and
  `probe_every_s` hold across processes. After `max_hold_s` the waits return at once until the trouble clears: a shell loop
  goes on at its own parallelism (only `gatetrain chain` drops to par 1).
- `gatetrain guard monitor [--once]`: a `GUARD_MONITOR` line every `monitor_every_s`, plus hold and release lines, until killed.
- `gatetrain guard status`: one check; exit 0 admit, 1 hold.
- `gatetrain guard env`: `[guard.caps]` as `GUARD_SCOPE=0|1`, `GUARD_CPU_QUOTA=`, `GUARD_MEMORY_MAX=`, `GUARD_MEMORY_HIGH=`, `GUARD_NICE=`,
  `GUARD_IONICE_IDLE=0|1`, `GUARD_CONTAINER_CPUS=`, `GUARD_CONTAINER_MEMORY=` (empty = unset). Values are limited to letters, digits, `.` and `%`.

## Log lines

| line | meaning |
|---|---|
| `GUARD_ON config=<file> hold: ...` | once, at the start: what holds |
| `GUARD_MONITOR at=<time> disk=[<path>=<GB>(<n>%used)] psi mem_full10=.. io_full10=.. cpu_some10=.. load1=.. probe=[<url>=<ms>ms/<status>]` | every `monitor_every_s`; `holding=<s>` while holding |
| `GUARD_HOLD <reason>; <reason>` | a hold began, or its set of reasons changed |
| `GUARD_RELEASE held=<s>s` | the hold ended |
| `GUARD_HOLD_TIMEOUT held=<s>s >= max_hold_s <n>; going on at par 1` | once per hold |
| `GUARD_CONFIG changed: <diff>` / `GUARD_CONFIG invalid (...)` | the file was edited |

Readers that do not know these lines skip them; a hold changes no step's result.
