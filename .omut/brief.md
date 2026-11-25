---
status: active
updated: 2026-11-25
verified: 2026-11-25
dreamed: 2026-11-25
by: devin/opus-5.5
---
## Why
Run one task across several CLI coding agents at once so the operator can compare how they handle the same brief without driving several terminals. The design grows from real tasks.

## What it is
Zori is a Rust CLI and an embedded local web console. A YAML flow connects tasks in a DAG. Each task selects a CLI, can select a model, and can pass its output to downstream tasks. See the [README](../README.md) and [usage guide](../docs/guide.md).

## Where it lives
- [Public repository](https://github.com/metravod/zori), default branch main · how: gh repo view metravod/zori · seen 2026-11-25
- [Release v0.2.3](https://github.com/metravod/zori/releases/tag/v0.2.3), four native archives and SHA256SUMS · how: gh release view v0.2.3 --repo metravod/zori · seen 2026-11-25
- [Homebrew formula](https://github.com/metravod/homebrew-tap/blob/main/Formula/zori.rb), version 0.2.3 · how: gh api repos/metravod/homebrew-tap/contents/Formula/zori.rb · seen 2026-11-25

## Now
Release history and the checks behind each feature are in the [log](log.md).
**2026-11-25:** the built-in codex harness runs with `-s danger-full-access` (D15). Under `workspace-write` `.git` is read-only and the network is off, so headless codex could neither fetch nor commit. A live `zori run` codex task committed and reached GitHub; shipped in v0.2.3. Devin already runs with `--permission-mode dangerous`.
**2026-11-25:** the engine merge (D13, F1/F2/F4 of the [review](threads/code-review-2026-11-24.md)) was done on `refactor/one-engine` and then lost: a later flow node deleted the only /tmp clone. The human chose to redo it by rerunning the flow on the fixed version. See [follow-ups](threads/one-engine-followups.md).
**2026-11-25:** console UI audited — a config editor with no view for comparing agents. Redesign order and open D14 are in the [thread](threads/console-ui-redesign.md).
**Verified 2026-11-25:** v0.2.3 released with the codex sandbox fix: [release run](https://github.com/metravod/zori/actions/runs/45674710887) green on all four targets, all archives match SHA256SUMS, the macOS arm64 archive reports `zori 0.2.3`; after the [tap update](https://github.com/metravod/homebrew-tap/actions/runs/45676197047) `brew upgrade` installed 0.2.3. Earlier releases passed the same bar.
**Verified 2026-11-22:** harnesses resolve built-in → `~/.zori/harnesses.yml` → a flow's `harnesses:` block everywhere (CLI listings, console picker, Fleet, planner brief); the ⚓ Fleet stores harness/model pairs in `~/.zori/fleet.json`; workflow skills live in `~/.zori/skills/` (D10, D11). See the [guide](../docs/guide.md#adding-a-harness-the-personal-store).
**Verified 2026-11-22:** planner chats run in the chosen project folder and proposals merge onto a live canvas (layered tests, no live browser session). See [combined verification](threads/planner-canvas-verification.md).
**Not verified:** full interactive planner-to-run coverage with real agents. See [console verification](threads/console-verification.md).
**Known defect:** stopping `zori ui` mid-attempt counts as a failure, starts a retry and leaves `state.json` non-terminal. See the [thread](threads/stale-status-after-dead-attempt.md).

## Next
- Rerun `zori-flows/zori-review-f1-f4.yml` on the fixed version to redo the engine merge; forbid the verify node to delete anything outside its own directory first. See the [thread](threads/one-engine-followups.md).
- Add a switch for agents' permission mode (D15): bypass stays the default, but a user can turn the dangerous mode off for every harness at once (personal setting, per flow, per task), mapped to each CLI's own flag (codex `-s`, Devin `--permission-mode`, …).
- Validate the planner proposal and rerun interactions in a browser with installed agents.
- Implement the agreed worktree design for parallel edits. See the [thread](threads/worktree-isolation.md).
- Redesign the console around comparison and supervision; decide D14 first. See the [thread](threads/console-ui-redesign.md).
