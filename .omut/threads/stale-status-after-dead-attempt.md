# Stale node status after a dead attempt

Seen 2026-11-23 on a `swe-review` console run (devin harness): the node's
restart attempt died in ~2s (fixed separately — relative `--prompt-file`
path), retries were exhausted, yet `state.json` still showed the node
non-terminal.

What the code says: every attempt end writes a terminal status and the
persister broadcasts each snapshot to `state.json`, and both `run_level`
and the steer/rerun paths settle driver errors as `Failed`. So an in-process
finish cannot leave a zombie `Running`.

Open question: was the `zori ui` server itself restarted while the attempt
was in flight? `spawn_persister` is in-memory — if the server dies, the last
broadcast (e.g. `running`/`waiting`) is what stays on disk, and reload
recovery only re-runs *pending* nodes. Reproduce: kill `zori ui` mid-attempt,
restart, inspect the reloaded node's status and whether it resumes or stays
stale.
