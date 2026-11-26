import { html, render, useState, useEffect, useRef } from './vendor/preact-htm.js';
import MD from './md.js';
import { S, set, useStore, boot, toggle, attention, urgent, draftCount, runDrafts, rerunStale, undoLast, removeSelected, focusNode, addNode, nodes, newCanvas, savePipeline } from './store.js';
import { Canvas, fitView, siblings, zoomBy, W, H } from './canvas.js';
import { Runs, Pipelines, Fleet, Artifacts, Timeline, Logs, NodePanel, Planner, Inbox, InboxList, addCentered } from './panels.js';
import { Icon, STATUS, dur, clock, size, runSecs, nowSecs } from './icons.js';

const TOOLS = {
  left: [['runs', 'Runs', 1], ['pipelines', 'Pipelines', 2], ['fleet', 'Fleet · harness × model', 3], ['artifacts', 'Artifacts', 4]],
  bottom: [['timeline', 'Timeline', 5], ['logs', 'Logs', 6]],
  right: [['node', 'Node', 7], ['planner', 'Planner (supermodel)', 8], ['inbox', 'Attention inbox', 9]],
};
const ALL = Object.entries(TOOLS).flatMap(([side, t]) => t.map(([id, title, k]) => ({ side, id, title, k })));

function StripBtn({ side, id, title, k, s, badge }) {
  return html`<button class=${'sb' + (s[side] === id ? ' on' : '')} title=${`${title}  ⌥${k}`} onClick=${() => toggle(side, id)}>
    <${Icon} n=${id} s=${17} w=${1.7} />${badge ? html`<b class="badge-n">${badge}</b>` : ''}</button>`;
}
function Strip({ s, side }) {
  if (side === 'left') return html`<nav class="strip">${TOOLS.left.map((t) => html`<${StripBtn} side="left" id=${t[0]} title=${t[1]} k=${t[2]} s=${s} />`)}
    <span class="grow" />${TOOLS.bottom.map((t) => html`<${StripBtn} side="bottom" id=${t[0]} title=${t[1]} k=${t[2]} s=${s} />`)}</nav>`;
  return html`<nav class="strip r">${TOOLS.right.map((t) => html`<${StripBtn} side="right" id=${t[0]} title=${t[1]} k=${t[2]} s=${s} badge=${t[0] === 'inbox' ? attention().length : 0} />`)}</nav>`;
}
function Resizer({ side }) {
  const down = (e) => {
    e.preventDefault();
    const start = side === 'bottom' ? e.clientY : e.clientX, s0 = S.size[side];
    const mv = (ev) => {
      const d = side === 'bottom' ? start - ev.clientY : side === 'left' ? ev.clientX - start : start - ev.clientX;
      set({ size: { ...S.size, [side]: Math.max(180, Math.min(side === 'bottom' ? 560 : 720, s0 + d)) } });
    };
    const up = () => { removeEventListener('pointermove', mv); removeEventListener('pointerup', up); localStorage.setItem('zori-size', JSON.stringify(S.size)); };
    addEventListener('pointermove', mv); addEventListener('pointerup', up);
  };
  return html`<div class=${'rz rz-' + side} onPointerDown=${down} />`;
}
const PANELS = { runs: Runs, pipelines: Pipelines, fleet: Fleet, artifacts: Artifacts, timeline: Timeline, logs: Logs, node: NodePanel, planner: Planner, inbox: Inbox };
function Panel({ s, side }) {
  const P = PANELS[s[side]];
  const style = side === 'bottom' ? { height: s.size.bottom } : { width: s.size[side] };
  return html`<section class=${'panel p-' + side} style=${style} key=${s[side]}><${Resizer} side=${side} /><${P} s=${s} /></section>`;
}

function Title({ s }) {
  const [edit, setEdit] = useState(false);
  if (s.runId) return html`<h1 title=${s.runId}>${s.snap?.flow_name || s.runId}</h1>`;
  return edit
    ? html`<input class="inp title-in" autoFocus value=${s.flow.name} onInput=${(e) => set({ flow: { ...S.flow, name: e.target.value } })} onBlur=${() => setEdit(false)} onKeyDown=${(e) => e.key === 'Enter' && setEdit(false)} />`
    : html`<h1 class="editable" title="Rename" onClick=${() => setEdit(true)}>${s.flow.name} <span class="dim">✎</span></h1>`;
}
function Top({ s }) {
  const ns = nodes(s).filter((n) => !n.draft), done = ns.filter((n) => n.status === 'done').length;
  const t0 = s.snap?.created_at, live = s.snap?.status === 'running';
  const elapsed = t0 ? (live ? nowSecs() : s.snap.updated_at) - t0 : 0;
  const u = attention().length, dr = draftCount(), stale = ns.filter((n) => n.status === 'stale').length;
  return html`<header class="top">
    <div class="logo">ZORI<i /></div>
    <div class="run-t"><div class="crumb">${s.runId ? (s.snap?.status || 'loading') + ' run' : 'draft canvas'} ›</div><${Title} s=${s} /></div>
    ${s.runId && html`<div class="run-m"><div class="prog"><i style=${{ width: `${ns.length ? (done / ns.length) * 100 : 0}%` }} /></div><span>${done}/${ns.length} done</span>
      <span class="sep" /><span><${Icon} n="clock" s=${13} /> ${clock(Math.max(0, elapsed))}</span>
      <span class="sep" /><span class=${'conn c-' + s.conn}>● ${s.conn}</span></div>`}
    <span class="grow" />
    ${dr > 0 && html`<button class="btn go" onClick=${() => runDrafts()}><${Icon} n="play" s=${12} /> ${s.runId ? `Run ${dr} new` : `Run flow (${dr})`}</button>`}
    ${!s.runId && dr > 0 && html`<button class="btn" onClick=${savePipeline}><${Icon} n="artifacts" s=${13} /> Save</button>`}
    ${stale > 0 && html`<button class="btn" onClick=${rerunStale}><${Icon} n="refresh" s=${13} /> Rerun stale (${stale})</button>`}
    ${s.runId && html`<button class="btn" onClick=${() => set({ compare: s.sel.length > 1 ? s.sel : siblings(s.sel[0] || ns[0]?.id) })}><${Icon} n="compare" s=${13} /> Compare</button>`}
    <span class="menu-wrap"><button class=${'btn need' + (urgent() ? ' hot' : '')} onClick=${() => set({ inbox: !s.inbox })}><${Icon} n="user" s=${13} /> ${u} need you <${Icon} n="chevD" s=${11} /></button>
      ${s.inbox && html`<div class="dropdown" onPointerLeave=${() => set({ inbox: false })}><div class="dd-h"><${Icon} n="inbox" s=${13} /> Attention inbox (${u})</div><${InboxList} s=${s} /></div>`}</span>
    <button class="btn kbd-btn" onClick=${() => set({ palette: '' })}><kbd>⌘K</kbd> Commands</button>
  </header>`;
}

function StatusBar({ s }) {
  return html`<footer class="status"><span><i class=${'dot ' + (s.harnesses.length ? 'st-done' : 'st-failed')} /> zori ui · ${location.host}</span>
    ${s.snap?.out_dir && html`<span title="run directory">${s.snap.out_dir}</span>`}
    <span class="grow" />${s.sel.length > 1 && html`<span>${s.sel.length} selected · Compare</span>`}
    <span>${Math.round(s.view.k * 100)}%</span><span>⌘Z undo · ⌫ delete · F fit · ⌥1–9 panels</span></footer>`;
}

function DragGhost({ d, k }) {
  if (!d) return null;
  const style = d.over ? { left: d.cx - 28 * k, top: d.cy - (H / 2) * k, width: W * k, height: H * k } : { left: d.cx + 10, top: d.cy - 14 };
  return html`<div class=${'ghost' + (d.over ? ' card-like' : '')} style=${style}>
    <div class="g-in" style=${d.over ? { transform: `scale(${k})`, width: W, height: H } : null}>
      <b>${d.harness}</b><span>${d.model || 'default model'}</span>${d.over && html`<div class="g-lines"><i /><i /><i /></div>`}</div></div>`;
}

function Picker({ title, items, onPick, onClose, style, cls = '' }) {
  const [q, setQ] = useState(''), [i, setI] = useState(0);
  const inp = useRef();
  useEffect(() => inp.current?.focus(), []);
  const list = items.filter((it) => it.label.toLowerCase().includes(q.toLowerCase())).slice(0, 14);
  const key = (e) => {
    if (e.key === 'ArrowDown') setI((i + 1) % Math.max(1, list.length)), e.preventDefault();
    else if (e.key === 'ArrowUp') setI((i - 1 + list.length) % Math.max(1, list.length)), e.preventDefault();
    else if (e.key === 'Enter' && list[i]) onPick(list[i]);
    else if (e.key === 'Escape') onClose();
  };
  return html`<div class=${'picker ' + cls} style=${style} onPointerDown=${(e) => e.stopPropagation()}>
    <div class="pk-in"><${Icon} n="search" s=${14} /><input ref=${inp} placeholder=${title} value=${q} onInput=${(e) => { setQ(e.target.value); setI(0); }} onKeyDown=${key} /></div>
    <div class="pk-list">${list.map((it, j) => html`<button class=${j === i ? 'on' : ''} onMouseEnter=${() => setI(j)} onClick=${() => onPick(it)}>
      ${it.icon && html`<${Icon} n=${it.icon} s=${13} />`}<span>${it.label}</span>${it.hint && html`<kbd>${it.hint}</kbd>`}</button>`)}
      ${!list.length && html`<div class="empty">no match</div>`}</div></div>`;
}
const fleetItems = () => {
  const seen = new Set(), out = [];
  for (const f of S.fleet) out.push({ label: `${f.harness} · ${f.model}`, h: f.harness, m: f.model, icon: 'star' }), seen.add(`${f.harness}/${f.model}`);
  for (const h of S.harnesses) {
    const list = S.models[h.id]?.models || [];
    if (!h.supports_model || !list.length) out.push({ label: `${h.id} · default model`, h: h.id, m: null, icon: 'fleet' });
    for (const m of list) if (!seen.has(`${h.id}/${m}`)) out.push({ label: `${h.id} · ${m}`, h: h.id, m, icon: 'fleet' });
  }
  return out;
};
function Palette({ s }) {
  const items = [
    { label: `Run new nodes (${draftCount()})`, icon: 'play', run: () => runDrafts() },
    { label: 'New canvas', icon: 'plus', run: newCanvas },
    { label: 'Save canvas as pipeline', icon: 'artifacts', run: savePipeline },
    { label: 'Rerun stale', icon: 'refresh', run: rerunStale },
    { label: 'Compare selection', icon: 'compare', run: () => set({ compare: s.sel.length > 1 ? s.sel : siblings(s.sel[0]) }) },
    { label: 'Fit graph to view', icon: 'fit', hint: 'F', run: fitView },
    { label: 'Undo', icon: 'undo', hint: '⌘Z', run: undoLast },
    ...ALL.map((t) => ({ label: `Toggle ${t.title}`, icon: t.id, hint: `⌥${t.k}`, run: () => toggle(t.side, t.id) })),
    ...nodes().map((n) => ({ label: `Go to node: ${n.id}`, icon: 'node', run: () => focusNode(n.id) })),
    ...s.runs.filter((r) => !r.chat_title).slice(0, 20).map((r) => ({ label: `Open run: ${r.flow_name} (${r.status})`, icon: 'runs', run: () => import('./store.js').then((m) => m.openRun(r.run_id)) })),
    ...fleetItems().map((f) => ({ ...f, label: `Add node: ${f.label}`, run: () => addCentered(f.h, f.m) })),
  ];
  return html`<div class="scrim" onPointerDown=${() => set({ palette: null })}><${Picker} cls="palette" title="Type a command, node, run or model…" items=${items}
    onPick=${(it) => { set({ palette: null }); it.run(); }} onClose=${() => set({ palette: null })} /></div>`;
}
function QuickAdd({ q }) {
  return html`<div class="scrim clear" onPointerDown=${() => set({ quick: null })}><${Picker} cls="quick" title=${q.from ? `new node fed by ${q.from}…` : 'add node: harness or model…'}
    items=${fleetItems()} style=${{ left: Math.min(q.cx, innerWidth - 300), top: Math.min(q.cy, innerHeight - 360) }}
    onPick=${(f) => { set({ quick: null }); addNode({ harness: f.h, model: f.m, x: Math.round(q.x / 10) * 10, y: Math.round(q.y / 10) * 10, from: q.from }); }}
    onClose=${() => set({ quick: null })} /></div>`;
}

function Compare({ s }) {
  const all = nodes(s), ids = s.compare.filter((id) => all.some((n) => n.id === id)), get = (id) => all.find((n) => n.id === id);
  const rows = [['time', (n) => dur(runSecs(n))], ['turns', (n) => (n.turns || []).length], ['output', (n) => size(n.output?.length || 0)],
    ['attempts', (n) => (n.attempts || []).length], ['watchdog', (n) => (n.nudges || []).length], ['exit', (n) => n.exit_code ?? '—']];
  const done = ids.map(get).filter((n) => n.status === 'done');
  const best = done.length > 1 ? Math.min(...done.map(runSecs)) : null;
  return html`<div class="compare"><div class="cmp-h"><${Icon} n="compare" s=${15} /><b>Compare</b><span class="dim">${ids.length} agents side by side</span><span class="grow" />
      <select class="inp sm" onChange=${(e) => e.target.value && set({ compare: [...ids, e.target.value] })}><option value="">+ add node</option>${all.filter((n) => !ids.includes(n.id) && !n.draft).map((n) => html`<option>${n.id}</option>`)}</select>
      <button class="ib" onClick=${() => set({ compare: null })}><${Icon} n="x" /></button></div>
    <div class="cmp-cols" style=${{ gridTemplateColumns: `repeat(${Math.max(1, ids.length)}, minmax(0, 1fr))` }}>${ids.map((id) => {
      const n = get(id);
      return html`<div class=${'cmp-col st-' + n.status}><div class="cmp-t"><b>${id}</b><span class=${'pill st-' + n.status}>${STATUS[n.status]?.label}</span>
          <button class="ib" onClick=${() => set({ compare: ids.filter((x) => x !== id) })}><${Icon} n="x" s=${12} /></button></div>
        <div class="dim">${n.harness} · ${n.model || 'default model'}</div>
        <table>${rows.map(([k, f]) => html`<tr><td>${k}</td><td class=${k === 'time' && best != null && n.status === 'done' && runSecs(n) === best ? 'win' : ''}>${f(n)}</td></tr>`)}</table>
        <div class="cmp-r md" dangerouslySetInnerHTML=${{ __html: MD.render(n.output || n.stderr || '') }} />
        <button class="btn" onClick=${() => { set({ compare: null }); focusNode(id); }}><${Icon} n="node" s=${12} /> Open</button></div>`;
    })}</div></div>`;
}

function App() {
  const s = useStore();
  useEffect(() => { const u = urgent(); const name = s.snap?.flow_name || s.flow.name; document.title = u ? `(${u}) zori · ${name}` : `zori · ${name}`; });
  useEffect(() => { const t = setInterval(() => S.snap?.status === 'running' && set({}), 1000); return () => clearInterval(t); }, []);
  return html`<div class=${'app' + (s.drag ? ' dragging' : '')}>
    <${Top} s=${s} />
    <div class="main">
      <${Strip} s=${s} side="left" />
      ${s.left && html`<${Panel} s=${s} side="left" />`}
      <div class="center">
        <div class="stage"><${Canvas} s=${s} />${s.compare && html`<${Compare} s=${s} />`}</div>
        ${s.bottom && html`<${Panel} s=${s} side="bottom" />`}
      </div>
      ${s.right && html`<${Panel} s=${s} side="right" />`}
      <${Strip} s=${s} side="right" />
    </div>
    <${StatusBar} s=${s} />
    <${DragGhost} d=${s.drag} k=${s.view.k} />
    ${s.palette != null && html`<${Palette} s=${s} />`}
    ${s.quick && html`<${QuickAdd} q=${s.quick} />`}
    ${s.toast && html`<div class=${'toast t-' + s.toast.kind} key=${s.toast.at}>${s.toast.text}</div>`}
  </div>`;
}

addEventListener('keydown', (e) => {
  const typing = /INPUT|TEXTAREA|SELECT/.test(document.activeElement?.tagName);
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') { e.preventDefault(); return set({ palette: S.palette == null ? '' : null }); }
  if (e.key === 'Escape') return set({ palette: null, quick: null, inbox: false, compare: null, ...(typing ? {} : { sel: [], selEdge: null }) });
  if (e.altKey && /^Digit[1-9]$/.test(e.code)) { const t = ALL.find((x) => x.k === +e.code.slice(5)); e.preventDefault(); return toggle(t.side, t.id); }
  if (typing) return;
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'z') { e.preventDefault(); return undoLast(); }
  if (e.key === 'Backspace' || e.key === 'Delete') return removeSelected();
  if (e.key === 'f' || e.key === 'F') return fitView();
  if (e.key === '+' || e.key === '=') return zoomBy(1.2);
  if (e.key === '-') return zoomBy(1 / 1.2);
});

try { const sz = JSON.parse(localStorage.getItem('zori-size') || 'null'); if (sz) S.size = { ...S.size, ...sz }; } catch { /* default sizes */ }
render(html`<${App} />`, document.getElementById('app'));
boot();
