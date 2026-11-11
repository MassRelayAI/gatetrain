---
name: preflight-and-chain
description: Stack finished lanes into a batch, pre-flight it on a free machine, run the full chain on the builder, and stop a run safely.
---

# preflight-and-chain

## Stack
1. For each lane find its base and what it already contains. Lanes that contain other lanes: list the outer one only. Lanes on different
   bases: stack in passes, each pass on the previous one.
2. Apply each lane's own commits; rebuild shared hubs instead of text-merging them.
3. Repair only known artefacts (a byte-identical duplicate row; a row added then edited - keep the last; test files dropped from a merged
   list). Anything else stays red for a person.

## Pre-flight (on a free host)
Every check under ~10 minutes: type checks, lints, structure and dependency rules, tracker and doc checks, the fast unit suites, the
"every test is listed" and "every gate is in the full run" checks. The chain starts only when this is green.

## Chain (on the builder)
One holder takes the locks; steps write their own exit codes; reds do not stop the run, so all of them arrive together. Delete old logs
before the run; decide on exit codes, never on a log line.

## Stop safely
Cancel the merger process **and** any queued or scheduled merger, then stop every chain process repeatedly until none is left. Reds that
appear after a stop are artefacts. Never edit a script while it runs. Rerun with a new label `<batch><n>`.
