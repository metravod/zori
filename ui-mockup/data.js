// Fake data for the console prototype.
//
// `run` is shaped like GET /api/runs/:id (RunState: run_id, flow_name, status,
// nodes{id → TaskNode}). Fields marked EXTRA are not in the API yet and are
// what the redesign needs from the backend: canvas position, calls, cost,
// tokens, last activity, an activity histogram, attempts with watchdog
// nudges, artifacts and a structured transcript.

const spark = (seed, n = 44, base = 0.25) => {
  let s = seed;
  return Array.from({ length: n }, () => {
    s = (s * 9301 + 49297) % 233280;
    return Math.min(1, base + (s / 233280) * 0.75);
  });
};

export const fleet = [
  { harness: 'coddy', models: ['kimi-k2.6', 'glm-5.3', 'deepseek-v4'] },
  { harness: 'codex', models: ['gpt-6.2-sol', 'gpt-6.2-astra', 'gpt-6-mini'] },
  { harness: 'opencode', models: ['glm-5.3-highspeed', 'qwen-4-coder'] },
  { harness: 'devin', models: ['opus-5.5', 'sonnet-5.5', 'swe-2'] },
  { harness: 'zcode', models: ['glm-5.3'] },
];
export const starred = ['coddy/kimi-k2.6', 'codex/gpt-6.2-sol', 'devin/opus-5.5'];

export const history = [
  { id: 'omut-weak-spot-audit', name: 'omut weak-spot audit', status: 'running', tasks: 6, took: '6m 27s', ago: 'now', current: true },
  { id: 'omut-review', name: 'omut-review', status: 'done', tasks: 6, took: '14m 32s', ago: '2h ago' },
  { id: 'zori-readme-audit', name: 'zori-readme-audit', status: 'running', tasks: 5, took: '6m 05s', ago: 'now' },
  { id: 'cli-router-benchmark', name: 'cli-router-benchmark', status: 'failed', tasks: 4, took: '9m 11s', ago: '5h ago' },
  { id: 'rag-spike', name: 'rag-spike', status: 'waiting', tasks: 3, took: '1m 18s', ago: 'yesterday' },
  { id: 'f1-live', name: 'f1-live', status: 'done', tasks: 3, took: '1m 02s', ago: 'yesterday' },
];

export const pipelines = [
  { name: 'audit repo', shape: ['evidence', 'report', 'final'], models: 2 },
  { name: 'release notes', shape: ['changelog', 'diff', 'publish'], models: 1 },
  { name: 'bug triage', shape: ['repro', 'analyze', 'fix plan'], models: 2 },
  { name: 'design review', shape: ['ux', 'tradeoffs', 'critique'], models: 3 },
  { name: 'fanout-review', shape: ['codex ∥ coddy', 'crosscheck'], models: 2 },
];

const T0 = 0; // seconds from run start
export const run = {
  run_id: 'omut-weak-spot-audit-1795684458482',
  flow_name: 'omut weak-spot audit',
  cwd: '~/oss_projects/zori',
  branch: 'main',
  status: 'running',
  elapsed: 387, // EXTRA: seconds since start
  window: 480, // EXTRA: timeline span
  nodes: {
    evidence: {
      id: 'evidence', harness: 'coddy', model: 'kimi-k2.6', status: 'done', depends_on: [], optional: false,
      x: 0, y: 0, secs: 190, calls: 12, cost: 0.08, tokens: 3900,
      activity: 'scanned repo, collected 41 refs', artifact: { name: 'evidence.md', size: '3.8 KB' },
      spark: spark(3), attempts: [{ n: 1, from: T0 + 15, to: 205, kind: 'done' }], nudges: [120],
      in: 'Collect evidence about weak spots in .omut/ project memory: stale claims, missing timestamps, unclear handoffs.',
    },
    'agent-ux': {
      id: 'agent-ux', harness: 'codex', model: 'gpt-6.2-sol', status: 'done', depends_on: [], optional: false,
      x: 0, y: 160, secs: 242, calls: 9, cost: 0.21, tokens: 6800,
      activity: 'finalizing audit notes', artifact: { name: 'notes.json', size: '2.1 KB' },
      spark: spark(11), attempts: [{ n: 1, from: 35, to: 277, kind: 'done' }], nudges: [],
      in: 'Audit the console UX of zori from an operator point of view. List gaps.',
    },
    'tool-design': {
      id: 'tool-design', harness: 'opencode', model: 'glm-5.3-highspeed', status: 'failed', depends_on: [], optional: true,
      x: 0, y: 320, secs: 161, calls: 7, cost: 0.12, tokens: 1100, exit_code: 1,
      activity: 'exit 1: rate limit (429)', spark: spark(29, 44, 0.1),
      attempts: [{ n: 1, from: 25, to: 114, kind: 'failed' }, { n: 2, from: 200, to: 272, kind: 'failed' }], nudges: [160],
      in: 'Review the tool surface (CLI flags, flow YAML) and propose simplifications.',
    },
    report: {
      id: 'report', harness: 'coddy', model: 'kimi-k2.6', status: 'running', depends_on: ['evidence', 'agent-ux', 'tool-design'], optional: false,
      x: 340, y: 160, secs: 104, calls: 6, cost: 0.10, tokens: 4200,
      activity: 'reading .omut/decisions.md', spark: spark(7, 44, 0.35),
      attempts: [{ n: 1, from: 283, to: null, est: 440, kind: 'running' }], nudges: [330],
      questions: [{ id: 0, text: 'Include open issues from /docs? ADR-014 mentions Redis — treat it as current?', options: ['yes', 'no, skip /docs'], answer: null, closed: false }],
      in: 'Merge the evidence and the UX notes into a ranked weak-spot report.\n\n${steps.evidence.output}\n${steps.agent-ux.output}',
    },
    critique: {
      id: 'critique', harness: 'codex', model: 'gpt-6.2-astra', status: 'stale', depends_on: ['report'], optional: false,
      x: 630, y: 160, secs: 0, calls: 0, cost: 0, tokens: 0, activity: 'waiting for re-run', note: 'upstream changed',
      spark: spark(5, 44, 0.05), attempts: [{ n: 1, from: 300, to: 345, kind: 'stale' }], nudges: [],
      in: 'Critique the report. What is overstated, what is missing?',
    },
    final: {
      id: 'final', harness: 'coddy', model: 'kimi-k2.6', status: 'waiting', depends_on: ['critique'], optional: false,
      x: 920, y: 160, secs: 0, calls: 0, cost: 0, tokens: 0, activity: 'parked for approval', approval: 'Write .omut/AUDIT.md + 2 patch files?',
      spark: spark(2, 44, 0.04), attempts: [{ n: 1, from: 345, to: null, kind: 'waiting' }], nudges: [],
      in: 'Write the final audit to .omut/AUDIT.md and propose patches.',
    },
  },
};

// EXTRA: structured transcript (today the API only has turns[] as markdown).
export const transcripts = {
  'agent-ux': [
    { kind: 'tool', title: 'bash', detail: "rg -n 'Next' .omut — 14 lines", body: "brief.md:31  ## Next\nbrief.md:32  - Release F1+F2 …\nthreads/console-ui-redesign.md:12  Proposed order …" },
    { kind: 'tool', title: 'read_file', detail: '.omut/brief.md', body: '(152 lines)' },
    { kind: 'edit', title: '.omut/audit_notes.md', add: 18, del: 4, diff: [
      ['ctx', '@@ -12,7 +12,11 @@'],
      ['del', '- Next: update the plan and split the work.'],
      ['add', '+ Next:'],
      ['add', '+   - align with the latest decisions (see .omut/decisions.md);'],
      ['add', '+   - add missing timestamps;'],
      ['add', '+   - clarify handoff to the report stage.'],
    ] },
    { kind: 'reason', title: 'reasoning', body: 'The brief asks for a weak-spot audit. I focused on project memory, decision records and handoff clarity. Found several gaps …' },
    { kind: 'report', title: 'Final report', body: [
      ['h', 'Weak spots'],
      ['li', '**Stale project memory.** `.omut/context.md` has not been updated for ~2 weeks and no longer reflects recent changes.'],
      ['li', '**Missing decision timestamps.** Several entries in `decisions.md` lack `created_at`.'],
      ['li', '**Unclear handoff boundaries.** Inputs/outputs between tool-design → report → critique are not explicit.'],
      ['h', 'Recommendations'],
      ['li', 'Keep the project memory fresh.'],
      ['li', 'Add `created_at` to all decisions.'],
      ['li', 'Define clear I/O contracts for each stage.'],
    ] },
  ],
  evidence: [
    { kind: 'tool', title: 'bash', detail: 'fd . .omut -e md — 23 files', body: '.omut/brief.md\n.omut/decisions.md\n.omut/log.md\n…' },
    { kind: 'tool', title: 'read_file', detail: '.omut/decisions.md', body: '(41 lines)' },
    { kind: 'report', title: 'Final report', body: [
      ['h', 'Evidence'],
      ['li', '**12 claims** in `brief.md` have no `how:` check.'],
      ['li', '**D14** has been open for 2 days and gates the redesign.'],
      ['li', '**3 threads** have no next step.'],
    ] },
  ],
  'tool-design': [
    { kind: 'tool', title: 'bash', detail: 'zori --help', body: 'Run a task across several CLI coding agents as a small DAG …' },
    { kind: 'error', title: 'exit 1', body: 'Error: 429 Too Many Requests — rate limit exceeded for glm-5.3-highspeed (retry after 60s)' },
  ],
  report: [
    { kind: 'tool', title: 'read_file', detail: 'evidence.md', body: '(3.8 KB)' },
    { kind: 'tool', title: 'read_file', detail: 'notes.json', body: '(2.1 KB)' },
    { kind: 'tool', title: 'read_file', detail: '.omut/decisions.md', body: '(running …)', live: true },
    { kind: 'ask', title: 'ASK', body: 'Include open issues from /docs? ADR-014 mentions Redis — treat it as current?' },
  ],
};

export const plannerChat = [
  { role: 'human', text: 'Audit omut for weak spots: memory freshness, decision hygiene, handoffs. Three agents in parallel, then a report.' },
  { role: 'agent', text: 'Proposed a 6-node flow: three independent probes (evidence, agent-ux, tool-design) → report → critique → final with an approval gate before any file is written.' },
];

export const logs = [
  [15, 'level', '── level 0: evidence, agent-ux, tool-design'],
  [15, 'start', '[evidence] started (harness=coddy)'],
  [25, 'start', '[tool-design] started (harness=opencode)'],
  [35, 'start', '[agent-ux] started (harness=codex)'],
  [114, 'fail', '[tool-design] failed exit 1: rate limit (429)'],
  [120, 'watch', '[evidence] watchdog: quiet 60s — nudged'],
  [160, 'watch', '[tool-design] retry 1/2 in 40s'],
  [205, 'done', '[evidence] done wrote evidence.md'],
  [272, 'fail', '[tool-design] failed exit 1: rate limit (429) — optional, degraded'],
  [277, 'done', '[agent-ux] done 3 UX gaps found'],
  [283, 'level', '── level 1: report'],
  [283, 'start', '[report] started (harness=coddy)'],
  [330, 'watch', '[report] watchdog: deadline extended on fresh output'],
  [371, 'ask', '[report] ASK: Include open issues from /docs?'],
];
