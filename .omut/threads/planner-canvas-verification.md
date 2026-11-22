---
status: active
updated: 2026-11-22
by: codex/GPT-6
---
# Verify planner proposals on an editable canvas

**Goal:** In a named chat with an external project folder, approve a prose plan, edit the resulting canvas, ask the planner to add a dependency, and launch the edited graph explicitly.

**Where we got:** The fresh binary built. Its ordinary `zori ui` command failed to bind loopback with `Operation not permitted` in the verification environment. The installed Coddy process, retried with a temporary home, stopped at DNS resolution for its configured provider.

**Verified:** An in-process test exercised the production Axum router, the tracked runner, a deterministic Coddy replacement, and the merge function from the shipped HTML. The run selector title used the first message. The planner process read `plan.txt` by relative path from the same external folder on three attempts, with a new marker on the edge request and the same absolute sessions directory. Validated proposals left only the chat run and no saved flow before launch. When the revised proposal carried the edited prompt, the merge kept it and the moved coordinates and added the edge. The explicit launch passed the full parsed canvas to the runner, including an output field absent from the proposal. The two echo tasks finished in successive DAG levels.

**Open:**

- Browser confirmation with a working provider. The fixes below are proven by layered tests (the Rust proposal gate, the JS merge and placement wiring), not by a live browser session.

**Fixed 2026-11-22 (opencode/GLM-5.3), all three reproduced by regression tests in `web/spec.test.mjs`:**

- Chat closing on a valid proposal: `placeProposalOnCanvas` now drops the node selection but keeps the chat, the SSE stream and the launch card alive; reopening no longer replays the close.
- Dependency lost on reopen: the merge base travels with the proposal (`FlowProposal.base` in `src/state.rs`, set by `proposal_gate` in `src/runner.rs` to the agent's last accepted graph, or the board shipped with the turn for a first proposal), so a revision that arrived while the chat was closed lands its edge on reopen — the client no longer needs board history it does not have.
- Old reply reverting a pre-send edit: because the base is the agent's last accepted graph rather than the board shipped with the turn (which already contains the edit), a reply echoing pre-edit values reads as agent-side only when the operator never touched the field — the manual edit wins. The blank board's `draft` placeholder is also dropped by the merge instead of surviving next to the agent's graph.

**Not verified:** The end-to-end browser interaction and model-generated continuation in this combined scenario.

**Next step:** Repeat the named-chat scenario in a browser with a working provider. Compare the launched graph to the final canvas, including the case where the planner omits a manual edit in its response.
