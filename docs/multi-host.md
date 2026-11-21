# Multi-host chains: one `gatetrain chain` run over several machines

A chain of many independent steps (for example 400 database-backed test suites of a few minutes each) is bound by the slots of one
machine. `gatetrain chain --hosts hosts.toml` spreads the steps of ONE run over every configured worker host and writes ONE result:
one log, one ledger run, one stamp. The rule that does not bend: **a step counts green only when it passed on a host that ran exactly
the coordinator's commit with exactly the coordinator's tool versions for that kind of step.** Every other outcome (a refused host, a
dropped connection, a timeout, an answer that cannot be read) is never green: the step runs again on another qualified host, or it is red.

Local-only stays the default. Without `--hosts`, or with a hosts file that has no enabled host whose `roles` include `"worker"`, `chain`
runs exactly as before.

## The pieces

| piece | where | what it does |
|---|---|---|
| the **coordinator** | the machine that runs `gatetrain chain --hosts` | reads the steps file and the ledger, checks every worker, places the steps, collects results, writes the log, the ledger run and the stamp |
| a **worker** | a `[[hosts]]` entry with `roles = [..., "worker"]` and `reach = "ssh <target>"` | a bare repository and a work dir (a worktree of it), plus the `gatetrain` binary; it runs the steps it is given |
| the **agent** | `gatetrain agent hello`, `agent run`, `agent clean`, started over ssh, attached | gets the commit into the work dir, runs `prepare`, answers the fingerprint commands; runs ONE step through the ordinary `chain` engine and reports it as one JSON line |
| the coordinator's own slots | `--local-slots N` (default: `--par`) | the coordinator is a host too, unless `--local-slots 0` |

There is no daemon, no open port and no shared filesystem. Everything goes over the ssh the host model already uses
(`reach = "ssh <target>"`, `BatchMode=yes`). Every agent runs attached to the ssh session that started it and ends with it.

## 1. Host capabilities: tags and slots

```toml
[multihost]
fingerprint = []                                   # commands every host must answer exactly as the coordinator does
local_tags = ["js"]                                # tags of the coordinator besides its platform tag
max_host_failures = 2                              # a host that loses this many steps is removed for the run
max_step_attempts = 3                              # a step lost this many times is red (rc 125)

[multihost.tag_fingerprint]                        # commands a host must also answer exactly for one of its tags to count
"x86_64-linux" = ["rustc -V", "cargo -V", "docker version --format '{{.Server.Version}}'"]
js = ["node -v"]

[[hosts]]
name = "linux-worker"
roles = ["worker"]
reach = "ssh builder@192.0.2.10"   # an IP when the worker cannot resolve names
tags = ["x86_64-linux", "js"]
slots = 8
cores = 32
repo = "/home/builder/gatetrain/repo.git"   # a bare repository the coordinator pushes into
workdir = "/home/builder/gatetrain/work"    # made as a worktree of `repo` on the first run
gatetrain_bin = "/home/builder/.cargo/bin/gatetrain"
prepare = "npm ci"                           # before the host's first step; a failure refuses the host
prepare_key = ["package-lock.json"]          # unchanged lockfile = not run again on this host
```

A step may say what it needs:

```toml
[[step]]
name = "web-unit-7"
tier = "slow"
requires = ["js"]
run = "npx vitest run --shard 7/40"
```

- A step runs only on a host whose tags include **all** of its `requires`.
- A step without `requires` requires the **coordinator's platform tag** (`<arch>-<os>`, for example `x86_64-linux`), so a step written
  for one platform never lands on another by accident. Give `requires = ["js"]` only to steps that really are platform-free.
- A remote host's tags are its declared `tags` plus the platform tag its agent reports. A host that declares a platform tag it does not
  report (says `x86_64-linux`, reports `aarch64-macos`) is refused.
- A step with `cores = N` only goes to a host with at least N `cores` (when the host declares `cores`).
- A step no accepted host qualifies for is red (rc 126, `no qualified host`), never skipped: a stamp never misses a step silently.
- Without `--hosts`, a step whose `requires` the local host lacks is a configuration error (exit 2); `--tags` adds tags to the local host.

## 2. Equivalence before anything counts

Before a host gets a step, the coordinator runs `gatetrain agent hello` on it. The agent:

1. refuses a work dir with changes (`git status --porcelain --untracked-files=all` not empty): it never overwrites work and never runs
   a dirty tree;
2. gets the commit (see Transport), then `git checkout --detach <sha>`;
3. runs `prepare` in the work dir, unless the same command already succeeded there for the same content of the `prepare_key` files
   (the key is the command plus the blob ids of those files at the commit; no `prepare_key` = run every time). A failing `prepare`
   refuses the host; its output stays in `<git dir>/gatetrain/prepare.log` on the host. The tree must still be clean after it;
4. does 1 to 3 for **every slot's checkout**: slot 0 is the work dir, slot k is `<workdir>-slot<k>`, a worktree of the same
   repository, so a step only ever sees (and can only dirty) its own tree;
5. reports `git rev-parse HEAD`, its platform tag, its gatetrain version and the answers to the fingerprint commands it was given: the
   common `[multihost] fingerprint` ones and those of every tag it declares. An answer is the command's stdout followed by its
   stderr, captured from the process (a compound command or a trailing comment loses neither), so **a warning on stderr is part of
   the fingerprint**: a host that prints a different warning is a different host. For a tool with noisy warnings write the command as
   `stdout:<command>` to answer with stdout only. A command that prints nothing refuses the host (an empty answer proves nothing).

The coordinator runs every fingerprint command once for itself, in its own checkout. Then, per host:

- HEAD must be the coordinator's commit **exactly**, the gatetrain version must be the coordinator's, and every common command must
  print exactly what it prints on the coordinator; otherwise the host is **refused** for the run (`HOST_REFUSED <host> <reason>`, the
  reason names the command and both answers);
- a tag whose `tag_fingerprint` commands answer differently is **dropped** for the run (`HOST_TAG_DROPPED <host> <tag> <reason>`); the
  host keeps its other tags. A laptop without `rustc` but with the same `node` keeps `js` and never gets a Rust step.

A worker named like the coordinator, or two workers with one name, is a configuration error (exit 2).

The host's **fingerprint** is the sha-256 over the gatetrain version and the (command, answer) pairs of the commands it was accepted
with. It is checked again for every step: `agent run` verifies HEAD, a clean tree and the answers before it runs, and the coordinator
compares the commit and the fingerprint in the result with what it accepted. A host whose tools change during the run is removed at
once, and the step it reported runs elsewhere.

A coordinator whose own tree is dirty, or whose steps file is not tracked, refuses to start a multi-host run (exit 2): every host must
run the same committed tree.

## 3. Transport

All over ssh, with `BatchMode=yes`, `ConnectTimeout=10`, `ServerAliveInterval=15` and `ServerAliveCountMax=4` (a silent host is lost
after about a minute):

- **The commit.** `sync = "push"` (the default): the coordinator runs `git push --force <push_url> <sha>:refs/gatetrain/incoming`,
  where `push_url` defaults to `<ssh target>:<repo>` (or `<ssh target>:<workdir>` without a `repo`). The worker needs no credentials to
  any git server, only the coordinator's ssh access. A missing work dir is created with `git worktree add --detach` from `repo`.
  `sync = "fetch"`: the work dir runs `git fetch` from its own remote instead.
- `ssh <target> '<gatetrain_bin> agent hello --workdir W --sha S --repo R --prepare ... --probe ...'` prints one `AGENT_HELLO {json}`.
- `ssh <target> '[command_prefix] <gatetrain_bin> agent run --workdir W --sha S --steps <path> --step NAME --run-id R --probe ...'`
  runs that one step through the ordinary engine (`chain --only NAME`, no cache, no retries) into `<git dir>/gatetrain/out/<R>`, never
  part of the tree. Its lines are kept in the coordinator's `<out>/<step>.<host>.agent.log` as progress; the verdict is only the final
  `AGENT_RESULT {json}` line: commit, fingerprint, the step's exit code, seconds, shards and output file names.
- `ssh <target> 'cat -- <file>'` copies each output file into the coordinator's `--out`, so `diagnose` works on the coordinator as for
  a local run. Only the step's own names are accepted (`<step>.full`, `<step>.check.full`, its `<step>.<i>of<n>.full` shard files,
  characters `[A-Za-z0-9._-]`): a worker can never write another step's file, a stamp, or anything outside `--out`.
- `agent run` writes no stamp; only the coordinator stamps. Before a full run the coordinator removes `stamp-<sha>.json` from
  `--out`, so an older stamp, or one of a run killed before its end, never stands for this run.
- The ssh command line is `ssh -o ... -- <target> '<command>'`, and a `reach` whose target starts with `-` is refused.
- At the end, `agent clean` removes the run's output folder on each worker (best effort).
- `command_prefix` wraps each `agent run` (for example `["caffeinate", "-i"]` keeps a macOS laptop awake while it runs a step).

The ssh program can be replaced with `GATETRAIN_SSH` (a JSON list, or words split on spaces); the tests stand a fixture in for ssh this way.

## 4. Scheduling

- **Weights.** Each step's weight is its median duration in the `--db` ledger (the same history `--order longest` uses); a step the
  ledger has not seen gets the median of the known ones. No history: every step weighs 1.
- **Longest-first packing.** Within a tier, the steps are sorted by weight, longest first, and each goes to the qualified host whose
  expected finish `(queued weight + weight) / slots` is smallest (ties: host order). That is the initial plan, one queue per host
  (`placed` events in the ledger).
- **Slots.** A host never runs more steps at once than its `slots` (default: `max_parallel_steps`, else 1; the coordinator:
  `--local-slots`).
- **Ready.** A slot takes the first step of its own queue whose `needs` are green. Tiers and `needs` keep their meaning: the slow tier
  starts when the fast tier is over and green (or with `--keep-going`); a step whose need is red or skipped is skipped.
- **Work stealing.** A host with a free slot and no ready step of its own takes the longest ready step it qualifies for from the queue
  of the host with the most queued weight (`STEP_STOLEN <step> from=<host> by=<host>`).
- A sharded step runs all its shards on one host.

## 5. Failure

| what happens | what the coordinator does |
|---|---|
| a worker does not answer `hello` (asleep, off the network, no route) | refused for the run; its share goes to the other hosts |
| ssh drops, times out, the agent dies, or the answer has no valid `AGENT_RESULT` | the step is **lost**, never green: `STEP_HOST_LOST <step> <host> <reason>`; it goes to the front of the queue and runs on another qualified host (never the one that lost it) |
| a host loses `max_host_failures` steps | the host is removed for the run (`HOST_REMOVED`); its queued steps go to the other hosts. A step it is still running counts only by its verified result |
| the result names another commit or fingerprint, or the agent refuses (wrong HEAD, tools changed) | the host is removed at once; the step is lost and runs elsewhere, and every result it still reports for steps that were running is lost too |
| a slot's checkout is not clean before a step | the step is lost (runs elsewhere), that checkout is retired for the run (`HOST_SLOT_RETIRED`, left as it is for a person); the host goes on with its other slots, and counts the loss against `max_host_failures` |
| a step leaves its checkout dirty (tracked changes or untracked files) | the step is **red** (rc 120), whatever its own exit code; it is not tried elsewhere, and that checkout is retired |
| every checkout of a host is retired | the host is removed |
| a step lost `max_step_attempts` times, or no qualified live host is left | the step is red: rc 125 (`lost`) or 126 (`no qualified host`) |
| a step is **red** on a host | it is red. It is not tried on another host (no host-shopping) |

Every decision is a `host_events` row of the run (`placed`, `stolen`, `lost`, `removed`, `refused`, `tag-dropped`, `unplaceable`) with
the host, the step and the reason, and a line in the log.

## 6. The stamp

A green multi-host run on a clean tree writes the same `stamp-<sha>.json` as a local run, with the same fields, so every stamp
consumer keeps working (the lane-stamp rule, the pre-flight rule, the evidence loader). Added:

- each step carries `"host"` (who ran it) and `"fingerprint"` (that host's fingerprint);
- `"hosts"`: every host of the run with its tags, slots, commit, fingerprint, and whether it was accepted (and why not);
- `"multi_host": true` and the `"run_id"`;
- A multi-host run with `--only` or `--skip` writes no stamp and leaves an existing one alone. (A local run with them writes a stamp
  marked `"partial": true`, which the lane-stamp and pre-flight rules refuse.)

Steps of a multi-host run carry no cache key (`"key": null`), so the evidence loader never reuses them as cached results.

## 7. The ledger (version 9)

- `steps.host`, `steps.host_fingerprint`: who ran each step (NULL in a local run: the run's host).
- `run_hosts(run_id, host, role, sha, fingerprint, tags, slots, accepted, reason)`: every host the run considered.
- `host_events(run_id, seq, at, host, step, event, reason)`: every placement, steal, loss, removal and refusal.
- Both tables are append-only (triggers abort UPDATE and DELETE), like every other table. `gatetrain ledger migrate` brings an older
  file forward.

## What a host needs to join

1. **sshd** reachable from the coordinator with key authentication (`BatchMode=yes`: no prompt), and a POSIX shell. Nothing else
   listens; no port is opened besides ssh. Use an IP in `reach` when names do not resolve between the machines. Hosts with many
   slots need room for that many sessions (`MaxSessions`, `MaxStartups` in the host's sshd).
2. **A bare repository** at `repo` (`git init --bare`), owned by the ssh user, for the coordinator to push into; the work dir is made
   from it. (Or, with `sync = "fetch"`, a clone at `workdir` whose remote has the commit.) Both are used only by gatetrain.
3. **The same gatetrain version** as the coordinator at `gatetrain_bin`, and the **same tools** for every tag it declares
   (`[multihost] fingerprint` and `tag_fingerprint`). Tools installed in the user's home must be on the PATH of a non-interactive
   ssh session, or named by absolute path in the commands.
4. Whatever its steps need (databases, containers, free ports), with each step independent of every other step running on the host;
   `prepare` installs dependencies (`npm ci`) once per lockfile.
5. **Awake while it works.** A laptop may sleep between runs (it is refused when it does not answer, and a step it drops is re-run
   elsewhere); `command_prefix = ["caffeinate", "-i"]` keeps macOS awake during a step.
6. **A Linux distribution under WSL** works as a worker when its own sshd runs as a systemd service inside WSL (WSL ends processes
   started detached from a `wsl.exe` that has exited; a session started by sshd is attached and lives as long as the step). Give its
   address, not the Windows host's name, in `reach`, and keep `slots` below what the person using that machine can spare.

## Limits (0.3.0)

- The coordinator's own slots share its one checkout: a step that dirties it can make a step running beside it red too. The run is red
  either way (no stamp is written over a dirty tree).
- Each worker slot is a checkout, so `prepare` runs once per checkout (and is then remembered by its key).
- Workers must be POSIX hosts (Linux, macOS, a Linux under WSL). A Windows worker is not supported; a Windows coordinator is.
- One run per worker at a time: two coordinators pushing to one worker would move its work dir under each other.
- `--cache` and `--retry-flakes` are refused with `--hosts`: a multi-host run is a proof-like run.
- Medians are measured per step, not per host: a slower host is not weighted down (work stealing evens it out).
- One ssh session per running step, plus one per output file copied back.
