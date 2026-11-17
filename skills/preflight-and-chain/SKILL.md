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
After regenerating files, run `gatetrain generated --config hosts.toml` (`[stack] generated_dirs`): a NEW generated file left untracked
makes a later "disk equals build" test red (L53). Before stacking, `gatetrain lanes ... --baseline-files <globs>` warns when a lane adds
routes or tables and touches none of the lists they must join (L54).
Every check under ~10 minutes: type checks, lints, structure and dependency rules, tracker and doc checks, the fast unit suites, the
"every test is listed" and "every gate is in the full run" checks. The chain starts only when this is green.
Run them as a steps file with `gatetrain chain --steps steps.toml --out out` (see `docs/chain.md`): the fast tier runs in parallel, every red is
reported with its full output in `out/<step>.full`, a red fast step skips the slow tier, and a green run on a clean tree writes a stamp. `--cache <dir>`
skips steps whose inputs are unchanged; use it for the pre-flight only, never for a proof run.

## Chain (on the builder)
One holder takes the locks; steps write their own exit codes; reds do not stop the run, so all of them arrive together. Delete old logs
before the run; decide on exit codes, never on a log line.

## Stop safely
Cancel the merger process **and** any queued or scheduled merger, then stop every chain process repeatedly until none is left. Reds that
appear after a stop are artefacts. Never edit a script while it runs. Rerun with a new label `<batch><n>`.
