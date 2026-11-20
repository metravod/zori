---
status: active
updated: 2026-11-20
by: codex/GPT-6
---
# Exercise the interactive console with real agents

**Goal:** Verify the complete operator workflow using two different installed agent CLIs.
**Where we got:** The release includes planner validation, proposal approval, session steering, operator questions, reruns and stale propagation.
**Verified:** Rust tests cover the runner and server. The browser-side suites exercise graph layout, Markdown and flow serialization.
**Not verified:** A real-agent planner proposal accepted in the browser, followed by a mixed-agent fan-out, join and downstream rerun.
**Open:** Check the final persisted state and the diagnostics shown to the operator after a failed or interrupted attempt.
**Next step:** Drive that flow in a browser and compare the final console state with the run manifest.
