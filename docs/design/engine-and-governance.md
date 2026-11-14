# Engine and governance

Design, 2026-11-15. Status: proposed, nothing here is built. Language: a Rust core in this repository, one binary; the existing Python
commands keep working and are ported slice by slice with parity proven first (sections 9 and 10).

gatetrain today is a toolkit: skills, a log parser, a ledger of runs, `doctor`, `lanes`, `check` and `tidy`, `stop`. A person or an agent
still carries the batch in their head and glues the steps together with their own scripts. This document turns the toolkit into an
**engine** that owns a batch from "these lanes are finished" to "merged, deployed, tidied", and adds **governance**: a policy file that
says what may happen without a person, and a hash-chained log of every decision the engine took or refused.

An adopting repository keeps its own build, test and deploy commands. It replaces the glue between them with thin calls into the engine.

## 1. Principles

1. **The ledger is the only memory.** No state lives in a shell variable, a scheduled job, a scratch folder or a session. Anything the
   engine needs after a crash or a reboot is a row.
2. **A transition is a decision with evidence.** It names the rule that allowed it, the commit it applies to and the digest of the log it
   read. A refusal is recorded the same way.
3. **Exit codes and anchored markers decide, never a wrapper or a free word** (lessons L3, L11, L16, L27).
4. **Policy can tighten the engine, never loosen its floor.** A short list of rules is fixed in code (section 6.2).
5. **Automatic means reversible and understood.** The engine repairs and retries only what a written rule covers. Everything else waits
   for a person, with the evidence attached.
6. **No service.** The engine is a command that runs, advances what it can, and exits. A timer or an agent's tick calls it again.

## 2. Three files, three questions

| file | answers | lives | read by |
|---|---|---|---|
| `gatetrain.toml` | **what** the pipeline is: the plugin commands, generated directories, baseline files (section 7) | in the repository | the engine |
| `hosts.toml` | **where** it runs: machines, roles, slots, disks, ports, locks (section 4) | on the control machine, git-ignored (exists today) | `doctor`, the scheduler |
| `gatetrain.policy.toml` | **who may**: merge rules, reviews by risk tag, rerun budgets, actors, deploy targets (section 6) | in the repository, read from the protected branch only | the gate in front of every transition |

The ledger (`gatetrain.db`, SQLite) and its lock file live on one **control machine**: the machine where the tick runs. Hosts are reached
through the existing runners (local subprocess or `ssh`).

## 3. The batch state machine

### 3.1 States

A batch moves along one line. Scout runs, retries and reviews are rows attached to the batch, not extra states, so the line stays short.

| state | meaning | facts persisted on entry |
|---|---|---|
| `open` | the batch exists: lanes, base ref, risk tags | lane refs and tips, base sha, policy hash |
| `stacked` | one branch holds every lane's own commits on the base | stack attempt `n`, branch name, tip sha, lane -> commits map |
| `settled` | every generator ran to a fixpoint and its output is committed | tip sha, generator pass count, paths committed |
| `preflighted` | the cheap checks are green on the settled tip | preflight run id, log digest, stamp bound to the tip sha |
| `proving` | a full chain runs on a proof host | proof run id, host, lease id, log path, tested sha |
| `red` | the proof finished with at least one red, triage pending | run id, findings (step, class, rule, evidence lines) |
| `proven` | a full chain is green on the tip | proof record: run ids, tested sha, retried steps, cached steps |
| `merged` | the protected branch points at the proven commit | old and new sha of the protected branch |
| `deploying` | an automatic target received the commit, health pending | target, push time, expected build id |
| `live` | every automatic target reports the new build healthy | health evidence per target |
| `closed` | tidy ran, the summary is written | tidy log reference, totals |

Two side states: `held` (waits on a person; carries the rule that stopped it and what would release it) and `abandoned` (closed without a
merge; the branches get archive tags, nothing is deleted).

### 3.2 Transitions and guards

| # | from -> to | guards (all must hold) | effect |
|---|---|---|---|
| T1 | `open` -> `stacked` | every lane carries the reviews its risk tags need, bound to its current tip (6.4); the base is an ancestor of every lane; `lanes` reports no certain conflict the stack strategy has no resolver for | create `batch/<id>/s<n>` at the base, apply each lane's own commits through the stack strategy (7.1), check that every lane's tip subject is in the result (L33) |
| T2 | `stacked` -> `settled` | the tree is clean apart from generated paths | run the regen command until a pass changes nothing (at most `max_passes`), commit the changed and new files inside the generated directories by name |
| T3 | `settled` -> `preflighted` | a light host has a free slot and passes `doctor` for `preflight` | run the preflight command there; green writes a stamp for exactly this sha |
| T4 | `preflighted` -> `proving` | the stamp matches the tip; a proof host is free (no lease, no lock, no chain process), awake, and passes `doctor` for `chain`; the full-chain budget is not spent | take the lease, start the holder with the chain command (4.3), record the run |
| T5 | `proving` -> `proven` | the log ends with `GATES_DONE`, mode is `full`, zero red steps, the log's commit equals the recorded tested sha, cached steps are allowed and verified | write the proof record, release the lease |
| T5r | `proving` -> `red` | `GATES_DONE` with at least one red | run `diagnose`, store findings, release the lease |
| T5s | `proving` -> `preflighted` | no `GATES_DONE`, the holder is gone or the host rebooted | mark the run `stopped` (not red, L42), release the lease, charge the infra budget |
| T6a | `red` -> `proving` | every red is in a retryable class and inside its budget (5.3) | red-only rerun of exactly those steps on the same sha and host |
| T6b | `red` -> `stacked` | a patch was applied to the batch (automatic or by a person) | new tip sha; every stamp and proof for the old sha is void |
| T6c | `red` -> `held` | any red is not automatic, or a budget is spent | escalation bundle (5.5) |
| T7 | `proven` -> `merged` | section 6.3: tested sha equals the tip equals the commit being merged; the protected branch can fast-forward to it; approvals the policy asks for exist | compare-and-swap the protected ref from its recorded old value to the proven sha |
| T8 | `merged` -> `deploying` -> `live` | the target is automatic for this tier and actor (6.6); the previous target is healthy | run the target's deploy command, then its health check until it reports the merged sha or times out |
| T9 | `live` -> `closed` | none | `tidy --auto` classes only, archive tags, summary rows |

Any state may go to `held` (a guard refused and no automatic path remains) or `abandoned` (a person's decision). `held` returns to the
state it came from when its release condition is met: an approval, a new review, a patch, a freed budget.

A guard that fails is not an error. The engine appends a refusal to the decision log (6.8), leaves the batch where it is, and says so
once; the next tick re-evaluates. Repeated identical refusals are not re-logged: the refusal carries a `since`, and one line per tick
reports "still held by `<rule>`".

### 3.3 What is persisted

New tables beside `runs` and `steps`. All are insert-only; `BEFORE UPDATE` and `BEFORE DELETE` triggers abort.

| table | one row per | key columns |
|---|---|---|
| `batches` | batch | `id`, `base_ref`, `opened_at`, `opened_by` |
| `batch_lanes` | lane in a batch | `batch`, `lane_ref`, `tip_sha`, `author`, `risk_tags` (declared and derived) |
| `batch_events` | intent or completion of a transition | `seq`, `batch`, `transition`, `phase` (`intent` / `done` / `failed`), `attempt`, `input_sha`, `output_sha`, `run_id`, `host`, `at`, `decision_seq` |
| `leases` | host lease event | `host`, `role`, `run_id`, `event` (`taken` / `released` / `expired`), `boot_id`, `at` |
| `findings` | classified red | `run_id`, `step`, `class`, `rule_id`, `evidence` (line refs and digest), `action` |
| `reviews` | review verdict | `lane_ref`, `tip_sha`, `reviewer`, `verdict`, `blockers_open`, `evidence_ref` |
| `proofs` | proof record | `batch`, `tested_sha`, `run_ids`, `retried_steps`, `cached_steps`, `host` |
| `decisions` | decision-log entry | section 6.8 |

The current state of a batch is **derived**: the newest `done` event of that batch. There is no status column to fall out of step with
the history. `gatetrain batch status` folds the events; so does a restart.

One consistency fix comes with this: the Python `ingest --replace` deletes and re-inserts a running run today. Under the engine, a run in progress is
kept as snapshots (`run_snapshots`, newest wins in views) and the final row is inserted once, so the ledger's "append-only" holds without
exception.

### 3.4 Resume after a crash or reboot

Every transition is written in two rows around its effect: `intent` before, `done` after. On start, for each batch:

1. Fold `batch_events` to the last `done`: that is the state.
2. If a later `intent` has no `done`, the engine does not assume either outcome. It runs that transition's **probe**: a read-only check
   of the world that answers `happened`, `not happened` or `partly`.
3. `happened` -> append `done` with what the probe found. `not happened` -> run the effect again. `partly` -> run the transition's
   repair, which is always "finish forward under a new attempt number"; the partial result is kept, never deleted.

The probes read only things that survive a reboot: git refs and commits, files on disk (logs, lock files, stamps), and the host.

| transition | probe | why a second run is harmless |
|---|---|---|
| T1 stack | does `batch/<id>/s<n>` exist, and is every lane's patch in it (`git cherry`)? | the branch name carries the attempt; a half-built attempt is left and `s<n+1>` starts clean. No branch is ever force-moved or deleted |
| T2 settle | does a regen pass change nothing on the tip? | a fixpoint is idempotent by definition; no change, no commit |
| T3 preflight | is there a green stamp for this exact sha? | a stamp is keyed by sha; a second run adds a second stamp |
| T4 launch | does the host lock file name this run id (4.3)? | the run id is fixed before launch; a live holder for it is adopted, never doubled |
| T5 observe | pure read of the log and the lock | reading twice changes nothing |
| T6a retry | is there a retry run for (sha, step, attempt)? | keyed like T4 |
| T7 merge | is the protected ref already the proven sha? | compare-and-swap with the recorded old value; a second swap is a no-op, a moved ref is a refusal |
| T8 deploy | does the health check report the merged sha? | the deploy command must be idempotent (7.6); the health check is the truth |
| T9 tidy | `check` shows nothing left for this batch | `tidy` already re-plans from the disk every run |

A reboot during `proving` is the common case. The lock file records the host's boot id next to the holder's pid. A different boot id, or
a dead pid, with no `released` line and no `GATES_DONE`, is a **stopped** run: T5s. It costs the infra budget, not the red budget, and the
batch relaunches on the next tick with a new run id and a new log file. A log is never reused or appended across runs (L11, L27).

## 4. Host scheduling

### 4.1 Roles

| role | runs | may its green be a merge proof |
|---|---|---|
| `proof` | the full chain | yes |
| `scout` | a full or preview chain, early, to find reds before the proof host is free | only if policy lists it (`proof.accept_roles`) and the host declares the same platform and toolchain as the proof host |
| `light` | stack, regen, preflight, lane gates, lint, the tick itself | no |

A host may hold several roles. The existing role names stay valid as aliases (`chain` -> `proof`, `preflight` and `lane-gates` ->
`light`); `merge` and `deploy` remain as they are.

### 4.2 Capacity comes from host config

Nothing about a machine is written in code or in a pipeline script. `hosts.toml` gains, per host:

```toml
[[hosts]]
name = "builder-1"
roles = ["proof"]
max_parallel_steps = 12        # exported to the chain as GATETRAIN_PARALLEL
max_chains = 1                 # proof chains at once; the engine never exceeds it
reserve_cores = 8              # left free for whatever else the machine serves
detach = "at"                  # how a long job outlives the caller: at | systemd-run | setsid
lock_file = "/run/gatetrain/chain.lock"
platform = "linux-x86_64"      # compared between scout and proof
```

The chain command receives its parallelism from the engine (`GATETRAIN_PARALLEL`, `GATETRAIN_CORES`), computed from this file. A
parallelism number typed into a script is the defect this removes.

Before any run starts on a host, the engine calls `doctor` for that host and run kind and requires zero FAIL: disk free on a disk allowed
for that use and fast enough (L18, L19, L30), fixed ports outside the ephemeral and reserved ranges and unique (L21, L22), no leftover
throwaway container holding a port (L31), toolchain present (L29), shared cache server alive (L20), poller count under the cap (L25),
no stale lock (L32). A FAIL is a refusal that names the check; the batch waits and the escalation says which fix `doctor` printed. The
engine fixes nothing on a host itself except what `tidy --auto` is already allowed to.

### 4.3 One proof chain per host: the holder

The lock is a real lock held by the **parent** of the chain, not a sibling that polls the process table.

```
gatetrain hold --lock <file> --run <run-id> --log <holder-log> -- <chain command...>
```

runs on the host and does, in order: take an exclusive lock on `<file>`; write `run-id pid boot-id started-at` into it; append
`HOLDS <run-id>` to the holder log; start the chain command as its child; wait; append `RELEASED <run-id> rc=<n>`; release. The engine
launches it through the host's `detach` method and then waits for the `HOLDS` line before it records T4 `done` (L32). No line within the
timeout is a failed launch, not a running chain.

Why a parent: a holder that watches for the chain by pattern can match nothing and let go, or match a test of itself and never let go.
A parent cannot lose its child. "Is a chain running" becomes a read of the lock file plus `kill -0` plus the boot id, never a grep over
command lines.

The lease in the ledger is the engine's intent; the lock on the host is the fact. Each tick reconciles them: a lease without a lock is
expired (T5s); a lock without a lease belongs to someone else, and the engine refuses to start (`host.foreign-lock`) and reports the pid.
While a proof chain holds a host, the scheduler places nothing heavier than log reads there (L26); other gates that need the same
resources queue behind the same lock file.

The lock itself is portable (the standard library's file lock). The liveness and reboot probes are written for Linux first, so a proof
host is a Linux host (including a Linux subsystem, attached, L23) until the Windows and macOS probes have their own tests; until then
such a host can be `light`, and `scout` when its platform matches the proof host.

### 4.4 Scout, then proof

1. As soon as a batch is `settled`, and a scout host is free, the engine starts a scout run. It does not wait for the proof host.
2. Scout reds are classified like any other (section 5). Reds of class `platform-windows`, or any class the scout host's platform
   explains, are recorded and ignored for the scout only. The rest are fixed on the batch while the proof host is still busy with the
   previous batch.
3. A fix moves the tip. The scout is not restarted for every commit: the engine reruns only the reds (red-only command) on the new tip
   and keeps going.
4. The proof host runs one full chain on the final tip. A scout green is advisory unless policy accepts it as proof; when it does, the
   same rules apply (full mode, zero red, same sha, no unverified cached step), and the proof record names the host.

The next batch is stacked, settled, preflighted and scouted while the current one proves. Only T4 queues.

## 5. Red handling

### 5.1 The classifier

`gatetrain diagnose` is the engine's red-handling module. Its classes are a **rules table that is data**, not code: one file
(`rules/red-classes.toml`) of rows `id`, `class`, `step` pattern, `match` patterns over the step's own log block, `unless` patterns.
The Python classifier and the Rust core read the same file, so the table moves over unchanged and a new rule is a new row with a new
fixture, never a new release of logic. The engine calls it once per finished run and stores each result as a `findings` row:

```rust
pub enum RedClass { FlakeLoad, InfraPort, GeneratedUncommitted, BaselineShrink, BaselineMissing, PlatformWindows, Real }

pub struct Finding {
    pub run_id: String,
    pub step: String,
    pub class: RedClass,
    pub rule_id: String,           // the row of the rules table that matched
    pub evidence: Vec<LogLine>,    // quoted log lines with their line numbers
    pub log_sha256: String,
}
```

Two properties are required of the classifier, and tested:

- **`real` is the default.** A red is anything else only when a rule matched positive evidence in this run's log. No match, an ambiguous
  match, or two rules of different classes matching the same step: `real`.
- **The classifier never changes a verdict.** It labels a red; it cannot turn one green.

### 5.2 Classes into actions

| class | what it means | action | ceiling fixed in code |
|---|---|---|---|
| `flake-load` | a timeout or resource starvation with no failed assertion | retry the step, same sha, same host | automatic, within budget |
| `infra-port` | a bind failure, a leftover container, a service that never came up | let `doctor` name the fix; apply it only if `tidy --auto` covers it; retry | automatic, within the infra budget |
| `generated-uncommitted` | a generator's output differs from what is committed | run regen, commit by name inside the generated directories, re-prove | automatic patch; the new sha needs a new proof |
| `baseline-missing` | something new is absent from a baseline (a count, a list, a census) | write the additive patch as a proposal | proposal; applying it is automatic only for baselines the policy marks `additive_auto` |
| `baseline-shrink` | a baseline lost entries, or a count went down | escalate with the removed entries listed | never automatic |
| `platform-windows` | fails only because of the platform of a non-proof host | record, ignore on that scout | never ignored on a proof host |
| `real` | a defect | escalate to the lane's owner with the evidence | never automatic |

Policy may map a class to a **stricter** action (for example `flake-load = "escalate"`). It cannot map one to a looser action than the
ceiling; the loader rejects that file and names the line.

### 5.3 Budgets

From `[rerun]` in the policy file; none has a default higher than shown.

| budget | default | spent by |
|---|---|---|
| `per_step_per_sha` | 1 | a retry of one step on one sha |
| `per_batch` | 3 | every flake retry in the batch |
| `infra_per_batch` | 2 | stopped runs and infra retries |
| `full_chains_per_batch` | 4 | every full proof chain; when spent, the batch is `held` |
| `flake_window` | 2 flakes in 10 batches | a step over it is `real` until a person resets it |

### 5.4 A flake must never hide a real red

1. A retry needs a matched rule. "It passed the second time" is not a classification; the engine never retries a red it has not labelled.
2. A retry runs on the **same sha**. A retry on a different commit proves nothing about the first.
3. One retry per step per sha. Red twice is `real`, whatever the rule says.
4. A step whose own output shows a failed assertion or a failed test count above zero is `real`, even when a flake rule also matches.
5. A retried green does not erase the red. The first run stays in the ledger as red; the proof record lists the step under
   `retried_steps`; the report says "green, 1 step retried". Policy decides whether such a proof may merge a batch that carries a risk
   tag (`merge.allow_retried_steps`, per tag).
6. A step that flakes more often than `flake_window` allows stops being retryable. The engine never quarantines, skips or loosens a step
   on its own.
7. A retry runs only the red steps, so a proof may be composed of one full run plus red-only runs. All of them must name the same sha,
   host and toolchain; the mode of the first must be `full`; a red-only run can never be the only evidence.
8. Reds that appear after a stop are artefacts and are not classified at all (L2); the run is `stopped`.

### 5.5 Escalation

An escalation is a bundle, not a message: batch, sha, run id, each finding with its rule and quoted lines, the lane most likely
responsible (the lanes whose own commits touch the files the step names; all of them when unknown), what was already tried and which
budget is left, and the exact command that would re-run the red. It is stored as a row, handed to the notifiers (7.7), and it is what
`gatetrain batch status` prints under "waits on you". An agent may pick up an escalation and propose a patch; the patch enters through
T6b like a person's.

### 5.6 What may be automatic, and what may never be

| automatic when policy allows | never automatic |
|---|---|
| stack, settle, preflight, scout, prove | merging with any red, or a sha that was not proven |
| retry inside a budget | dropping, skipping or loosening a step, a test or a gate |
| committing generator output by name | lowering a baseline, an expected count or a ratchet |
| applying an additive baseline patch marked `additive_auto` | removing a lane from a batch |
| fast-forward merge of a proven sha | overriding a refusal |
| deploy to a non-production target, health wait | deploy to a production target; a rollback |
| `tidy --auto` classes | deleting a branch without an archive tag, or anything with uncommitted work |
| | changing the policy, or acting under an unapproved policy change |
| | any force-push to a protected branch, any history rewrite of one |

## 6. Governance

### 6.1 The policy file

TOML, like the host file; YAML would add a second format for no gain. The loader deserialises into typed structs, validates every
field and names the failing one (`review.by_tag.money.reviews`), as the host config does today. Unknown keys are errors
(`deny_unknown_fields`): a typo must not silently disable a rule.

```toml
schema = 1

[protected]
branches   = ["main"]
policy_ref = "main"                 # the policy that judges a batch is read from this ref, never from the batch

[merge]
method               = "fast-forward"
require_mode         = "full"
tested_equals_merged = "sha"        # sha | generated-delta | patch-id   (6.3)
allow_cached_steps   = "verified"   # never | verified
allow_retried_steps  = true

[proof]
accept_roles  = ["proof"]           # add "scout" to let a scout green count
max_age_hours = 24

[[risk]]                            # a lane gets a tag when its diff touches a path; tags are also declared per lane
tag   = "money"
paths = ["billing/**", "**/invoice*"]

[[risk]]
tag   = "permission"
paths = ["auth/**", "**/policies/**"]

[[risk]]
tag   = "leak"
paths = ["**/export*", "**/masking/**"]

[[risk]]
tag   = "egress"
paths = ["**/http_client*", "integrations/**"]

[review.by_tag.money]               # same table for permission, leak, egress
reviews       = 1
independent   = true                # the reviewer is not the lane's author and received the diff, not a summary
reviewer      = ["human", "agent"]
bound_to      = "lane-tip"          # a commit after the review needs a new one
blockers_open = 0
allow_retried_steps = false         # a retried green cannot merge this tag

[review.by_tag.governance]          # set by the engine when a lane touches the policy or pipeline file
reviews  = 1
reviewer = ["human"]

[rerun]
per_step_per_sha      = 1
per_batch             = 3
infra_per_batch       = 2
full_chains_per_batch = 4
flake_window          = { batches = 10, max_flakes = 2 }

[red]                               # may only be stricter than the ceilings of 5.2
"flake-load"            = "retry"
"infra-port"            = "retry-after-doctor"
"generated-uncommitted" = "patch-auto"
"baseline-missing"      = "patch-propose"
"baseline-shrink"       = "escalate"
"platform-windows"      = "ignore-on-scout"
"real"                  = "escalate"

[[baseline]]
path          = "tests/expected-counts.json"
additive_auto = true

[[baseline]]
path          = "security/allowlist.json"
additive_auto = false               # an addition here is a decision, not bookkeeping

[[actor]]
id   = "engine"
kind = "automation"
may  = ["stack", "settle", "preflight", "scout", "prove", "retry", "patch:generated", "merge",
        "push:mirror", "deploy:staging", "tidy:auto"]

[[actor]]
id   = "agent:*"
kind = "agent"
may  = ["review", "propose-patch", "patch:lane"]

[[actor]]
id   = "human:owner"
kind = "human"
may  = ["*"]
key  = "ssh-ed25519 AAAA... owner"

[approvals]
verify       = "ssh-sig"            # none | ssh-sig   (6.5)
always_human = ["deploy:production", "rollback", "override", "policy-change", "eject-lane",
                "baseline-shrink", "abandon", "tidy:worktrees"]

[[deploy]]
name    = "mirror"
tier    = "mirror"                  # a push with no side effect
auto    = true

[[deploy]]
name    = "staging"
tier    = "staging"
auto    = true
after   = ["mirror"]
health_timeout_min = 30

[[deploy]]
name    = "production"
tier    = "production"
auto    = false                     # auto = true with tier = "production" is rejected by the loader
approvals = 1
```

### 6.2 The floor: rules fixed in code

No policy file can switch these off. Each has a stable rule id that a refusal prints.

| rule id | rule |
|---|---|
| `floor.zero-red` | a merge needs zero red steps in the proof |
| `floor.full-mode` | a preview, red-only or partial run is never a proof on its own |
| `floor.proven-sha` | the merged commit is covered by a proof in the declared equality mode |
| `floor.no-force` | no force-push and no history rewrite on a protected branch |
| `floor.production-human` | a production target never deploys without a human approval |
| `floor.named-paths` | the engine stages paths by name; a stray modified or untracked path stops the commit and is listed |
| `floor.no-destroy` | no hard reset, no stash drop, no forced worktree removal, no branch delete without an archive tag |
| `floor.policy-from-protected` | the policy is read from the protected ref |
| `floor.log-append-only` | the decision log is never updated or deleted; an unverifiable chain stops every automatic transition |
| `floor.review-before-stack` | a lane with a risk tag is not stacked without the reviews its tag requires |
| `floor.red-default-real` | an unclassified red is `real` |

### 6.3 Merge rules

A merge (T7) needs, in order: `GATES_DONE`; `require_mode`; zero red; the log's commit equal to the recorded tested sha; that sha equal
to the batch tip now; cached steps either absent or each verified against its stored entry; a proof no older than `max_age_hours`; the
protected branch able to fast-forward to the tip; approvals, if any tag asks for one.

`tested_equals_merged` says what "the same commit" means:

- **`sha`** (default): byte-for-byte the commit the chain ran. If the protected branch moved, the batch is restacked on it and proven
  again.
- **`generated-delta`**: the merged commit may differ from the tested one only inside the declared generated directories, and a regen
  pass on the merged commit must change nothing. For repositories whose chain regenerates files.
- **`patch-id`**: the batch may be rebased over a moved protected branch if its code patch id is unchanged, files absorbed by the
  protected branch are byte-identical to it, and a configured light check set is green on the result. The weakest mode; every merge
  under it records both shas and the patch id.

The mode in force is written into every merge decision, so a reader of the log knows which promise was kept.

The merge itself is a compare-and-swap of the ref: from the value recorded when the proof began to the proven sha. It needs no working
tree and cannot be half done.

### 6.4 Risk tags and reviews

A lane's tags are the union of what its brief declared and what the `[[risk]]` globs derive from its diff. A derived tag cannot be
removed by a declaration; only a recorded human override removes one. A lane that touches `gatetrain.policy.toml` or `gatetrain.toml`
gets the tag `governance`.

A review is a row: lane, tip sha, reviewer, verdict, open blockers, a reference to the findings. T1 checks, per tag, that enough reviews
exist for the lane's **current** tip, by a reviewer who is not the lane's author, with zero open blockers. `gatetrain review record`
writes the row; the `review-lane` skill produces the content. The engine does not judge the quality of a review. It enforces that one
exists, is independent, and is not stale.

### 6.5 Actors and approvals

Every decision names an actor: `engine`, `agent:<name>` or `human:<name>`. The `may` lists decide which transitions and protected actions
an actor can trigger.

The engine cannot authenticate whoever types a command. With `verify = "none"` an actor is a claim, and the log is an honest record of
claims; that is enough when one person runs everything. With `verify = "ssh-sig"` an approval for an `always_human` action carries a
signature over the canonical approval body (6.8), made with `ssh-keygen -Y sign` and checked with `ssh-keygen -Y verify` against the keys
in the policy. An agent that holds a shell but not the key cannot approve. Nothing else is signed: transitions by the engine need no key.

### 6.6 Push and deploy

Every remote is a deploy target with a tier. A push that triggers a deployment **is** a deployment and is governed as one; that is why
`mirror` and `staging` are different targets even when both are `git push`. Targets run in `after` order. A target runs automatically
only when `auto = true`, its tier is not `production`, and the actor has `deploy:<tier>`. Otherwise the batch stops at `merged` with the
target listed under "waits on you", and `gatetrain approve deploy <batch> <target>` releases it.

Health is the target's health command reporting **the merged sha** as the running build, not a 200 from whatever was running before.
A timeout is `deploy-unconfirmed`: an escalation, never an automatic rollback.

### 6.7 Refusals name the rule

```text
$ gatetrain batch advance batch-42
REFUSED merge batch-42
  rule    merge.tested-equals-merged (mode sha)   policy 3f2a9c1e
  because tested 0a1b2c3d4, tip 9d8e7f6a5 (1 commit after the proof began)
  to proceed: gatetrain batch advance batch-42 --reprove
  decision #1184
```

Exit code 3 means "refused by a rule" (0 advanced or nothing to do, 1 failed, 2 usage or config). `gatetrain policy explain <rule>`
prints the rule, where it is set, and the last decisions that cited it.

### 6.8 The decision log

One append-only table, `decisions`, with update and delete triggers that abort. Each entry:

| field | content |
|---|---|
| `seq` | 1, 2, 3 ... no gaps |
| `at` | ISO 8601 with zone |
| `actor` | `engine`, `agent:<name>`, `human:<name>` |
| `kind` | `transition`, `refusal`, `approval`, `override`, `retry`, `patch`, `escalation`, `deploy`, `tidy`, `policy-change`, `anchor` |
| `batch` | batch id or null |
| `subject` | what it applies to: sha, run id, target, lane |
| `rule` | the rule id that allowed or refused it |
| `policy_hash` | sha256 of the canonical form of the parsed policy in force |
| `evidence` | digests and counts: log sha256, steps, reds, the finding ids |
| `outcome` | `allowed`, `refused`, `recorded` |
| `reason` | one sentence |
| `signature` | present on signed approvals |
| `prev_hash`, `hash` | the chain |

**Canonical form.** The entry without `hash`, as JSON: UTF-8, object keys sorted, no whitespace, no floats (numbers are integers; times
are strings), strings written as given with the minimal JSON escapes. Keys are the fixed ASCII field names of the schema, so byte order
and code-point order agree. For this value subset that is the output of RFC 8785; in Rust it is `serde_json::to_string` over a sorted
map, and one golden file of entries with their expected bytes pins it for any other implementation.

**Chain.** `hash = sha256(prev_hash + "\n" + canonical(body))` in lowercase hex; the first entry's `prev_hash` is 64 zeros.

**Verification.** `gatetrain log verify` walks from `seq` 1: recomputes each hash, checks `prev_hash` linkage, checks `seq` has no gap,
verifies signatures on approvals, and checks each `policy_hash` against a `policy-change` entry at or before it. It prints the first bad
`seq` and exits 1. The engine runs the tail of this check (from the last anchor) at the start of every tick; a failure stops every
automatic transition (`floor.log-append-only`) and escalates.

**Anchors.** A chain protects against edits in the middle, not against replacing the whole file. So after each merge the engine writes
the head hash somewhere the ledger's owner cannot quietly rewrite: by default an annotated tag `gatetrain/<batch>` on the merged commit
whose message holds `seq` and `hash`, pushed with the mirror target. `verify --anchors` compares. A different anchor (a second remote,
a notifier) is a plugin.

**Export.** `gatetrain log export` writes JSON Lines in canonical form; `verify --file` checks an export without the database.

### 6.9 Changing the policy

The policy is a file in the repository, so a change is a commit, reviewed like any other and tagged `governance`.

1. The engine reads the policy from `policy_ref` at every tick and computes `policy_hash`.
2. A hash it has not seen makes the engine append a `policy-change` entry first: old hash, new hash, the commit that changed the file,
   its author, and the list of rule ids whose value changed.
3. Until a human approval for that entry exists, the engine acts under the **old** policy for running batches and refuses to open new
   ones (`policy.unapproved-change`). With `ssh-sig`, the approval is signed.
4. A batch records the policy hash at `open` and at `merged`. A policy that changed in between is not an error; both hashes are in the
   log, and the stricter of the two applies to that batch's merge.

A change that would violate the floor never loads: the engine keeps the last valid policy and refuses with the loader's message.

## 7. Plugin points

The engine knows nothing about a language, a build tool or a deployment. `gatetrain.toml` supplies commands as argv lists. Contract for
every command: it receives `GATETRAIN_BATCH`, `GATETRAIN_SHA`, `GATETRAIN_RUN_ID`, `GATETRAIN_LOG`, `GATETRAIN_BASE`,
`GATETRAIN_PARALLEL` in its environment, runs in a checkout of the batch, and its **exit code** is its verdict.

```toml
[stack]
strategy     = "own-commits"                 # built in; or command = [...]
branch       = "batch/{batch}/s{attempt}"
hubs         = [{ glob = "**/routes.json", rebuild = ["tools/merge-json", "{base}", "{ours}", "{theirs}"] }]
resolvers    = [{ glob = "tests/**", take = "lane" }, { glob = "**/*.lock", take = "manual" }]

[regen]
command        = ["tools/regen-all"]
max_passes     = 3
generated_dirs = ["docs/_generated", "site/reference"]
baseline_files = ["tests/expected-counts.json"]

[preflight]
command = ["tools/preflight"]

[chain]
command          = ["tools/run-chain"]       # must print the log format of docs/log-format.md
red_only_command = ["tools/run-chain", "--only", "{steps}"]
preview_command  = ["tools/run-chain", "--preview"]

[deploy.staging]
command = ["git", "push", "staging", "{sha}:refs/heads/main"]
health  = ["tools/health", "--expect", "{sha}"]

[notify]
commands = [["tools/notify"]]                # each receives one JSON event on stdin
```

1. **Stack strategy.** Built in: `own-commits` (each lane's commits since its proven base, in passes per base, carried commits once;
   the plan comes from `gatetrain lanes --json`). Per-glob resolvers say whose side wins or that a person must merge; hub files are
   rebuilt by a command from base, ours and theirs, never text-merged (L5, L6, L7). A hub rebuild is followed by a built-in guard:
   nothing the base or a lane added may be missing unless a lane removed it. A file with no resolver that conflicts is `MANUAL` and
   the batch is held. A repository with an unusual history replaces the whole strategy with a command that leaves a branch behind.
2. **Regen command.** One command that runs every generator; the engine owns the fixpoint loop, the change detection and the commit.
3. **Generated directories.** The only places where the engine commits files it did not get from a lane, new files included. A changed
   file outside them after regen is a refusal (`floor.named-paths`), listed by name.
4. **Baseline files.** Counts, lists and censuses that ratchet. The engine diffs them structurally to tell `baseline-missing` from
   `baseline-shrink`.
5. **Chain runner.** Any command that prints the documented markers. Parallelism comes from the environment. The red-only and preview
   forms are optional; without a red-only form a retry is a full run and is charged as one.
6. **Health check.** Exit 0 only when the target serves the expected sha. The deploy command must be safe to run twice.
7. **Notifiers.** Events: `refused`, `held`, `escalated`, `proven`, `merged`, `deploy-unconfirmed`, `live`, `log-broken`. A notifier that
   fails is logged and never blocks.

Plugins are commands, not linked code. The boundary is a process and an exit code, so an adopter writes them in whatever the repository
already uses, and they do not change while commands move from Python to Rust.

## 8. Commands

```
gatetrain batch open <id> --base REF LANE...     create the batch
gatetrain batch advance <id> [--reprove]         evaluate guards, take every transition that is allowed, stop at the first wait
gatetrain batch status [<id>] [--json]           derived state, what runs where, what waits on whom
gatetrain batch hold|release|abandon <id>        a person's decisions, recorded
gatetrain tick                                   doctor per host, advance every open batch, tidy --auto, report 3-6 lines
gatetrain hold --lock F --run ID -- CMD...       the holder (4.3)
gatetrain diagnose <run>                         classify reds (5.1)
gatetrain review record ...                      a review verdict
gatetrain approve <action> <batch> [...]         an approval, signed when the policy says so
gatetrain policy check|explain                   validate the file; explain a rule
gatetrain log verify|export|show                 the decision log
```

`tick` is the whole scheduler. It takes a file lock on the ledger for its duration, so two ticks never interleave. It replaces every
"armed" follow-up: nothing is queued to fire later, so a stop has nothing left behind to cancel (L1), and the next tick after a green
chain performs the merge.

## 9. Implementation: a Rust core, one binary

### 9.1 Layout

```
Cargo.toml                    workspace
crates/gatetrain-core/        library: no terminal, no argument parsing
  src/state.rs                states, transitions, guards; decide(state, facts, policy) -> Decision is pure, no I/O
  src/policy.rs               the policy file, the floor rules, rule ids
  src/ledger.rs               SQLite schema, append-only triggers, event fold, leases
  src/declog.rs               canonical form, hash chain, verify, anchors, export
  src/sched.rs                host roles, capacity, leases against locks, scout then proof
  src/red.rs                  the rules table, findings, budgets, class -> action
  src/hold.rs                 the holder
  src/logfmt.rs               the chain log parser (docs/log-format.md)
  src/exec.rs                 running a plugin command or a host command (local, ssh); the only module that spawns
crates/gatetrain-cli/         binary `gatetrain`: clap commands, text and --json output, exit codes
rules/red-classes.toml        the red rules table (data, shared with the Python classifier)
gatetrain/  tests/  skills/   the Python package, its tests and the skills, as today
```

Everything that touches the world goes through a trait (`Clock`, `Exec`, `Git`), so the core is tested with fakes and a fake clock,
the way the Python tests inject a fake runner today.

### 9.2 Dependencies

A small set, all MIT or Apache-2.0 (or dual with one of them), checked in CI by `cargo deny`. Nothing is added without a line here.

| crate | why |
|---|---|
| `serde` (derive) | typed structs for the policy, the pipeline file, hosts, decisions and events |
| `serde_json` | the canonical form of the decision log, `--json` output, notifier events |
| `toml` | the three TOML files and the red rules table |
| `rusqlite` (feature `bundled`) | the ledger; SQLite is compiled in, so a release binary needs nothing installed |
| `sha2` | the hash chain, log digests, the policy hash |
| `clap` (derive) | the command line; same commands and flags as the Python CLI |
| `thiserror` | typed errors in the core: a refusal carries its rule id as a value, not a string |
| `anyhow` | error context in the CLI crate only |
| `time` | ISO 8601 with zone, parsed and printed exactly |
| `regex` | the anchored log markers and the patterns of the rules table |
| `globset` | path globs of risk tags, resolvers and hubs |
| dev only: `proptest`, `tempfile` | property tests of the hash chain; throwaway repositories and ledgers |

Deliberately absent: an async runtime (the engine runs, waits on a child at most, and exits), a git library (it shells out to `git`,
as an adopter's scripts do, and `doctor` checks the tool), an ssh library (`ssh`), a crypto library for approvals (`ssh-keygen -Y`),
a file-lock crate (`std::fs::File::lock`, stable since Rust 1.89, which is therefore the minimum version), an HTTP client.

### 9.3 Python and Rust side by side

- The Rust binary is named `gatetrain`. A subcommand it has not ported is forwarded to `python -m gatetrain` with the same arguments
  and exit code; `gatetrain --which <command>` says which implementation answers. Without Python installed, a forwarded command fails
  with one line that names the command and the release in which it is planned.
- The engine's new commands (`batch`, `tick`, `hold`, `policy`, `log`, `approve`, `review`) exist in Rust only. They are never written
  in Python first.
- An existing command is ported behind its **own existing tests**: the synthetic fixtures and expected outputs become a shared corpus
  (`tests/corpus/<command>/<case>/{args,stdin,expected-stdout,expected-exit}`), and both implementations must produce the same bytes and
  exit code on every case. A port is done when the corpus is green for both on Linux and Windows. The Python version is retired only one
  release after that, in a commit that removes it and its forwarding entry together.
- Both read and write the same ledger file during the overlap. The schema carries a version (`PRAGMA user_version`); only the Rust
  side migrates it, and the Python side refuses a version it does not know rather than guess.
- Order of ports, by what the engine needs first: the log parser and `ingest`, `status` and `next-tick`, `diagnose` (the rules table),
  `doctor`, `stop`, then `lanes`, `check` and `tidy` last (the largest, and the ones where a parity gap could delete something: `tidy`
  stays Python until its corpus includes every guard violation).

### 9.4 Distribution

- **Release binaries** for Linux (x86_64 and aarch64, statically linked against musl), Windows (x86_64) and macOS (aarch64 and x86_64),
  built by a plain CI matrix on a version tag, each with a SHA-256 file. A matrix is preferred over a release tool: five targets, one
  workflow file, nothing to learn; a release tool can replace it later without changing what users download.
- **`cargo install gatetrain-cli`** from the registry, and `cargo install --git` for an unreleased commit.
- The skills and the plugin manifest are unchanged; they call `gatetrain` on the PATH and do not care which language answers.

## 10. Migration path for an adopting repository

### Phase 0: wrap

The repository's scripts keep doing the work. Each existing script becomes one plugin command, unchanged: the stack script behind
`[stack].command`, the regen script, the preflight script, the chain launcher behind `[chain].command` under `gatetrain hold`. The engine
records states and decisions and enforces nothing beyond the floor rules that need no cooperation (it will not report `proven` for a red
or partial log). Run it in `--observe` for a few batches beside the old flow and compare its verdicts with what happened.

Gain: one status, crash-resume, a real host lock, the decision log from day one. Risk: none to the old flow; it still merges.

### Phase 1: the engine owns the state machine

The glue goes: the "finish" scripts that chain regen, push, preflight and launch; the armed merger; the ad-hoc holder; the "is a chain
running" grep; hard-coded parallelism; per-batch rerun scripts. The engine performs T1-T9, including the merge compare-and-swap, the
regen fixpoint commit and red-only reruns. Red classes drive retries and proposals. The repository's merge script shrinks to what is
truly its own (see below) or disappears when the mode is `sha`.

### Phase 2: policies enforced

`gatetrain.policy.toml` on the protected branch; risk tags derived from paths; reviews required at T1; budgets; actors; signed approvals
if chosen; deploy targets with tiers. Refusals now stop real actions. Turn rules on one at a time: each starts in `warn` (a refusal is
logged, the action proceeds) and moves to `enforce` after it has been seen to refuse correctly on a synthetic violation.

### What stays in the repository's own scripts

- The chain itself: which steps exist, how they are grouped, cached, pinned to cores and run in parallel.
- Every generator, and the order between them.
- Structural merges of the repository's own hub files (the commands behind `hubs`).
- The preflight's content.
- Deploy and health commands, and anything that touches credentials.
- Per-lane gates.

## 11. Risks

| risk | mitigation |
|---|---|
| The engine becomes a second place where rules live, and drifts from the skills | The skills call the commands; the module pages cite rule ids. One rule, one id. |
| A wrong flake rule hides a defect | Section 5.4; `real` by default; a retried green stays visible; the flake window; no retries for tagged batches unless policy says so. |
| The ledger is lost or corrupted | It is one SQLite file: an online backup after each merge, plus the anchors. A batch can be re-opened from git alone; only history is lost. |
| The control machine is down | Nothing advances; nothing breaks. A running chain finishes under its holder and is observed at the next tick. |
| An actor is spoofed | `ssh-sig` for the actions that matter; without it the log says so in every entry (`verify: none`). |
| Policy read from the batch | Fixed in code: read from the protected ref. |
| The stack strategy is too clever | It repairs only declared hubs and resolvers; anything else is `MANUAL`. The hub guard checks its own output. |
| `generated-delta` and `patch-id` weaken the proof | They are opt-in, named in every merge decision, and each has a mechanical check. |
| Windows and macOS proof hosts | The holder's liveness and reboot probes are Linux first; other platforms are `light` or `scout` until tested, and `doctor` says so. |
| Over-building | For a single-person repository the value is in slices 1-4; slices 5 and 6 are optional and can wait for a second person or a production target. |

| Two implementations during the port | One shared corpus, byte parity required; only Rust migrates the schema; new engine commands exist in Rust only; a Python command is removed one release after parity, never before. |
| A dependency goes bad | Eleven direct crates, each with its reason in 9.2; `cargo deny` on licences and advisories in CI; `Cargo.lock` committed. |

## 12. Non-goals

- **No orchestrator service, no web UI, no API server.** A command, a file, a timer.
- **No new daemon.** The only long-lived process is the holder, and it lives exactly as long as the chain it parents. A watcher or a
  daemon is added only after a measurement shows the tick misses something that matters.
- **No async runtime.** Nothing here waits on more than one child process and a clock.
- **Not a CI system or a step runner.** The chain command runs steps; the engine reads its log.
- **No scheduler for lanes or agents.** `plan` stays advice. The engine starts at "lanes are finished".
- **No secrets.** The engine stores none and passes none; deploy commands find their own.
- **No big-bang rewrite.** Python commands are ported one at a time behind their tests (9.3); none is rewritten and switched in the
  same release.
- **No plugin API in a language.** Extension is a command with an exit code.

## 13. Test strategy

Red first, always: a test is written, seen to fail for the stated reason, then made to pass, and both counts go in the commit message.
A guard is proven by a fixture that violates it, never by switching it off (L36).

- **Synthetic fixtures only.** Tests build their world in a temp folder: `git init` repositories made by a builder (lanes, bases,
  conflicts, hubs), fake plugins (tiny scripts that print the log markers and exit as told), a fake `Exec` with canned host output (as
  `tests/test_ops.py` does today), a fake clock. No log, path, name or number taken from a real project. New log fixtures are written by
  a generator from a spec, not derived from a real run.
- **Unit tests per module**, next to the code (`state`, `policy`, `ledger`, `declog`, `sched`, `red`, `hold`, `logfmt`), plus
  integration tests in `crates/gatetrain-cli/tests` that run the built binary.
- **The pure core.** `decide()` is tested as a table: for each state, each guard false in turn must yield a refusal with the right rule
  id; all true yields the transition. Every row of 3.2 and every floor rule of 6.2 has a red test.
- **Crash tests.** For each transition: stop after `intent`, after the effect, after `done`; restart; assert the same final state and no
  doubled effect (one branch, one run, one ref move). Reboot test: a lock file with a different boot id is a `stopped` run.
- **The holder.** Two holders on one lock: the second waits or refuses; a holder whose child dies writes `RELEASED` with its exit code;
  no `HOLDS` line means the launch failed.
- **Red handling.** One fixture per class; an unmatched red is `real`; a flake rule plus a failed assertion is `real`; a second red on
  retry is `real`; a retried green appears in the proof record; a retry on a different sha is refused; budgets refuse at the limit. The
  rules table is loaded by both implementations and must give the same findings on every fixture.
- **Policy.** The example file loads; each invalid file names its field; a looser-than-ceiling action is rejected; production with
  `auto = true` is rejected; an unknown key is rejected; policy from the batch branch is ignored.
- **Decision log.** Golden canonical bytes for fixed entries; an update or delete statement aborts; a signed approval verifies and a
  tampered one does not. **Property tests** (`proptest`, cheap here because the chain is a pure function over a list): for any sequence
  of entries, `verify` accepts the chain; for any single change (a flipped byte in any field, a deleted, duplicated, reordered or
  truncated entry) `verify` fails at or before the changed `seq`; canonical bytes survive a parse and re-serialise round trip.
- **Parity.** The shared corpus of 9.3 runs against both implementations for every ported command; a difference in one byte or one exit
  code is a failure.
- **Lints.** `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`; `cargo deny check`. States, red classes, actions and
  rule ids are enums, so a misspelt one does not compile.
- **Run.** `cargo test` for the workspace, then `python -m unittest discover -s tests -v` while Python commands remain, on Linux,
  Windows and macOS. A test that cannot run on a platform is reported as skipped with its reason, never counted as passed.

## 14. Slice plan

Each slice is shippable alone, leaves every existing command working, and is ordered so that an adopter can delete a script of their
own as early as possible.

| # | slice | ships | an adopter can retire | tests (red first) |
|---|---|---|---|---|
| 1 | **Workspace, ledger v2, decision log** | the Cargo workspace and release matrix; forwarding to Python; `logfmt` and `ingest` ported with parity; the new tables with append-only triggers and run snapshots; canonical form, hash chain, `log verify / export / show` | nothing yet | parity corpus for `ingest`; golden bytes; property tests of the chain; triggers abort; clippy clean |
| 2 | **The holder and the host lock** | `gatetrain hold`, lock file with pid and boot id, `HOLDS` / `RELEASED`, leases, "is a chain running" as a lock read, capacity and parallelism from `hosts.toml` | **the first replacement**: a hand-written lock-holder script, a "is a chain running" process grep, and parallelism typed into launch scripts | holder contention; child death; no `HOLDS` is a failed launch; a new boot id is `stopped`; a foreign lock is refused |
| 3 | **The state machine, wrapping (phases 0 and 1)** | `batch open / advance / status`, intent and done events, probes, plugins as commands, `--observe`, `tick`, `doctor` as a precondition, scout then proof, the floor rules | the "finish" script that chains regen, commit, preflight and launch; the follow-up merger queued to fire later; per-batch rerun launchers | the transition table; a crash at every point of every transition; a red or partial log is never `proven`; a stray path stops a commit |
| 4 | **Red handling** | the rules table read by Rust (`diagnose` ported with parity), findings, budgets, red-only retries, the generated patch, baseline diff and proposals, escalation bundles, notifiers | hand triage of repeat reds; rerun-verdict scripts | one fixture per class; every rule of 5.4; budgets at the limit; a composed proof needs one sha |
| 5 | **Policy enforced (phase 2)** | `gatetrain.policy.toml`, risk tags, reviews at T1, actors, `approve`, `ssh-sig`, `warn` then `enforce`, policy-change entries, `policy explain` | review and approval rules that live only in prose | loader errors by field; ceilings; a stale review refuses; an unapproved policy change refuses; signature verify and tamper |
| 6 | **Merge, deploy, close** | the built-in `own-commits` strategy with hubs, resolvers and the hub guard; the three equality modes; compare-and-swap merge; targets and tiers; health on the expected sha; anchors; `tidy --auto` at close; the migration guide | the generic half of a stack script, and the merge script | builder repositories with conflicts, carried commits and hubs; a moved protected ref refuses; each equality mode's check; production never automatic; health on the old build is not healthy |

Ports of the remaining Python commands (`status`, `next-tick`, `doctor`, `stop`, `lanes`, `check`, `tidy`) ride along with the slice that
first needs them in-process (`doctor` and `stop` with slice 3) or follow slice 6; until then they are forwarded.

Slices 1-3 replace the fragile glue. Slice 4 removes most reruns. Slices 5 and 6 are where governance becomes binding.
