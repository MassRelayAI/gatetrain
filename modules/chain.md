# chain

The long verification run: one lock holder, steps with their own exit codes, an optional step cache, a stop that also cancels queued follow-ups.

A run label is `<batch><n>`. Logs are deleted before a run (L11). A stop repeats until no chain process is left and cancels scheduled followers (L1, L2).

## Stop: `gatetrain stop PATTERN [--follower PATTERN ...] [--yes] [--config hosts.toml --host NAME]` (L1, L2)

Dry run by default: lists (a) processes whose command line matches PATTERN, (b) queued `at` jobs (`atq`, `at -c ID`) and user systemd
timers whose command matches a follower pattern. With `--yes`: removes the queued followers first, terminates the matching processes
(TERM in rounds 1-2, `kill -9` only from round 3 and only for processes listed again that round), repeats up to 4 rounds until none
match, and prints what is left (exit 1 if anything is). Anything not matching is never touched. Linux only: elsewhere it prints
"not supported" and exits 2.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
