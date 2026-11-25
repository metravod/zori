---
status: parked
parked_because: priority
updated: 2026-11-25
by: devin/sonnet-5.5
---
# SIGHUP can leave child agent processes alive

**Goal:** closing the terminal (SIGHUP) during `zori run` must stop every agent process group, like Ctrl-C does.

**Where we got:** found in review of `refactor/one-engine` (deferred, not fixed). Ctrl-C exits 130 and cleans up with retries disabled (test added). SIGHUP is not handled the same way: the shutdown can race with a watchdog retry that spawns a new child. See [follow-ups](one-engine-followups.md).

**Not verified:** no reproduction was run; the claim is the reviewer's, read from the code.

**Open:** whether to install a SIGHUP handler beside the existing death handlers in the runner's process module (`src/runner.rs` on main; `src/runner/process.rs` after the split) and block retries once shutdown starts.

**Next step:** reproduce with a flow of `sleep` tasks and `kill -HUP` on the zori pid, then add a regression test next to the SIGINT one.
