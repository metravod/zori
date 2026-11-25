---
status: parked
parked_because: priority
updated: 2026-11-24
by: devin/opus-5.5
---
# Code review findings, 2026-11-24

**Goal:** record a whole-project review so the structural debt is planned
before more features land on it. Findings only — plan and work come later,
by the human's call. Each finding taken into work gets its own thread.

**Where we got:** review done at a391aad (v0.2.1). Findings, by priority:

- **F1 — two execution engines.** `zori run` goes through the classic
  `runner::run` → `run_task` (src/runner.rs:508, :862); the console goes
  through `TrackedRun::exec_once` → `spawn_captured` (:2221, :2740). Two
  spawn sites (~:986 and ~:2753). The classic path has no supervision: only
  the watchdog preamble in the prompt, no nudges, retries, operator
  questions, transcript chat. Same flow, different behaviour in the terminal
  and in the console; fixes land twice (the 2026-11-23 prompt-file fix had
  to patch both, see log). D12 (Stop / steer / approve) would widen the gap.
  Proposal: the CLI drives `TrackedRun` with a terminal event printer and
  the classic engine is removed. Waits for the human: D13.
- **F2 — oversized files.** runner.rs is 5520 lines (production ~2915,
  tests from :2915); `impl TrackedRun` spans ~:1344–2637, `exec_once`
  alone ~400 lines. ui.rs 3034 (production ~2376). web/index.html 3443 in
  one file without a build. Suggested split of runner.rs: process spawn /
  kill, prompt render + interpolation, level barrier, steer / rerun /
  stale, questions + proposal gate. Best done after F1 (F1 deletes code).
- **F3 — the main scenario is unverified.** Planner → run with real agents
  and a mixed-agent fan-out / join on a real repository are still in "Not
  verified" (see [console verification](console-verification.md)). Review
  opinion: verify these before worktree isolation and node-level operations.
- **F4 — narrative comments (low, taste).** Some comments retell bug
  history (e.g. `sessions_dir` in `TrackedRun::new`, "Review F10" labels in
  ui.rs). That history belongs in log / commit messages; the code comment
  should keep only the invariant.

**Verified:** `cargo test` — 212 passed; `node --test web/*.test.mjs` — 3
files pass; `cargo clippy` (after touching sources) — 0 warnings. F1 by
reading main.rs:233 (only caller of `runner::run`) and grepping the classic
span for watchdog use. Line counts by `wc -l`.

**Checked, not an issue:** `unwrap()` in production code is rare — ui.rs 2
(`models_cache` lock), runner.rs 11 (5 on locks: `RUNNING_GROUPS`,
`SharedBuf`); the run registry already recovers from poisoning
(`runs_guard`, ui.rs:450). Earlier "~270 unwraps" counted tests — wrong.

**Not verified:** no runtime check of F1's behaviour difference (same flow
under `zori run` vs `zori ui` with a failing agent); F2 split boundaries are
a sketch, not checked against borrow / ownership constraints.

**Open:** D13 — merge the engines or keep both.

**Next step:** the human decides D13 and the order against
[node-level operations](node-level-operations.md) (D12 is easier to build
once, on one engine). Then a plan for F1 → F2.
