import { html, render, useState, useEffect, useRef } from './vendor/preact-htm.js';
import * as D from './data.js';
import { S, set, useStore, toggle, attention, urgent, drafts, runDrafts, rerunStale, stopRun, undoLast, removeSelected, focusNode, addNode, select } from './store.js';
import { Canvas, fitView, siblings, zoomBy, W, H } from './canvas.js';
import { Runs, Pipelines, Fleet, Artifacts, Timeline, Logs, NodePanel, Planner, Inbox, InboxList, fleetDrag } from './panels.js';
import { Icon, STATUS, dur, clock, money, tok, md } from './icons.js';

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
      const d = (side === 'bottom' ? start - ev.clientY : side === 'left' ? ev.clientX - start : start - ev.clientX);
      set({ size: { ...S.size, [side]: Math.max(180, Math.min(side === 'bottom' ? 520 : 640, s0 + d)) } });
    };
    const up = () => { removeEventListener('pointermove', mv); removeEventListener('pointerup', up); };
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

function Top({ s }) {
  const ns = Object.values(s.run.nodes), done = ns.filter((n) => n.status === 'done').length;
  const cost = ns.reduce((a, n) => a + n.cost, 0), toks = ns.reduce((a, n) => a + n.tokens, 0);
  const u = attention().length, dr = drafts().length;
  return html`<header class="top">
    <div class="logo">ZORI<i /></div>
    <div class="run-t"><div class="crumb">${s.run.cwd} ›</div><h1>${s.run.flow_name}</h1></div>
    <div class="run-m"><div class="prog"><i style=${{ width: `${(done / ns.length) * 100}%` }} /></div><span>${done}/${ns.length} done</span>
      <span class="sep" /><span><${Icon} n="clock" s=${13} /> ${clock(s.run.elapsed)}</span><span class="sep" /><span>${money(cost)} · ${tok(toks)} tok</span></div>
    <span class="grow" />
    ${dr > 0 && html`<button class="btn go" onClick=${runDrafts}><${Icon} n="play" s=${12} /> Run ${dr} new</button>`}
    <button class="btn" onClick=${rerunStale}><${Icon} n="refresh" s=${13} /> Rerun stale</button>
    <button class="btn" onClick=${stopRun}><${Icon} n="stop" s=${12} /> Stop</button>
    <button class="btn" onClick=${() => set({ compare: s.sel.length > 1 ? s.sel : siblings(s.sel[0] || 'evidence') })}><${Icon} n="compare" s=${13} /> Compare</button>
    <span class="menu-wrap"><button class=${'btn need' + (u ? ' hot' : '')} onClick=${() => set({ inbox: !s.inbox })}><${Icon} n="user" s=${13} /> ${u} need you <${Icon} n="chevD" s=${11} /></button>
      ${s.inbox && html`<div class="dropdown" onPointerLeave=${() => set({ inbox: false })}><div class="dd-h"><${Icon} n="inbox" s=${13} /> Attention inbox (${u})</div><${InboxList} s=${s} /></div>`}</span>
    <button class="btn kbd-btn" onClick=${() => set({ palette: '' })}><kbd>⌘K</kbd> Command palette</button>
  </header>`;
}

function StatusBar({ s }) {
  return html`<footer class="status"><span><i class="dot st-done" /> local daemon :7070</span><span><${Icon} n="branch" s=${12} /> ${s.run.branch}</span>
    <span>${s.run.run_id}</span><span class="grow" />${s.sel.length > 1 && html`<span>${s.sel.length} selected · Compare</span>`}
    <span>${Math.round(s.view.k * 100)}%</span><span>⌘Z undo · ⌫ delete · F fit</span></footer>`;
}

function DragGhost({ d, k }) {
  if (!d) return null;
  const style = d.over
    ? { left: d.cx - 28 * k, top: d.cy - (H / 2) * k, width: W * k, height: H * k }
    : { left: d.cx + 10, top: d.cy - 14 };
  return html`<div class=${'ghost' + (d.over ? ' card-like' : '')} style=${style}>
    <div class="g-in" style=${d.over ? { transform: `scale(${k})`, width: W, height: H } : null}>
      <b>${d.harness}</b><span>${d.model}</span>${d.over && html`<div class="g-lines"><i /><i /><i /></div>`}</div></div>`;
}

function Picker({ title, items, onPick, onClose, style, cls = '' }) {
  const [q, setQ] = useState(''), [i, setI] = useState(0);
  const inp = useRef();
  useEffect(() => inp.current?.focus(), []);
  const list = items.filter((it) => it.label.toLowerCase().includes(q.toLowerCase())).slice(0, 12);
  const key = (e) => {
    if (e.key === 'ArrowDown') setI((i + 1) % list.length), e.preventDefault();
    else if (e.key === 'ArrowUp') setI((i - 1 + list.length) % list.length), e.preventDefault();
    else if (e.key === 'Enter' && list[i]) onPick(list[i]);
    else if (e.key === 'Escape') onClose();
  };
  return html`<div class=${'picker ' + cls} style=${style} onPointerDown=${(e) => e.stopPropagation()}>
    <div class="pk-in"><${Icon} n="search" s=${14} /><input ref=${inp} placeholder=${title} value=${q} onInput=${(e) => { setQ(e.target.value); setI(0); }} onKeyDown=${key} /></div>
    <div class="pk-list">${list.map((it, j) => html`<button class=${j === i ? 'on' : ''} onMouseEnter=${() => setI(j)} onClick=${() => onPick(it)}>
      ${it.icon && html`<${Icon} n=${it.icon} s=${13} />`}<span>${it.label}</span>${it.hint && html`<kbd>${it.hint}</kbd>`}</button>`)}
      ${!list.length && html`<div class="empty">no match</div>`}</div></div>`;
}
const fleetItems = () => D.fleet.flatMap((h) => h.models.map((m) => ({ label: `${h.harness} · ${m}`, h: h.harness, m, icon: 'fleet' })));
function Palette({ s }) {
  const items = [
    { label: `Run new nodes (${drafts().length})`, icon: 'play', run: runDrafts },
    { label: 'Rerun stale', icon: 'refresh', run: rerunStale },
    { label: 'Compare selection', icon: 'compare', run: () => set({ compare: s.sel.length > 1 ? s.sel : siblings(s.sel[0] || 'evidence') }) },
    { label: 'Fit graph to view', icon: 'fit', hint: 'F', run: fitView },
    { label: 'Undo', icon: 'undo', hint: '⌘Z', run: undoLast },
    ...ALL.map((t) => ({ label: `Toggle ${t.title}`, icon: t.id, hint: `⌥${t.k}`, run: () => toggle(t.side, t.id) })),
    ...Object.keys(s.run.nodes).map((id) => ({ label: `Go to node: ${id}`, icon: 'node', run: () => focusNode(id) })),
    ...fleetItems().map((f) => ({ ...f, label: `Add node: ${f.label}`, run: () => quickAddAtCenter(f) })),
  ];
  return html`<div class="scrim" onPointerDown=${() => set({ palette: null })}><${Picker} cls="palette" title="Type a command, node or model…" items=${items}
    onPick=${(it) => { set({ palette: null }); it.run(); }} onClose=${() => set({ palette: null })} /></div>`;
}
function quickAddAtCenter(f) {
  const n = Object.values(S.run.nodes), sel = S.sel.length === 1 ? S.run.nodes[S.sel[0]] : null;
  const x = sel ? sel.x + 310 : Math.max(...n.map((m) => m.x)) + 310, y = sel ? sel.y : 160;
  addNode({ harness: f.h, model: f.m, x, y, from: sel?.id });
}
function QuickAdd({ q }) {
  return html`<div class="scrim clear" onPointerDown=${() => set({ quick: null })}><${Picker} cls="quick" title=${q.from ? `new node after ${q.from}…` : 'add node: harness or model…'}
    items=${fleetItems()} style=${{ left: Math.min(q.cx, innerWidth - 300), top: Math.min(q.cy, innerHeight - 340) }}
    onPick=${(f) => { set({ quick: null }); addNode({ harness: f.h, model: f.m, x: Math.round(q.x / 10) * 10, y: Math.round(q.y / 10) * 10, from: q.from }); }}
    onClose=${() => set({ quick: null })} /></div>`;
}

function Compare({ s }) {
  const ids = s.compare.filter((id) => s.run.nodes[id]);
  const rows = [['time', (n) => dur(n.secs)], ['tool calls', (n) => n.calls], ['cost', (n) => money(n.cost)], ['tokens', (n) => tok(n.tokens)],
    ['attempts', (n) => n.attempts.length], ['watchdog nudges', (n) => n.nudges.length], ['exit', (n) => (n.exit_code == null ? (n.status === 'done' ? '0' : '—') : n.exit_code)]];
  const report = (id) => (s.transcripts[id] || []).find((x) => x.kind === 'report');
  const best = (k) => { const v = ids.map((id) => s.run.nodes[id]).filter((n) => n.status === 'done').map((n) => n[k]); return Math.min(...v.filter((x) => x > 0)); };
  return html`<div class="compare"><div class="cmp-h"><${Icon} n="compare" s=${15} /><b>Compare</b><span class="dim">same brief · ${ids.length} agents</span><span class="grow" />
      <select class="inp sm" onChange=${(e) => e.target.value && set({ compare: [...ids, e.target.value] })}><option value="">+ add node</option>${Object.keys(s.run.nodes).filter((id) => !ids.includes(id)).map((id) => html`<option>${id}</option>`)}</select>
      <button class="ib" onClick=${() => set({ compare: null })}><${Icon} n="x" /></button></div>
    <div class="cmp-cols" style=${{ gridTemplateColumns: `repeat(${ids.length}, 1fr)` }}>${ids.map((id) => {
      const n = s.run.nodes[id], r = report(id);
      return html`<div class=${'cmp-col st-' + n.status}><div class="cmp-t"><b>${id}</b><span class=${'pill st-' + n.status}>${STATUS[n.status]?.label}</span>
          <button class="ib" onClick=${() => set({ compare: ids.filter((x) => x !== id) })}><${Icon} n="x" s=${12} /></button></div>
        <div class="dim">${n.harness} · ${n.model}</div>
        <table>${rows.map(([k, f]) => html`<tr><td>${k}</td><td class=${(k === 'time' && n.secs === best('secs')) || (k === 'cost' && n.cost === best('cost')) ? 'win' : ''}>${f(n)}</td></tr>`)}</table>
        <div class="cmp-r">${r ? r.body.map(([k, t]) => (k === 'h' ? html`<h4>## ${t}</h4>` : html`<div class="li" dangerouslySetInnerHTML=${{ __html: '– ' + md(t) }} />`)) : html`<div class="dim">${n.activity}</div>`}</div>
        <button class="btn pri" onClick=${() => set({ compare: null })}><${Icon} n="check" s=${12} /> Use this result</button></div>`;
    })}</div></div>`;
}

function App() {
  const s = useStore();
  useEffect(() => { const u = urgent(); document.title = u ? `(${u}) zori · ${s.run.flow_name}` : `zori · ${s.run.flow_name}`; });
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
  if (typing) return;
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'z') { e.preventDefault(); return undoLast(); }
  if (e.altKey && /^Digit[1-9]$/.test(e.code)) { const t = ALL.find((x) => x.k === +e.code.slice(5)); e.preventDefault(); return toggle(t.side, t.id); }
  if (e.key === 'Backspace' || e.key === 'Delete') return removeSelected();
  if (e.key === 'f' || e.key === 'F') return fitView();
  if (e.key === '+' || e.key === '=') return zoomBy(1.2);
  if (e.key === '-') return zoomBy(1 / 1.2);
});

render(html`<${App} />`, document.getElementById('app'));
