import { html, useState, useEffect, useRef } from './vendor/preact-htm.js';
import * as D from './data.js';
import { S, set, select, focusNode, addNode, attention, answer, approve, retry, steer, toast, updateNode, disconnect, plannerSend, applyProposal } from './store.js';
import { canvasApi, toWorld, overCanvas, nearestOut, siblings, H, W } from './canvas.js';
import { Icon, STATUS, dur, clock, money, tok, md } from './icons.js';

const Head = ({ title, children }) => html`<div class="ph"><span class="ph-t">${title}</span><span class="ph-a">${children}</span></div>`;
const Dot = ({ st }) => html`<i class=${'dot st-' + st} />`;

// ── left: Runs ─────────────────────────────────────────────────────────────
export function Runs() {
  const [q, setQ] = useState('');
  return html`<${Head} title="Runs"><button title="New run" class="ib" onClick=${() => set({ right: 'planner' })}><${Icon} n="plus" /></button><//>
    <div class="pad"><input class="inp" placeholder="filter runs…" value=${q} onInput=${(e) => setQ(e.target.value)} /></div>
    <div class="list">${D.history.filter((r) => r.name.includes(q)).map((r) => html`
      <div class=${'row' + (r.current ? ' cur' : '')} onClick=${() => !r.current && toast(`Would open ${r.name} (prototype: one live run)`)}>
        <div class="row-t"><${Dot} st=${r.status === 'done' ? 'done' : r.status} /> ${r.name}</div>
        <div class="row-s">${r.status} · ${r.tasks} tasks · ${r.took} · ${r.ago}</div>
      </div>`)}</div>`;
}

// ── left: Pipelines (saved flows / templates) ──────────────────────────────
export function Pipelines() {
  return html`<${Head} title="Pipelines"><button class="ib" title="Save canvas as pipeline" onClick=${() => toast('Saved canvas as pipeline (prototype)')}><${Icon} n="plus" /></button><//>
    <div class="list">${D.pipelines.map((p) => html`
      <div class="row" onClick=${() => toast(`Would open ${p.name} on a new canvas`)}>
        <div class="row-t"><${Icon} n="pipelines" s=${13} /> ${p.name}</div>
        <div class="chain">${p.shape.map((x, i) => html`${i ? html`<span class="arr">→</span>` : ''}<span class="chip">${x}</span>`)}</div>
        <div class="row-s">${p.shape.length} nodes · ${p.models} models</div>
      </div>`)}</div>`;
}

// ── left: Fleet (harness × model, draggable onto the canvas) ───────────────
function track(onMove, onUp) {
  const mv = (e) => onMove(e), up = (e) => { removeEventListener('pointermove', mv); removeEventListener('pointerup', up); onUp(e); };
  addEventListener('pointermove', mv); addEventListener('pointerup', up);
}
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
      snap = nearestOut(wx, wy + H / 2)?.id || null;
    }
    set({ drag: { harness, model, cx: ev.clientX, cy: ev.clientY, over, wx, wy, snap } });
  }, () => {
    const dr = S.drag;
    set({ drag: null });
    if (!moved) return addCentered(harness, model);
    if (dr?.over) addNode({ harness, model, x: dr.wx, y: dr.wy, from: dr.snap });
  });
}
function addCentered(harness, model) {
  const r = canvasApi.el.getBoundingClientRect(), from = S.sel.length === 1 ? S.run.nodes[S.sel[0]] : null;
  const c = toWorld(r.left + r.width / 2, r.top + r.height / 2);
  addNode(from ? { harness, model, x: from.x + 310, y: from.y, from: from.id } : { harness, model, x: c.x - W / 2, y: c.y - H / 2 });
}
export function Fleet() {
  const [q, setQ] = useState('');
  const pairs = D.fleet.flatMap((h) => h.models.map((m) => [h.harness, m])).filter(([h, m]) => `${h} ${m}`.includes(q));
  const Item = ([h, m]) => html`<div class="fl" onPointerDown=${(e) => fleetDrag(e, h, m)} title="Drag onto the canvas · click to add after the selection">
      <${Icon} n="grip" s=${12} w=${3} cls="grip" /><span class=${'hz hz-' + h}>${h}</span><span class="fl-m">${m}</span>
      ${D.starred.includes(`${h}/${m}`) && html`<${Icon} n="star" s=${11} cls="star" />`}</div>`;
  return html`<${Head} title="Fleet"><button class="ib" title="Add harness" onClick=${() => toast('Harnesses come from ~/.zori/harnesses.yml')}><${Icon} n="plus" /></button><//>
    <div class="pad"><input class="inp" placeholder="harness or model…" value=${q} onInput=${(e) => setQ(e.target.value)} /></div>
    <div class="list">
      ${!q && html`<div class="grp">Starred</div>${pairs.filter(([h, m]) => D.starred.includes(`${h}/${m}`)).map(Item)}`}
      ${D.fleet.map((g) => { const ps = pairs.filter(([h]) => h === g.harness); return ps.length ? html`<div class="grp">${g.harness}</div>${ps.map(Item)}` : null; })}
    </div>
    <div class="foot-hint">drag onto the canvas → node</div>`;
}

// ── left: Artifacts ────────────────────────────────────────────────────────
export function Artifacts({ s }) {
  const items = Object.values(s.run.nodes).filter((n) => n.artifact);
  return html`<${Head} title="Artifacts" />
    <div class="list">${items.map((n) => html`<div class="row" onClick=${() => focusNode(n.id)}>
      <div class="row-t"><${Icon} n="artifacts" s=${13} /> ${n.artifact.name}</div>
      <div class="row-s">${n.artifact.size} · from ${n.id}</div></div>`)}
      ${!items.length && html`<div class="empty">nothing yet</div>`}</div>`;
}

// ── bottom: Timeline ───────────────────────────────────────────────────────
export function Timeline({ s }) {
  const t = s.timeline, R = s.run, win = R.window;
  const ns = Object.values(R.nodes).sort((a, b) => a.x - b.x || a.y - b.y);
  const step = win * t.zoom > 900 ? 30 : 60, pct = (sec) => `${(sec / win) * 100}%`;
  const opt = (k) => html`<label class="ck"><input type="checkbox" checked=${t[k]} onChange=${() => set({ timeline: { ...t, [k]: !t[k] } })} />${k === 'nudges' ? 'Watchdog nudges' : k[0].toUpperCase() + k.slice(1)}</label>`;
  return html`<${Head} title="Attempts timeline">${opt('attempts')}${opt('nudges')}${opt('costs')}
      <span class="zm"><button class="ib" onClick=${() => set({ timeline: { ...t, zoom: Math.max(1, t.zoom / 1.5) } })}><${Icon} n="minus" s=${12} /></button>${Math.round(t.zoom * 100)}%<button class="ib" onClick=${() => set({ timeline: { ...t, zoom: t.zoom * 1.5 } })}><${Icon} n="plus" s=${12} /></button></span><//>
    <div class="tl">
      <div class="tl-names"><div class="tl-hd">Node (${ns.length})</div>${ns.map((n) => html`
        <div class=${'tl-n' + (s.sel.includes(n.id) ? ' sel' : '')} onClick=${() => select(n.id)}><${Dot} st=${n.status} />${n.id}</div>`)}</div>
      <div class="tl-scroll"><div class="tl-body" style=${{ width: `${t.zoom * 100}%` }}>
        <div class="tl-hd tl-ticks">${Array.from({ length: Math.floor(win / step) + 1 }, (_, i) => html`<span style=${{ left: pct(i * step) }}>${Math.floor((i * step) / 60)}:${String((i * step) % 60).padStart(2, '0')}</span>`)}</div>
        ${Array.from({ length: Math.floor(win / step) + 1 }, (_, i) => html`<i class="tl-grid" style=${{ left: pct(i * step) }} />`)}
        ${ns.map((n) => html`<div class=${'tl-lane' + (s.sel.includes(n.id) ? ' sel' : '')} onClick=${() => select(n.id)}>
          ${t.attempts && n.attempts.map((a) => {
            const end = a.to ?? R.elapsed, live = a.to == null;
            const label = a.kind === 'waiting' ? '(waiting)' : `#${a.n} ${live && a.kind === 'running' ? `(running, ${dur(end - a.from)})` : dur(end - a.from)}${a.kind === 'failed' ? ' (failed)' : a.kind === 'stale' ? ' (stale)' : ''}`;
            return html`<div class=${'bar st-' + a.kind} style=${{ left: pct(a.from), width: pct(Math.max(4, end - a.from)) }}><span>${label}</span></div>
              ${live && a.est > end && html`<div class=${'bar est st-' + a.kind} style=${{ left: pct(end), width: pct(a.est - end) }} />`}`;
          })}
          ${t.nudges && n.nudges.map((x) => html`<i class="nudge" style=${{ left: pct(x) }} title="watchdog nudge" />`)}
          ${t.costs && n.cost > 0 && html`<span class="tl-cost" style=${{ left: pct((n.attempts.at(-1)?.to ?? R.elapsed) + 4) }}>${money(n.cost)}</span>`}
        </div>`)}
        <i class="tl-now" style=${{ left: pct(R.elapsed) }}><b>${clock(R.elapsed)}</b></i>
      </div></div>
    </div>`;
}

// ── bottom: Logs (what `zori run` prints) ──────────────────────────────────
export function Logs({ s }) {
  const [q, setQ] = useState('');
  const ref = useRef();
  useEffect(() => { ref.current && (ref.current.scrollTop = ref.current.scrollHeight); }, [s.logs.length]);
  return html`<${Head} title="Logs"><input class="inp sm" placeholder="filter…" value=${q} onInput=${(e) => setQ(e.target.value)} /><//>
    <div class="logs" ref=${ref}>${s.logs.filter((l) => l[2].includes(q)).map(([t, k, text]) => html`<div class=${'lg lg-' + k}><span>${clock(t)}</span>${text}</div>`)}</div>`;
}

// ── right: Node (inspector + chat with the node's agent) ───────────────────
function Collapse({ head, children, open: o = false, cls = '' }) {
  const [open, setOpen] = useState(o);
  return html`<div class=${'tx ' + cls + (open ? ' open' : '')}><div class="tx-h" onClick=${() => setOpen(!open)}><${Icon} n=${open ? 'chevD' : 'chevR'} s=${12} />${head}</div>${open && html`<div class="tx-b">${children}</div>`}</div>`;
}
function Report({ body, id }) {
  return html`<div class="report"><div class="report-h"><${Icon} n="artifacts" s=${14} /> FINAL REPORT</div>
    <div class="report-b">${body.map(([k, t]) => (k === 'h' ? html`<h4>## ${t}</h4>` : html`<div class="li" dangerouslySetInnerHTML=${{ __html: '– ' + md(t) }} />`))}</div>
    <button class="btn pri" onClick=${() => toast(`${id}'s report is now the result passed downstream`)}><${Icon} n="check" s=${12} /> Use as result</button></div>`;
}
function Transcript({ n, items }) {
  if (!items.length) return html`<div class="empty">${n.status === 'draft' ? 'Not run yet — write the brief, then ▶ Run.' : n.activity}</div>`;
  return items.map((it, i) => {
    if (it.kind === 'tool') return html`<${Collapse} head=${html`<b class="k-tool">tool-call</b> ${it.title} <span class="dim">· ${it.detail}</span>${it.live ? html`<i class="spin" />` : ''}`}><pre>${it.body}</pre><//>`;
    if (it.kind === 'edit') return html`<${Collapse} open head=${html`<b class="k-edit">file-edit</b> ${it.title} <span class="add">+${it.add}</span> <span class="del">−${it.del}</span>`}>
      <pre class="diff">${it.diff.map(([k, l]) => html`<div class=${k}>${l}</div>`)}</pre><//>`;
    if (it.kind === 'reason') return html`<${Collapse} head=${html`<b class="k-reason">reasoning</b> <span class="dim">(truncated)</span>`}><p class="reason">${it.body}</p><//>`;
    if (it.kind === 'error') return html`<div class="tx err"><b>${it.title}</b> ${it.body}</div>`;
    if (it.kind === 'ask') return null;
    if (it.kind === 'human') return html`<div class="bub me">${it.body}</div>`;
    if (it.kind === 'agent') return html`<div class="bub ai">${it.body}</div>`;
    if (it.kind === 'report') return html`<${Report} key=${i} body=${it.body} id=${n.id} />`;
    return null;
  });
}
function AskBox({ n, q }) {
  const [v, setV] = useState('');
  return html`<div class="askbox"><div class="ask-q"><${Icon} n="user" s=${13} /> <b>${n.id} asks</b> ${q.text}</div>
    <div class="opts">${q.options.map((o) => html`<button class="chip btn-chip" onClick=${() => answer(n.id, o)}>${o}</button>`)}</div>
    <div class="row-in"><input class="inp" placeholder="reply…" value=${v} onInput=${(e) => setV(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && v.trim() && answer(n.id, v.trim())} />
    <button class="ib" onClick=${() => v.trim() && answer(n.id, v.trim())}><${Icon} n="send" /></button></div></div>`;
}
function Brief({ n, s }) {
  const models = D.fleet.find((h) => h.harness === n.harness)?.models || [n.model];
  return html`<div class="brief">
    <label>Harness · model</label>
    <div class="row-in"><select class="inp" onChange=${(e) => updateNode(n.id, { harness: e.target.value, model: D.fleet.find((h) => h.harness === e.target.value).models[0] })}>${D.fleet.map((h) => html`<option selected=${h.harness === n.harness}>${h.harness}</option>`)}</select>
      <select class="inp" onChange=${(e) => updateNode(n.id, { model: e.target.value })}>${models.map((m) => html`<option selected=${m === n.model}>${m}</option>`)}</select></div>
    <label>Inputs (wired on the canvas)</label>
    <div class="chain">${n.depends_on.length ? n.depends_on.map((d) => html`<span class="chip">\${steps.${d}.output}<button onClick=${() => disconnect(d, n.id)} title="unwire">×</button></span>`) : html`<span class="dim">none — pull a wire into this card</span>`}</div>
    <label>Brief</label>
    <textarea class="inp ta" rows="7" value=${n.in} onInput=${(e) => updateNode(n.id, { in: e.target.value })} placeholder="What should this agent do?" />
    <label class="ck"><input type="checkbox" checked=${n.optional} onChange=${() => updateNode(n.id, { optional: !n.optional })} /> optional (best-effort: a failure does not fail the run)</label>
    <div class="row-in"><button class="btn pri" onClick=${() => retry(n.id)}><${Icon} n="play" s=${12} /> Run this node</button></div>
  </div>`;
}
export function NodePanel({ s }) {
  const id = s.sel[0], n = s.run.nodes[id];
  const [tab, setTab] = useState('Transcript');
  const [msg, setMsg] = useState('');
  const [menu, setMenu] = useState(false);
  useEffect(() => { n && setTab(n.status === 'draft' ? 'Brief' : 'Transcript'); }, [id]);
  if (!n) return html`<${Head} title="Node" /><div class="empty big">Select a card on the canvas.<br /><span class="dim">Shift-click several to compare.</span></div>`;
  const ids = Object.keys(s.run.nodes), i = ids.indexOf(id);
  const st = STATUS[n.status] || STATUS.pending;
  const items = s.transcripts[id] || [];
  const q = (n.questions || []).find((x) => !x.closed);
  const send = () => { if (msg.trim()) { steer(id, msg.trim()); setMsg(''); } };
  return html`<div class="np">
    <div class="np-h"><b>${n.id}</b><span class="ph-a">
      <button class="ib" onClick=${() => select(ids[(i - 1 + ids.length) % ids.length])}><${Icon} n="chevL" /></button>
      <button class="ib" onClick=${() => select(ids[(i + 1) % ids.length])}><${Icon} n="chevR" /></button>
      <button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button></span></div>
    <div class="np-sub">${n.harness} · ${n.model}</div>
    <div class="np-m"><span class=${'pill st-' + n.status}><${Icon} n=${st.icon} s=${11} w=${2.4} /> ${st.label}</span>
      <span><${Icon} n="clock" s=${13} /> ${dur(n.secs)}</span><span><${Icon} n="coin" s=${13} /> ${money(n.cost)}</span><span><${Icon} n="tokens" s=${13} /> ${tok(n.tokens)} tok</span></div>
    <div class="tabs">${['Transcript', 'Brief', `Files (${items.filter((x) => x.kind === 'edit').length})`, `Artifacts (${n.artifact ? 1 : 0})`].map((t) => html`
      <button class=${tab === t.split(' ')[0] ? 'on' : ''} onClick=${() => setTab(t.split(' ')[0])}>${t}</button>`)}</div>
    <div class="np-body">
      ${tab === 'Transcript' && html`${q && html`<${AskBox} n=${n} q=${q} />`}<${Transcript} n=${n} items=${items} />`}
      ${tab === 'Brief' && html`<${Brief} n=${n} s=${s} />`}
      ${tab === 'Files' && html`<${Transcript} n=${n} items=${items.filter((x) => x.kind === 'edit')} />`}
      ${tab === 'Artifacts' && (n.artifact ? html`<div class="row"><div class="row-t"><${Icon} n="artifacts" s=${13} /> ${n.artifact.name}</div><div class="row-s">${n.artifact.size}</div></div>` : html`<div class="empty">none</div>`)}
    </div>
    <div class="steer">
      <textarea class="inp ta" rows="2" placeholder=${n.status === 'draft' ? 'draft — run it first' : '› steer the agent…'} value=${msg}
        onInput=${(e) => setMsg(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && (e.metaKey || e.ctrlKey) && send()} />
      <div class="row-in"><button class="btn pri" onClick=${send}>Send & steer <kbd>⌘↵</kbd></button>
        <span class="menu-wrap"><button class="btn" onClick=${() => setMenu(!menu)}><${Icon} n="refresh" s=${12} /> Rerun <${Icon} n="chevD" s=${11} /></button>
        ${menu && html`<div class="menu up" onMouseLeave=${() => setMenu(false)}>
          <button onClick=${() => { retry(id); setMenu(false); }}>Rerun on ${n.model}</button>
          ${D.fleet.flatMap((h) => h.models.map((m) => [h.harness, m])).filter(([, m]) => m !== n.model).slice(0, 6).map(([h, m]) => html`
            <button onClick=${() => { updateNode(id, { harness: h }); retry(id, m); setMenu(false); }}>on ${h} · ${m}</button>`)}
          <button onClick=${() => { set({ compare: siblings(id) }); setMenu(false); }}>Compare with siblings…</button></div>`}</span>
        <button class="btn" title="Compare" onClick=${() => set({ compare: siblings(id) })}><${Icon} n="compare" s=${12} /></button></div>
    </div></div>`;
}

// ── right: Planner (chat with the supermodel) ──────────────────────────────
export function Planner({ s }) {
  const [v, setV] = useState('');
  const ref = useRef();
  useEffect(() => { ref.current && (ref.current.scrollTop = ref.current.scrollHeight); }, [s.chat.length, s.chat.at(-1)?.typing]);
  const send = () => { if (v.trim()) { plannerSend(v.trim()); setV(''); } };
  return html`<div class="np"><${Head} title="Planner"><span class="dim">coddy · kimi-k2.6</span><button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button><//>
    <div class="np-body chat" ref=${ref}>${s.chat.map((m) => m.typing ? html`<div class="bub ai typing"><i /><i /><i /></div>` : html`
      <div class=${'bub ' + (m.role === 'human' ? 'me' : 'ai')}>${m.text}
        ${m.proposal && html`<div class="prop"><div class="prop-h"><${Icon} n="planner" s=${13} /> proposal · +${m.proposal.nodes.length} nodes</div>
          ${m.proposal.nodes.map((pn) => html`<div class="prop-n"><span class="chip">${pn.from}</span>→<b>${pn.id}</b><span class="dim">${pn.harness} · ${pn.model}</span>${pn.to ? html`→<span class="chip">${pn.to}</span>` : ''}</div>`)}
          ${m.proposal.status === 'applied' ? html`<div class="dim">applied to canvas as drafts</div>` : html`<div class="row-in"><button class="btn pri" onClick=${() => applyProposal(m.proposal)}>Apply to canvas</button><button class="btn" onClick=${() => { m.proposal.status = 'applied'; set({}); }}>Dismiss</button></div>`}</div>`}
      </div>`)}</div>
    <div class="steer"><textarea class="inp ta" rows="3" placeholder="what to do, in which folder…" value=${v} onInput=${(e) => setV(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && (e.metaKey || e.ctrlKey) && send()} />
      <div class="row-in"><button class="btn pri" onClick=${send}>Send <kbd>⌘↵</kbd></button><span class="dim">in ${s.run.cwd}</span></div></div></div>`;
}

// ── right / dropdown: Attention inbox ──────────────────────────────────────
function Reply({ n, q }) {
  const [v, setV] = useState('');
  return html`<div class="row-in"><input class="inp" placeholder="reply…" value=${v} onClick=${(e) => e.stopPropagation()} onInput=${(e) => setV(e.target.value)} onKeyDown=${(e) => e.key === 'Enter' && v && answer(n.id, v)} />
    <button class="ib" onClick=${(e) => { e.stopPropagation(); v && answer(n.id, v); }}><${Icon} n="send" /></button></div>`;
}
function RetryOther({ n }) {
  const [open, setOpen] = useState(false);
  return html`<span class="menu-wrap"><button class="btn" onClick=${(e) => { e.stopPropagation(); setOpen(!open); }}>Retry with other model <${Icon} n="chevD" s=${11} /></button>
    ${open && html`<div class="menu" onMouseLeave=${() => setOpen(false)}>${D.fleet.flatMap((h) => h.models.map((m) => [h.harness, m])).filter(([, m]) => m !== n.model).slice(0, 6).map(([h, m]) => html`
      <button onClick=${(e) => { e.stopPropagation(); updateNode(n.id, { harness: h }); retry(n.id, m); }}>${h} · ${m}</button>`)}</div>`}</span>`;
}
export function InboxList({ s }) {
  const items = attention();
  if (!items.length) return html`<div class="empty big"><${Icon} n="check" s=${18} /><br />Nothing needs you.</div>`;
  return html`<div class="inbox">${items.map((a) => html`<div class=${'ib-item k-' + a.kind} onClick=${() => focusNode(a.node.id)}>
    ${a.kind === 'question' && html`<div class="ib-t"><${Icon} n="user" s=${14} /> <b>${a.node.id}</b> asks: ${a.q.text}</div>
      <div class="opts">${a.q.options.map((o) => html`<button class="chip btn-chip" onClick=${(e) => { e.stopPropagation(); answer(a.node.id, o); }}>${o}</button>`)}</div><${Reply} n=${a.node} q=${a.q} />`}
    ${a.kind === 'failed' && html`<div class="ib-t"><${Icon} n="x" s=${14} /> <b>${a.node.id}</b> failed · exit ${a.node.exit_code ?? 1} : ${a.node.activity.replace(/^exit \d+: /, '')}${a.node.optional ? html` <span class="dim">(optional)</span>` : ''}</div>
      <div class="dim">${a.node.harness} · ${a.node.model}</div><div class="row-in"><${RetryOther} n=${a.node} /><button class="btn" onClick=${(e) => { e.stopPropagation(); retry(a.node.id); }}>Retry</button></div>`}
    ${a.kind === 'approval' && html`<div class="ib-t"><${Icon} n="pause" s=${14} /> <b>${a.node.id}</b> parked for approval</div><div class="dim">${a.node.approval}</div>
      <div class="row-in"><button class="btn pri" onClick=${(e) => { e.stopPropagation(); approve(a.node.id, true); }}>Approve</button><button class="btn" onClick=${(e) => { e.stopPropagation(); approve(a.node.id, false); }}>Reject</button></div>`}
  </div>`)}</div>`;
}
export const Inbox = ({ s }) => html`<div class="np"><${Head} title=${`Attention inbox (${attention().length})`}><button class="ib" onClick=${() => set({ right: null })}><${Icon} n="x" /></button><//><div class="np-body"><${InboxList} s=${s} /></div></div>`;
