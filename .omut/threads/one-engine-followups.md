---
status: active
updated: 2026-11-25
by: devin/opus-5.5
---
# One engine: merge and follow-ups

**Goal:** land `refactor/one-engine` (F1/F2/F4 of the [review](code-review-2026-11-24.md)) in main and close what the review deferred.

**Where we got:** the branch had five commits on base d76980c: 67d351d (F1), bbf5dba and dc33170 (F2), c602d57 (F4), 7013642 (review fixes). It was worked in `/private/tmp/zori-one-engine.HVi7bt`. That clone no longer exists, `git cat-file` finds none of the five hashes here, and `origin` has only `main`. The branch was never pushed. Uncommitted project-meta edits made in that clone are lost too.

**How it was lost (found 2026-11-25):** codex `f1-engine` ran under the built-in `-s workspace-write`: `.git` read-only, no network. It asked to work in a writable clone in /tmp and the operator agreed. The coddy `verify` node (kimi-k2.6), told to work only in a throwaway dir under /tmp, ended with `rm -rf /tmp/zori-one-engine.HVi7bt` as "cleanup" (its session `messages.json`, 14:21 UTC). Fixed at the root: codex now defaults to `danger-full-access`. Still open: the verify prompt must forbid deleting anything outside its own directory; partial diffs of `67d351d..7013642` survive in the verify session's `tool_calls/*/result.md`, and codex rollouts for the run are in `~/.codex/sessions/2026/11/25/`.

**Verified:** `git branch -a`, the five hashes and `ls /private/tmp` in this checkout on 2026-11-25 — nothing found.

**Not verified:** nothing left to check — the human decided on 2026-11-25 not to recover the branch but to redo it.

**Open:**
- Deferred review items: public `on_event` field with an implicit console default; heartbeat output interleaving with an open terminal prompt; SIGHUP leaving child processes ([thread](sighup-orphan-children.md)).
- Rejected on purpose (revisit only on new evidence): auto-answer after EOF, capped deadline extensions, finite default question timeout.
- Not done: live provider-backed run; console checked in a real browser; clean-environment replay.
- `web/index.html` split — deferred, tied to D14 ([thread](console-ui-redesign.md)).
- Three rustdoc link warnings on private items (seen on the lost branch).

**Next step:** add to the verify node of `zori-flows/zori-review-f1-f4.yml` a ban on deleting anything outside its own directory, rerun the flow on the version with the codex sandbox fix, push the branch as soon as F1 lands, then merge.
