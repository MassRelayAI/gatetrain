# tidy

Cleanup that cannot destroy work.

Archive tag before a branch is deleted; links unlinked before a worktree is removed (L12); containers removed by exact name; never the main checkout, never anything with uncommitted work; check the main checkout afterwards.

`gatetrain doctor` lists leftover exited throwaway containers (configurable name prefix) by exact name with the `docker rm` command, and never removes them (L31). The tidy command itself is not built yet.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
