// Tiny observable store + every action the UI can take. A live build swaps
// the mock source for GET /api/runs/:id + SSE; the actions map onto the
// existing endpoints (steer, answer, rerun, rerun-stale, flows/boot).
import { useEffect, useReducer } from './vendor/preact-htm.js';
import * as D from './data.js';

const clone = (v) => JSON.parse(JSON.stringify(v));
const subs = new Set();

export let S = {
  run: clone(D.run),
  transcripts: clone(D.transcripts),
  chat: clone(D.plannerChat),
  logs: clone(D.logs),
  sel: ['agent-ux'],
  view: { x: 90, y: 90, k: 1 },
  left: null, bottom: 'timeline', right: 'node',
  size: { left: 268, bottom: 232, right: 408 },
  inbox: false, palette: null, compare: null, quick: null,
  drag: null, toast: null, timeline: { attempts: true, nudges: true, costs: false, zoom: 1 },
};
const undo = [];

export function set(patch) {
  S = { ...S, ...(typeof patch === 'function' ? patch(S) : patch) };
  subs.forEach((f) => f());
}
export function useStore() {
  const [, force] = useReducer((x) => x + 1, 0);
  useEffect(() => { subs.add(force); return () => subs.delete(force); }, []);
  return S;
}

const nodes = () => S.run.nodes;
function setNodes(fn, remember = true) {
  if (remember) undo.push(clone(S.run.nodes)), undo.length > 50 && undo.shift();
  const next = clone(S.run.nodes);
  fn(next);
  set({ run: { ...S.run, nodes: next } });
}
let toastT;
export function toast(text, kind = 'info') {
  clearTimeout(toastT);
  set({ toast: { text, kind, at: Date.now() } });
  toastT = setTimeout(() => set({ toast: null }), 2600);
}
function log(kind, text) { set({ logs: [...S.logs, [S.run.elapsed, kind, text]] }); }

// ── derived ────────────────────────────────────────────────────────────────
export const edges = () => Object.values(nodes()).flatMap((n) => n.depends_on.filter((d) => nodes()[d]).map((d) => [d, n.id]));
export function attention() {
  const out = [];
  for (const n of Object.values(nodes())) {
    for (const q of n.questions || []) if (!q.closed && q.answer == null) out.push({ kind: 'question', node: n, q });
    if (n.status === 'failed') out.push({ kind: 'failed', node: n });
    if (n.approval && n.status === 'waiting') out.push({ kind: 'approval', node: n });
  }
  return out;
}
export const urgent = () => attention().filter((a) => a.kind !== 'failed' || !a.node.optional).length;
export const drafts = () => Object.values(nodes()).filter((n) => n.status === 'draft');
function ancestors(id, seen = new Set()) {
  for (const d of nodes()[id]?.depends_on || []) if (!seen.has(d)) seen.add(d), ancestors(d, seen);
  return seen;
}

// ── panels & selection ─────────────────────────────────────────────────────
export function toggle(side, id) { set({ [side]: S[side] === id ? null : id }); }
export function select(id, additive = false) {
  if (!id) return set({ sel: [] });
  const sel = additive ? (S.sel.includes(id) ? S.sel.filter((s) => s !== id) : [...S.sel, id]) : [id];
  set({ sel, right: sel.length && !additive && S.right == null ? 'node' : S.right });
}
export function focusNode(id) { select(id); set({ right: 'node' }); }

// ── graph edits ────────────────────────────────────────────────────────────
export function uniqueId(base) {
  const b = base.replace(/[^a-z0-9-]+/gi, '-').toLowerCase();
  if (!nodes()[b]) return b;
  for (let i = 2; ; i++) if (!nodes()[`${b}-${i}`]) return `${b}-${i}`;
}
function freeSpot(x, y) {
  const hit = (yy) => Object.values(nodes()).some((n) => Math.abs(n.x - x) < 256 && Math.abs(n.y - yy) < 150);
  for (let i = 0; i < 12; i++) for (const yy of [y + i * 160, y - i * 160]) if (!hit(yy)) return yy;
  return y;
}
export function addNode({ harness, model, x, y, from, id }) {
  y = freeSpot(x, y);
  const nid = id || uniqueId(harness === 'devin' ? 'devin' : `${harness}-${model.split('-')[0]}`);
  setNodes((ns) => {
    ns[nid] = {
      id: nid, harness, model, status: 'draft', depends_on: from ? [from] : [], optional: false, x, y,
      secs: 0, calls: 0, cost: 0, tokens: 0, activity: 'draft — write the brief', spark: [], attempts: [], nudges: [],
      in: from ? `\${steps.${from}.output}\n\n` : '', born: Date.now(),
    };
  });
  set({ sel: [nid], right: 'node' });
  log('level', `+ node ${nid} (${harness} · ${model})${from ? ` ← ${from}` : ''}`);
  return nid;
}
export function connect(from, to) {
  if (from === to) return 'self';
  if (nodes()[to].depends_on.includes(from)) return 'dup';
  if (ancestors(from).has(to)) return 'cycle';
  setNodes((ns) => { ns[to].depends_on.push(from); ns[to].newEdge = { from, at: Date.now() }; });
  return null;
}
export function disconnect(from, to) { setNodes((ns) => { ns[to].depends_on = ns[to].depends_on.filter((d) => d !== from); }); }
export function moveNodes(ids, dx, dy, remember) {
  setNodes((ns) => ids.forEach((id) => { ns[id].x += dx; ns[id].y += dy; }), remember);
}
export function snapNodes(ids) { setNodes((ns) => ids.forEach((id) => { ns[id].x = Math.round(ns[id].x / 10) * 10; ns[id].y = Math.round(ns[id].y / 10) * 10; }), false); }
export function removeSelected() {
  if (S.selEdge) { disconnect(...S.selEdge); set({ selEdge: null }); return toast('Edge removed · ⌘Z to undo'); }
  if (!S.sel.length) return;
  const ids = S.sel;
  setNodes((ns) => { ids.forEach((id) => delete ns[id]); Object.values(ns).forEach((n) => (n.depends_on = n.depends_on.filter((d) => !ids.includes(d)))); });
  set({ sel: [] });
  toast(`Removed ${ids.join(', ')} · ⌘Z to undo`);
}
export function duplicate(id) {
  const n = nodes()[id];
  return addNode({ harness: n.harness, model: n.model, x: n.x + 30, y: n.y + 150, id: uniqueId(id) });
}
export function undoLast() {
  const prev = undo.pop();
  if (!prev) return toast('Nothing to undo');
  set({ run: { ...S.run, nodes: prev } });
}
export function updateNode(id, patch) { setNodes((ns) => Object.assign(ns[id], patch), false); }

// ── run control ────────────────────────────────────────────────────────────
function start(ns, id, extra = {}) {
  const n = ns[id];
  Object.assign(n, { status: 'running', secs: 0, doneAt: 14, activity: 'starting…', exit_code: null, ...extra });
  n.attempts = [...n.attempts.filter((a) => a.kind !== 'waiting'), { n: n.attempts.length + 1, from: S.run.elapsed, to: null, est: S.run.elapsed + 90, kind: 'running' }];
}
export function runDrafts() {
  const ds = drafts().map((d) => d.id);
  if (!ds.length) return;
  setNodes((ns) => ds.forEach((id) => (ns[id].depends_on.every((d) => ns[d]?.status === 'done') ? start(ns, id) : Object.assign(ns[id], { status: 'pending', activity: 'queued' }))));
  ds.forEach((id) => log('start', `[${id}] ${nodes()[id].status === 'running' ? 'started' : 'queued'} (harness=${nodes()[id].harness})`));
  toast(`Launched ${ds.length} node${ds.length > 1 ? 's' : ''}`);
}
export function retry(id, model) {
  setNodes((ns) => start(ns, id, model ? { model } : {}));
  log('start', `[${id}] retry${model ? ` with ${model}` : ''}`);
  toast(`${id}: retrying${model ? ` on ${model}` : ''}`);
}
export function answer(id, text) {
  setNodes((ns) => { ns[id].questions.forEach((q) => { if (!q.closed) Object.assign(q, { answer: text, closed: true }); }); ns[id].activity = `answer received: “${text}”`; ns[id].doneAt = ns[id].secs + 12; });
  S.transcripts[id] = [...(S.transcripts[id] || []), { kind: 'human', body: text }];
  log('ask', `[${id}] answered: ${text}`);
  toast(`Answer sent to ${id}`);
}
export function approve(id, ok) {
  setNodes((ns) => { const n = ns[id]; n.approval = null; ok ? start(ns, id) : Object.assign(n, { status: 'skipped', activity: 'rejected by operator' }); });
  log(ok ? 'start' : 'fail', `[${id}] ${ok ? 'approved' : 'rejected'}`);
}
export function steer(id, text) {
  S.transcripts[id] = [...(S.transcripts[id] || []), { kind: 'human', body: text }];
  setNodes((ns) => start(ns, id, { activity: 'steered — continuing session' }), false);
  log('watch', `[${id}] steer: ${text}`);
  setTimeout(() => {
    S.transcripts[id] = [...S.transcripts[id], { kind: 'agent', body: `Got it — ${text.toLowerCase().replace(/[.!]$/, '')}. Updating the result.` }];
    set({});
  }, 1400);
}
export function rerunStale() {
  const st = Object.values(nodes()).filter((n) => n.status === 'stale').map((n) => n.id);
  if (!st.length) return toast('Nothing stale');
  setNodes((ns) => st.forEach((id) => (ns[id].depends_on.every((d) => ns[d]?.status === 'done') ? start(ns, id) : Object.assign(ns[id], { status: 'pending', activity: 'queued after upstream' }))));
  toast(`Queued ${st.join(', ')}`);
}
export function stopRun() {
  setNodes((ns) => Object.values(ns).forEach((n) => n.status === 'running' && Object.assign(n, { status: 'failed', activity: 'stopped by operator', exit_code: 130 })));
  set({ run: { ...S.run, status: 'failed' } });
  toast('Run stopped', 'warn');
}

// ── planner (fake supermodel) ──────────────────────────────────────────────
export function plannerSend(text) {
  set({ chat: [...S.chat, { role: 'human', text }, { role: 'agent', typing: true }] });
  setTimeout(() => {
    const chat = S.chat.slice(0, -1);
    chat.push({ role: 'agent', text: 'Two additions: a security pass on the evidence, and a second opinion on the report from a different vendor so critique compares two takes.',
      proposal: { id: Date.now(), status: 'proposed', nodes: [
        { id: 'security-scan', harness: 'devin', model: 'opus-5.5', from: 'evidence', dx: 310, dy: -170 },
        { id: 'report-b', harness: 'codex', model: 'gpt-6.2-sol', from: 'agent-ux', dx: 310, dy: 330, to: 'critique' },
      ] } });
    set({ chat });
  }, 900);
}
export function applyProposal(p) {
  p.nodes.forEach((pn, i) => setTimeout(() => {
    const src = nodes()[pn.from];
    if (nodes()[pn.id]) return;
    addNode({ harness: pn.harness, model: pn.model, x: src.x + pn.dx, y: src.y + pn.dy, from: pn.from, id: pn.id });
    if (pn.to) connect(pn.id, pn.to);
  }, i * 220));
  p.status = 'applied';
  set({});
  setTimeout(() => import('./canvas.js').then((c) => c.fitView()), p.nodes.length * 220 + 80);
}

// ── live simulation ────────────────────────────────────────────────────────
const ACTS = ['reading .omut/decisions.md', 'rg -n "created_at" .omut', 'drafting section 2/3', 'cross-checking evidence.md', 'writing report.md'];
setInterval(() => {
  if (S.run.status !== 'running' || S.drag) return;
  const ns = clone(S.run.nodes);
  let changed = false;
  for (const n of Object.values(ns)) {
    if (n.status !== 'running') continue;
    changed = true;
    n.secs += 1;
    n.spark = [...n.spark.slice(-43), 0.25 + Math.random() * 0.75];
    if (n.secs % 7 === 0) n.calls += 1, n.cost = +(n.cost + 0.01).toFixed(2), n.tokens += 180 + Math.round(Math.random() * 200);
    if (n.secs % 9 === 0 && !(n.questions || []).some((q) => !q.closed)) n.activity = ACTS[(n.secs / 9) % ACTS.length];
    if (n.doneAt != null && n.secs >= n.doneAt) {
      Object.assign(n, { status: 'done', activity: 'done — wrote result', artifact: { name: `${n.id}.md`, size: '1.4 KB' } });
      n.attempts.at(-1).to = S.run.elapsed; n.attempts.at(-1).kind = 'done';
      for (const m of Object.values(ns)) if (m.status === 'pending' && m.depends_on.every((d) => ns[d]?.status === 'done')) start(ns, m.id);
    }
  }
  set({ run: { ...S.run, elapsed: S.run.elapsed + 1, window: Math.max(S.run.window, S.run.elapsed + 60), nodes: changed ? ns : S.run.nodes } });
}, 1000);
