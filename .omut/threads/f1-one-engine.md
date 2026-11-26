---
status: done
updated: 2026-11-26
by: opencode/GLM-5.3
---
# F1 — Shared execution engine

**Goal:** implement D13 from the [review](code-review-2026-11-24.md) on a branch from current main.

**Where we got:** committed on local branch `refactor/one-engine` (implementation restored by codex/GPT-6 after the first branch was lost; re-reviewed, docs-aligned and committed 2026-11-26). `zori run` drives `TrackedRun::run_all` and prints its events. Required failure exits 1. Optional failure is reported as degraded and its dependents can continue. A TTY can answer questions, and redirected input uses unattended supervision. The classic driver and its spawn site are gone.

**Verified:** re-run 2026-11-26 on the committed tree: `cargo build`, `cargo test` (214 unit + 3 CLI), `cargo clippy --all-targets -- -D warnings`, `node --test web/*.test.mjs` (3 suites, 116 cases), plus a terminal smoke run (required failure → exit 1, attempts r0/r1 in the manifest, artifact layout unchanged). The restoring session's pseudo-terminal checks covered answering a question, EOF, and a second question after the first timed out.

**Not verified:** live provider-backed runs and the console in a browser.

**Next step:** F2 can proceed from the local `refactor/one-engine` branch. The file split and remaining review findings are tracked in the [review](code-review-2026-11-24.md).
