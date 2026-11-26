---
status: done
updated: 2026-11-26
by: codex/GPT-6
---
# Theme-painted checkboxes

**Goal:** Style the independent timeline filters as checkboxes and the Brief `optional` flag as a switch. Preserve their native checkbox behavior.

**Where we got:** The original change in fb370f4 added the CSS and the `sw` class. Review made the unchecked border and focus outline more visible, retained the native checkbox in forced colors, and let the timeline header and switch label wrap at narrow widths. The update shipped in [v0.3.1](https://github.com/metravod/zori/releases/tag/v0.3.1).

**Verified:** Ran the live console on the completed echo flow in Chrome, Firefox, Safari and Playwright WebKit. Checked native names and states, keyboard focus and Space, the timeline filters, and the Brief switch. In Safari at 200% both panels can be open and the controls are visible. Chrome emulation of forced colors selects the native controls. Verified 64 web cases, 219 Rust tests, and Clippy with warnings treated as errors.

**Release verification:** The [release workflow](https://github.com/metravod/zori/actions/runs/45852465390) passed all four native builds and smoke runs. The four published archives matched SHA256SUMS, and the macOS arm64 archive reported `zori 0.3.1`. The [tap update](https://github.com/metravod/homebrew-tap/actions/runs/45853572930) published a formula with matching URLs and digests. Homebrew upgraded 0.3.0 to 0.3.1, and the installed binary reported `zori 0.3.1`.
