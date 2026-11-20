---
status: active
updated: 2026-11-20
verified: 2026-11-20
by: codex/GPT-6
---
## Why
Run one task across several CLI coding agents at once so the operator can compare how they handle the same brief without driving several terminals. The design grows from real tasks.

## What it is
Zori is a Rust CLI and an embedded local web console. A YAML flow connects tasks in a DAG. Each task selects a CLI, can select a model, and can pass its output to downstream tasks. See the [README](../README.md) and [usage guide](../docs/guide.md).

## Where it lives
The first public release targets [metravod/zori](https://github.com/metravod/zori), [GitHub Releases](https://github.com/metravod/zori/releases) and [Homebrew](https://github.com/metravod/homebrew-tap) · 2026-11-20 · how: inspect the release and tap

## Now
**Verified:** The code includes both runners, session steering, a bounded watchdog, planner proposals, model discovery and embedded skills. The release workflow builds the CLI for macOS and Linux.
**Not verified:** Full interactive planner-to-run coverage with real agents remains open.

## Next
- Validate the planner proposal and rerun interactions in a browser with installed agents.
- Exercise a mixed-agent fan-out and join on a real repository.
- Implement the agreed worktree design for parallel edits. See the [thread](threads/worktree-isolation.md).
