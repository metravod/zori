// Console state and every action, against the zori API. The canvas shows one
// graph: the open run's nodes (live, from the SSE snapshot) plus client-side
// drafts. Drafts become real nodes on "Run": in a run via POST /patch, with no
// run open (compose mode) via POST /api/flows/boot.
import { useEffect, useReducer } from './vendor/preact-htm.js';
import { layout, wouldCycle, uniqueId, serverTask, freeSpot } from './graph.js';

const subs = new Set();
const blankFlow = () => ({ name: 'new-flow', tasks: [] });
export let S = {
  runs: [], runId: null, snap: null, flow: blankFlow(), conn: 'idle',
  drafts: {}, pos: {}, spark: {}, logs: [],
  harnesses: [], models: {}, fleet: [], pipelines: [],
  chat: null, chatSnap: null, plannerPick: null,
  sel: [], selEdge: null, view: { x: 80, y: 80, k: 1 }, glide: false,
  left: 'runs', bottom: null, right: null, size: { left: 268, bottom: 232, right: 420 },
  inbox: false, palette: null, compare: null, quick: null, drag: null, toast: null,
  timeline: { attempts: true, nudges: true, zoom: 1 },
};
export function set(patch) {
  S = { ...S, ...(typeof patch === 'function' ? patch(S) : patch) };
  subs.forEach((f) => f());
}
export function useStore() {
  const [, force] = useReducer((x) => x + 1, 0);
  useEffect(() => { subs.add(force); return () => subs.delete(force); }, []);
  return S;
}

// ── api ────────────────────────────────────────────────────────────────────
export async function api(path, body, method) {
  const r = await fetch(path, body === undefined && !method ? undefined : {
    method: method || 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body ?? {}),
  });
  const text = await r.text();
  if (!r.ok) throw new Error(text.replace(/^"|"$/g, '') || `${r.status} ${r.statusText}`);
  return text ? JSON.parse(text) : null;
}
let toastT;
export function toast(text, kind = 'info') {
  clearTimeout(toastT);
  set({ toast: { text: String(text), kind, at: Date.now() } });
  toastT = setTimeout(() => set({ toast: null }), kind === 'error' ? 6000 : 2800);
}
const fail = (e) => toast(e.message || e, 'error');
const enc = encodeURIComponent;
const now = () => Math.floor(Date.now() / 1000);

// ── derived graph ──────────────────────────────────────────────────────────
// One list of canvas nodes: run tasks merged with their live snapshot, then
// drafts. Every node carries x/y and the fields the cards render.
export function nodes(s = S) {
  const out = [];
  const tasks = s.flow?.tasks || [];
  for (const t of tasks) {
    const n = s.snap?.nodes?.[t.id];
    if (s.runId && !n) continue;
    const p = s.pos[t.id] || { x: 0, y: 0 };
    out.push({ ...(n || {}), task: t, id: t.id, harness: n?.harness || t.harness, model: n?.model ?? t.model, depends_on: n?.depends_on || t.depends_on || [],
      optional: n?.optional ?? !!t.optional, status: n?.status || 'draft', x: p.x, y: p.y, draft: !s.runId });
  }
  for (const d of Object.values(s.drafts)) out.push({ ...d, task: d, status: 'draft', draft: true, depends_on: d.depends_on || [] });
  return out;
}
export const byId = (id) => nodes().find((n) => n.id === id);
export function edges(ns = nodes()) {
  const ids = new Set(ns.map((n) => n.id)), out = [];
  for (const n of ns) for (const d of n.depends_on) if (ids.has(d)) out.push([d, n.id]);
  for (const n of ns) for (const f of n.feeds || []) if (ids.has(f)) out.push([n.id, f]);
  return out;
}
export const draftCount = () => Object.keys(S.drafts).length + (S.runId ? 0 : (S.flow?.tasks.length || 0));
export function attention() {
  const out = [];
  for (const n of Object.values(S.snap?.nodes || {})) {
    for (const q of n.questions || []) if (!q.closed && q.answer == null) out.push({ kind: 'question', node: n, q });
    if (n.status === 'failed') out.push({ kind: 'failed', node: n });
  }
  const stale = Object.values(S.snap?.nodes || {}).filter((n) => n.status === 'stale');
  if (stale.length) out.push({ kind: 'stale', nodes: stale });
  const p = S.chatSnap?.nodes?.chat?.proposal;
  if (p && p.status === 'proposed') out.push({ kind: 'proposal', proposal: p });
  return out;
}
export const urgent = () => attention().filter((a) => a.kind === 'question' || (a.kind === 'failed' && !a.node.optional)).length;

// ── bootstrapping ──────────────────────────────────────────────────────────
export async function boot() {
  const [harnesses, fleet] = await Promise.all([api('/api/harnesses'), api('/api/fleet')]).catch((e) => (fail(e), [[], { models: [] }]));
  set({ harnesses, fleet: fleet.models || [] });
  refreshRuns();
  refreshPipelines();
  setInterval(refreshRuns, 5000);
  const want = new URLSearchParams(location.hash.slice(1)).get('run');
  const runs = S.runs.filter((r) => !r.chat_title);
  if (want) openRun(want);
  else if (runs[0]?.status === 'running') openRun(runs[0].run_id);
}
export async function refreshRuns() {
  try { set({ runs: await api('/api/runs') }); } catch { /* the next tick retries */ }
}
export async function refreshPipelines() {
  try { set({ pipelines: await api('/api/flows') }); } catch (e) { fail(e); }
}
export async function loadModels(harness, force) {
  if (S.models[harness] && !force) return;
  set({ models: { ...S.models, [harness]: { loading: true, models: [] } } });
  try {
    const list = await api(`/api/harnesses/${enc(harness)}/models${force ? '?refresh=1' : ''}`);
    set({ models: { ...S.models, [harness]: list } });
  } catch (e) {
    set({ models: { ...S.models, [harness]: { error: e.message, models: [] } } });
  }
}

// ── opening runs and flows ─────────────────────────────────────────────────
let es = null;
function closeStream() { if (es) es.close(); es = null; }
export async function openRun(id) {
  closeStream();
  set({ runId: id, snap: null, drafts: {}, sel: [], selEdge: null, compare: null, logs: [], spark: {}, conn: 'connecting' });
  location.hash = `run=${enc(id)}`;
  try {
    const [snap, flow] = await Promise.all([api(`/api/runs/${enc(id)}`), api(`/api/runs/${enc(id)}/flow`)]);
    if (S.runId !== id) return;
    set({ snap, flow, pos: layout(flow.tasks) });
    requestAnimationFrame(() => import('./canvas.js').then((c) => c.fitView()));
  } catch (e) { return fail(e); }
  es = new EventSource(`/api/runs/${enc(id)}/events`);
  es.addEventListener('state', (ev) => onSnapshot(id, JSON.parse(ev.data)));
  es.onopen = () => set({ conn: 'live' });
  es.onerror = () => set({ conn: 'reconnecting' });
}
async function onSnapshot(id, snap) {
  if (S.runId !== id) return;
  const prev = S.snap, logs = [...S.logs], spark = { ...S.spark };
  for (const n of Object.values(snap.nodes)) {
    const p = prev?.nodes?.[n.id];
    if (p && p.status !== n.status) logs.push([now(), n.status, `[${n.id}] ${n.status}${n.status === 'failed' ? ' — ' + (n.stderr || '').split('\n')[0] : ''}`]);
    if (p && (n.nudges?.length || 0) > (p.nudges?.length || 0)) logs.push([now(), 'watch', `[${n.id}] watchdog intervened`]);
    if (n.status === 'running') {
      const delta = Math.max(0, (n.progress?.bytes || 0) - (p?.progress?.bytes || 0));
      spark[n.id] = [...(spark[n.id] || []).slice(-43), delta];
    }
  }
  const known = new Set(S.flow.tasks.map((t) => t.id));
  const changed = Object.keys(snap.nodes).some((k) => !known.has(k)) || S.flow.tasks.length !== Object.keys(snap.nodes).length;
  set({ snap, logs: logs.slice(-500), spark });
  if (changed) {
    const flow = await api(`/api/runs/${enc(id)}/flow`).catch(() => null);
    if (flow && S.runId === id) set({ flow, pos: { ...layout(flow.tasks), ...S.pos } });
  }
}
export function newCanvas() {
  closeStream();
  location.hash = '';
  set({ runId: null, snap: null, flow: blankFlow(), drafts: {}, pos: {}, sel: [], compare: null, logs: [], conn: 'idle' });
}
export async function openPipeline(path) {
  try {
    const { flow } = await api(`/api/flows/open?path=${enc(path)}`);
    closeStream();
    location.hash = '';
    set({ runId: null, snap: null, flow, drafts: {}, pos: layout(flow.tasks), sel: [], logs: [], conn: 'idle', pipelinePath: path });
    requestAnimationFrame(() => import('./canvas.js').then((c) => c.fitView()));
  } catch (e) { fail(e); }
}

// ── panels & selection ─────────────────────────────────────────────────────
export function toggle(side, id) { set({ [side]: S[side] === id ? null : id }); }
export function select(id, additive = false) {
  if (!id) return set({ sel: [] });
  const sel = additive ? (S.sel.includes(id) ? S.sel.filter((s) => s !== id) : [...S.sel, id]) : [id];
  set({ sel, selEdge: null });
}
export function focusNode(id) { select(id); set({ right: 'node' }); }

// ── graph edits ────────────────────────────────────────────────────────────
// Local state (drafts, compose-mode tasks, positions) is undoable; edits
// already sent to a live run are not.
const undo = [];
const remember = () => { undo.push(JSON.stringify({ drafts: S.drafts, flow: S.flow, pos: S.pos })); if (undo.length > 60) undo.shift(); };
export function undoLast() {
  const prev = undo.pop();
  if (!prev) return toast('Nothing to undo');
  const p = JSON.parse(prev);
  set(S.runId ? { drafts: p.drafts, pos: { ...S.pos, ...p.pos } } : p);
}
// In compose mode the flow's own tasks are drafts too.
function editTask(id, fn) {
  if (S.drafts[id]) return set({ drafts: { ...S.drafts, [id]: fn({ ...S.drafts[id] }) } });
  if (!S.runId) return set({ flow: { ...S.flow, tasks: S.flow.tasks.map((t) => (t.id === id ? fn({ ...t }) : t)) } });
  return null;
}
export function updateDraft(id, patch) { editTask(id, (t) => ({ ...t, ...patch })); }
const isLocal = (id) => !!S.drafts[id] || !S.runId;

export function addNode({ harness, model, x, y, from, id }) {
  remember();
  const all = nodes();
  const short = model ? model.split('/').pop().split(/[-:]/)[0] : '';
  const nid = id || uniqueId(short ? `${harness}-${short}` : harness, new Set(all.map((n) => n.id)));
  y = freeSpot(all, x, y);
  const task = { id: nid, harness, model: model || null, in: '', depends_on: from ? [from] : [], x, y, born: Date.now() };
  if (S.runId) set({ drafts: { ...S.drafts, [nid]: task } });
  else set({ flow: { ...S.flow, tasks: [...S.flow.tasks, { id: nid, harness, model: model || null, in: '', depends_on: from ? [from] : [], born: task.born }] }, pos: { ...S.pos, [nid]: { x, y } } });
  set({ sel: [nid], right: 'node' });
  return nid;
}
export async function connect(from, to) {
  if (edges().some(([a, b]) => a === from && b === to)) return 'Already connected';
  if (wouldCycle(edges(), from, to)) return from === to ? 'A node cannot feed itself' : 'That wire would close a cycle';
  if (isLocal(to)) { remember(); editTask(to, (t) => ({ ...t, depends_on: [...(t.depends_on || []), from], newEdge: { from, at: Date.now() } })); return null; }
  if (S.drafts[from]) { remember(); editTask(from, (t) => ({ ...t, feeds: [...(t.feeds || []), to] })); return null; }
  return patch({ wire: [[from, to]] });
}
export async function disconnect(from, to) {
  if (isLocal(to)) { remember(); return editTask(to, (t) => ({ ...t, depends_on: (t.depends_on || []).filter((d) => d !== from) })); }
  if (S.drafts[from]) { remember(); return editTask(from, (t) => ({ ...t, feeds: (t.feeds || []).filter((d) => d !== to) })); }
  const err = await patch({ unwire: [[from, to]] });
  if (err) toast(err, 'error');
}
export function moveNodes(ids, dx, dy, first) {
  if (first) remember();
  const pos = { ...S.pos }, drafts = { ...S.drafts };
  for (const id of ids) {
    if (drafts[id]) drafts[id] = { ...drafts[id], x: drafts[id].x + dx, y: drafts[id].y + dy };
    else if (pos[id]) pos[id] = { x: pos[id].x + dx, y: pos[id].y + dy };
  }
  set({ pos, drafts });
}
let posT;
export function settleMove(ids) {
  const snap = (v) => Math.round(v / 10) * 10;
  const pos = { ...S.pos }, drafts = { ...S.drafts };
  for (const id of ids) {
    if (drafts[id]) drafts[id] = { ...drafts[id], x: snap(drafts[id].x), y: snap(drafts[id].y) };
    else if (pos[id]) pos[id] = { x: snap(pos[id].x), y: snap(pos[id].y) };
  }
  set({ pos, drafts });
  const remote = ids.filter((id) => S.runId && !S.drafts[id]);
  if (remote.length) {
    clearTimeout(posT);
    posT = setTimeout(() => patch({ pos: Object.fromEntries(remote.map((id) => [id, S.pos[id]])) }, true), 400);
  }
}
export async function removeSelected() {
  if (S.selEdge) { const [a, b] = S.selEdge; set({ selEdge: null }); await disconnect(a, b); return; }
  const ids = S.sel;
  if (!ids.length) return;
  const local = ids.filter(isLocal), remote = ids.filter((id) => !isLocal(id));
  if (local.length) {
    remember();
    const drop = (t) => ({ ...t, depends_on: (t.depends_on || []).filter((d) => !local.includes(d)), feeds: (t.feeds || []).filter((d) => !local.includes(d)) });
    const drafts = Object.fromEntries(Object.entries(S.drafts).filter(([k]) => !local.includes(k)).map(([k, v]) => [k, drop(v)]));
    set(S.runId ? { drafts } : { drafts, flow: { ...S.flow, tasks: S.flow.tasks.filter((t) => !local.includes(t.id)).map(drop) } });
  }
  set({ sel: [] });
  if (remote.length) {
    const err = await patch({ remove: remote });
    if (err) return toast(err, 'error');
  }
  toast(`Removed ${ids.join(', ')}${remote.length ? '' : ' · ⌘Z to undo'}`);
}
export function duplicate(id) {
  const n = byId(id);
  return addNode({ harness: n.harness, model: n.model, x: n.x + 30, y: n.y + 150 });
}

// ── run control ────────────────────────────────────────────────────────────
async function patch(body, quiet) {
  try {
    const snap = await api(`/api/runs/${enc(S.runId)}/patch`, body);
    set({ snap });
    const flow = await api(`/api/runs/${enc(S.runId)}/flow`);
    set({ flow });
    return null;
  } catch (e) {
    if (!quiet) return e.message;
    return null;
  }
}
// Launch the drafts: into the open run, or as a new run (compose mode).
export async function runDrafts(only) {
  if (!S.runId) return bootCompose(only);
  const ids = only ? [only] : Object.keys(S.drafts);
  if (!ids.length) return;
  const empty = ids.filter((id) => !S.drafts[id].in?.trim() && !S.drafts[id].depends_on?.length);
  if (empty.length) { focusNode(empty[0]); return toast(`Write a brief for ${empty.join(', ')} first`, 'warn'); }
  const add = ids.map((id) => serverTask(S.drafts[id]));
  const wire = ids.flatMap((id) => (S.drafts[id].feeds || []).map((f) => [id, f]));
  const pos = Object.fromEntries(ids.map((id) => [id, { x: S.drafts[id].x, y: S.drafts[id].y }]));
  const err = await patch({ add, wire });
  if (err) return toast(err, 'error');
  const drafts = { ...S.drafts };
  ids.forEach((id) => delete drafts[id]);
  set({ drafts, pos: { ...S.pos, ...pos } });
  toast(`Launched ${ids.join(', ')}`);
}
async function bootCompose(only) {
  const tasks = S.flow.tasks.map((t) => serverTask({ ...t, ...S.pos[t.id] }));
  if (!tasks.length) return toast('Drop a model from Fleet onto the canvas first', 'warn');
  const empty = tasks.filter((t) => !t.in.trim() && !t.depends_on?.length);
  if (empty.length) { focusNode(empty[0].id); return toast(`Write a brief for ${empty.map((t) => t.id).join(', ')} first`, 'warn'); }
  try {
    const { run_id } = await api('/api/flows/boot', { flow: { ...S.flow, tasks }, name: S.flow.name, focus: only || undefined });
    toast(`Started ${run_id}`);
    await refreshRuns();
    openRun(run_id);
  } catch (e) { fail(e); }
}
export async function savePipeline() {
  const tasks = (S.runId ? S.flow.tasks : S.flow.tasks).map((t) => serverTask({ ...t, ...S.pos[t.id] }));
  try {
    const r = await api('/api/flows/save', { flow: { ...S.flow, tasks }, name: S.flow.name });
    toast(`Saved ${r.path}`);
    refreshPipelines();
  } catch (e) { fail(e); }
}
export async function rerun(id, task) {
  try { set({ snap: await api(`/api/runs/${enc(S.runId)}/rerun`, { node: id, task: task ? serverTask(task) : undefined }) }); toast(`${id}: rerunning`); } catch (e) { fail(e); }
}
export async function rerunStale() {
  try { set({ snap: await api(`/api/runs/${enc(S.runId)}/rerun-stale`, {}) }); toast('Rerunning stale nodes'); } catch (e) { fail(e); }
}
export async function answer(id, text, qid) {
  try { await api(`/api/runs/${enc(S.runId)}/answer`, { node: id, text, question_id: qid }); toast(`Answer sent to ${id}`); } catch (e) { fail(e); }
}
export async function steer(id, text) {
  const n = S.snap?.nodes?.[id];
  const open = n?.questions?.find((q) => !q.closed && q.answer == null);
  if (open) return answer(id, text, open.id);
  try { set({ snap: await api(`/api/runs/${enc(S.runId)}/steer`, { node: id, text }) }); } catch (e) { fail(e); }
}
export async function retryOn(id, harness, model) {
  const task = { ...S.flow.tasks.find((t) => t.id === id), harness, model };
  return rerun(id, task);
}

// ── planner (chat with the supermodel) ─────────────────────────────────────
let chatEs = null;
const canvasGraph = () => ({ ...S.flow, tasks: nodes().map((n) => serverTask({ ...n.task, x: n.x, y: n.y })) });
export async function plannerSend(text, harness, model, cwd) {
  try {
    if (!S.chat) {
      const { run_id } = await api('/api/chats', { harness, model, prompt: text, cwd: cwd || undefined, graph: canvasGraph(), attached_run: S.runId || undefined });
      openChat(run_id);
    } else {
      const chat = S.chatSnap?.nodes?.chat;
      const action = chat?.status === 'waiting' ? 'answer' : 'steer';
      await api(`/api/runs/${enc(S.chat)}/${action}`, { node: 'chat', text, graph: canvasGraph(), attached_run: S.runId || undefined });
    }
  } catch (e) { fail(e); }
}
export function openChat(id) {
  if (chatEs) chatEs.close();
  set({ chat: id, chatSnap: null, right: 'planner' });
  if (!id) return;
  chatEs = new EventSource(`/api/runs/${enc(id)}/events`);
  chatEs.addEventListener('state', (ev) => S.chat === id && set({ chatSnap: JSON.parse(ev.data) }));
}
// Put a proposal on the canvas as drafts: new nodes only, keeping what is
// already there (a live run stays as it is, its new nodes wait for Run).
export function applyProposal(p) {
  const spec = p.spec;
  if (!spec?.tasks) return;
  remember();
  const have = new Set(nodes().map((n) => n.id));
  const fresh = spec.tasks.filter((t) => !have.has(t.id));
  if (!S.runId && !S.flow.tasks.length) {
    set({ flow: { ...spec, tasks: spec.tasks.map((t) => ({ ...t, born: Date.now() })) }, pos: layout(spec.tasks) });
  } else {
    const all = [...nodes().map((n) => ({ id: n.id, depends_on: n.depends_on, pos: { x: n.x, y: n.y } })), ...fresh];
    const pos = layout(all);
    fresh.forEach((t, i) => setTimeout(() => {
      const feeds = spec.tasks.filter((u) => have.has(u.id) && (u.depends_on || []).includes(t.id)).map((u) => u.id);
      const task = { ...t, depends_on: t.depends_on || [], feeds, x: pos[t.id].x, y: pos[t.id].y, born: Date.now() };
      if (S.runId) set({ drafts: { ...S.drafts, [t.id]: task } });
      else set({ flow: { ...S.flow, tasks: [...S.flow.tasks, task] }, pos: { ...S.pos, [t.id]: pos[t.id] } });
    }, i * 200));
    setTimeout(() => import('./canvas.js').then((c) => c.fitView()), fresh.length * 200 + 80);
  }
  toast(fresh.length ? `Placed ${fresh.length} node${fresh.length > 1 ? 's' : ''} — Run when ready` : 'Nothing new in the proposal');
}
export async function runProposal(p) {
  try {
    const r = await api(`/api/runs/${enc(S.chat)}/run-proposal`, { node: 'chat', hash: p.hash, flow: p.spec });
    toast(`Started ${r.run_id}`);
    await refreshRuns();
    openRun(r.run_id);
  } catch (e) { fail(e); }
}
