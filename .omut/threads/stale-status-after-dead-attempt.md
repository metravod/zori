---
status: active
updated: 2026-11-25
by: devin/opus-5.5
---
# Stale node status after a dead attempt

**Next step:** on shutdown, stop the watchdog from counting the killed child as a failed attempt, block retries, and persist a terminal status before exit; regression test: kill `zori ui` mid-attempt.

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

Reproduced 2026-11-25, twice (`chat-1795606541013`, `chat-1795607693383`):
Ctrl+C on `zori ui` mid-attempt kills the Devin child, the watchdog reads it
as "no exit status", records the failure and starts a restart (`r1/prompt.txt`
written, the new child briefly orphaned to PID 1) before the server exits.
`state.json` stays `running` with the restart turn as the last entry. So the
answer to the open question is yes, and there is a second defect: a shutdown
must not count as an attempt failure and must not trigger a retry.

