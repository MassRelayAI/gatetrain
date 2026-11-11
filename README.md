<p align="center">
  <img src="docs/assets/hero.svg" alt="gatetrain - lanes are stacked into a train, proven at the gates, and merged only when green" width="100%">
</p>

<p align="center">
  <b>Run coding agents and long build chains across many machines.</b><br>
  Local PCs, home servers, cloud runners &mdash; every run measured, every red explained, nothing merges unproven.
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> &middot;
  <a href="#how-it-works">How it works</a> &middot;
  <a href="#skills">Skills</a> &middot;
  <a href="#profiles-from-zero-cost-to-cloud">Profiles</a> &middot;
  <a href="docs/lessons.md">Lessons</a> &middot;
  <a href="#roadmap">Roadmap</a>
</p>

<p align="center"><sub>Status: early. The method runs every day on a large monorepo; the scripts are being extracted and made generic. Skills and docs are usable today.</sub></p>

---

## Why gatetrain

Once several agents write code in parallel and one verification run takes hours, the hard part is no longer writing code. It is keeping the
line moving without lying to yourself.

| what goes wrong | what gatetrain does |
|---|---|
| A three-hour run dies on a mistake a ten-minute check would have caught. | **Pre-flight** every batch on a free machine first; the expensive run starts only when it is green. |
| Combining many branches quietly duplicates a line, drops a list entry, or re-applies old commits. | **Stack** by base, repair only known artefacts, and leave everything else red for a person. |
| An agent says "tests pass". | **Every claim carries its counts**, red first: a test that never failed has not proven anything. |
| Risky changes (money, permissions, data, network) slip through with the rest. | An **independent review** that gets the diff, not your conclusion, before the change is stacked. |
| Machines sit idle while work queues; the bill or the disk surprises you. | One **ledger** of every run and step: waiting time, idle hosts, flaky steps, cost per batch, agent quality. |
| Cleanup deletes real work. | **Tidy** that cannot: archive tags first, links unlinked first, exact names only, never the main checkout. |

## Quick start

Adopt it in layers; each one is useful on its own.

**1. Read the method (5 minutes).** [`docs/method.md`](docs/method.md) &mdash; roles, a lane, a batch, the tick, measurement.

**2. Give your coding agent the skills.** They are plain Markdown any agent can read; for Claude Code, copy them into your project:

```bash
git clone https://github.com/MassRelayAI/gatetrain
mkdir -p .claude/skills && cp -r gatetrain/skills/* .claude/skills/
```

Then ask your agent to "run the tick", "plan these lanes", "pre-flight this batch" or "review this lane".

**3. Describe your machines and pick a profile.**

```bash
cp gatetrain/config/hosts.example.yaml gatetrain/config/hosts.yaml   # git-ignored; name your machines, roles, disks, cost
```

**4. Start the ledger** (SQLite or Postgres) so the numbers exist from day one:

```bash
sqlite3 gatetrain.db < gatetrain/ledger/schema.sql
```

The command-line scripts (`gatetrain tick`, `gatetrain stack`, `gatetrain chain stop` ...) follow on the [roadmap](#roadmap).

## How it works

<p align="center"><img src="docs/assets/flow.svg" alt="The flow: plan, lanes, review, stack, pre-flight, chain, merge on proof, tidy; the tick watches every host; the ledger records every run" width="100%"></p>

Work is cut into **lanes** (one branch, one worktree, one owner, a list of owned files). Lanes whose files do not overlap run **in parallel**
on whatever machine has a free slot. Finished lanes are **stacked** into a **batch**, checked cheaply in a **pre-flight**, then proven by one
**chain** (the full verification run) on the builder. Only a chain that is green on exactly the tested commit **merges**. A cheap recurring
**tick** watches every machine, chain, lane and disk, acts only where a written rule covers it, and reports in a few lines. Everything is a row
in the **ledger**. Every failure that happened twice becomes a **lesson**: a rule plus a test that proves it.

### What a tick looks like (example)

```text
tick 14:53  batch-42 run 2: 255/440 steps, 0 red, merger armed
            preflight-1: lane gates idle; builder-1 SSD 176 GB free; 0 leftover containers
            lane auth-door READY (review: 2 should-fix, fixed; 8/8 gates as the app role)
            next: stack batch-43 when 42 merges (3 lanes, 1 base)
            waits on you: nothing
```

## Skills

| skill | use it to |
|---|---|
| [`tick`](skills/tick/SKILL.md) | run the recurring check across hosts, chains, lanes and disks, and report in a few lines |
| [`plan-lanes`](skills/plan-lanes/SKILL.md) | split work into independent lanes with owned files, and decide what can run in parallel |
| [`preflight-and-chain`](skills/preflight-and-chain/SKILL.md) | stack a batch, pre-flight it on a free machine, run the chain, stop it safely |
| [`review-lane`](skills/review-lane/SKILL.md) | brief a worker, then get an independent adversarial review of a risky lane |
| [`tidy`](skills/tidy/SKILL.md) | clean up finished lanes, build outputs and containers without touching real work |

## Modules

| module | what it does |
|---|---|
| [`plan`](modules/plan.md) | detect independent work (owned files, shared hubs, hidden coupling, resources); schedule it across hosts, critical path first |
| [`hosts`](modules/hosts.md) | the machines (local, server, cloud), their roles, limits and cost |
| [`tick`](modules/tick.md) | the recurring check; acts only on written rules |
| [`watch`](modules/watch.md) | watchers on remote logs that cannot go quietly wrong |
| [`chain`](modules/chain.md) | lock holder, per-step results, step cache, a stop that also cancels queued follow-ups |
| [`stack`](modules/stack.md) | combine lanes by base, repair known artefacts, pre-flight before the expensive run |
| [`review`](modules/review.md) | briefs, independent review for risky lanes, citation checks, persona juries |
| [`merge`](modules/merge.md) | merge only on proof, then deploy and wait for health |
| [`ledger`](modules/ledger.md) | runs, steps, hand-backs and host samples as rows |
| [`tidy`](modules/tidy.md) | cleanup that cannot destroy work |

## Profiles: from zero cost to cloud

| profile | where work runs | spend |
|---|---|---|
| `zero-cost` | local devices only: PCs, laptops, SSDs, home or office servers, WSL; local models where good enough | 0 |
| `budget` | local first; a cloud runner only when the measured wait passes your limit, under a monthly cap | capped |
| `speed` | the long chain on cloud runners; pre-flight and lane checks on local machines | metered |
| `mixed` | a build server for the chain, a workstation for pre-flight, cloud APIs for the agents | metered |

## How it compares

gatetrain is not a CI service or a workflow engine; it sits on top of whatever runs your steps.

| | merge queues (Zuul, bors, hosted queues) | CI analytics | workflow engines | **gatetrain** |
|---|---|---|---|---|
| test the merged result before merging | yes | &ndash; | &ndash; | yes, per batch |
| many machines incl. your own PCs, zero cost | partly | &ndash; | yes | **yes, by profile** |
| coding agents as workers, with briefs and reviews | &ndash; | &ndash; | &ndash; | **yes** |
| cheap pre-flight before the expensive run | &ndash; | &ndash; | &ndash; | **yes** |
| flaky steps, waiting time, cost per batch | &ndash; | yes | partly | **yes (ledger)** |
| safe cleanup of branches, worktrees, outputs | &ndash; | &ndash; | &ndash; | **yes** |

## Lessons it is built on

Fifteen so far, each with the rule and what enforces it ([`docs/lessons.md`](docs/lessons.md)). Three of them:

- A stop that kills the run but not the merger scheduled five minutes later lets that merger attach to the next run. **A stop cancels queued follow-ups too.**
- Database tests that run as a superuser pass while the real application role is denied. **Every database lane runs one step as the application role.**
- Three long runs stopped on reds a ten-minute check would have found. **Pre-flight first.**

## Roadmap

- [x] Method, lessons, module pages, host profiles, ledger schema
- [x] Agent skills: tick, plan-lanes, preflight-and-chain, review-lane, tidy
- [ ] `gatetrain tick` and `gatetrain watch` (generic, host-aware)
- [ ] `gatetrain chain` (holder, step runner, stop) and the ledger writer
- [ ] `gatetrain stack` with base detection, artefact repair and pre-flight on a free host
- [ ] `gatetrain plan` (owned-file conflicts, parallel schedule, critical path)
- [ ] Ledger views: critical path, flaky steps, cost per batch, agent scorecard
- [ ] Cloud adapters for the `budget` and `speed` profiles

## Contributing

Issues and lessons are welcome: a failure you hit twice, the rule you wrote for it, and the test that proves it.

## Licence

To be decided (Apache-2.0 proposed). Until then, all rights reserved.
