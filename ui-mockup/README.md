# Console UI mockup

Static, data-driven mockup of the redesigned zori console (see
[`.omut/threads/console-ui-redesign.md`](../.omut/threads/console-ui-redesign.md)).
Built to settle **D14** (native ES modules vs a framework with a build step)
before any real front-end work: the mockup is already split into native ES
modules with no bundler and no dependencies — click around and judge the
approach.

Not part of the binary: only `web/index.html` is embedded (`include_str!` in
`src/ui.rs`). This folder is pure design material.

Run: `python3 -m http.server` here, or Coddy's preview server.

Layout follows the two references:

- left: run history + saved pipelines
- center: run header, DAG, agent cards, timeline (Gantt)
- right: attention inbox + final report; clicking an agent card swaps it to
  the agent detail (transcript with collapsible tool calls, diff, steer box)
