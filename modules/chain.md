# chain

The long verification run: one lock holder, steps with their own exit codes, an optional step cache, a stop that also cancels queued follow-ups.

A run label is `<batch><n>`. Logs are deleted before a run (L11). A stop repeats until no chain process is left and cancels scheduled followers (L1, L2).

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
