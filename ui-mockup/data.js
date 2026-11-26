// Fake data shaped like what the console's run/state API would expose.
// One fully populated run ("omut weak-spot audit") mirrors the references;
// the other runs are history rows only.

export const runs = [
  {
    id: "omut-weak-spot-audit",
    name: "omut weak-spot audit",
    status: "running",
    progress: "4/6 done",
    elapsed: "06:27",
    cost: "$0.57",
    tokens: "18.4k tok",
    current: true,
  },
  { id: "omut-review", name: "omut-review", status: "done", progress: "6 tasks", elapsed: "14m 32s", ago: "2h ago" },
  { id: "zori-readme-audit", name: "zori-readme-audit", status: "running", progress: "5 tasks", elapsed: "6m 05s", ago: "now" },
  { id: "cli-router-benchmark", name: "cli-router-benchmark", status: "failed", progress: "4 tasks", elapsed: "9m 11s", ago: "5h ago" },
  { id: "rag-spike", name: "rag-spike", status: "waiting", progress: "3 tasks", elapsed: "1m 18s", ago: "yesterday" },
];

export const pipelines = [
  { name: "audit repo", shape: "evidence → report → final", nodes: "3 nodes", models: "2 models" },
  { name: "release notes", shape: "changelog « diff → publish", nodes: "3 nodes", models: "1 model" },
  { name: "bug triage", shape: "repro → analyze → fix plan", nodes: "3 nodes", models: "2 models" },
  { name: "design review", shape: "ux → tradeoffs → critique", nodes: "3 nodes", models: "3 models" },
];

// statuses: running | done | failed | waiting | queued | approval
export const run = {
  name: "omut weak-spot audit",
  cwd: "~/oss_projects/zori",
  status: "running",
  elapsed: "06:27",
  cost: "$0.57",
  tokens: "18.4k tok",
  question: { from: "report", text: "include open issues from /docs?" },
  dag: {
    meta: "6 nodes · 3 parallel · critical path 8m 14s",
    // x,y in percent of the dag canvas; edges by node id
    nodes: [
      { id: "evidence", x: 4, y: 12, harness: "coddy · kimi-k2.6", status: "done", note: "done — wrote evidence.md" },
      { id: "agent-ux", x: 4, y: 62, harness: "codex · gpt-5.2-sol", status: "done", note: "done — 3 UX gaps found" },
      { id: "tool-design", x: 38, y: 12, harness: "opencode · glm-5.3-hs", status: "failed", note: "exit 1: rate limit (429)" },
      { id: "report", x: 38, y: 62, harness: "codex · gpt-5.2-astra", status: "running", note: "reading evidence + notes" },
      { id: "critique", x: 70, y: 37, harness: "coddy · kimi-k2.6", status: "queued", note: "queued · est. $0.06" },
      { id: "final", x: 70, y: 78, harness: "codex · gpt-5.2-astra", status: "approval", note: "parked for your review" },
    ],
    edges: [
      ["evidence", "tool-design"], ["evidence", "report"], ["agent-ux", "report"],
      ["tool-design", "critique"], ["report", "critique"], ["report", "final"],
    ],
  },
  agents: [
    {
      id: "evidence", name: "evidence", harness: "coddy · kimi-k2.6", status: "done",
      activity: "done — wrote evidence.md", time: "6m 18s", calls: 12, cost: "$0.08", tokens: "3.8k",
    },
    {
      id: "agent-ux", name: "agent-ux", harness: "codex · gpt-5.2-sol", status: "done",
      activity: "done — 3 UX gaps found", time: "4m 02s", calls: 9, cost: "$0.21", tokens: "6.8k",
      detail: {
        tabs: ["Transcript", "Files (3)", "Artifacts (1)"],
        transcript: [
          { kind: "tool", label: "tool-call bash", detail: "rg -n 'Next' .omut — 14 lines", body: "brief.md:41  Next: …\ndecisions.md:12  …" },
          { kind: "tool", label: "tool-call read_file", detail: ".omut/brief.md", body: "(file contents elided)" },
          { kind: "edit", label: "file-edit .omut/audit_notes.md", detail: "+18 −4", diff: [
            ["ctx", "@@ -12,7 +12,11 @@"],
            ["del", "- Next: update the plan and split the work."],
            ["add", "+ Next:"],
            ["add", "+ - align with the latest decisions (see .omut/decisions.md);"],
            ["add", "+ - add missing timestamps;"],
            ["add", "+ - clarify handoff to the report stage."],
          ] },
          { kind: "reason", label: "reasoning (truncated)", body: "The brief asks for a weak-spot audit. I focused on project memory, decision records, and handoff clarity. Found several gaps…" },
        ],
        report: {
          title: "Final report",
          sections: [
            { h: "Weak spots", items: [
              "<b>Stale project memory.</b> The context file hasn't been updated for ~2 weeks and no longer reflects recent changes.",
              "<b>Missing decision timestamps.</b> Several entries in decisions.md lack created_at, which makes a reliable history impossible.",
              "<b>Unclear handoff boundaries.</b> Inputs/outputs between tool-design → report + critique are not explicit, which leads to rework.",
            ] },
            { h: "Recommendations", items: [
              "Update and keep the project memory fresh.",
              "Add created_at timestamps to all decisions.",
              "Define clear I/O contracts for each stage.",
            ] },
          ],
        },
      },
    },
    {
      id: "tool-design", name: "tool-design", harness: "opencode · glm-5.3-hs", status: "failed",
      activity: "exit 1: rate limit exceeded (429)", time: "2m 04s", calls: 5, cost: "$0.00", tokens: "1.1k",
      actions: ["Retry with other model"],
    },
    {
      id: "report", name: "report", harness: "codex · gpt-5.2-astra", status: "running",
      activity: "▸ reading .omut/decisions.md", time: "4m 44s", calls: 6, cost: "$0.28", tokens: "5.2k",
    },
    {
      id: "critique", name: "critique", harness: "coddy · kimi-k2.6", status: "queued",
      activity: "waiting on report + tool-design", time: "—", calls: 0, cost: "est. $0.06", tokens: "—", stale: true,
    },
    {
      id: "final", name: "final", harness: "codex · gpt-5.2-astra", status: "approval",
      activity: "waiting for your review of the report", time: "0m 31s", calls: 1, cost: "$0.00", tokens: "0.4k",
      actions: ["Approve", "Reject"],
    },
  ],
  // timeline in seconds from run start
  timeline: {
    total: 480, ticks: 8,
    lanes: [
      { node: "evidence", segments: [{ from: 0, to: 378, kind: "done" }] },
      { node: "agent-ux", segments: [{ from: 20, to: 262, kind: "done" }] },
      { node: "tool-design", segments: [{ from: 40, to: 90, kind: "retry" }, { from: 105, to: 224, kind: "failed" }], nudges: [70] },
      { node: "report", segments: [{ from: 120, to: 404, kind: "running" }], nudges: [250, 330] },
      { node: "critique", segments: [{ from: 404, to: 480, kind: "queued" }] },
      { node: "final", segments: [{ from: 300, to: 331, kind: "waiting" }] },
    ],
  },
  inbox: [
    { kind: "q", label: "QUESTION", who: "report · gpt-5.2-astra",
      text: "include open issues from /docs? ADR-014 mentions Redis — parse it as current?" },
    { kind: "f", label: "FAILED", who: "tool-design · glm-5.3-hs",
      text: "exit 1: rate limit exceeded (429)", actions: ["Retry with other model", "Skip (optional branch)"] },
    { kind: "a", label: "APPROVAL", who: "final · parked",
      text: "Write .omut/AUDIT.md + 2 patch files?", actions: ["Approve", "Reject"] },
  ],
  finalReport: {
    source: "report · gpt-5.2-astra",
    items: [
      "<b>Silent timeout in runner.rs:88.</b> Process exits 0 on watchdog timeout; CLI should return 124 + print resume hint.",
      "<b>Unguarded retry loop (exec.rs:42).</b> 429 backoff resets on a partial stream.",
      "<b>Stale .omut/run.lock after SIGINT.</b> Blocks re-runs until the lock file is deleted by hand.",
    ],
  },
};
