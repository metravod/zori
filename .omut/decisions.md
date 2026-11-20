# Decisions

- D1 [accepted 2026-11-14] Port the level-based scheduler from zymi-core and write a process driver for CLI agents. This keeps flows independent of the upstream runtime. → [DAG planner](../src/dag.rs)
- D2 [accepted 2026-11-14] Use the name Zori for this standalone agent orchestration project. → [project brief](brief.md)
- D3 [accepted 2026-11-14] A failed task skips dependent branches while independent tasks continue. Level barriers keep the execution order predictable. → [runner](../src/runner.rs)
- D4 [accepted 2026-11-14] Optional branches may fail without blocking a join. Downstream prompts explicitly identify the missing result. → [configuration](../src/config.rs)
- D5 [accepted 2026-11-15] Drive agents through their CLIs. ZCode's bundled CLI has no per-run model selection, so headless ZCode stays outside the supported v0 agents. → [connectors](../src/connectors.rs)
- D6 [accepted 2026-11-16] Both runners execute level siblings concurrently, capped by max_parallel, using a snapshot of completed upstream outputs. → [runner](../src/runner.rs)
- D7 [accepted 2026-11-16] One run tree stores per-task working directories, attempts, logs and a provenance manifest for both runners. → [artifacts](../src/artifacts.rs)
- D8 [accepted 2026-11-18] Design a per-task Git worktree mode for parallel edits, with changes passed to downstream tasks as patches. Implementation is pending. → [worktree design](threads/worktree-isolation.md)
- D9 [accepted 2026-11-18] Supervise tracked attempts with bounded retries, session nudges and operator questions. → [watchdog](../src/watchdog.rs)
