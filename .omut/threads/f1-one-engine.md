---
status: done
updated: 2026-11-26
by: devin/opus-5.5
---
# F1 — Shared execution engine

**Goal:** implement D13 from the [review](code-review-2026-11-24.md) on a branch from current main.

**Where we got:** committed on local branch `refactor/one-engine` (implementation restored by codex/GPT-6 after the first branch was lost; re-reviewed, docs-aligned and committed 2026-11-26). `zori run` drives `TrackedRun::run_all` and prints its events. Required failure exits 1. Optional failure is reported as degraded and its dependents can continue. A TTY can answer questions, and redirected input uses unattended supervision. The classic driver and its spawn site are gone.

**Verified:** re-run 2026-11-26 on the committed tree: `cargo build`, `cargo test` (214 unit + 3 CLI), `cargo clippy --all-targets -- -D warnings`, `node --test web/*.test.mjs` (3 suites, 116 cases), plus a terminal smoke run (required failure → exit 1, attempts r0/r1 in the manifest, artifact layout unchanged). The restoring session's pseudo-terminal checks covered answering a question, EOF, and a second question after the first timed out.

**Verified live 2026-11-26 (devin/opus-5.5):** a coddy + opencode fan-out with a coddy join ran under `zori run` (stdin redirected) and through `zori ui` `POST /api/runs`; outputs and artifact trees matched (the console adds `state.json`). A coddy task asked an `ASK:` question on a TTY; the typed answer reached the agent and became its output. Merged into main by fast-forward.

**Not verified:** the console in a real browser.

**Next step:** none for F1; release and the remaining review findings are tracked in [follow-ups](one-engine-followups.md).
