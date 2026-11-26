---
status: parked
parked_because: blocked
updated: 2026-11-25
by: devin/sonnet-5.5
---
# Code review findings, 2026-11-24

**Goal:** record a whole-project review so the structural debt is planned
before more features land on it. Findings only — plan and work come later,
by the human's call. Each finding taken into work gets its own thread.

**Where we got:** review done at a391aad (v0.2.1); work on branch
`refactor/one-engine` (base d76980c), reported at 2026-11-25. The branch was
lost unmerged the same day ([follow-ups](one-engine-followups.md)); the
"done" below describes the lost work, not main:

**Current F1:** redone on local branch `refactor/one-engine` based on ba7ef62. The [F1 thread](f1-one-engine.md) records current verification.

- **F1 — done** (67d351d, review fixes 7013642). `zori run` and the console
  drive `TrackedRun::run_all`; `runner::run`, `run_task` and `spawn_captured`
  duplicates are gone. Terminal printer, TTY prompts, unattended mode when
  stdin is redirected; required failure exits 1, optional counts as degraded.
  D13 accepted.
- **F2 — done** (bbf5dba runner split, dc33170 ui split; cycles removed in
  7013642). `src/runner/` has 10 modules, `src/ui/` 9; pure move checked by
  line multiset and identical test names. **Partly open:** `web/index.html`
  (3443 lines) is NOT split — deferred on purpose, tied to D14.
- **F3 — done by local harnesses.** Disposable repo, flow with two custom
  harnesses, echo and a failing `false` (one required, one optional), fan-out
  and a join. `zori run` and the console API (`zori ui`) agree on every node
  status, watchdog retries, skip-on-required, run-on-optional with the
  placeholder text, exit code 1/0 and the artifact layout. No mismatch found.
- **F4 — done** (c602d57). About 70 history comments cut to invariants; the
  diff touches comment lines only. One heartbeat comment corrected in 7013642.

**Review of the branch:** two independent reviews, zero blockers. Fixed: closed
question blocking later prompts, non-completed run exiting 0, deadline
extensions bypassing the event hook, dry run reading stdin, lost boot-error
context, failure output separator, two module cycles, broken links, test
coverage (223 Rust tests). Rejected: auto-answer after interactive EOF; capped
extensions / finite default question timeout (shared watchdog defaults).
Deferred: see [follow-ups](one-engine-followups.md).

**Verified:** by the verify stage on the branch: `cargo build`, `cargo test`
(215 unit + 8 CLI), `cargo clippy --all-targets -- -D warnings`, `node --test
web/*.test.mjs` (116 cases) all pass; each fixer claim checked in the code.

**Not verified:** a live provider-backed run (Codex/OpenCode/Coddy/Devin);
the console in a real browser (API only); a clean-environment replay. Not
rechecked in this checkout: the branch and its commits do not exist here, so
this record rests on the stage reports.

**Checked, not an issue:** `unwrap()` in production code is rare — ui.rs 2
(`models_cache` lock), runner.rs 11 (5 on locks: `RUNNING_GROUPS`,
`SharedBuf`); the run registry already recovers from poisoning
(`runs_guard`, ui.rs:450). Earlier "~270 unwraps" counted tests — wrong.

**Open:** current F1 is unmerged. F2, F3 and F4 remain for later stages.
The `web/index.html` split is deferred to D14.

**Next step:** continue with the F2 file split from the F1 branch.
