---
status: active
updated: 2026-11-26
by: opencode/GLM-5.3
---
# Theme-painted checkboxes (branch `checkbox-theme-restyle`)

**Goal:** replace the two remaining native browser checkboxes in the
console with a theme-consistent control — timeline filters as real
checkboxes, the node Brief `optional` flag as a switch — without touching
any JS logic.

**Where we got:** implemented on branch `checkbox-theme-restyle`
(fb370f4, not pushed), from the plan written by the coddy/qwen3.8 session
(`zori-runs/new-2dflow-1795707093568/sessions/.../plans/checkbox-theme-restyle.plan.md`):
- `web/styles.css` — `accent-color` replaced by an `appearance:none`
  primitive following the `.btn`/`.inp` lifecycle (14px box, `--line-2`
  border, hover `#3d4856`, checked `--blue-d`, `.inp` focus ring on
  `:focus-visible`); check glyph is the `check` icon from icons.js drawn
  via CSS `mask`; `.ck.sw` switch variant (26×15 track, 11px knob, cyan
  `--cyan` mode accent like `.btn.go`).
- `web/panels.js:218` — the only JS change: label class `ck` → `ck sw`
  (kept `ck` so `.brief label:not(.ck)` keeps matching); timeline
  filters at panels.js:127 untouched.
- Verified: grep invariants (2 checkboxes, 0 `accent-color`), braces
  balanced, mask data-URI decodes to the exact icons.js path,
  `node --test web/*.test.mjs` green (64 cases). No Rust touched.

**Open:** operator review of the branch (requested before push). The
plan's browser acceptance is still to run at review time: visual pass in
Chrome/Firefox/Safari (Safari mandatory for `-webkit-appearance`/`mask`),
keyboard Tab/Space/focus-ring walkthrough on both panels, and 200% zoom.
Plan B if a browser drops the mask glyph: inline `<svg>` in the label
shown via `input:checked + svg`. If a light native trace appears after
`appearance:none`, add `color-scheme: dark` to `:root`.

**Next step:** review the branch; on approval push/merge, run the
browser checklist, close this thread. Deliberately out of scope: the
four native `<select>`s (app.js:146, panels.js:199, 289, 293) — a
separate task if wanted.
