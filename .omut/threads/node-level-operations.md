---
status: active
updated: 2026-11-24
by: devin/SWE-2
---
# Node-level pipeline operations

**Goal:** the operator's model — stop the whole run or any node, change
configs and restart, or add instructions in the node chat and let the same
session continue; after a task finishes, supplement it or change and re-run
with downstream cleanup; optional per-node human approval gate.

**Where we got:** scope agreed with the human 2026-11-24 (simpler than the
first draft — no add-node, no fork-from-node). Agreed design:
- `POST /api/runs/:id/cancel {node?}` — kill the process group, node/run
  lands in a new `Stopped` status; without `node` all Running nodes stop.
- A stopped node KEEPS its session handle: steer resumes the same session
  with the added instructions (coddy/codex/opencode/transcript_chat);
  non-steerable harnesses fall back to rerun.
- Steer gate widens to `Done | Stopped | Failed` when a session exists.
- Per-task `approve: true` → after Done the node parks in a review status,
  the level barrier holds downstream; Approve releases, Reject offers BOTH
  steer-with-feedback and rerun-with-fix.
- Downstream stale stays mark-then-`⟳ Rerun stale` (no auto-cascade).

**Verified:** code reading only so far — no cancel endpoint or Stopped
status exists; steer is gated to `Done` in ui.rs; `Waiting` already parks
the level barrier (the approve gate copies that shape).

**Open:** none blocking — implementing.

**Next step:** implement in this order — (1) `Stopped` status + cancel
plumbing through exec_once (kill group, settle status honestly), (2) steer
gate + session retention, (3) `approve:` flag + review status + barrier
gate + approve/reject endpoints, (4) console UI (Stop button, review card).
