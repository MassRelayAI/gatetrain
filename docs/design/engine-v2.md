# Engine v2: one structured run, measured hardware, less waiting

Design, 2026-11-17. Status: partly built. On `main` at `22b71d2`: slices E1, E2, E3 (batch side) and E5, plus the earlier D, C and G (section 14 marks each). The rest is proposed. Base: `main` at `88bf73b`. It extends
[`engine-and-governance.md`](engine-and-governance.md) (the state machine and the floor rules stay as written there) and changes nothing
in [`adr-001-decision-log.md`](adr-001-decision-log.md).

How to read it: every statement about today's code carries `file:line` at `88bf73b`. A statement marked **unverified** was not checked
against code or a measurement and must be checked by the slice that relies on it. Numbers marked **measured** come from the live adopter
(section 1); numbers marked **estimate** are arithmetic on those and name their formula.

The goal, in the adopter's words: cut the build and merge bottleneck with the work structured inside the engine and its governance, save
time, and detect and monitor hardware (for example a new SSD beside an HDD) from performance, on any kind of machine.

## 1. The baseline this design is measured against

The live adopter is a large monorepo. Its proof host P has 40 cores, an HDD root and an SSD build folder. Its workstation W is Windows
with a Linux subsystem, 32 threads. All figures below are **measured** there unless marked.

| what | figure | what it means |
|---|---|---|
| pre-check, serial | 3398 s (56m38s) for its checks, one after another | a red at about minute 25 was only acted on at minute 56: about 31 minutes of waiting on a known red |
| one batch (call it A) | 6 pre-check and chain rounds | 1 real defect. The other rounds: reds hidden behind an `&&`, generated-file drift, two checks racing on one scratch folder, a regenerated database baseline that did not parse |
| the baseline that did not parse | about 440 chain steps red, seen after 1h40m | one cause, read as 440 failures |
| one green chain (call it B) | wall 2h29m (8940 s), busy 16h38m (59880 s), average parallelism 6.7 of 12 allowed | see the bounds below |
| slowest steps of B | test-api 41m24s (2484 s), cargo-test 30m45s (1845 s), test-tools 25m13s (1513 s; 253 s with a parallel runner), an end-to-end gate 14m, database gates 6 to 9 min each | test-api ran its 392 files one at a time: wall 2483 s against 1448 s of summed test time |
| the same work twice | test-api, test-tools, test-web, test-ops, clippy and cargo-deny run in the pre-check and again in the chain, on the same commit | at least 2484 + 1513 = 3997 s of busy time per chain (the other four were not measured here) |
| host contention | the same test files ran 40 to 90 times slower on P than on a 4-core cloud machine | P's HDD saturated in the past; build folders were moved to the SSD by hand |
| the ledger | 3 runs, ingested by hand on 2026-11-17 | every figure above was collected outside gatetrain |

Two bounds follow from B, both **estimates** (formula: a run cannot be shorter than its longest step, nor than busy time divided by the
allowed parallelism; this is the same formula `predict` uses, `crates/gatetrain-core/src/predict.rs:93`):

- packing bound: 59880 / 12 = 4990 s (1h23m). B took 8940 s, so up to 3950 s (65m50s) is lost to how the steps are arranged, **if**
  the host could really run 12 at once without each step slowing down. Whether it can is exactly what nobody has measured (section 10).
- longest-step bound: 2484 s (test-api). With test-api split in 4 it becomes 1845 s (cargo-test).

The adopter's own pre-check script confirms three of the causes. It runs its checks in a serial loop
(`prechain-checks.sh:39-45`, the `run` lines at `:63-90`). Three of its checks are `a && b` in one step (`:66`, `:67`, `:78`), so a red in
`a` hides the verdict of `b`. It repeats the three unit suites the chain runs (`:85-89`, with the reason at `:86`). The script lists 27
checks (`:25`), the measured run had 26: **unverified** which copy ran.

## 2. What today's code does, where it matters for this design

| # | finding | evidence |
|---|---|---|
| F1 | A `gatetrain chain` run can never be a proof: it always prints `CHAIN_MODE preflight` and stamps `mode: preflight`; the batch engine accepts only `full`. | `chain.rs:214`, `chain.rs:323`, `batch.rs:1166-1168` |
| F2 | `chain` writes a step's output to `<out>/<step>.full`, not into the log. `diagnose` reads output only between a `=== <step>` banner and the next marker in the log. A red of a `chain` run therefore reaches the classifier with no lines. | `chain.rs:143-144`, `diagnose.rs:121-142`, `diagnose.rs:328-338` |
| F3 | The host a `chain` log names comes from the environment (`COMPUTERNAME`, `HOSTNAME`), else `unknown`. It is not the name in `hosts.toml`. | `chain.rs:174-179` |
| F4 | The engine's pre-flight (T3) runs its command and waits, then writes its own stamp, a file holding the sha. It does not read the stamp `chain` writes. | `batch.rs:1025-1036`, `batch.rs:797-799`, `chain.rs:327` |
| F5 | A red proof is the end of the line in code: `advance` refuses every `red` batch, findings are step and exit code only, there is no retry (T6a), no scout and no policy file. | `batch.rs:651`, `batch.rs:1169-1178`; `crates/gatetrain-core/src/` holds no `policy.rs`, `sched.rs` or `red.rs` (directory listing) |
| F6 | Admission for a light run applies the chain's memory prediction and never looks at the proof lock; only role `proof` does. A pre-check may start on a host that is proving. | `hostlock.rs:105-113`, `hostlock.rs:126-130` |
| F7 | `host_samples` is a table nothing writes or reads; `steps.waited_sec` and `steps.peak_rss_mb` are never written. | `ledger/schema.sql:45-53`, `:25-26`; `runs.rs:60-63` names seven columns; a search for `host_samples`, `load_1m`, `iowait`, `rotational`, `loadavg` in `crates/**/*.rs` finds nothing; `modules/ledger.md:22` says "tables only" |
| F8 | Disk speed is a boolean a person types. `doctor` warns from it and capacity trusts it. Nothing is measured. | `config.rs:58`, `config.rs:349`, `doctor.rs:113-123`, `doctor.rs:148` |
| F9 | The ledger is fed only when someone runs `ingest` or `status` on a log file. `chain` writes no row; `hold` writes one memory peak and the build-folder size. | `cmd_runs.rs:44-56`, `cmd_runs.rs:127-140`, `holdcmd.rs:266-273` |
| F10 | Platforms: `os` may be `auto`, `linux` or `windows`; a local host that is not Windows is treated as Linux; the boot id and the total memory are unknown outside Linux and Windows; `stop` is Linux only. The lock itself works on Linux, macOS and Windows. | `config.rs:12`, `config.rs:240-248`, `hold.rs:100-121`, `memory.rs:195-223`, `stop.rs:1`, `hold.rs:8-12` |
| F11 | A host whose `reach` is neither `local` nor `ssh ...` has no runner, so the cloud host of the example file cannot be reached. | `runner.rs:274-282`, `config/hosts.example.toml:79`, `README.md:493` |
| F12 | Host keys are a strict allow-list. `max_chains`, `detach` and `platform` from the earlier design are not on it, so they are not built. Every host key this design adds must be added there. | `config.rs:354`, `config.rs:359-364`, `engine-and-governance.md:156-160` |
| F13 | The report's parallelism is computed from step intervals, one interval per step. | `analysis.rs:18-48` |
| F14 | A step that needs a step outside `--only` / `--skip` does not wait for it. | `chain.rs:264-268` |

## 3. The decisions

| # | decision | why |
|---|---|---|
| V1 | **One evidence rule** decides both "may the proof reuse a pre-check result" and "may a step run on another host". A step result counts for a proof only when its commit, its input key and its host class all match, and the engine verifies it from its own ledger. Chain never decides this. | Two questions, one answer, one refusal code that already exists (`proof.cached-unverified`). Section 7. |
| V2 | **A multi-host run is one full run on the proof host in which the steps already proven elsewhere are verified hits.** There is no "segment" log and no second log format. | The proof log keeps today's shape and today's T5 reads it unchanged. Section 9. |
| V3 | **A sharded step is one step in the log.** Shards add lines with a new prefix that today's parsers skip. | Section 6. |
| V4 | **The sampler lives in processes that already exist** (the holder, or `chain` when it runs without one) and writes a plain file. No daemon. | `hold` already samples memory every 2 s (`holdcmd.rs:246-259`). |
| V5 | **`probe` is a command, bounded, refused while a proof holds the host.** Its numbers go in the ledger; the `fast` flag stays a person's statement that `doctor` now checks against the measurement. | F8. |
| V6 | **`advise` only reads and prints.** It has no flag that applies anything. A move happens when a person changes a config file. | Principle 5 of the earlier design (`engine-and-governance.md:21-22`). |
| V7 | **The pre-flight becomes a launched run** (a new state `preflighting`), on a light host that is not proving, and the next batch may be stacked on the tip of the batch that is proving. | F4, F6; this is the pipelining. Section 8. |
| V8 | **`chain` feeds the ledger itself** and gets a `--mode`. A partial run can never call itself `full`. | F1, F9. |
| V9 | **A cache hit must be earned in shadow first.** A step may be skipped on a key only after that key model was seen to agree with real runs. | "A check never seen red has not run" (`docs/method.md:41`); the adopter's own cache works this way. |
| V10 | **Unknown is a value.** Every hardware figure is `measured` or `unknown` with a reason; no rule computes minutes from an unknown. | Section 10.4. |
| V11 | **No new crate, no new service, no new long-lived process.** Windows counters and `setsid` are reached with `extern` declarations, as `memory.rs:200-223` and `disk.rs:113-125` already do. | Section 15. |

## 4. Components and their boundaries

| component | owns | must never |
|---|---|---|
| **chain** (`chain.rs`, `chaincmd.rs`) | running a steps file on one host: tiers, `needs`, shards, exclusive keys, the step cache, the log lines, one stamp, the sampler when no holder runs it | decide whether a hit counts for a proof; classify a red; write to git; read the policy; call another host |
| **ledger** (`ledger.rs`, `runs.rs`) | rows: runs, steps, shards, skips, stamps, evidence, findings, causes, probes, samples, advice. Forward-only migrations. | update or delete a row; hold a status column that history could contradict; be the only copy of a fact that also lives in a file on a host (the file is the fact, the row is the index) |
| **diagnose** (`diagnose.rs`, `rules/red-classes.toml`) | failure blocks, the class of each red, causes: reds that share one root, each with a stable fingerprint | turn a red green; retry anything; call a red anything but `real` without a matched rule (`floor.red-default-real`) |
| **batch** (`batch.rs`, `batchcfg.rs`, `hostlock.rs`) | the state machine: stack, settle, pre-flight, prove, merge, deploy, close; every guard and refusal; verifying evidence; the pipelining | run a step itself; trust a marker it did not verify; merge a red or unproven sha; move a protected ref except by compare-and-swap or fast-forward (`batch.rs:1095-1111`) |
| **host model** (`config.rs`, `disk.rs`, `memory.rs`, `doctor.rs`) | what a host is and may do: roles, disks and their uses, memory, ports, the host class | invent a number (`disk.rs:1-5`, `memory.rs:7-8`); fix anything on a host |
| **probe** (new `probe.rs`) | a short benchmark of one host and of each configured disk; the rotational flag; rows in the ledger | run while the host lock is held; write outside a configured disk path; run on a disk nobody named; exceed its time and byte budget |
| **sampler** (new `sample.rs`) | load, CPU busy, I/O wait, I/O stall, disk busy, free memory, every N seconds during a run, appended to a file beside the log | start a process that outlives the run; cost more than a few file reads per sample; guess a value the platform does not expose |
| **advise** (new `advise.rs`) | rules that turn ledger rows into proposed moves, each with evidence, an estimate and a confidence; rows in `advice` | change a config file, a host or a batch; print minutes it cannot derive |
| **tick** (new `tick.rs`) and **watch** (`watch.rs`) | one pass: doctor per host, ingest finished logs and samples, advance every open batch, `tidy --auto`, one line of advice, a report of 3 to 6 lines. `watch` stays the read-only look at lanes (`watch.rs:1-13`). | open a batch; choose lanes; apply advice; leave anything queued to fire later (`engine-and-governance.md:629-631`); run a probe unless `[probe] auto = true` and the host is free |

## 5. Chain

### 5.1 What exists

A steps file of `[[step]]` rows with `name`, `tier`, `run` or `argv`, `needs`, `timeout_s` (`chain.rs:26-36`). The fast tier runs, then
the slow tier, each up to `--par` at once (`chain.rs:222-301`); the slow tier is skipped when a fast step is red unless `--keep-going`
(`chain.rs:227-233`); a step whose need is red or skipped is skipped (`chain.rs:237-255`); a started tier runs to the end; a red prints
`RED_NOW` when it happens (`chain.rs:298`). A green run on a tree that is clean before and after writes `stamp-<sha>.json` with the
sha-256 of the steps file (`chain.rs:308-333`, `chain.rs:338-340`).

That already answers two of the measured problems once the adopter's pre-check is a steps file: every check runs and reports its own
exit code, and a fast red is known in minutes. The rest of this section is what is added.

### 5.2 Steps-file additions

All are optional keys of `[[step]]` unless noted. `Step` denies unknown fields (`chain.rs:27`), so each key is a deliberate change.

| key | type | meaning | slice |
|---|---|---|---|
| `inputs` | list of git pathspecs | what the step reads; enables the cache (branch `feat/chain-cache`) | C |
| `cache_salt` | string | part of the key (branch `feat/chain-cache`) | C |
| `key_cmd` | argv | its stdout is hashed into the key: tool versions, an image id, anything outside git the step depends on | E3 |
| `portable` | bool, default false | the author states the step does not depend on the host beyond its class (section 7.2). Only a portable step's result may be evidence on another host. | E9 |
| `shards` | integer 1 to 64 | run the command N times at once with `{shard}` and `{shards}` replaced (1-based: shard 1 of N is the first); one logical step (section 6). `shards = 1` is an ordinary single run (the placeholders read 1 and 1). **Built.** | E2 |
| `shard_check` | argv | runs after all shards are green, with `GATETRAIN_SHARD_OUT` naming the shard output files (an OS path list); non-zero makes the step red. Its output is `<step>.check.full`. For a check that every shard ran something. **Built.** | E2 |
| `shard_env` | bool, default false | the command reads `GATETRAIN_SHARD` and `GATETRAIN_SHARDS` itself, so it needs no `{shard}` placeholder. **Built.** | E2 |
| `excl` | list of strings | steps that share a string never run at once (a fixed port, a container name) | E12 |
| `slots` | integer, default 1 | how many of the `--par` slots the step takes (a step that uses many cores); clamped to `--par` | E12 |
| top-level `step_tmp` | bool, default false | each step (and each shard) gets its own empty folder `<out>/tmp/<step>[.<i>]`, exported as `GATETRAIN_STEP_TMP`, `TMPDIR`, `TMP` and `TEMP` | E0 |

Built (E2): each shard writes `<step>.<i>of<n>.full`; shards queue for `--par` slots, one slot each; `STEP_SHARD` lines carry each shard's start and exit; the step's exit code is 0 only when every shard and the `shard_check` are 0, else that of the lowest-numbered red shard. Rules: `{shard}` in a step without `shards` is a config error; `shards > 1` without `{shard}` in `run` or `argv` is a config error
unless `shard_env = true` says the command reads `GATETRAIN_SHARD` and `GATETRAIN_SHARDS`. `step_tmp` is the fix for two checks racing
on one scratch folder.

### 5.3 Command-line additions

| flag | meaning | slice |
|---|---|---|
| `--mode preflight\|lane\|full` | what the run claims to be; default `preflight` (today's fixed value, F1). `full` with `--only` or `--skip` is refused, exit 2. **Built.** | E1 |
| `--host NAME` | the name in `hosts.toml`; replaces the environment guess (F3). Under `hold` it comes from `GATETRAIN_HOST`. **Built.** | E1 |
| `--db FILE` | at the end, write the run, its steps, shards, skips and stamp into the ledger; without it nothing changes. The file is created or migrated forward. **Built.** | E1 |
| `--run-id ID` | the run's id in the ledger; default `chain-<UTC time>-<commit>`. **Built.** | E1 |
| `--cache DIR`, `--no-cache-for a,b` | the step cache (branch `feat/chain-cache`) | C |
| `--cache-shadow` | compute keys, store green results, run every step anyway, print `STEP_CACHE_SHADOW` (section 6) | E3 |
| `--evidence FILE` | a list of step results the engine is willing to accept for this sha (section 7.3) | E3 |
| `--embedded` | print only step lines: no header, no `RED_SUMMARY`, no `GATES_DONE`. For use as one phase inside an adopter's own chain script. | E3 |
| `--samples N` | sample the host every N seconds into `<out>/samples.jsonl` (default 10; 0 = off; off when a holder already samples) | E7 |

Also built in E1: the log gains one `CHAIN_OUT <folder>` line (the absolute `--out` folder) after `CHAIN_PARALLEL`, and `diagnose --out DIR` reads a red step's output from its `.full` files (or its red shards' files), by default from the folder `CHAIN_OUT` names. That closes F2.

`--embedded` exists because a nested full run would otherwise print `GATES_DONE` before the outer chain ends, and the parser takes the
first `GATES_DONE` it sees as the end of the run (`logfmt.rs:394-395`).

### 5.4 The stamp

One format for every kind of run. Today's fields stay (`chain.rs:322-326`); new fields are added, none is renamed.

```json
{ "v": 1, "kind": "preflight", "sha": "...", "host": "builder-1", "host_class": "sha256...", "at": "...", "mode": "preflight",
  "par": 8, "total_seconds": 512.4, "digest": "<sha-256 of the steps file>",
  "steps": [ { "name": "typecheck", "tier": "fast", "rc": 0, "seconds": 61.2, "key": "...", "cached_from": null, "shards": 1 } ],
  "cached": ["test-web"], "prev": ["<stamp digest>", "..."] }
```

- A stamp's **digest** is the sha-256 of the file's bytes. The file is written once and never changed.
- `prev` makes the **stamp chain**: a pre-flight stamp names the lane stamps of the batch's lanes; a proof stamp names the pre-flight
  stamp and the stamp of every run it took evidence from; the merge decision records the proof stamp's digest in its `evidence`
  (`engine-and-governance.md:520`). One logical run is the proof stamp plus everything reachable through `prev`.
- The engine's own `<sha>.stamp` file (F4) is retired in slice E5: T3 is done when a `chain` stamp exists for the tip with the digest of
  the configured steps file.

## 6. Log format: shards, shadow and keys, without breaking a parser

Today's parser dispatches on `starts_with` of ten fixed markers and ignores every other line (`logfmt.rs:360-410`). `diagnose` treats
any line that starts with `STEP_`, `RED_`, `GATES_DONE`, `CHAIN_` or `branch ` as a marker, not as output (`diagnose.rs:124`). The
adopter's shell readers match anchored `^STEP_EXIT ` and `^RED_SUMMARY: ` (for example `rerun-verdict.sh:20`). So a new line that
starts with `STEP_` but with none of `STEP_START`, `STEP_TIME`, `STEP_EXIT`, `STEP_CACHED`, `STEP_SKIPPED ` is skipped by all three.

New lines, all optional:

| line | meaning |
|---|---|
| `STEP_SHARD <step> <i>/<n> START <ISO time>` | shard i of n began |
| `STEP_SHARD <step> <i>/<n> EXIT <rc> <N>s` | shard i ended with this exit code after N seconds |
| `STEP_KEY <step> <key>` | the input key of a step that ran (so a later run can cite it) |
| `STEP_CACHE_SHADOW <step> would-hit\|miss <key> rc=<n>` | shadow mode: what the cache would have done, and what the real run did |
| `STEP_CACHED <step> <key> from <run-id>` | today's marker with two more words; today's regex reads only the step (`logfmt.rs:313`) |

A sharded step in the log:

```text
STEP_START test-api 2026-11-17T09:00:00Z
STEP_SHARD test-api 1/4 START 2026-11-17T09:00:00Z
STEP_SHARD test-api 2/4 START 2026-11-17T09:00:00Z
...
STEP_SHARD test-api 3/4 EXIT 1 611.0s
RED_NOW test-api rc=1 at 2026-11-17T09:10:11Z (shard 3/4; full output: test-api.3of4.full)
STEP_SHARD test-api 4/4 EXIT 0 640.2s
STEP_TIME test-api 640.2s
STEP_EXIT test-api 1
```

Rules:

- One `STEP_START`, one `STEP_TIME` (first shard start to last shard end) and one `STEP_EXIT` per logical step. The exit code is 0 only
  when every shard is 0 and `shard_check`, if any, is 0; otherwise it is the exit code of the lowest-numbered red shard.
- `RED_NOW` is printed once per logical step, at its first red shard; the other shards run to the end. (The engine builds its red list
  from `RED_NOW` lines, `batch.rs:1169`, so a second line would count twice.)
- A shard that never reported an exit makes the step red with exit code 125 and a note in its `.full`.
- Each shard takes one `--par` slot. Output goes to `<out>/<step>.<i>of<n>.full`; `<out>/<step>.full` lists them.
- A cache key covers the logical step, including `shards`. A hit skips all shards; an entry is written only when all are green.
- The ledger keeps shards in `step_shards`; `report` and `trace` count a sharded step's slots from there (F13), so average parallelism
  stays true.

`docs/log-format.md` gets these rows; its existing rows do not change. Tests: the unchanged fixtures in `tests/fixtures/` parse to the
same JSON; a fixture with every new line parses to the same steps as the same log without them.

## 7. Evidence: when a proof may accept a result it did not run

### 7.1 What stays

`proof.cached-unverified` keeps its meaning and its code: a step the proof log marks `STEP_CACHED` counts only when it is verified for
this sha on this host, and the proof is refused otherwise (`batch.rs:1191-1206`, lesson L69 at `docs/lessons.md:76`). Policy can
forbid cached steps altogether (`merge.allow_cached_steps = "never"`, `engine-and-governance.md:322`). A release proof stays a full run
with no hits; that is the adopter's rule today and becomes a policy value, not code.

### 7.2 The rule

A **step result** is: step name, input key, commit sha, host, host class, exit code 0, the run it came from, that run's stamp digest,
the time. The engine accepts a result for a proof of sha S on host H only when **all** of these hold. Each failing line is a refusal
with the one existing code and a `why`:

| # | condition | `why` |
|---|---|---|
| 1 | the result's sha is S | `evidence-sha` |
| 2 | the result's key equals the key the proof host computed for the step on S | `evidence-key` |
| 3 | the result's host is H, or the step is `portable` and the result's host class equals H's class and policy lists that host's role in `proof.accept_roles` (`engine-and-governance.md:326`); the source host's role is read from `[proof.host_roles]` (host name to role), built in E3 | `evidence-class` |
| 4 | the source run has a stamp (green, tree clean before and after) and its steps-file digest equals the proof's | `evidence-stamp` |
| 5 | in the source run every step the result's step `needs` was green or itself accepted (F14: `--only` can skip a need) | `evidence-partial` |
| 6 | the result is no older than `proof.max_age_hours` (`engine-and-governance.md:327`) | `evidence-stale` |
| 7 | the step has `inputs`; a step without them is never reused | `evidence-undeclared` |
| 8 | the step's key model is proven in shadow (section 7.4) | `evidence-unproven` |
| 9 | the engine finds the result in its own ledger, or `[chain] cache_verify_command` exits 0 for it (today's plug-in point, `batchcfg.rs:45-46`) | `evidence-missing` |

So a proof **may accept** a pre-check result when the pre-check ran on the same commit, with the same key, on the proof host (or a host
of the same class, for a portable step), in a stamped run of the same steps file. It **must re-run** a step when the commit moved, an
input or the key command's output changed, the step has no `inputs`, the result came from another class of host (a Windows workstation
and a Linux proof host), the source run was partial in a way that skipped a need, the result is old, or the policy says `never`.

Why condition 1 is there although the key already covers the inputs: `inputs` is a person's list. A step may read a file the list
forgot. Requiring the same commit means a forgotten input cannot differ. Reuse across commits on the key alone is allowed for
`--mode preflight` and `--mode lane` runs (a cheap filter that the proof re-establishes) and never for a proof.

### 7.3 The key and the host class

- **Key** = sha-256 over: a version tag, the step name, `run` or `argv` as written, `shards`, the working directory **relative to the
  repository top level**, `cache_salt`, the output of `git ls-files -s -- <inputs>`, and the stdout of `key_cmd` when set. The host is
  not in the key, so the same step on the same commit has the same key on two machines.
- **Host class** = sha-256 over: the OS family, the CPU architecture, and the version line of every tool the host lists in `tools`
  (the lines `doctor` already reads, `doctor.rs:239-253`), plus an optional `class_salt` host key. It is computed by `probe` and
  `doctor` and stored. Two hosts of one class are expected to give the same verdict for a portable step. The class is necessary, not
  sufficient; that is why `portable` is a statement per step and why shadow comes first.

How a result reaches the proof host: at launch (T4) the engine selects the results that pass conditions 1 and 3 to 8 from its ledger and
writes them as a small JSON file on the proof host through the runner (`runner.rs:63`, `runner.rs:260-267`), passed as `--evidence`.
`chain` computes each step's key on the proof host; on an equal key it prints `STEP_CACHED <step> <key> from <run-id>` and does not run
the step; on any difference it runs the step and says why in its `.full`. At T5 the engine checks every `STEP_CACHED` line again
against its own rows (condition 9). The proof host is never trusted to vouch for itself.

### 7.4 Shadow first

A key that says "same inputs" while the verdict differs is a silent wrong green. So:

- With `--cache-shadow`, every step with `inputs` runs, and the log says whether the cache would have hit and what the real exit code
  was. A would-hit whose real run is red is a **key-model defect**: the ledger records it, `report` prints it first, and the step's hits
  are off until a person changes `inputs`, `key_cmd` or `cache_salt` (which changes the key model).
- A step's results become acceptable as proof evidence after `proof.shadow_min` (default 3) would-hits with no defect. Pre-flight and
  lane runs may use hits from the first day; they are filters.
- The test that proves the guard: a fixture step that reads a file outside its `inputs`; change the file; shadow must report the defect.

### 7.5 What it saves

**Estimate.** With the six duplicated steps accepted from the pre-check on the proof host, a chain like B loses at least 3997 s of
busy time (test-api and test-tools; the other four are unmeasured). Its longest step becomes cargo-test (1845 s) and its packing bound
(59880 - 3997) / 12 = 4657 s. Nothing is saved until the proof log comes from a run that honours `--evidence`: a `gatetrain chain`
run, or one embedded in the adopter's chain script (section 5.3).

## 8. Batch: stack, pre-flight, prove, merge, and the pipelining

The line of states stays (`engine-and-governance.md:42-54`). Changes:

| change | from | to |
|---|---|---|
| pre-flight is launched, then observed | T3 runs a command and blocks (`batch.rs:1025-1036`) | T3 launches a `chain --mode preflight` run under a holder on a light host and records the run; a new state `preflighting`; T3o reads its log and stamp like T5 reads a proof. State is derived from events (`batch.rs:132-170`), so a new state needs no migration. |
| a light run respects a proving host | role `light` ignores the lock (F6) | a light run is refused on a host whose proof lock is held (`host.refuse-proof-running`), unless that host sets `light_while_proving = true`; its memory admission uses the pre-flight's own steps, not the chain's |
| the next batch does not wait | a batch is stacked on the protected branch | `batch new --base <batch-id>` stacks on the tip of a batch that is `preflighted`, `proving` or `proven`. The base check already exists (`batch.rs:812-816`). |
| a red is read | findings are step and exit code (`batch.rs:1176`) | T5r runs `diagnose` on the log and the `.full` files and stores findings and causes (section 11, slice E4) |
| a retry is gated | every red is refused (`batch.rs:651`) | branch `feat/batch-governance`, section 12 |

**The pipelining, step by step.** Batch N is `proving` on the proof host. Batch N+1 is opened on N's tip, stacked, settled and
pre-flighted on the light host. When N is `proven` it merges by fast-forward, so the protected branch **is** N's tip and N+1's base is
unchanged: its stamp is still for its sha and its proof starts at once. If N goes red and is patched, N's tip moves; N+1 is restacked on
the new tip (a new sha), and its pre-flight reuses, by key, every step whose inputs did not change (allowed for a pre-flight, section
7.2). Rules:

- N+1 may not start **proving** before N is `proven`, unless policy says `[pipeline] prove_ahead = true` (a proof on an unproven base
  may be wasted; that is a budget decision).
- N+1 may not **merge** before its base is on the protected branch. **Correction (built in E5):** the fast-forward check does not refuse this. N+1 descends from N's tip, so a fast-forward to N+1's tip would carry N's commits unproven. The refusal is a new guard at T7, `pipeline.base-not-merged`: the base batch's tip must already be an ancestor of the protected branch. (The earlier text cited `batch.rs:883`; that line is the pre-flight stamp check and has no bearing on this.)
- Depth is limited: `[pipeline] max_ahead = 1` by default.

**Estimate of what it saves.** One pre-flight wall time per batch in elapsed time (56m38s today, less after slice E0), because it
overlaps the proof of the batch before; and the idle gap between "N merged" and "N+1 starts proving" goes to one tick.

**Stopping early on one cause.** A chain collects all reds (`docs/method.md:27-28`). Two mechanisms cover "one cause, 440 reds":
a cheap fast-tier step that checks the shared thing (for the measured case: "the regenerated baseline parses"), which every dependent
step `needs`, so they are skipped in minutes instead of failing for 1h40m; and, at each tick, `diagnose` over the running log: when
one cause explains at least `[chain] storm_reds` (default 20) reds the tick reports it and names the stop command. It stops the run
itself only when policy sets `[chain] stop_on_storm = true`. A stopped run is `stopped`, not red (`batch.rs:1162-1164`).

## 9. Multi-host

### 9.1 Shape

The control machine holds the ledger and runs the tick (`engine-and-governance.md:33-34`). A host is reached through a runner.

```text
light host (workstation, a Linux subsystem, a second server)      proof host
  chain --mode preflight   fast tier, then slow tier of the           hold -- chain --mode full --evidence e.json
  pre-check steps file     -> stamp, results in the ledger            every step of the full steps file:
                                                                         accepted result  -> STEP_CACHED (verified at T5)
                                                                         anything else    -> runs here
                                                                      -> one log, CHAIN_MODE full, one proof stamp with prev
```

It is one logical run: one proof log on the proof host, in which the fast tier proven on the light host appears as verified hits, and
one stamp chain (section 5.4). When the light host is of another class, or a step is not `portable`, nothing breaks: the step runs on
the proof host and the light run was a filter.

### 9.2 What the engine needs per host

| need | Linux | macOS | Windows | Linux subsystem on Windows |
|---|---|---|---|---|
| runner | `local`, `ssh` (exists, `runner.rs:207-271`) | `local`, `ssh` | `local` (exists). `ssh` to a Windows host: not supported, the ssh runner quotes for a POSIX shell (`runner.rs:55-57`) | new `reach = "wsl <distro>"`: `wsl.exe -d <distro> -- sh -c ...` |
| the binary on the host | `doctor` gains a check: `gatetrain --version` on the host equals the control machine's | same | same | same |
| the commit on the host | a plug-in command `[hosts.sync] command` (for example a `git push` to a remote the host fetches); before launch the engine reads `git rev-parse HEAD` there and refuses on a difference (`proof.sha-not-on-host`) | same | same | same |
| a run that outlives the caller | `hold --detach`: the holder starts itself in a new session (`setsid`), prints its pid after the `HOLDS` line, and returns | same call (**unverified** on macOS) | new process group and detached-process flags (**unverified**) | not possible: a detached job dies when the last `wsl.exe` exits (lesson L23, `docs/lessons.md:31`). The run is attached: the command that launched it stays until it ends. |
| reading the log | one bounded read per tick per run through the runner, into the control machine's state folder; T5 then reads the local copy as today (`batch.rs:1152-1157`). Poller cap as in L25 (`docs/lessons.md:33`). | same | local file | same |
| may be a proof host | yes | not until boot id, memory total and `stop` have tests (F10) | local only, not until `stop` works there | yes, attached, when its class equals the merge target's |

`hold --detach` replaces the three launch methods of the earlier design (`engine-and-governance.md:158`) with one that needs no `at`,
no `systemd-run` and no `setsid` binary. The holder forwards a hang-up to its child (root `Cargo.toml`, the `ctrlc` line), so a holder
started plainly over ssh ends with the session; the new session is what prevents that. Nothing survives a user-wide kill.

`os = "macos"` is added to the allowed values (F10). Until then a macOS host is read as Linux and its `/proc`-based checks fail as
"could not run"; **unverified**, no macOS host was available.

### 9.3 Cloud hosts, by profile and budget

A cloud host is a host with a provision plug-in, not a new runner kind:

```toml
[[hosts]]
name = "cloud-runner"
kind = "cloud"
roles = ["scout"]
cost_per_hour_usd = 1.20
provision_up   = ["tools/cloud-up"]      # prints the ssh target on stdout; must be safe to run twice
provision_down = ["tools/cloud-down"]
```

The engine may start one only when the profile allows paid resources (`config/hosts.example.toml:3-7`) and
`cost_per_hour_usd x predicted hours` (the ledger's median wall time for that steps file on that host class, or `unknown`, which
refuses) fits what is left of `monthly_cap_usd` after this month's `runs.cost_usd` (`ledger/schema.sql:14`). Refusal: `budget.cloud-cap`.
`provision_down` runs when the run ends, and again from every tick while a row says the host is up with no live run. A cloud host starts
as `scout` or light; it becomes proof evidence only through the class rule of section 7. Last slice; nothing else depends on it.

## 10. Hardware: probe, sample, advise

### 10.1 `gatetrain probe [--config hosts.toml] [--host NAME] [--path P] [--budget-s 30] [--discover]`

A short benchmark, stored in the ledger. Per host: logical cores, total memory, OS and architecture, whether it is a virtual guest, and
a CPU figure (sha-256 over a fixed 64 MB buffer on one thread, then on all threads: MB/s each; 5 s at most; `sha2` is already a
dependency). Per configured disk (`[[hosts.disks]]`), in a folder `.gatetrain-probe-<pid>` under the disk's path:

| measurement | how | unit |
|---|---|---|
| durable small writes | 200 writes of 4 KB, each followed by `sync_data` | p50 and p95 latency, ms |
| sequential write | 256 MB in 1 MB blocks, one `sync_all` at the end | MB/s |
| small files | create, write 4 KB, close, then delete 2000 files | files/s |
| rotational | read from the OS, never inferred from the numbers (10.4) | 1, 0, or unknown |
| free and total space | as `doctor` reads it (`doctor.rs:76-89`) | GB |

The durable small write is the figure that separates a spinning disk from a solid-state one without bypassing the page cache, and it
needs only the standard library.

Bounds and refusals:

- **Never during a proof.** Refused (`probe.host-busy`, exit 75) while the host lock is held (`hold::status`, `hold.rs:180`) or the
  ledger shows a live run on the host. There is no override flag.
- At most `--budget-s` seconds and 512 MB per disk; a measurement that runs out is `unknown: budget`, the others stand.
- Refused per disk when free space is below `min_free_gb` + 1 GB (`probe.disk-low`), when the path is a link, or when it fails the
  path rules `tidy` uses (L62, `docs/lessons.md:69`). The probe folder is removed at the end and by the next probe if a crash left one.
- Only configured disks are written to. `--discover` lists mounted filesystems that are not in `hosts.toml` (size, type, rotational
  flag) and writes nothing. `--path P` probes one named path on a person's word.

`doctor` then compares: a disk with `fast = true` whose p95 durable write is more than `[probe] slow_factor` (default 5) times the best
disk on the host gets a WARN `disk-speed-measured` naming both figures. The flag is not changed.

### 10.2 The sampler

Every `--samples` seconds (default 10) while a run is alive, one JSON line is appended to `<log>.samples.jsonl` (under `hold`) or
`<out>/samples.jsonl` (`chain` alone). The file is plain, append-only and read by `ingest` with the log; a host without the ledger
loses nothing. The holder already has the loop (`holdcmd.rs:246-259`); `chain` waits on a channel with a timeout (`chain.rs:284`) and
samples on the same wake-up. A run like B gives about 894 lines, under 200 KB.

Each line: time, host, run id, steps running now, and the figures of 10.4. A figure the platform does not give is `null` with a reason
in a `"unknown"` object in the first line of the file.

### 10.3 `gatetrain advise [--db FILE] [--config hosts.toml] [--host NAME] [--json]`

Rules as data in code, each a row: an id, the evidence it needs, the move, how minutes are estimated, the confidence. Output per
advice: the move as a config line to change, the evidence (run ids, probe times, the sample window, the figures), minutes saved per
chain or `unknown`, and a confidence.

| rule | fires when (all from ledger rows) | move | minutes |
|---|---|---|---|
| `advise.target-to-fast-disk` | a build folder (`[chain] target_dirs`, `batchcfg.rs:50-51`) sits on disk D; another disk E on the host has room for the folder's median size plus its `min_free_gb`; the probe's p50 durable write on E is at most a third of D's; and during the last 3 full runs D was busy at least 80% in at least 20% of samples, or the I/O stall was at least 10% | point that build folder at E | stall formula below |
| `advise.add-device` | the same I/O evidence, and no disk on the host qualifies as E | add a solid-state disk of at least (median folder size + `min_free_gb`), `use = ["chain"]` | stall formula with the best probed disk of any host as the reference; `unknown` when no such probe exists |
| `advise.new-device` | `probe --discover` saw a mounted filesystem that is not in `hosts.toml` | declare it and probe it; `target-to-fast-disk` then gives the minutes | none |
| `advise.plan-shape` | average parallelism is under 75% of the allowed one while CPU busy p50 is under 60%, no disk is busy over 70% and memory is free: the host has room, the plan is serial | name the stretches with the fewest running steps and the `needs`, tier barrier or exclusive step behind each | wall minus max(longest step, busy / allowed) |
| `advise.par-down` | CPU busy p50 is at least 95%, or CPU stall is at least 20%, and steps run longer than their median from runs at lower parallelism | lower `max_parallel_steps` by the stated amount | from the regression of step time on parallelism when at least 3 runs at different values exist; else `unknown` |
| `advise.mem` | free memory p05 is under `memory_reserve_gb`, or a finding of class `infra-memory` exists | lower parallelism or `CARGO_BUILD_JOBS` to the value `memory::parallelism` gives (`memory.rs:163-168`) | `unknown` |
| `advise.shard-step` | a step is the longest of the run and at least 15% of wall, and average parallelism is under the allowed one | `shards = N`, N = min(4, free slots) | step time x (1 - 1/N), capped at (step time - next longest step) |
| `advise.reuse` | the same step ran green in a pre-flight and in a proof on one sha and one host class, and has no `inputs` | declare `inputs` | the step's busy time; wall saving only if it was the longest step |
| `advise.move-step` | a step runs on the proof host; another enabled host of the same class is free during proofs and its CPU probe is no slower; the step would be `portable` | mark it `portable` and run it in the pre-flight on that host | as `advise.reuse` |
| `advise.split-step` | a `run` string contains `&&` or `;` | split into steps with `needs` | none; it prevents a hidden red |
| `advise.needs-gate` | one cause explained at least 20 reds in a run | add a cheap fast-tier step for the shared thing and `needs` on it | the time between the first of those reds and the end of the run |
| `advise.flag-contradicts-probe` | `fast = true` on a disk the probe measured slower than a disk without it | correct the flag | none |

**The stall formula.** Linux reports, as a share of wall time, how long at least one task (`some`) and how long all tasks (`full`) were
stalled on I/O. For a run of wall time W and probe latencies `L_new` and `L_old` (p50 durable write):

```text
saved_low  = W x full_share x (1 - L_new / L_old)
saved_high = W x some_share x (1 - L_new / L_old)
```

Both are printed. Where the platform gives no stall share (10.4), the advice is still printed with its busy-disk evidence and
`minutes: unknown (this platform reports no I/O stall)`.

**Confidence.** `high`: the ledger holds runs before and after the same move on this host and the measured change is within the printed
range. `medium`: at least 3 runs agree and the probes are under 30 days old. `low`: fewer runs or older probes. An advice with any
`unknown` input is at most `low`.

**Closing the loop.** `advise` records each advice with the hash of the config files it read. When a later run on that host has another
config hash and the moved thing changed, `advise` writes a check row: predicted range, measured change, verdict. That is the only way an
advice reaches `high`, and it is how a wrong rule is found.

For chain B today `advise` could not choose between `advise.plan-shape` (the host has room, the plan is serial) and
`advise.par-down` (the host is full at 6.7), because no sample exists. That is the point of the order in section 14: sample first.

### 10.4 What is measured on each platform, and what is unknown

"measured" names the source. "unknown" is stored as NULL with the reason; no rule turns it into a number.

| figure | Linux | Linux subsystem on Windows | macOS (**unverified**, no host) | Windows |
|---|---|---|---|---|
| load, 1 min | `/proc/loadavg` | same (the guest's) | `sysctl -n vm.loadavg` | **unknown**: the OS has no load average |
| CPU busy % | `/proc/stat` deltas | same | **unknown** in the first version | `GetSystemTimes` deltas |
| I/O wait % | `/proc/stat` `iowait` | same (the virtual disk) | **unknown**: not exposed | **unknown**: not exposed |
| I/O, CPU, memory stall % | `/proc/pressure/*` when the kernel has it, else **unknown** | same | **unknown** | **unknown** |
| disk busy % | `/proc/diskstats`, time doing I/O, per device | same, for the virtual disk only | **unknown**; throughput from `iostat -Id` deltas | `% Idle Time` of the physical disk through the performance-counter API with English counter names (**unverified**); else **unknown** |
| free memory | `MemAvailable` in `/proc/meminfo` | same (inside the VM cap) | `vm_stat`, marked as an estimate | `GlobalMemoryStatusEx` (the call `memory.rs:200-219` already makes) |
| total memory | `/proc/meminfo` (`memory.rs:195-198`) | same | `sysctl -n hw.memsize` | exists |
| rotational | `/sys/block/<dev>/queue/rotational` of the device under the mount, through `slaves/` for stacked devices; mixed or unresolved: **unknown** | **unknown**: the disk is a virtual file on a Windows disk; ask the Windows host | `diskutil info -plist`, key `SolidState`; absent: **unknown** | `Get-PhysicalDisk` media type; `Unspecified`, a storage pool or a RAID volume: **unknown** |
| network filesystem | type from `/proc/self/mountinfo`; rotational **unknown**; probed only with `--path` | same (`/mnt/c` is one) | `mount` | drive type |
| probe write latencies | measured | measured (they include the virtual-disk layer; say so in the row) | measured | measured |

## 11. Data model

The ledger was at version 3 when this was written (`ledger.rs:103`); E1 and E2 have since raised it to 5, and a v3 file is brought forward by `gatetrain ledger migrate` (or by any `--db` write): the base schema, run snapshots and decisions, step memory, batch events
(`ledger.rs:13-100`). Migration is one transaction of `if from < N` blocks and refuses a file newer than the binary
(`ledger.rs:121-128`, `ledger.rs:139-161`). That stays. Every new table is insert-only with the two triggers the existing ones carry
(`ledger.rs:29-32`). Columns are only ever added. NULL means unknown.

One version per slice, so each slice ships alone. **The numbers are assigned here and a lane takes the next free one at stack time**;
`CURRENT_VERSION` is a hub two lanes must not both raise.

| version | slice | adds |
|---|---|---|
| 4 | E1 (built) | columns on `runs`: `mode`, `par`, `plan_digest`, `log_sha256`, `config_hash`, `host_class`, `gatetrain_version`. Columns on `steps`: `tier`, `input_key`, `cached_from`, `shards`. Tables `step_skips`, `stamps`, `stamp_links`. |
| 5 | E2 (built) | table `step_shards` |
| 6 | E4 | tables `findings`, `causes` |
| 7 | E3 | tables `step_results`, `proof_evidence`, `cache_shadow` |
| 8 | E6 | tables `host_probes`, `disk_probes` |
| 9 | E7 | columns on `host_samples`; table `disk_samples`; append-only triggers for both |
| 10 | E8 | tables `advice`, `advice_checks` |

| table | one row per | key | other columns |
|---|---|---|---|
| `step_skips` | step that never ran | (`run_id`, `step`) | `reason` |
| `step_shards` | shard of a step of a run | (`run_id`, `step`, `shard`) | `of`, `started_at`, `duration_sec`, `exit_code`, `log_ref` |
| `stamps` | stamp file | `digest` | `kind`, `sha`, `host`, `run_id`, `plan_digest`, `at`, `body` (the file's text) |
| `stamp_links` | `prev` entry | (`digest`, `prev_digest`) | |
| `step_results` | green step with a key, per run | (`run_id`, `step`) | `input_key`, `sha`, `host`, `host_class`, `stamp_digest`, `at`; index on (`step`, `input_key`, `sha`) |
| `proof_evidence` | accepted hit of a proof | (`proof_run`, `step`) | `source_run`, `input_key`, `source_host`, `verified_by` (`ledger` or `command`) |
| `cache_shadow` | shadow line | (`run_id`, `step`) | `input_key`, `would_hit`, `exit_code`; a would-hit with a non-zero exit is the key-model defect |
| `findings` | failure block of a red step | (`run_id`, `step`, `block_no`) | `cls`, `rule`, `evidence`, `rerun`, `cause_id` |
| `causes` | cause in a run | (`run_id`, `cause_id`) | `fingerprint`, `cls`, `rule`, `evidence`, `steps` (count) |
| `host_probes` | probe of a host | (`host`, `at`) | `os`, `arch`, `virt`, `cores`, `mem_total_mb`, `cpu_one_mbps`, `cpu_all_mbps`, `host_class`, `tool_versions` (JSON), `duration_ms`, `unknown` (JSON: field to reason) |
| `disk_probes` | probe of a disk | (`host`, `at`, `path`) | `device`, `fs_type`, `rotational`, `rotational_source`, `fsync_p50_ms`, `fsync_p95_ms`, `seq_write_mbps`, `smallfile_per_s`, `free_gb`, `total_gb`, `bytes_written`, `duration_ms`, `unknown` |
| `host_samples` (exists, `ledger/schema.sql:45-53`) | sample | (`host`, `at`) | added: `run_id`, `running_steps`, `cpu_busy_pct`, `iowait_pct`, `mem_free_mb`, `stall_io_some`, `stall_io_full`, `stall_cpu_some`, `stall_mem_some` |
| `disk_samples` | disk in a sample | (`host`, `at`, `device`) | `path`, `busy_pct`, `read_mbps`, `write_mbps`, `free_gb` |
| `advice` | advice printed | `advice_id` | `at`, `rule`, `host`, `subject`, `move` (JSON), `minutes_low`, `minutes_high`, `confidence`, `evidence` (JSON), `config_hash` |
| `advice_checks` | later comparison | (`advice_id`, `run_id`) | `at`, `measured_minutes`, `verdict` |

A cause's `fingerprint` is the sha-256 of its class, its rule id and its evidence line with digits, hex runs and quoted paths removed,
so the same failure in two steps (another thread id, another duration) is one cause. The full-chain budget and the retry gate need no
table: they fold `batch_events` and read `causes`.

Migration tests, as for version 3 (`docs/lessons.md:79`): a file at each of versions 0 to 9 migrates to 10 with its rows unchanged; a
file at 11 is refused; an UPDATE or DELETE on each new table aborts.

## 12. Governance: the rules as rows

Every row is a guard in the engine with a stable refusal code. A refusal is appended to the decision log, the batch stays where it is
and the command exits 3 (`engine-and-governance.md:503`), or with the host exit codes 75 to 79 (`batch.rs:186-193`). "exists" means the
code is at `88bf73b`; "branch" means `feat/batch-governance`, which has no commit yet, so its codes are proposed here.

| code | rule | checked at | state |
|---|---|---|---|
| `floor.zero-red` | a merge needs zero red steps in the proof | T5, T7 (`batch.rs:651`, `:1175-1178`) | exists |
| `floor.full-mode` | only a `full` run is a proof | T5 (`batch.rs:1166-1168`) | exists |
| `floor.proven-sha` | the log's commit is the recorded tested sha | T5 (`batch.rs:1179-1182`) | exists |
| `merge.tested-equals-merged` | the tested sha is the tip that is merged | T7 (`batch.rs:876-878`) | exists |
| `chain.tree-differs` | after the chain only generated files may differ | T5, T7 (`batch.rs:1185-1190`) | exists |
| `floor.clean-tree`, `settle.tip-moved`, `preflight.stamp-for-sha` | the chain starts on the settled, stamped, clean tip | T4 (`batch.rs:850-862`) | exists |
| `proof.cached-unverified` | a cached step counts only when verified; `why` names the failing condition of section 7.2 | T5 (`batch.rs:1191-1206`) | exists, extended in E3 |
| `lane.stamp` | a lane is stacked only when a green stamp exists for its current tip from the configured lane steps file (`why`: `missing`, `red`, `stale-tip`, `other-plan`) | T1 | branch |
| `retry.cause-unchanged` | after a red proof, a new proof of the batch is refused until the tip moved, or every cause of the red run carries a recorded note of class flake or infra | T4 | branch; needs `causes` |
| `budget.full-chains` | at most `full_chains_per_batch` proof launches per batch (`engine-and-governance.md:263`); then the batch is held | T4 | branch |
| `host.refuse-proof-running` | no pre-flight, lane run or probe on a host whose proof lock is held (lesson L26) | T3 admission, `probe` | new, E5 |
| `proof.sha-not-on-host` | the proof host's checkout is at the tested sha before launch | T4 | new, E9 |
| `pipeline.base-unproven` | a batch stacked on another batch does not start proving before its base is proven, unless policy allows | T4 | new, E5 |
| `chain.mode-partial` | `--mode full` with `--only` or `--skip` is not a full run | `chain`, exit 2 | new, E1 |
| `cache.key-model-defect` | hits are off for a step whose shadow showed a would-hit with a red run, until its key model changes | `chain`, T4 selection | new, E3 |
| `probe.host-busy`, `probe.disk-low` | section 10.1 | `probe` | new, E6 |
| `advise.read-only` | `advise` and the tick's advice line change no config file, no host and no batch | by construction: there is no apply path; a test hashes every config file and the batch events before and after | new, E8 |
| `budget.cloud-cap` | no paid host beyond the monthly cap, and none on an unknown prediction | T3, T4 | new, E11 |
| `floor.red-default-real` | a red no rule explains is `real` | `diagnose` (`diagnose.rs:294-297`) | exists |

The floor of the earlier design is unchanged (`engine-and-governance.md:426-438`). Two rows are added to "never automatic"
(`engine-and-governance.md:291-301`): applying an advice, and running a probe on a host that is proving.

Each rule gets the test the method asks for: a fixture that violates it and is refused with exactly that code, and the same fixture
without the violation passing (`docs/lessons.md:44`, L36).

## 13. The glue that goes

The adopter's scripts were read in its repository. Three named in the brief are **not there**: the batch-finish, chain-holder and
auto-merge scripts. A search of the scripts folder and of the tracked files for those names finds nothing; their host copies were not
read. Their rows below are from their names and from the earlier design's description of such scripts
(`engine-and-governance.md:720-723`): **unverified**.

| script (lines) | what it does | becomes | slice |
|---|---|---|---|
| `prechain-checks.sh` (101) | 27 checks in a serial loop, a stamp, `--only` / `--skip`, a required-list | **gone.** A steps file of 27 `[[step]]` rows, fast and slow, compound checks split; `gatetrain chain` writes the stamp; the steps-file digest replaces the required-list ("a check added later makes older stamps too old" is a digest mismatch). Its three helper functions become three small commands in the adopter's repository. Its first check, the regeneration, becomes T2 (`batch.rs:988-1024`). | E0 |
| `chain-stamp-gate.sh` (21), `preflight-stamp.cjs` | refuse a chain without a green stamp for the sha | **gone**: `preflight.stamp-for-sha` (`batch.rs:855-857`) | E5 |
| the chain-holder script (**unverified**) | hold a lock while the chain runs | **gone**: `gatetrain hold` (`hold.rs:1-16`) | built |
| `chain-start.sh` (63) | start one chain detached, refuse a second, copy scripts to the build folder | **gone**: T4 under `hold --detach`; "is one running" is the lock (`README.md:113`); nothing is copied because the steps file is in the tested tree | E9 |
| the remote-run script (33) | push the branch, copy scripts, start the chain over ssh, fetch the regenerated files | **gone**: `[hosts.sync] command` plus the ssh runner; the generated delta is already committed at the merge (`batch.rs:1058-1083`) | E9 |
| the batch-finish script (**unverified**) | chain regeneration, commit, pre-check and launch | **gone**: `batch advance`, T2 to T4 | E5 |
| the auto-merge scripts (**unverified**) | wait for the chain, then merge | **gone**: the tick advances `proving` to `merged`; nothing is armed (`engine-and-governance.md:629-631`) | E10 |
| `stack-batch.sh` (290) | stack lanes from their own commits, rebuild hub files, resolve known conflicts, then pre-launch checks, regeneration and the pre-check | **shrinks**: first the whole script behind `[stack] command` (`batch.rs:934-958`); its last third (checks, regeneration, pre-check) is deleted because T2 and T3 do it; its hub rebuilds move to `[stack] hubs` (`batchcfg.rs:17-23`) one file type at a time. The repository-specific resolvers stay. | E5, then later |
| `merge-batch.sh` (140) | apply the chain's regenerated files, rebase over the protected branch, check, fast-forward, push | **shrinks**: the fast-forward, the generated delta and the checks are T7 (`batch.rs:1055-1113`). With batches stacked on each other and merged by fast-forward, the rebase is needed only when the protected branch moved outside a batch; that part stays a plug-in until the `patch-id` mode exists (`engine-and-governance.md:452-454`). | E5 |
| the chain script (268), `par-gates.sh` (170), the port grouping tool | about 440 steps: cheap checks, three large suites side by side, database gates N at a time grouped by port, one gate alone at the end | **a steps file**, produced by `[chain] steps_command` (the gate list is generated): `needs`, `excl` for shared ports, `slots` for the large suites, `shards` | E12 |
| `step-cache.sh` (520) and its class file | a step-result cache with a wide key and a shadow mode | **gone** once `inputs`, `key_cmd` and shadow cover its classes; until then it stays the plug-in behind `cache_verify_command` | E3, E12 |
| `redonly.sh` (37), `rerun-verdict.sh` (33) | rerun only the red steps on the same sha; read two green reruns as green | **gone** with T6a (`engine-and-governance.md:70`, `:266-279`), gated by causes | after E4 |
| `stamp-batch.sh` (32) | earn the release stamp from the chain log | **shrinks** to reading the proof stamp chain | E3 |
| the host tidy scripts | prune build outputs and leftover containers after a chain | **gone**: `tidy --auto` (`README.md:304`) | built |
| the hub merge tools | structural merges of the adopter's own files | **stay**, as `rebuild` commands | - |

Rule for what replaces glue: a command with an exit code in a config file (`engine-and-governance.md:610-611`). No script calls
`gatetrain` in a loop, and `gatetrain` generates no script for a person to keep.

## 14. Build order

Small slices; each has its tests written and seen red first, ships alone, and names what to measure afterwards. Two tracks can run at
once because they touch different files: **T** (time: chain and batch) and **H** (hardware). The three branches in progress are slices
D, C and G.

| # | track | slice | ships | tests, red first | saves, against section 1 | measure after |
|---|---|---|---|---|---|---|
| E0 | T | **the pre-check as a steps file** (adopter config) plus `step_tmp` | the adopter's 27 checks as `[[step]]` rows, compound checks split; `step_tmp` in `chain` | two steps writing one file name under `TMPDIR` collide without `step_tmp` and not with it; a compound fixture hides a red, the split one shows both | wall from 3398 s towards max(longest check, 3398 / par): 425 s at par 8 if no check is longer (**estimate**; per-check times in this setting are unmeasured). A red is printed when it happens instead of up to 31 minutes later. Removes the hidden-red and scratch-race rounds of batch A. | wall and busy of 3 pre-checks; seconds from a red's end to its `RED_NOW`; rounds per batch |
| D | T | **`feat/diagnose-causes`** | reds grouped by cause, better evidence | on the branch (5 tests) | 440 reds read as 1 cause | causes per red run; share of reds explained by the top cause |
| E1 (built) | T | **`chain` feeds the ledger**; `--mode`, `--host`, `--db`; ledger 4; `diagnose --out DIR` reads `.full` files | F1, F2, F3, F9 closed | a green and a red `chain --db` run leave runs, steps, skips and a stamp row; `--mode full --only x` exits 2; a red `chain` step is classified from its `.full` by a fixture rule; migration 3 to 4 from each older version | nothing by itself: it makes every later number come from the tool | runs in the ledger per day, with zero hand ingests |
| C | T | **`feat/chain-cache`** | `inputs`, the key, `--cache`, `STEP_CACHED` | on the branch (10 tests) | repeat pre-checks and lane runs skip unchanged steps | hits and misses per run; minutes not run |
| E2 (built) | T | **shards**; ledger 5 | `shards`, `{shard}`, `STEP_SHARD` lines, `step_shards`, `shard_check` | a 4-shard fixture prints one `STEP_EXIT`; one red shard gives one `RED_NOW` and a red step; old fixtures parse to the same JSON; a missing shard exit is exit 125; `{shard}` without `shards` is a config error | test-api 2484 s towards 621 s at 4 shards (**estimate**: even split, no added contention). The chain's longest step becomes cargo-test, 1845 s. | the step's wall and its summed shard time; the run's wall; average parallelism from `step_shards` |
| E4 | T | **findings and causes in the ledger**; ledger 6 | T5r runs `diagnose` over the log and the `.full` files and stores findings and causes; `batch status` prints the causes | a red fixture run leaves one cause row for three steps with one message; a rerun of the fold gives the same fingerprints; an unmatched red is stored as `real` | the read of a red run: 1 line per cause instead of 1 per step | causes per red run in the ledger, with zero hand reads of a log |
| G | T | **`feat/batch-governance`** | `lane.stamp`, `retry.cause-unchanged`, `budget.full-chains` | per rule: a violating fixture refused with its code | a red lane no longer costs a batch round; a blind rerun (a full chain, 2h29m) is refused | refusals per code; full chains per merged batch |
| E3 (built, batch side) | T | **evidence**: `key_cmd`, `--cache-shadow`, `--evidence`, `--embedded`, repository-relative working directory in the key; engine verification; ledger 7 | section 7 | one test per row of 7.2, each refused with `proof.cached-unverified` and its `why`; the shadow defect fixture; an embedded run prints no `GATES_DONE`; a hit not in the ledger is refused | at least 3997 s of busy time per chain; packing bound 4657 s (**estimate**) | busy time of a proof; hits accepted and refused by `why`; shadow defects (must be 0) |
| E5 (built) | T | **the pre-flight as a launched run; pipelining** | state `preflighting`, light admission, `--base <batch>`, one stamp format | a crash after launch resumes by adopting the run; a pre-flight on a proving host is refused; batch N+1 on N's tip proves at once after N's fast-forward; N+1 is restacked when N's tip moves; N+1 does not prove before N is proven | one pre-flight wall per batch in elapsed time; the gap between a merge and the next proof goes to one tick | minutes from "N merged" to "N+1 proving"; merged batches per day |
| E6 | H | **`probe`**; ledger 8; `doctor` compares | section 10.1 | refused while a lock is held; stops at its budget and reports `unknown: budget`; leaves no file; a link path is refused; a fake `/sys` tree gives rotational 1, 0 and unknown; a flag that contradicts a probe is a WARN | nothing by itself; it replaces a typed flag by a figure | probe rows per host; the latency ratio between the proof host's disks |
| E7 | H | **the sampler**; ledger 9 | section 10.2 | fixture `/proc` files give the expected figures; a platform without a source writes null and the reason; the file is ingested with its log; no process is left after the run | nothing by itself; it answers whether the proof host is full at 6.7 or the plan is serial | per run: CPU busy p50, I/O stall, the busiest disk, free memory p05 |
| E8 | H | **`advise`**; ledger 10 | section 10.3 | per rule: a ledger fixture that fires it and one that does not; an unknown input gives `minutes: unknown`; config hashes and batch events are equal before and after | per advice, as printed. For B, `plan-shape` bounds the gain at 3950 s if the host has room | each advice's predicted range against the measured change after the move |
| E9 | T | **multi-host**: `hold --detach`, `reach = "wsl"`, `[hosts.sync]`, the version check, `portable`, host class, `os = "macos"` | section 9 | a fake runner transcript for launch, log read and sha check; a result from another class is refused (`evidence-class`); the detached holder survives its parent | the fast tier of a proof never runs on the proof host when the light host is of its class | busy time on the proof host per proof; where each step ran |
| E10 | T | **`tick`** | one pass; the cause-storm line | a fixture of two batches and three hosts gives the same 3 to 6 lines; two ticks cannot overlap (the ledger lock) | no armed merger, no polling agent | ticks per day; transitions per tick; minutes a batch waits on a tick |
| E12 | T | **the chain itself as a steps file**: `steps_command`, `excl`, `slots` | the adopter's chain script and gate runner retired | two steps with one `excl` key never overlap in the log; `slots` are counted against `--par` | whatever `advise.plan-shape` shows after E7; up to 3950 s | wall, busy, average parallelism of 3 full chains against B |
| E11 | T | **cloud by profile and budget** | section 9.3 | a cap that is spent refuses; an unknown prediction refuses; `provision_down` runs after a crash | nothing on a zero-cost profile | cost per batch |

Order within T: E0, then D, E1, C in any order, then E2, E4, G, E3, E5, E9, E10, E12, E11. Track H: E6, E7, E8, at any time after E1
(they need the ledger version order of section 11).

### Deviations from this design, as built (2026-11-17, `main` at `22b71d2`)

- **The old pre-flight keys and file stay for now.** E5 adds `[preflight] chain_command`, `steps_file` and `stamp_dir` (a launched pre-flight in state `preflighting`), but the earlier `[preflight] command` and the engine's own `<sha>.stamp` are not retired yet; section 5.4 said slice E5 would retire the stamp file. Both forms work: a chain stamp for the tip with the steps file's digest, or the old stamp.
- **Condition 3 of section 7.2** reads the source host's role from `[proof.host_roles]` (host name to role), not from the host list.
- **The guard `pipeline.base-not-merged`** is at T7 (section 8, correction).
- **`--run-id`** is a flag of `chain` with a default, `chain-<UTC time>-<commit>`; section 5.3 had not listed it.
- **E3 is built on the batch side only.** The new keys are `[merge] allow_cached_steps` (`verified` or `never`), `[proof] accept_roles`, `host_roles`, `max_age_hours`, `shadow_min`, `host`, `host_class`, `[chain] steps_file` and `[evidence] stamp_dirs`. The `chain` flags `--cache-shadow`, `--evidence` and `--embedded`, and the step key `key_cmd` of section 5, are not built.

### Conflicts with the branches in progress

Read on 2026-11-17: `feat/diagnose-causes` has one commit (`1214887`, tests in `diagnose.rs`), `feat/chain-cache` has one commit
(`83f1eef`, tests in `crates/gatetrain-cli/tests/chain.rs`), `feat/batch-governance` has none.

| # | branch | conflict or requirement | what to do |
|---|---|---|---|
| X1 | `feat/chain-cache` | The key includes the working directory. If it is the absolute path, a pre-check in the batch worktree and a proof in the chain worktree never share a key, even on one host, and two hosts never do. The branch's tests use one folder for both runs, so they cannot show it. | Hash the directory relative to the repository top level. Add a test: two worktrees of one repository at one commit give one key. |
| X2 | `feat/chain-cache` | The key has no place for what lies outside git (tool versions, a container image). The adopter's cache keys on those. | `key_cmd` (slice E3). Until then such a step must leave `inputs` out, so it always runs. |
| X3 | `feat/chain-cache` | A hit in a proof log is refused by the engine as it stands (`batch.rs:1194-1196`), so the cache saves nothing in a proof until E3. This is correct and should stay. | Say so in the branch's module page: the cache is for pre-flight and lane runs until evidence lands. |
| X4 | `feat/chain-cache` | The cache entry's content is not fixed by the tests (they count entries). Evidence needs: key, sha, host, run id, stamp digest, time. | Write those fields in the entry now; it costs nothing and avoids a second entry format. |
| X5 | `feat/chain-cache` | `STEP_CACHED a` is asserted with `contains`; this design appends the key and the source run to the line. | Compatible. Print the longer line from the start. |
| X6 | `feat/chain-cache` | The stamp gains `cached`. `feat/batch-governance` reads lane stamps, and this design adds `prev`, `kind`, `host_class`. Three slices touch one JSON. | Section 5.4 is the stamp's contract: fields are only added. `lane.stamp` must compare `sha` with the lane tip and `digest` with the lane steps file, not test that a file exists. |
| X7 | `feat/chain-cache`, E2, E7 | All three edit `Step` (`chain.rs:26-36`) and the start loop (`chain.rs:256-300`). | Land in the order C, E2, E7; each of the later two rebases over the earlier. |
| X8 | `feat/diagnose-causes` | Its tests assert text (`causes:` first, "N red steps from M causes"). `retry.cause-unchanged` and the `causes` table need the causes as data with a stable fingerprint. | Add to the branch a function returning causes (fingerprint, class, rule, evidence, steps) that the text is printed from. |
| X9 | `feat/diagnose-causes` | Collapsing duplicate blocks changes how many findings a step has; evidence for a panic block changes. The parity corpus for `diagnose` (`tests/corpus/`) may hold the old text. **Unverified** whether any corpus case is affected. | Run the corpus on the branch; a changed case is a decided change, recorded in the commit. |
| X10 | `feat/batch-governance` | `retry.cause-unchanged` needs causes (X8), stored where the engine can read them (slice E4), and a place for a flake or infra note. | The note is a decision-log entry (kind `note`, the cause's fingerprint in `subject`); no table. The branch depends on D and E4; without them it can only gate on "the tip moved". |
| X11 | `feat/batch-governance`, E5 | Both edit the state dispatch (`batch.rs:645-658`) and the T3 and T4 guards (`batch.rs:837-872`). | G lands first; E5 rebases. |
| X12 | all three, E1 | None has touched `ledger.rs` yet. Any that adds a table raises `CURRENT_VERSION` (`ledger.rs:103`). | Versions are assigned in section 11. A branch that needs a table takes the next free number at stack time. |

## 15. Risks, and the never list

| risk | mitigation |
|---|---|
| A wrong key gives a green that never ran. | Shadow before hits (7.4); the same commit required for a proof (7.2, row 1); the engine verifies from its own ledger; a release proof runs everything. |
| A portable step is not portable. | Only steps a person marks; the same class required; shadow on the proof host first; `evidence-class` otherwise. |
| A sharded step silently runs fewer tests. | The tool's own partitioner is the contract; `shard_check`; the ledger shows the summed shard time, and a sharded step far below its unsharded busy time is reported. This is not proven by gatetrain and the doc of the key says so. |
| A probe hurts the disk it measures or the run beside it. | Refused while proving; time and byte budget; free-space floor; its own folder, removed; never an undeclared disk. |
| An advice is wrong. | Evidence and range are printed; nothing is applied; the check row compares the prediction with the measurement and a rule whose checks fail is found by `report`. |
| Minutes are invented where a platform gives no figure. | Section 10.4; an unknown input yields `unknown` minutes and at most `low` confidence, by test. |
| Pipelining stacks work on a base that then changes. | Depth 1; no proof ahead of the base by default; a restack reuses unchanged steps; nothing merges ahead of its base. |
| The sampler becomes a monitor that never stops. | It runs inside the holder or `chain` and ends with it; a test asserts no process is left. |
| The ledger version becomes a merge hazard between lanes. | Numbers assigned here; one number per slice; the tests migrate from every older version. |
| Windows and macOS details are wrong on paper. | Every such cell is marked unverified; each slice's tests run on the three CI platforms (`README.md:246`); a platform without a test stays `unknown` or light-only. |
| This design grows the binary's surface faster than it is used. | Each slice names its saving and its measurement; a slice whose measurement shows nothing after 3 batches is reverted or parked, and that is recorded as a lesson. |

Never, unless a measurement in the ledger shows the tick or a command cannot do the job:

- No glue: no shell script between two `gatetrain` commands, and none generated for a person to keep.
- No new service, no web page, no API server, no queue.
- No daemon. The only long-lived processes are the holder and `chain`, each exactly as long as its run.
- No second binary and no second language (`docs/lessons.md:86`, L79). No new crate for this design.
- No state outside plain files and the one ledger file. A fact on a host is a file there; the ledger indexes it.
- No number that was not measured, and no measurement without its source. Unknown stays unknown.
- No rule that was never seen to refuse: every guard ships with the fixture that violates it.
- No automatic move from an advice, and no probe on a proving host.
- No merge of a red or unproven sha, by any path, including a cached one.

## 16. What is unverified

| item | where it is used |
|---|---|
| The three adopter scripts that are not in its repository (batch-finish, chain-holder, auto-merge) | section 13 |
| Which copy of the pre-check ran for the 3398 s figure (26 checks measured, 27 in the script) and each check's own duration | sections 1, 14 (E0) |
| Whether 12 steps at once fit the proof host without each slowing down | sections 1, 10.3; settled by E7 |
| The duration of test-web, test-ops, clippy and cargo-deny | section 7.5 |
| That 4 shards of test-api take a quarter of its wall time | section 14 (E2) |
| Every macOS cell; the Windows disk-busy counter; `hold --detach` on macOS and Windows | sections 9.2, 10.4 |
| Whether the `diagnose` parity corpus is affected by `feat/diagnose-causes` | X9 |
| What `diagnose` prints today for a red step of a `chain` run (the code path gives it no lines) | F2 |
