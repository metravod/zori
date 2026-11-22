---
status: active
updated: 2026-11-22
verified: 2026-11-20
by: codex/GPT-6
---
## Why
Run one task across several CLI coding agents at once so the operator can compare how they handle the same brief without driving several terminals. The design grows from real tasks.

## What it is
Zori is a Rust CLI and an embedded local web console. A YAML flow connects tasks in a DAG. Each task selects a CLI, can select a model, and can pass its output to downstream tasks. See the [README](../README.md) and [usage guide](../docs/guide.md).

## Where it lives
- [Public repository](https://github.com/metravod/zori), default branch main · how: gh repo view metravod/zori · seen 2026-11-20
- [Release v0.1.0](https://github.com/metravod/zori/releases/tag/v0.1.0), four native archives and SHA256SUMS · how: gh release view v0.1.0 --repo metravod/zori · seen 2026-11-20
- [Homebrew formula](https://github.com/metravod/homebrew-tap/blob/main/Formula/zori.rb), version 0.1.0 · how: brew info metravod/tap/zori · seen 2026-11-20

## Now
**Verified 2026-11-22:** a planner chat started with a project folder runs its agent process in that folder from the first message and on every continuation (live check on a temp project: both attempts' `meta.json` cwd, relative-path marker read); chats without a folder keep the isolated work dir.
**Verified 2026-11-22:** the combined planner cwd and canvas path reaches the runner through the console router with a deterministic planner process. The three client failures it exposed (chat closing on a valid proposal, a revision's edge lost on reopen, a stale reply reverting a pre-send edit) are fixed the same day: the merge base travels with the proposal (`FlowProposal.base`) and placement keeps the chat alive, each reproduced by a regression test. See [combined verification](threads/planner-canvas-verification.md).
**Verified:** v0.1.0 passed the Rust and browser-side suites and an echo flow on macOS and GNU/Linux, on both arm64 and x86_64. All four published archives matched the checksum manifest. A Homebrew installation on Apple Silicon ran the README example and served the embedded console over HTTP. See the [release run](https://github.com/metravod/zori/actions/runs/44761947645) and [tap update](https://github.com/metravod/homebrew-tap/actions/runs/44768420484).
**Not verified:** Full interactive planner-to-run coverage with real agents remains open. See [console verification](threads/console-verification.md).

## Next
- Validate the planner proposal and rerun interactions in a browser with installed agents.
- Exercise a mixed-agent fan-out and join on a real repository.
- Implement the agreed worktree design for parallel edits. See the [thread](threads/worktree-isolation.md).
