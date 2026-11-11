# watch

Watchers on local or remote logs that cannot go quietly wrong.

Anchored markers only (lessons L3, L4); the remote side bounded by a timeout; a watch that expires is re-armed; every terminal state (green, red, stopped, crashed) produces an event.

`gatetrain doctor` already reports stray watchers (`tail -F` older than `watcher_max_age_min`) by exact pid (L24); the watcher itself is not built yet.

Status: being extracted. See docs/method.md and docs/lessons.md for the rules this module carries.
