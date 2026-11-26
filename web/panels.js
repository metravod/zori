import { html, useState, useEffect, useRef } from './vendor/preact-htm.js';
import MD from './md.js';
import { S, set, select, focusNode, addNode, attention, answer, rerun, rerunStale, steer, toast, updateDraft, disconnect, plannerSend, applyProposal, runProposal,
  openRun, newCanvas, openPipeline, savePipeline, refreshPipelines, loadModels, runDrafts, retryOn, nodes, openChat } from './store.js';
import { canvasApi, toWorld, overCanvas, track, siblings, activityOf, H, W } from './canvas.js';
import { nearestOut } from './graph.js';
import { Icon, STATUS, dur, clock, size, runSecs, nowSecs, firstLine } from './icons.js';

const Head = ({ title, children }) => html`<div class="ph"><span class="ph-t">${title}</span><span class="ph-a">${children}</span></div>`;
const Dot = ({ st }) => html`<i class=${'dot st-' + st} />`;
const runSt = (s) => ({ completed: 'done', failed: 'failed', running: 'running', created: 'pending' }[s] || s);
const ago = (t) => { const s = nowSecs() - t; return s < 60 ? 'now' : s < 3600 ? `${Math.floor(s / 60)}m ago` : s < 86400 ? `${Math.floor(s / 3600)}h ago` : `${Math.floor(s / 86400)}d ago`; };
const mdHtml = (t) => ({ __html: MD.render(t || '') });

// ── left: Runs ─────────────────────────────────────────────────────────────
export function Runs({ s }) {
  const [q, setQ] = useState('');
  const runs = s.runs.filter((r) => !r.chat_title && `${r.flow_name} ${r.run_id}`.includes(q));
  return html`<${Head} title="Runs"><button title="New canvas" class="ib" onClick=${newCanvas}><${Icon} n="plus" /></button><//>
    <div class="pad"><input class="inp" placeholder="filter runs…" value=${q} onInput=${(e) => setQ(e.target.value)} /></div>
    <div class="list">
      <div class=${'row' + (!s.runId ? ' cur' : '')} onClick=${newCanvas}><div class="row-t"><${Icon} n="plus" s=${13} /> New canvas</div><div class="row-s">compose a flow, then ▶ Run</div></div>
      ${runs.map((r) => html`
      <div class=${'row' + (r.run_id === s.runId ? ' cur' : '')} onClick=${() => openRun(r.run_id)}>
        <div class="row-t"><${Dot} st=${runSt(r.status)} /> ${r.flow_name}</div>
        <div class="row-s">${r.status} · ${r.nodes} nodes · ${ago(r.updated_at)}</div>
      </div>`)}
      ${!runs.length && html`<div class="empty">no runs yet</div>`}</div>`;
}

// ── left: Pipelines (saved flows) ──────────────────────────────────────────
export function Pipelines({ s }) {
  return html`<${Head} title="Pipelines"><button class="ib" title="Refresh" onClick=${refreshPipelines}><${Icon} n="refresh" /></button>
      <button class="ib" title="Save the canvas as a pipeline" onClick=${savePipeline}><${Icon} n="plus" /></button><//>
    <div class="list">${s.pipelines.map((p) => html`
      <div class=${'row' + (s.pipelinePath === p.path && !s.runId ? ' cur' : '')} onClick=${() => openPipeline(p.path)} title=${p.path}>
        <div class="row-t"><${Icon} n="pipelines" s=${13} /> ${p.name.replace(/\.ya?ml$/, '')}</div>
        <div class="row-s">${ago(p.modified)}</div>
      </div>`)}
      ${!s.pipelines.length && html`<div class="empty">nothing saved yet — compose a canvas and press +</div>`}</div>`;
}

// ── left: Fleet (harness × model, draggable onto the canvas) ───────────────
export function fleetDrag(e, harness, model) {
  if (e.button !== 0) return;
  e.preventDefault();
  const sx = e.clientX, sy = e.clientY;
  let moved = false;
  track((ev) => {
    moved = moved || Math.hypot(ev.clientX - sx, ev.clientY - sy) > 4;
    if (!moved) return;
    const over = overCanvas(ev.clientX, ev.clientY);
    let wx = 0, wy = 0, snap = null;
    if (over) {
      const w = toWorld(ev.clientX, ev.clientY);
      wx = Math.round((w.x - 28) / 10) * 10; wy = Math.round((w.y - H / 2) / 10) * 10;
      snap = nearestOut(nodes(), wx, wy + H / 2)?.id || null;
    }
    set({ drag: { harness, model, cx: ev.clientX, cy: ev.clientY, over, wx, wy, snap } });
  }, () => {
    const dr = S.drag;
    set({ drag: null });
    if (!moved) return addCentered(harness, model);
    if (dr?.over) addNode({ harness, model, x: dr.wx, y: dr.wy, from: dr.snap });
  });
}
export function addCentered(harness, model) {
  const r = canvasApi.el.getBoundingClientRect(), from = S.sel.length === 1 ? nodes().find((n) => n.id === S.sel[0]) : null;
  const c = toWorld(r.left + r.width / 2, r.top + r.height / 2);
  addNode(from ? { harness, model, x: from.x + 300, y: from.y, from: from.id } : { harness, model, x: Math.round(c.x - W / 2), y: Math.round(c.y - H / 2) });
}
function FleetItem({ h, m, star }) {
  return html`<div class="fl" onPointerDown=${(e) => fleetDrag(e, h, m)} title="Drag onto the canvas · click to add after the selection">
    <${Icon} n="grip" s=${12} w=${3} cls="grip" /><span class=${'hz hz-' + h}>${h}</span><span class="fl-m">${m || 'default model'}</span>
    ${star && html`<${Icon} n="star" s=${11} cls="star" />`}</div>`;
}
function FleetGroup({ h, s, q }) {
  const [open, setOpen] = useState(false);
  const list = s.models[h.id];
  useEffect(() => { if ((open || q) && h.supports_model) loadModels(h.id); }, [open, !!q]);
  const models = (list?.models || []).filter((m) => `${h.id} ${m}`.includes(q));
  const matches = !q || h.id.includes(q) || models.length;
  if (!matches) return null;
  return html`<div class="grp g-click" onClick=${() => setOpen(!open)}><${Icon} n=${open || q ? 'chevD' : 'chevR'} s=${11} /> ${h.id}
      ${list?.models && html`<span class="dim">${list.models.length}</span>`}
      ${open && h.supports_model && html`<button class="ib sm" title="Refresh the model list" onClick=${(e) => { e.stopPropagation(); loadModels(h.id, true); }}><${Icon} n="refresh" s=${11} /></button>`}</div>
    ${(open || q) && (!h.supports_model
      ? html`<${FleetItem} h=${h.id} m=${null} />`
      : list?.loading ? html`<div class="empty sm">listing models…</div>`
      : list?.error ? html`<div class="empty sm err">${list.error}</div>`
      : html`${list?.default && !q && html`<${FleetItem} h=${h.id} m=${list.default} />`}
          ${models.filter((m) => m !== list?.default || q).slice(0, q ? 40 : 25).map((m) => html`<${FleetItem} h=${h.id} m=${m} />`)}`)}`;
}
export function Fleet({ s }) {
  const [q, setQ] = useState('');
  const agents = s.harnesses.filter((h) => h.is_agent), utils = s.harnesses.filter((h) => !h.is_agent);
  const starred = s.fleet.filter((f) => `${f.harness} ${f.model}`.includes(q));
  return html`<${Head} title="Fleet" />
    <div class="pad"><input class="inp" placeholder="harness or model…" value=${q} onInput=${(e) => setQ(e.target.value)} /></div>
    <div class="list">
      ${starred.length > 0 && html`<div class="grp">Your fleet</div>${starred.map((f) => html`<${FleetItem} h=${f.harness} m=${f.model} star />`)}`}
      ${agents.map((h) => html`<${FleetGroup} h=${h} s=${s} q=${q} />`)}
      ${utils.some((h) => h.id.includes(q)) && html`<div class="grp">Utility</div>${utils.filter((h) => h.id.includes(q)).map((h) => html`<${FleetItem} h=${h.id} m=${null} />`)}`}
    </div>
    <div class="foot-hint">drag onto the canvas → node · fleet lives in ~/.zori/fleet.json</div>`;
}

// ── left: Artifacts ────────────────────────────────────────────────────────
export function Artifacts({ s }) {
  const ns = Object.values(s.snap?.nodes || {}).filter((n) => n.output);
  return html`<${Head} title="Artifacts" />
    ${s.snap?.out_dir && html`<div class="pad"><div class="row-s path" title="click to copy" onClick=${() => { navigator.clipboard?.writeText(s.snap.out_dir); toast('Run dir copied'); }}>${s.snap.out_dir}</div></div>`}
    <div class="list">${ns.map((n) => html`<div class="row" onClick=${() => focusNode(n.id)}>
      <div class="row-t"><${Icon} n="artifacts" s=${13} /> ${n.id}</div>
      <div class="row-s">${size(n.output.length)} · ${firstLine(n.output).slice(0, 60)}</div></div>`)}
      ${!ns.length && html`<div class="empty">${s.runId ? 'no outputs yet' : 'open a run to see its outputs'}</div>`}</div>`;
}

// ── bottom: Timeline ───────────────────────────────────────────────────────
export function Timeline({ s }) {
  const t = s.timeline, ns = nodes(s).filter((n) => !n.draft).sort((a, b) => a.x - b.x || a.y - b.y);
  const starts = ns.flatMap((n) => (n.attempts || []).map((a) => a.started_at));
  const t0 = starts.length ? Math.min(...starts) : (s.snap?.created_at || nowSecs());
  const tnow = s.snap?.status === 'running' ? nowSecs() : Math.max(t0 + 10, ...ns.flatMap((n) => (n.attempts || []).map((a) => a.ended_at || a.started_at)));
  const win = Math.max(60, tnow - t0) * 1.04, pct = (sec) => `${(sec / win) * 100}%`;
  const step = [10, 30, 60, 120, 300, 600, 1800, 3600].find((x) => win / x <= 10 * t.zoom) || 3600;
  const opt = (k, label) => html`<label class="ck"><input type="checkbox" checked=${t[k]} onChange=${() => set({ timeline: { ...t, [k]: !t[k] } })} />${label}</label>`;
  if (!s.runId) return html`<${Head} title="Attempts timeline" /><div class="empty">open a run to see its attempts</div>`;
  return html`<${Head} title="Attempts timeline">${opt('attempts', 'Attempts')}${opt('nudges', 'Watchdog')}
      <span class="zm"><button class="ib" onClick=${() => set({ timeline: { ...t, zoom: Math.max(1, t.zoom / 1.5) } })}><${Icon} n="minus" s=${12} /></button>${Math.round(t.zoom * 100)}%<button class="ib" onClick=${() => set({ timeline: { ...t, zoom: t.zoom * 1.5 } })}><${Icon} n="plus" s=${12} /></button></span><//>
    <div class="tl">
      <div class="tl-names"><div class="tl-hd">Node (${ns.length})</div>${ns.map((n) => html`
        <div class=${'tl-n' + (s.sel.includes(n.id) ? ' sel' : '')} onClick=${() => select(n.id)}><${Dot} st=${n.status} />${n.id}</div>`)}</div>
      <div class="tl-scroll"><div class="tl-body" style=${{ width: `${t.zoom * 100}%` }}>
        <div class="tl-hd tl-ticks">${Array.from({ length: Math.floor(win / step) + 1 }, (_, i) => html`<span style=${{ left: pct(i * step) }}>${clock(i * step)}</span>`)}</div>
        ${Array.from({ length: Math.floor(win / step) + 1 }, (_, i) => html`<i class="tl-grid" style=${{ left: pct(i * step) }} />`)}
        ${ns.map((n) => html`<div class=${'tl-lane' + (s.sel.includes(n.id) ? ' sel' : '')} onClick=${() => select(n.id)}>
          ${t.attempts && (n.attempts || []).map((a, i) => {
            const end = a.ended_at ?? tnow, kind = a.ended_at ? (a.status || 'done') : 'running';
            return html`<div class=${'bar st-' + kind} title=${`#${i + 1} ${kind} · ${dur(end - a.started_at)}`} style=${{ left: pct(a.started_at - t0), width: pct(Math.max(2, end - a.started_at)) }}>
              <span>#${i + 1} ${dur(end - a.started_at)}${kind !== 'done' && kind !== 'running' ? ` (${kind})` : ''}</span></div>`;
          })}
          ${t.nudges && (n.nudges || []).map((x) => html`<i class="nudge" style=${{ left: pct(x - t0) }} title="watchdog intervention" />`)}
        </div>`)}
        ${s.snap?.status === 'running' && html`<i class="tl-now" style=${{ left: pct(tnow - t0) }}><b>${clock(tnow - t0)}</b></i>`}
      </div></div>
    </div>`;
}

// ── bottom: Logs ───────────────────────────────────────────────────────────
export function Logs({ s }) {
  const [q, setQ] = useState('');
  const ref = useRef();
  useEffect(() => { ref.current && (ref.current.scrollTop = ref.current.scrollHeight); }, [s.logs.length]);
  const t0 = s.snap?.created_at || 0;
  return html`<${Head} title="Logs"><input class="inp sm" placeholder="filter…" value=${q} onInput=${(e) => setQ(e.target.value)} /><//>
    <div class="logs" ref=${ref}>${s.logs.filter((l) => l[2].includes(q)).map(([t, k, text]) => html`<div class=${'lg lg-' + k}><span>${clock(Math.max(0, t - t0))}</span>${text}</div>`)}
      ${!s.logs.length && html`<div class="empty">status changes of this session appear here</div>`}</div>`;
}

// ── right: Node (inspector + chat with the node's agent) ───────────────────
function Collapse({ head, children, open: o = false, cls = '' }) {
  const [open, setOpen] = useState(o);
  return html`<div class=${'tx ' + cls + (open ? ' open' : '')}><div class="tx-h" onClick=${() => setOpen(!open)}><${Icon} n=${open ? 'chevD' : 'chevR'} s=${12} />${head}</div>${open && html`<div class="tx-b">${children}</div>`}</div>`;
}
function Report({ text, title = 'FINAL REPORT' }) {
  return html`<div class="report"><div class="report-h"><${Icon} n="artifacts" s=${14} /> ${title}
      <button class="ib sm" title="Copy" onClick=${() => { navigator.clipboard?.writeText(text); toast('Copied'); }}><${Icon} n="dup" s=${12} /></button></div>
    <div class="report-b md" dangerouslySetInnerHTML=${mdHtml(text)} /></div>`;
}
function Transcript({ n }) {
  const turns = n.turns || [];
  if (n.draft) return html`<div class="empty">Not run yet — write the brief, then ▶ Run.</div>`;
  if (!turns.length && n.status !== 'failed') return html`<div class="empty">${activityOf(n)}</div>`;
  const last = turns.map((t) => t.role).lastIndexOf('agent');
  return html`${turns.map((t, i) => {
    if (t.role === 'human') return html`<div class="bub me">${t.text}</div>`;
    if (t.role === 'system') return html`<${Collapse} cls="sys" head=${html`<b class="k-reason">watchdog</b> <span class="dim">${firstLine(t.text).replace('[watchdog] ', '').slice(0, 80)}</span>`}><pre>${t.text}</pre><//>`;
    if (i === last && n.status === 'done') return html`<${Report} text=${t.text} />`;
    return t.text.length > 900
      ? html`<${Collapse} head=${html`<b class="k-tool">agent</b> <span class="dim">${firstLine(t.text).slice(0, 70)}</span>`}><div class="md" dangerouslySetInnerHTML=${mdHtml(t.text)} /><//>`
      : html`<div class="bub ai md" dangerouslySetInnerHTML=${mdHtml(t.text)} />`;
  })}
  ${n.status === 'failed' && n.stderr && html`<div class="tx err"><b>${n.exit_code != null ? `exit ${n.exit_code}` : 'failed'}</b><pre>${n.stderr.slice(0, 4000)}</pre></div>`}
  ${n.status === 'running' && html`<div class="live-line"><i class="spin" /> ${n.progress?.activity || 'working…'}</div>`}`;
}
function AskBox({ n, q }) {
  const [v, setV] = useState('');
  return html`<div class="askbox"><div class="ask-q"><${Icon} n="user" s=${13} /> <b>${n.id} asks</b> ${q.text}</div>
    ${q.options?.length > 0 && html`<div class="opts">${q.options.map((o) => html`<button class="chip btn-chip" onClick=${() => answer(n.id, o, q.id)}>${o}</button>`)}</div>`}
    <div class="row-in"><input class="inp" placeholder="reply…" value=${v} onInput=${(e) => setV(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && v.trim() && answer(n.id, v.trim(), q.id)} />
    <button class="ib" onClick=${() => v.trim() && answer(n.id, v.trim(), q.id)}><${Icon} n="send" /></button></div></div>`;
}
function ModelPicker({ harness, model, onPick }) {
  const s = S, h = s.harnesses.find((x) => x.id === harness);
  useEffect(() => { if (h?.supports_model) loadModels(harness); }, [harness]);
  const models = s.models[harness]?.models || [];
  return html`<div class="row-in">
    <select class="inp" onChange=${(e) => onPick(e.target.value, null)}>${s.harnesses.filter((x) => x.is_agent || x.id === harness).map((x) => html`<option selected=${x.id === harness}>${x.id}</option>`)}</select>
    ${h?.supports_model && html`<input class="inp" list=${'models-' + harness} value=${model || ''} placeholder="default model" onChange=${(e) => onPick(harness, e.target.value || null)} />
      <datalist id=${'models-' + harness}>${models.map((m) => html`<option value=${m} />`)}</datalist>`}</div>`;
}
function Brief({ n }) {
  const [edit, setEdit] = useState(null);
  useEffect(() => setEdit(null), [n.id]);
  const t = n.draft ? n.task : { ...n.task, ...(edit || {}) };
  const up = (patch) => (n.draft ? updateDraft(n.id, patch) : setEdit({ ...(edit || {}), ...patch }));
  return html`<div class="brief">
    <label>Harness · model</label>
    <${ModelPicker} harness=${t.harness} model=${t.model} onPick=${(h, m) => up({ harness: h, model: m })} />
    <label>Inputs (wired on the canvas)</label>
    <div class="chain">${n.depends_on.length ? n.depends_on.map((d) => html`<span class="chip">${d}<button onClick=${() => disconnect(d, n.id)} title="unwire">×</button></span>`) : html`<span class="dim">none — pull a wire into this card</span>`}</div>
    <div class="dim hint">Each input’s result is appended to the prompt under “# <id>”.</div>
    <label>Brief</label>
    <textarea class="inp ta" rows="8" value=${t.in || ''} onInput=${(e) => up({ in: e.target.value })} placeholder="What should this agent do?" />
    <label>Expected output (optional)</label>
    <textarea class="inp ta" rows="2" value=${t.out || ''} onInput=${(e) => up({ out: e.target.value || undefined })} placeholder="e.g. findings only, file:line + fix" />
    <label class="ck"><input type="checkbox" checked=${!!t.optional} onChange=${() => up({ optional: !t.optional })} /> optional (a failure does not fail the run)</label>
    <div class="row-in">${n.draft
      ? html`<button class="btn pri" onClick=${() => runDrafts(n.id)}><${Icon} n="play" s=${12} /> Run this node</button>`
      : html`<button class="btn pri" disabled=${!edit} onClick=${() => { rerun(n.id, t); setEdit(null); }}><${Icon} n="refresh" s=${12} /> Rerun with changes</button>
          ${edit && html`<button class="btn" onClick=${() => setEdit(null)}>Discard</button>`}`}</div>
  </div>`;
}
function RerunMenu({ n, up = true }) {
  const [menu, setMenu] = useState(false);
  const alts = S.fleet.filter((f) => !(f.harness === n.harness && f.model === n.model)).slice(0, 8);
  return html`<span class="menu-wrap"><button class="btn" onClick=${(e) => { e.stopPropagation(); setMenu(!menu); }}><${Icon} n="refresh" s=${12} /> Rerun <${Icon} n="chevD" s=${11} /></button>
    ${menu && html`<div class=${'menu' + (up ? ' up' : '')} onMouseLeave=${() => setMenu(false)}>
      <button onClick=${(e) => { e.stopPropagation(); rerun(n.id); setMenu(false); }}>Rerun on ${n.model || n.harness}</button>
      ${alts.map((f) => html`<button onClick=${(e) => { e.stopPropagation(); retryOn(n.id, f.harness, f.model); setMenu(false); }}>on ${f.harness} · ${f.model}</button>`)}
      ${!alts.length && html`<div class="dim pad">add models to your fleet to retry on them</div>`}</div>`}</span>`;
}
export function NodePanel({ s }) {
  const id = s.sel[0], n = nodes(s).find((x) => x.id === id);
  const [tab, setTab] = useState('Transcript');
  const [msg, setMsg] = useState('');
  useEffect(() => { n && setTab(n.draft ? 'Brief' : 'Transcript'); }, [id, !!n?.draft]);
  const bodyRef = useRef();
  useEffect(() => { if (tab === 'Transcript' && bodyRef.current) bodyRef.current.scrollTop = bodyRef.current.scrollHeight; }, [id, tab, n?.turns?.length]);
  if (!n) return html`<${Head} title="Node"><button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button><//><div class="empty big">Select a card on the canvas.<br /><span class="dim">Shift-click several to compare.</span></div>`;
  const ids = nodes(s).map((x) => x.id), i = ids.indexOf(id);
  const st = STATUS[n.status] || STATUS.pending;
  const q = (n.questions || []).find((x) => !x.closed && x.answer == null);
  const send = () => { if (msg.trim()) { steer(id, msg.trim()); setMsg(''); } };
  const canSteer = !!q || (['done', 'failed', 'stale'].includes(n.status) && n.session);
  return html`<div class="np">
    <div class="np-h"><b>${n.id}</b><span class="ph-a">
      <button class="ib" onClick=${() => select(ids[(i - 1 + ids.length) % ids.length])}><${Icon} n="chevL" /></button>
      <button class="ib" onClick=${() => select(ids[(i + 1) % ids.length])}><${Icon} n="chevR" /></button>
      <button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button></span></div>
    <div class="np-sub">${n.harness} · ${n.model || 'default model'}</div>
    <div class="np-m"><span class=${'pill st-' + n.status}><${Icon} n=${st.icon} s=${11} w=${2.4} /> ${st.label}</span>
      <span><${Icon} n="clock" s=${13} /> ${dur(runSecs(n))}</span><span><${Icon} n="logs" s=${13} /> ${(n.turns || []).length} turns</span>
      <span><${Icon} n="artifacts" s=${13} /> ${size(n.output?.length || 0)}</span>${(n.attempts || []).length > 1 && html`<span>${n.attempts.length} attempts</span>`}</div>
    <div class="tabs">${['Transcript', 'Brief', 'Output'].map((t) => html`<button class=${tab === t ? 'on' : ''} onClick=${() => setTab(t)}>${t}</button>`)}</div>
    <div class="np-body" ref=${bodyRef}>
      ${tab === 'Transcript' && html`${q && html`<${AskBox} n=${n} q=${q} />`}<${Transcript} n=${n} />`}
      ${tab === 'Brief' && html`<${Brief} n=${n} />`}
      ${tab === 'Output' && (n.output ? html`<${Report} text=${n.output} title="OUTPUT" />` : html`<div class="empty">no output yet</div>`)}
    </div>
    ${n.draft ? html`<div class="steer"><div class="row-in"><button class="btn pri" onClick=${() => runDrafts(n.id)}><${Icon} n="play" s=${12} /> Run ${n.id}</button><span class="dim">or ▶ Run all new nodes in the top bar</span></div></div>`
      : html`<div class="steer">
      <textarea class="inp ta" rows="2" disabled=${!canSteer} placeholder=${q ? `› answer ${n.id}…` : canSteer ? '› steer the agent (continues its session)…' : n.status === 'running' ? 'working — steer once it settles' : 'nothing to steer yet'}
        value=${msg} onInput=${(e) => setMsg(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && (e.metaKey || e.ctrlKey) && send()} />
      <div class="row-in"><button class="btn pri" disabled=${!canSteer} onClick=${send}>${q ? 'Answer' : 'Send & steer'} <kbd>⌘↵</kbd></button>
        <${RerunMenu} n=${n} />
        <button class="btn" title="Compare with siblings" onClick=${() => set({ compare: siblings(id) })}><${Icon} n="compare" s=${12} /></button></div>
    </div>`}</div>`;
}

// ── right: Planner (chat with the supermodel) ──────────────────────────────
export function Planner({ s }) {
  const [v, setV] = useState('');
  const [cwd, setCwd] = useState('');
  const chats = s.runs.filter((r) => r.chat_title);
  const agents = s.harnesses.filter((h) => h.is_agent && h.supports_model && h.supports_resume);
  const pick = s.plannerPick || s.fleet.find((f) => agents.some((a) => a.id === f.harness)) || (agents[0] && { harness: agents[0].id, model: '' });
  const ref = useRef();
  const node = s.chatSnap?.nodes?.chat;
  useEffect(() => { ref.current && (ref.current.scrollTop = ref.current.scrollHeight); }, [node?.turns?.length, node?.status]);
  const send = () => {
    if (!v.trim()) return;
    if (!s.chat && !pick?.model) return toast('Pick the planner model first', 'warn');
    plannerSend(v.trim(), pick?.harness, pick?.model, cwd); setV('');
  };
  const p = node?.proposal;
  return html`<div class="np"><${Head} title="Planner">
      <select class="inp sm" onChange=${(e) => openChat(e.target.value || null)}><option value="">${s.chat ? '+ new chat' : 'new chat'}</option>${chats.map((c) => html`<option value=${c.run_id} selected=${c.run_id === s.chat}>${c.chat_title || c.run_id}</option>`)}</select>
      <button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button><//>
    ${!s.chat && html`<div class="pad col">
      <label class="lbl">Planner model</label>
      <select class="inp" onChange=${(e) => { const [harness, model] = e.target.value.split('\u0001'); set({ plannerPick: { harness, model } }); }}>
        ${s.fleet.filter((f) => agents.some((a) => a.id === f.harness)).map((f) => html`<option value=${f.harness + '\u0001' + f.model} selected=${pick?.harness === f.harness && pick?.model === f.model}>${f.harness} · ${f.model}</option>`)}
        ${!s.fleet.length && html`<option>add models to your fleet first</option>`}
      </select>
      <label class="lbl">Project folder (optional)</label>
      <input class="inp" placeholder="~/projects/my-repo" value=${cwd} onInput=${(e) => setCwd(e.target.value)} /></div>`}
    <div class="np-body chat" ref=${ref}>
      ${!s.chat && html`<div class="empty">Describe the job. The planner proposes a graph; you place it on the canvas, edit it, run it.</div>`}
      ${(node?.turns || []).map((t) => t.role === 'system' ? null : html`<div class=${'bub ' + (t.role === 'human' ? 'me' : 'ai md')} dangerouslySetInnerHTML=${t.role === 'human' ? undefined : mdHtml(t.text.replace(/```zori-flow[\s\S]*?```/g, '_(graph proposal below)_'))}>${t.role === 'human' ? t.text : null}</div>`)}
      ${node?.status === 'running' && html`<div class="bub ai typing"><i /><i /><i /></div>`}
      ${p && html`<div class="prop"><div class="prop-h"><${Icon} n="planner" s=${13} /> proposal · ${p.spec?.tasks?.length || 0} nodes · ${p.status}</div>
        ${!p.valid && html`<div class="err">${(p.errors || []).join('; ')}</div>`}
        ${p.valid && p.spec?.tasks?.map((t) => html`<div class="prop-n"><b>${t.id}</b><span class="dim">${t.harness}${t.model ? ' · ' + t.model : ''}</span>${(t.depends_on || []).length ? html`<span class="dim">← ${t.depends_on.join(', ')}</span>` : ''}</div>`)}
        ${p.valid && p.status === 'proposed' && html`<div class="row-in"><button class="btn pri" onClick=${() => applyProposal(p)}>Place on canvas</button><button class="btn" onClick=${() => runProposal(p)}>Run as new run</button></div>`}
        ${p.run_id && html`<button class="btn" onClick=${() => openRun(p.run_id)}>Open ${p.run_id}</button>`}</div>`}
    </div>
    <div class="steer"><textarea class="inp ta" rows="3" placeholder=${s.chat ? '› reply to the planner…' : 'what to do, in which folder…'} value=${v} onInput=${(e) => setV(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && (e.metaKey || e.ctrlKey) && send()} />
      <div class="row-in"><button class="btn pri" onClick=${send}>Send <kbd>⌘↵</kbd></button><span class="dim">${s.chat ? (node?.status || '') : pick ? `${pick.harness} · ${pick.model}` : ''}</span></div></div></div>`;
}

// ── right / dropdown: Attention inbox ──────────────────────────────────────
function Reply({ n, q }) {
  const [v, setV] = useState('');
  return html`<div class="row-in"><input class="inp" placeholder="reply…" value=${v} onClick=${(e) => e.stopPropagation()} onInput=${(e) => setV(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && v && answer(n.id, v, q.id)} />
    <button class="ib" onClick=${(e) => { e.stopPropagation(); v && answer(n.id, v, q.id); }}><${Icon} n="send" /></button></div>`;
}
export function InboxList({ s }) {
  const items = attention();
  if (!items.length) return html`<div class="empty big"><${Icon} n="check" s=${18} /><br />Nothing needs you.</div>`;
  return html`<div class="inbox">${items.map((a) => html`<div class=${'ib-item k-' + a.kind} onClick=${() => a.node && focusNode(a.node.id)}>
    ${a.kind === 'question' && html`<div class="ib-t"><${Icon} n="user" s=${14} /> <b>${a.node.id}</b> asks: ${a.q.text}</div>
      ${a.q.options?.length > 0 && html`<div class="opts">${a.q.options.map((o) => html`<button class="chip btn-chip" onClick=${(e) => { e.stopPropagation(); answer(a.node.id, o, a.q.id); }}>${o}</button>`)}</div>`}<${Reply} n=${a.node} q=${a.q} />`}
    ${a.kind === 'failed' && html`<div class="ib-t"><${Icon} n="x" s=${14} /> <b>${a.node.id}</b> failed${a.node.exit_code != null ? ` · exit ${a.node.exit_code}` : ''}${a.node.optional ? html` <span class="dim">(optional)</span>` : ''}</div>
      <div class="dim clip">${firstLine(a.node.stderr)}</div><div class="row-in"><${RerunMenu} n=${a.node} up=${false} /></div>`}
    ${a.kind === 'stale' && html`<div class="ib-t"><${Icon} n="clock" s=${14} /> ${a.nodes.map((n) => n.id).join(', ')} ${a.nodes.length > 1 ? 'are' : 'is'} stale</div>
      <div class="row-in"><button class="btn" onClick=${(e) => { e.stopPropagation(); rerunStale(); }}>Rerun stale</button></div>`}
    ${a.kind === 'proposal' && html`<div class="ib-t"><${Icon} n="planner" s=${14} /> the planner proposes ${a.proposal.spec?.tasks?.length || 0} nodes</div>
      <div class="row-in"><button class="btn pri" onClick=${(e) => { e.stopPropagation(); applyProposal(a.proposal); }}>Place on canvas</button><button class="btn" onClick=${(e) => { e.stopPropagation(); set({ right: 'planner' }); }}>Open chat</button></div>`}
  </div>`)}</div>`;
}
export const Inbox = ({ s }) => html`<div class="np"><${Head} title=${`Attention inbox (${attention().length})`}><button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button><//><div class="np-body"><${InboxList} s=${s} /></div></div>`;
