---
name: using-gatetrain
description: Use at the start of any session that runs more than one lane, more than one machine, or a long verification run. Introduces the gatetrain skills and when each one applies.
---

# using-gatetrain

gatetrain is for the moment the work stops being one branch on one machine.

| when | use |
|---|---|
| a piece of work is big enough for more than one agent or branch | `plan-lanes` |
| a lane is about to be handed to a worker, or a risky lane is finished | `review-lane` |
| lanes are finished and need to become one proven batch | `preflight-and-chain` |
| on a timer, or "how is everything going" | `tick` |
| a run, a batch or an agent went wrong and you want to know why | `diagnose-run` |
| a batch merged; branches, worktrees, outputs and containers are left | `tidy` |

Rules every skill keeps: claims carry counts; tests are seen to fail before they pass; risky lanes get an independent review; nothing merges
unless the tested commit is the one being merged; nothing is deleted without an archive. Works alongside single-branch development skills
(brainstorming, plans, test-driven development): gatetrain is the layer above them, many lanes and many machines.
