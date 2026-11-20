---
status: active
updated: 2026-11-20
by: codex/GPT-6
---
# Per-task Git worktrees

**Goal:** Let parallel agents edit separate checkouts and deliver their changes to a join.
**Where we got:** D8 specifies one worktree per task, kept across attempts. Select a base ref (default HEAD), commit each attempt's changes, record its patch in the manifest and apply dependency patches in declared order. Report conflicts to the joiner.
**Verified:** Today's workspace modes are task and flow in [config](../../src/config.rs).
**Not verified:** Worktree creation, patch transfer, cleanup and failure recovery.
**Open:** Retention must account for the Git metadata and branches associated with each run.
**Next step:** Implement the third workspace mode and exercise parallel conflicting edits in a disposable repository.
