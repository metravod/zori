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
- [Release v0.2.0](https://github.com/metravod/zori/releases/tag/v0.2.0), four native archives and SHA256SUMS · how: gh release view v0.2.0 --repo metravod/zori · seen 2026-11-22
- [Homebrew formula](https://github.com/metravod/homebrew-tap/blob/main/Formula/zori.rb), version 0.2.0 · how: brew info metravod/tap/zori · seen 2026-11-22

## Now
**Verified 2026-11-22:** Chromium exercised both node retry buttons after a model failure during a parallel level. The join waited for the corrected attempt and received both branch outputs. See the [console guide](../docs/guide.md).
**Verified 2026-11-22:** harnesses resolve in three layers — built-in → `~/.zori/harnesses.yml` → a flow's `harnesses:` block — so a personally-added CLI (Devin is wired as the first resident) shows up everywhere at once: `zori harnesses` (source + PATH marker), `zori models`, `zori catalog`, `/api/harnesses`, the console picker, the Fleet dialog and the planner brief. Devin PONG-verified through a real zori run; SWE-2 models are Pro-gated on this account (documented in its model_notes). 204 Rust + 116 web checks green. See [the personal store guide](../docs/guide.md#adding-a-harness-the-personal-store).
**Verified 2026-11-22:** the ⚓ Fleet picker saves harness and model pairs to `~/.zori/fleet.json` for planner proposals. Chromium verified that the picker starts hidden on desktop and mobile widths, opens centered over the whole console, and stays closed after reload. Esc and Close discard edits, and Save updates the header count. Its browser check used a test roster. Storage and validation have separate coverage in `src/fleet.rs`. See the [fleet guide](../docs/guide.md#the-model-fleet).
**Verified 2026-11-22:** the console opens the planner on an unselected canvas. Chromium checked that switching nodes preserves the planner session and draft, and reopening a historical run finds its launching chat. Replaying two OpenCode streams confirmed the watchdog passes the final report downstream after a late cancelled-input diagnostic. See the [console guide](../docs/guide.md#start-a-prompt-or-a-model-chat).
**Verified 2026-11-22:** the interface is fully English — web console, supermodel chat protocol, watchdog prompts and run messages, proposal protocol, API errors, built-in skills, guide and example flow (199 Rust + 112 JS checks green; repo-wide Cyrillic grep is clean except the watchdog's deliberate multilingual progress-word list, which stays Russian-detecting and test-covered because agents answer in the operator's language).
**Verified 2026-11-22:** the personal skill store `~/.zori/skills/` carries six workflow skills (onboard, feat, bugfix, critique, reviewer, crossreview) adapted from Pavel Rykov's rpa-skills set, plus the personal pipeline `zori-flows/onboard-fix-review.yml` wiring them into an onboard → fix → blind review fan-out → crossreview join; `zori skills` lists the set and a dry-run resolves every name and model. Workflow skills stay personal (D10) — repo built-ins remain `zori-config`/`zori-planner` only.
**Verified 2026-11-22:** the console watchdog recognizes Coddy's per-turn step limit and requests one final report in the same session when a nudge is available. A console run with the installed Coddy CLI and a local test provider hit a one-step limit, retained the tool result across turns and passed the final report to the dependent task. See the [watchdog guide](../docs/guide.md#the-watchdog-nudges-retries-and-questions-to-you).
**Verified 2026-11-22:** a planner chat started with a project folder runs its agent process in that folder from the first message and on every continuation (live check on a temp project: both attempts' `meta.json` cwd, relative-path marker read); chats without a folder keep the isolated work dir.
**Verified 2026-11-22:** the combined planner cwd and canvas path reaches the runner through the console router with a deterministic planner process. The three client failures it exposed (chat closing on a valid proposal, a revision's edge lost on reopen, a stale reply reverting a pre-send edit) are fixed the same day: the merge base travels with the proposal (`FlowProposal.base`) and placement keeps the chat alive, each reproduced by a regression test. See [combined verification](threads/planner-canvas-verification.md).
**Verified:** v0.2.0 passed the Rust and browser-side suites and an echo flow on macOS and GNU/Linux, on both arm64 and x86_64. All four published archives matched the checksum manifest; the macOS arm64 archive ran `zori 0.2.0` and the smoke-echo flow locally. `brew upgrade metravod/tap/zori` installed 0.2.0 on Apple Silicon. See the [release run](https://github.com/metravod/zori/actions/runs/45069448243) and [tap update](https://github.com/metravod/homebrew-tap/actions/runs/45070147181). v0.1.0 passed the same bar on [its release run](https://github.com/metravod/zori/actions/runs/44761947645).
**Not verified:** Full interactive planner-to-run coverage with real agents remains open. See [console verification](threads/console-verification.md).

## Next
- Validate the planner proposal and rerun interactions in a browser with installed agents.
- Exercise a mixed-agent fan-out and join on a real repository.
- Implement the agreed worktree design for parallel edits. See the [thread](threads/worktree-isolation.md).
