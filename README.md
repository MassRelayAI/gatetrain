<p align="center">
  <img src="docs/assets/hero.svg" alt="gatetrain - lanes are stacked into a train, proven at the gates, and merged only when green" width="100%">
</p>

<p align="center">
  <b>Run coding agents and long build chains across many machines.</b><br>
  Local PCs, home servers, cloud runners - every run measured, every red explained, nothing merges unproven.
</p>

<p align="center">
  <a href="docs/method.md">Method</a> &middot;
  <a href="#modules">Modules</a> &middot;
  <a href="#profiles">Profiles</a> &middot;
  <a href="docs/lessons.md">Lessons</a> &middot;
  <a href="ledger/schema.sql">Ledger</a>
</p>

---

gatetrain is a method plus small tested scripts. It grew out of running a large monorepo with many parallel agent lanes, one long
verification chain per batch, and two to three machines. Every module here exists because something went wrong without it.

## The idea in one paragraph

Work is cut into **lanes** (one branch, one worktree, one owner). Finished lanes are **stacked** into a **batch**. A batch is proven by
one **chain** (the full verification run) on the fastest suitable machine, after a cheap **pre-flight** on a free one. Only a chain that is
green on exactly the tested commit **merges**. A cheap recurring **tick** watches every machine, chain, lane and disk, fixes what a rule
covers, and reports in a few lines. Risky lanes get an independent **review** before they are stacked. Everything that happens is a row in
the **ledger**, so bottlenecks, flaky steps, agent quality and cost are measured, not guessed. Every failure that happened twice becomes a
**lesson**: a rule plus a test that proves it.

<p align="center"><img src="docs/assets/flow.svg" alt="The flow: plan, lanes, review, stack, pre-flight, chain, merge on proof, tidy; the tick watches every host; the ledger records every run" width="100%"></p>

## Modules

| module | what it does |
|---|---|
| [`plan`](modules/plan.md) | detect which work is independent (owned files, shared hubs, hidden coupling, resources) and schedule it in parallel across hosts, critical path first |
| [`hosts`](modules/hosts.md) | the registry of machines (local, server, cloud), their roles, limits and cost; profiles (zero-cost, budget, speed, mixed) |
| [`tick`](modules/tick.md) | the recurring check across all hosts; acts only on written rules; 3-6 line report |
| [`watch`](modules/watch.md) | safe watchers on remote logs: bounded, failure-aware, re-armable, no stale lines |
| [`chain`](modules/chain.md) | lock holder, step runner with per-step results, step cache, stop that also cancels queued follow-ups |
| [`stack`](modules/stack.md) | combine lanes into a batch: base detection, passes per base, known-artefact repair, pre-flight before the expensive run |
| [`review`](modules/review.md) | agent briefs, independent adversarial review for risky lanes, citation spot-checks, persona juries |
| [`merge`](modules/merge.md) | merge only on proof: zero red, full mode, tested commit == tip; then deploy and wait for health |
| [`ledger`](modules/ledger.md) | every run, step, review and hand-back as a row: flaky detection, critical path, agent scorecard, cost |
| [`tidy`](modules/tidy.md) | cleanup that cannot destroy work: archive tags first, links unlinked first, exact names only |
| [`lessons`](docs/lessons.md) | the failures that shaped every rule above |

## Profiles

| profile | where work runs | spend |
|---|---|---|
| `zero-cost` | local devices only (PCs, laptops, SSDs, home or office servers, WSL); local models where good enough | 0 |
| `budget` | local first; a cloud runner only when the queue wait passes a limit, under a monthly cap | capped |
| `speed` | the long chain on cloud runners; pre-flight and lane checks on local machines | metered |
| `mixed` | a build server for the chain, a workstation for pre-flight, cloud APIs for agents | metered |

See [`config/hosts.example.yaml`](config/hosts.example.yaml).

## Status

Early. The method is in daily use; the scripts are being extracted from the project they grew in and made generic. Start with
[`docs/method.md`](docs/method.md).

## Licence

To be decided (Apache-2.0 proposed). Until then, all rights reserved.
