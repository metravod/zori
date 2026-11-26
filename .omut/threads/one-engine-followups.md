---
status: active
updated: 2026-11-25
by: devin/opus-5.5
---
# One engine: merge and follow-ups

**Goal:** land `refactor/one-engine` (F1/F2/F4 of the [review](code-review-2026-11-24.md)) in main and close what the review deferred.

**Where we got:** the branch was rebuilt from main: F1 committed 2026-11-25/26
(104d1f9), F2 the same day (ccf21f6, the runner/ui module split as a verified
pure move). F3 (mixed-agent fan-out/join parity) and F4 (history comments) are
not yet redone. The original five-commit branch remains lost: it was worked in
`/private/tmp/zori-one-engine.HVi7bt`, which no longer exists, `git cat-file`
finds none of its five hashes, and `origin` has only `main`. Uncommitted
project-meta edits made in that clone are lost too.

**How it was lost (found 2026-11-25):** codex `f1-engine` ran under the built-in `-s workspace-write`: `.git` read-only, no network. It asked to work in a writable clone in /tmp and the operator agreed. The coddy `verify` node (kimi-k2.6), told to work only in a throwaway dir under /tmp, ended with `rm -rf /tmp/zori-one-engine.HVi7bt` as "cleanup" (its session `messages.json`, 14:21 UTC). Fixed at the root: codex now defaults to `danger-full-access`. Still open: the verify prompt must forbid deleting anything outside its own directory; partial diffs of `67d351d..7013642` survive in the verify session's `tool_calls/*/result.md`, and codex rollouts for the run are in `~/.codex/sessions/2026/11/25/`.

**Verified:** `git branch -a`, the five hashes and `ls /private/tmp` in this checkout on 2026-11-25 — nothing found.

**Not verified:** nothing left to check — the human decided on 2026-11-25 not to recover the branch but to redo it.

**Open:**
- Deferred review items: public `on_event` field with an implicit console default; heartbeat output interleaving with an open terminal prompt; SIGHUP leaving child processes ([thread](sighup-orphan-children.md)).
- Rejected on purpose (revisit only on new evidence): auto-answer after EOF, capped deadline extensions, finite default question timeout.
- Not done: live provider-backed run; console checked in a real browser; clean-environment replay.
- `web/index.html` split — deferred, tied to D14 ([thread](console-ui-redesign.md)).
- Three rustdoc link warnings on private items (seen on the lost branch).

**Next step:** push `refactor/one-engine` (now F1+F2) so it can never be lost again, redo F4 on it, then merge on the human's approval and close the deferred review items.
