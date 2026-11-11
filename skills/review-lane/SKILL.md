---
name: review-lane
description: Brief a worker agent for one lane, then have the lane independently and adversarially reviewed before it is stacked.
---

# review-lane

## The brief (what a worker gets)
- the goal in two lines, and what is already ruled out;
- **owned files** (touch nothing else) and the shared hubs it may add to;
- what to read first, with `file:line`;
- **red first**: write the test, show it failing, then implement; paste both counts;
- for database work: one step runs as the **application role**, not a superuser;
- how to verify and what to hand back (commit, counts, unproven items);
- what never to do (force-push, skip hooks, stash, stage everything, touch other lanes).

## The review (money, permission, data leak, network egress)
- The reviewer gets **the diff and the code**, not your conclusion, and is asked to break it.
- Findings come ranked: blocker / should / nit, each with `file:line` and a concrete failure scenario.
- Ask also: which checks in the full run will go red when this is stacked, and what each needs.
- Spot-check every citation (worker and reviewer); a summary relayed through a notification is checked against the code.
- Fix blockers before stacking; record the rest as findings.
