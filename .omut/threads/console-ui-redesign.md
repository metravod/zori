---
status: active
updated: 2026-11-26
by: coddy/qwen3.8-flash-next-ru
---
# Console UI redesign: from config editor to supervision surface

**Goal:** Make the console serve zori's "why" — comparing how several agents handle one brief and supervising them — instead of being a form-driven DAG editor. Agents make "control panel" UI (forms, selects, manual wiring) less needed; they make supervision and judgment UI (see many parallel things, compare, decide, intervene) more needed.
**Where we got:** Audit from screenshots of `zori ui` 0.2.2 at 1440×900 (empty console, the `omut-review` run, an opened node):
- The top two bars are mostly form fields (Pipelines/Runs selects, name, launch directory, ＋ Task, Template, Run); the empty state is a "draft / coddy" node and an unprompted "Chat with the supermodel".
- Node cards show a status dot, harness/model, a clipped prompt and "1 turn"/"exit 1"/"stale" — no elapsed time, current activity, cost or failure reason.
- The inspector takes ~46% of the width even when empty; the graph is squeezed left.
- Runs live in a dropdown: no history list, no durations or outcomes, no run-to-run comparison.
- The transcript is one markdown wall; tool calls, reasoning and the final report are not separated.
- There is no comparison view at all, although comparison is the project's stated purpose.
- The whole UI is one `web/index.html` (3443 lines, 171 KB); see D14.
**Proposed order:**
1. Compare view — pick sibling nodes on the same brief: side-by-side final reports, time, turns, tokens/cost, watchdog interventions, exit; per-agent file diffs once [worktrees](worktree-isolation.md) land; "use this result" passes it downstream.
2. Attention inbox — questions, failures, stale nodes, pending approvals (D12) in a persistent zone, plus tab-title badge and system notification. Today `#inbox` hides in the header.
3. Live node cards — timer, last-activity line (last tool call), status as the whole border colour, failure reason on the card; a timeline/Gantt view of a run.
4. Intent-first layout — start screen is one "what to do, in which folder" input that the planner turns into a graph; name/cwd/Template move to a secondary menu; runs become a history sidebar; the inspector collapses when nothing is selected.
5. Structured transcript — collapsible tool-call and file-edit blocks, highlighted final report, in-run navigation; Cmd+K and keyboard navigation on the graph.
**Verified:** Only the audit above; no UI code changed.
**Not verified:** Whether the run/state API already exposes timings, token counts and last activity per attempt for items 1 and 3.
**Open:** D14 (front-end foundation) gates items 1–5. The owner is trying an external UI generator on a mock-up prompt first.
**Next step:** Decide D14, then build a compare-view mock-up against a real fan-out run (e.g. `omut-review`).

## 2026-11-26: layout mockup (branch `feature/console-ui-mockup`, from main)

`ui-mockup/` in the repo root — a static mockup of the overall layout from the
owner's two references: left sidebar (run history, saved pipelines), center
(run header with the agent's pending question, DAG with statuses, agent cards
showing time/calls/cost/tokens and per-node actions, Gantt timeline with
retry segments and watchdog-nudge ticks), right column (attention inbox:
question / failed / approval with actions, final report with "use as result").
Clicking an agent card swaps the right column to the agent detail: collapsible
tool-call/reasoning items, file-edit diff, final report, steer box.

It is deliberately built as **native ES modules, no bundler, no dependencies**
(D14 option A in practice) and is not embedded in the binary — only
`web/index.html` is (`include_str!` in `src/ui.rs`). Fake data in
`ui-mockup/data.js`, shaped like a run/state API payload.

**Owner clicks it and decides D14.** If option A holds: port the mockup's
module split into `web/` and wire it to the real API, then item 1 (compare
view) and item 3 (live cards) land on that skeleton. If a framework wins, the
mockup still fixes the layout contract; the data.js shapes transfer as-is.
**Still open:** whether run/state exposes per-attempt timings, cost/tokens and
last activity (the mockup assumes it does).
