---
status: active
updated: 2026-11-25
by: devin/sonnet-5.5
---
# One engine: merge and follow-ups

**Goal:** land `refactor/one-engine` (F1/F2/F4 of the [review](code-review-2026-11-24.md)) in main and close what the review deferred.

**Where we got:** the branch had five commits on base d76980c: 67d351d (F1), bbf5dba and dc33170 (F2), c602d57 (F4), 7013642 (review fixes). It was worked in `/private/tmp/zori-one-engine.HVi7bt`. That clone no longer exists, `git cat-file` finds none of the five hashes here, and `origin` has only `main`. The branch was never pushed. Uncommitted project-meta edits made in that clone are lost too.

**Verified:** `git branch -a`, the five hashes and `ls /private/tmp` in this checkout on 2026-11-25 — nothing found.

**Not verified:** whether the clone survives elsewhere; if not, the work must be redone (the stage reports describe it in detail).

**Open:**
- Deferred review items: public `on_event` field with an implicit console default; heartbeat output interleaving with an open terminal prompt; SIGHUP leaving child processes ([thread](sighup-orphan-children.md)).
- Rejected on purpose (revisit only on new evidence): auto-answer after EOF, capped deadline extensions, finite default question timeout.
- Not done: live provider-backed run; console checked in a real browser; clean-environment replay.
- `web/index.html` split — deferred, tied to D14 ([thread](console-ui-redesign.md)).
- Housekeeping from `omut check`: the brief is over its size budget; [stale-status thread](stale-status-after-dead-attempt.md) lacks frontmatter. Three rustdoc link warnings on private items.

**Next step:** ask the human where the clone went; if it is gone, redo F1 → F2 → F4 from the reports, push the branch, then merge.
