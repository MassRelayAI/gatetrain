# tidy

Cleanup that cannot destroy work.

Archive tag before a branch is deleted; links unlinked before a worktree is removed (L12); containers removed by exact name; never the main checkout, never anything with uncommitted work; check the main checkout afterwards.

`gatetrain doctor` lists leftover exited throwaway containers (configurable name prefix) by exact name with the `docker rm` command, and never removes them (L31). `gatetrain check` and `gatetrain tidy` (dry run, `--apply`, `--auto`) are built: see the README section "Tidy and check".

Status: built. See docs/method.md and docs/lessons.md for the rules this module carries.
