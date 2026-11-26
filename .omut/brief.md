---
status: active
updated: 2026-11-26
verified: 2026-11-26
dreamed: 2026-11-25
by: devin/opus-5.5
---
## Why
Run one task across several CLI coding agents at once so the operator can compare how they handle the same brief without driving several terminals. The design grows from real tasks.

## What it is
Zori is a Rust CLI and an embedded local web console. A YAML flow connects tasks in a DAG. Each task selects a CLI, can select a model, and can pass its output to downstream tasks. See the [README](../README.md) and [usage guide](../docs/guide.md).

## Where it lives
- [Public repository](https://github.com/metravod/zori), default branch main · how: gh repo view metravod/zori · seen 2026-11-25
- [Release v0.2.4](https://github.com/metravod/zori/releases/tag/v0.2.4), four native archives and SHA256SUMS · how: gh release view v0.2.4 --repo metravod/zori · seen 2026-11-25
- [Homebrew formula](https://github.com/metravod/homebrew-tap/blob/main/Formula/zori.rb), version 0.2.4 · how: gh api repos/metravod/homebrew-tap/contents/Formula/zori.rb · seen 2026-11-25

## Now
Release history and the checks behind each feature are in the [log](log.md).
**2026-11-25:** the built-in codex harness runs with `-s danger-full-access` (D15). Under `workspace-write` `.git` is read-only and the network is off, so headless codex could neither fetch nor commit. A live `zori run` codex task committed and reached GitHub, shipped in v0.2.3. Devin runs with `--permission-mode dangerous --respect-workspace-trust false` (personal store), OpenCode with `--auto` on first run and resume (v0.2.4); coddy has no rights switch and can do everything.
**Verified 2026-11-26:** F1 (D13) is merged into main (not yet released): the CLI and console drive `TrackedRun` and share its watchdog, questions and process spawn path; `zori run` retries, nudges and answers `ASK:` on a TTY, unattended with redirected stdin. Live coddy+opencode fan-out/join gave the same result under `zori run` and the console. [F1 verification](threads/f1-one-engine.md)
**2026-11-26:** F2 is merged too: `src/runner/` (10 modules) and `src/ui/` (9 modules) split off their former 4931/3034-line files as a verified pure move. [review](threads/code-review-2026-11-24.md)
**2026-11-26:** The embedded console now uses the live run API for its canvas, comparison, inbox and timeline. [Guide](../docs/guide.md)
**Verified 2026-11-25:** v0.2.4 released (OpenCode `--auto`; v0.2.3 brought codex `danger-full-access`): [release run](https://github.com/metravod/zori/actions/runs/45686074730) green on all four targets, all archives match SHA256SUMS, the macOS arm64 archive reports `zori 0.2.4`; after the [tap update](https://github.com/metravod/homebrew-tap/actions/runs/45687587060) `brew upgrade` installed 0.2.4. Earlier releases passed the same bar.
**Verified 2026-11-22:** harnesses resolve built-in → `~/.zori/harnesses.yml` → a flow's `harnesses:` block everywhere (CLI listings, console picker, Fleet, planner brief); the ⚓ Fleet stores harness/model pairs in `~/.zori/fleet.json`; workflow skills live in `~/.zori/skills/` (D10, D11). See the [guide](../docs/guide.md#adding-a-harness-the-personal-store).
**Verified 2026-11-22:** planner chats run in the chosen project folder and proposals merge onto a live canvas (layered tests, no live browser session). See [combined verification](threads/planner-canvas-verification.md).
**Not verified:** full interactive planner-to-run coverage with real agents. See [console verification](threads/console-verification.md).
**Known defect:** stopping `zori ui` mid-attempt counts as a failure, starts a retry and leaves `state.json` non-terminal. See the [thread](threads/stale-status-after-dead-attempt.md).

## Next
- Release the new console and F1+F2 together in v0.3.0; redo F4 (history comments) and the remaining review work on main. [Follow-ups](threads/one-engine-followups.md)
- Add a switch for agents' permission mode (D15): bypass stays the default, but a user can turn the dangerous mode off for every harness at once (personal setting, per flow, per task), mapped to each CLI's own flag (codex `-s`, Devin `--permission-mode`, opencode `--auto`, zcode `--mode`; coddy has none).
- Validate the planner proposal and rerun interactions in a browser with installed agents.
- Implement the agreed worktree design for parallel edits. See the [thread](threads/worktree-isolation.md).
