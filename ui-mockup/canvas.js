// The graph canvas: pan (drag background / trackpad), zoom (pinch or ⌘+wheel),
// drag cards, pull a wire from a card's ● output onto another card, drop a
// harness·model from the Fleet to materialise a node.
import { html, useRef, useEffect, useState } from './vendor/preact-htm.js';
import { S, set, select, connect, moveNodes, snapNodes, edges, toast, focusNode, duplicate, retry, removeSelected } from './store.js';
import { Icon, STATUS, dur, money } from './icons.js';

export const W = 236, H = 134;
export const canvasApi = { el: null };
export function toWorld(cx, cy) {
  const r = canvasApi.el.getBoundingClientRect(), v = S.view;
  return { x: (cx - r.left - v.x) / v.k, y: (cy - r.top - v.y) / v.k };
}
export function overCanvas(cx, cy) {
  const r = canvasApi.el?.getBoundingClientRect();
  return !!r && cx >= r.left && cx <= r.right && cy >= r.top && cy <= r.bottom;
}
const outP = (n) => [n.x + W, n.y + H / 2];
const inP = (n) => [n.x, n.y + H / 2];
const bez = ([x1, y1], [x2, y2]) => { const dx = Math.max(50, Math.abs(x2 - x1) * 0.5); return [x1, y1, x1 + dx, y1, x2 - dx, y2, x2, y2]; };
const d = (b) => `M${b[0]},${b[1]} C${b[2]},${b[3]} ${b[4]},${b[5]} ${b[6]},${b[7]}`;
const at = (b, t) => { const u = 1 - t, f = (i) => u * u * u * b[i] + 3 * u * u * t * b[i + 2] + 3 * u * t * t * b[i + 4] + t * t * t * b[i + 6]; return [f(0), f(1)]; };

export function hitNode(wx, wy, except) {
  return Object.values(S.run.nodes).find((n) => n.id !== except && wx >= n.x - 28 && wx <= n.x + W && wy >= n.y - 10 && wy <= n.y + H + 10);
}
// The upstream a freshly dropped node would auto-wire to: a card whose
// output port sits just left of the drop point.
export function nearestOut(wx, wy) {
  let best = null, bd = Infinity;
  for (const n of Object.values(S.run.nodes)) {
    const [px, py] = outP(n), dx = wx - px, dy = wy - py;
    if (dx < 30 || dx > 320 || Math.abs(dy) > 160) continue;
    const dist = Math.hypot(dx, dy * 1.6);
    if (dist < bd) bd = dist, best = n;
  }
  return best;
}
export function fitView() {
  const ns = Object.values(S.run.nodes), r = canvasApi.el?.getBoundingClientRect();
  if (!ns.length || !r) return;
  const x0 = Math.min(...ns.map((n) => n.x)), y0 = Math.min(...ns.map((n) => n.y));
  const x1 = Math.max(...ns.map((n) => n.x + W)), y1 = Math.max(...ns.map((n) => n.y + H));
  const k = Math.max(0.4, Math.min(1.1, (r.width - 120) / (x1 - x0), (r.height - 120) / (y1 - y0)));
  set({ glide: true, view: { k, x: (r.width - (x1 - x0) * k) / 2 - x0 * k, y: (r.height - (y1 - y0) * k) / 2 - y0 * k } });
  setTimeout(() => set({ glide: false }), 380);
}
export function zoomBy(f, cx, cy) {
  const r = canvasApi.el.getBoundingClientRect(), v = S.view;
  const mx = cx == null ? r.width / 2 : cx - r.left, my = cy == null ? r.height / 2 : cy - r.top;
  const k = Math.min(2, Math.max(0.3, v.k * f));
  set({ view: { k, x: mx - (mx - v.x) * (k / v.k), y: my - (my - v.y) * (k / v.k) } });
}

function track(onMove, onUp) {
  const mv = (e) => onMove(e), up = (e) => { removeEventListener('pointermove', mv); removeEventListener('pointerup', up); onUp(e); };
  addEventListener('pointermove', mv); addEventListener('pointerup', up);
}

const Spark = ({ v, st }) => html`<div class=${'spark st-' + st}>${v.map((h) => html`<i style=${{ height: `${Math.round(h * 100)}%` }} />`)}</div>`;

function Card({ n, sel, single, target, onDown, onPort }) {
  const st = STATUS[n.status] || STATUS.pending;
  const fresh = n.born && Date.now() - n.born < 1200;
  const failed = n.status === 'failed';
  return html`<div class=${`card st-${n.status}${sel ? ' sel' : ''}${fresh ? ' born' : ''}${target ? ' target' : ''}`}
      style=${{ left: n.x, top: n.y }} onPointerDown=${(e) => onDown(e, n)} onDblClick=${(e) => { e.stopPropagation(); focusNode(n.id); }}>
    ${single && html`<div class="card-tools" onPointerDown=${(e) => e.stopPropagation()}>
      <button title="Run / rerun" onClick=${() => retry(n.id)}><${Icon} n="play" s=${12} /></button>
      <button title="Steer (open node chat)" onClick=${() => focusNode(n.id)}><${Icon} n="steer" s=${12} /></button>
      <button title="Duplicate on another model" onClick=${() => duplicate(n.id)}><${Icon} n="dup" s=${12} /></button>
      <button title="Compare with siblings" onClick=${() => set({ compare: siblings(n.id) })}><${Icon} n="compare" s=${12} /></button>
      <button title="Delete (⌫)" onClick=${removeSelected}><${Icon} n="trash" s=${12} /></button>
    </div>`}
    <div class="card-top"><b class="card-title">${n.id}</b>
      <span class="badge"><${Icon} n=${st.icon} s=${11} w=${2.4} /> ${st.label}</span></div>
    <div class="card-sub">${n.harness} · ${n.model}${n.optional ? html` · <em>optional</em>` : ''}</div>
    <div class="card-metrics">
      <span><${Icon} n="clock" s=${12} /> ${dur(n.secs)}</span><i />
      <span><${Icon} n="wrench" s=${12} /> ${n.calls} calls</span><i />
      <span>${n.status === 'draft' ? 'est. $0.05' : money(n.cost)}</span>
    </div>
    <div class="card-act">${failed ? '>' : '▸'} ${n.note || n.activity}</div>
    ${failed && html`<button class="card-retry" onPointerDown=${(e) => e.stopPropagation()} onClick=${() => retry(n.id)}><${Icon} n="refresh" s=${12} /> Retry</button>`}
    ${(n.questions || []).some((q) => !q.closed) && html`<span class="card-ask" title="asks you">?</span>`}
    <${Spark} v=${n.spark.length ? n.spark : Array(44).fill(0.04)} st=${n.status} />
    <div class="port in" />
    <div class="port out" title="Drag to connect" onPointerDown=${(e) => onPort(e, n)} />
  </div>`;
}

export function siblings(id) {
  const n = S.run.nodes[id];
  const sib = Object.values(S.run.nodes).filter((m) => m.id !== id && JSON.stringify(m.depends_on) === JSON.stringify(n.depends_on) && m.status !== 'draft');
  return [id, ...sib.map((m) => m.id)].slice(0, 3);
}

export function Canvas({ s }) {
  const ref = useRef();
  const [link, setLink] = useState(null);
  const [moving, setMoving] = useState(false);
  useEffect(() => {
    canvasApi.el = ref.current;
    const wheel = (e) => {
      e.preventDefault();
      if (e.ctrlKey || e.metaKey) return zoomBy(Math.exp(-e.deltaY * 0.01), e.clientX, e.clientY);
      set({ view: { ...S.view, x: S.view.x - e.deltaX, y: S.view.y - e.deltaY } });
    };
    ref.current.addEventListener('wheel', wheel, { passive: false });
    requestAnimationFrame(fitView);
    return () => ref.current?.removeEventListener('wheel', wheel);
  }, []);

  const bgDown = (e) => {
    if (e.button !== 0 || e.target.closest('.card, .cv-ui')) return;
    const sx = e.clientX, sy = e.clientY, v0 = { ...S.view };
    let moved = false;
    ref.current.classList.add('panning');
    track((ev) => {
      const dx = ev.clientX - sx, dy = ev.clientY - sy;
      moved = moved || Math.abs(dx) + Math.abs(dy) > 3;
      if (moved) set({ view: { ...v0, x: v0.x + dx, y: v0.y + dy } });
    }, () => { ref.current.classList.remove('panning'); if (!moved) set({ sel: [], selEdge: null, quick: null }); });
  };
  const bgDbl = (e) => {
    if (e.target.closest('.card, .cv-ui')) return;
    const w = toWorld(e.clientX, e.clientY);
    set({ quick: { x: w.x - W / 2, y: w.y - H / 2, cx: e.clientX, cy: e.clientY } });
  };
  const cardDown = (e, n) => {
    if (e.button !== 0 || e.target.closest('button, .port')) return;
    e.stopPropagation();
    const add = e.shiftKey || e.metaKey;
    const ids = S.sel.includes(n.id) && !add ? S.sel : [n.id];
    let last = [e.clientX, e.clientY], moved = false, first = true;
    track((ev) => {
      const dx = (ev.clientX - last[0]) / S.view.k, dy = (ev.clientY - last[1]) / S.view.k;
      if (!moved && Math.abs(ev.clientX - e.clientX) + Math.abs(ev.clientY - e.clientY) < 4) return;
      if (!moved) { moved = true; setMoving(true); if (!S.sel.includes(n.id)) select(n.id); }
      moveNodes(ids, dx, dy, first);
      first = false; last = [ev.clientX, ev.clientY];
    }, () => { setMoving(false); moved ? snapNodes(ids) : select(n.id, add); set({ selEdge: null }); });
  };
  const portDown = (e, n) => {
    e.stopPropagation(); e.preventDefault();
    const p = toWorld(e.clientX, e.clientY);
    setLink({ from: n.id, x: p.x, y: p.y });
    let tgt = null;
    track((ev) => {
      const w = toWorld(ev.clientX, ev.clientY);
      tgt = hitNode(w.x, w.y, n.id);
      setLink({ from: n.id, x: w.x, y: w.y, target: tgt?.id });
    }, (ev) => {
      setLink(null);
      if (tgt) {
        const err = connect(n.id, tgt.id);
        err ? toast({ cycle: 'That wire would close a cycle', dup: 'Already connected', self: 'A node cannot feed itself' }[err], 'warn')
            : toast(`${n.id} → ${tgt.id} · ${tgt.id} now reads \${steps.${n.id}.output}`);
      } else if (overCanvas(ev.clientX, ev.clientY)) {
        const w = toWorld(ev.clientX, ev.clientY);
        set({ quick: { x: w.x, y: w.y - H / 2, cx: ev.clientX, cy: ev.clientY, from: n.id } });
      }
    });
  };

  const ns = s.run.nodes, v = s.view, es = edges();
  const single = s.sel.length === 1 && !moving && !link ? s.sel[0] : null;
  const now = Date.now();
  const labelled = new Set();
  const snap = s.drag?.over && s.drag.snap && ns[s.drag.snap];
  const counts = Object.values(ns).reduce((m, n) => ((m[n.status] = (m[n.status] || 0) + 1), m), {});
  return html`<div class=${'canvas' + (link ? ' linking' : '') + (s.drag?.over ? ' dropping' : '')} ref=${ref} onPointerDown=${bgDown} onDblClick=${bgDbl}
      style=${{ backgroundPosition: `${v.x}px ${v.y}px`, backgroundSize: `${22 * v.k}px ${22 * v.k}px` }}>
    <div class=${'world' + (s.glide ? ' glide' : '')} style=${{ transform: `translate(${v.x}px,${v.y}px) scale(${v.k})` }}>
      <svg class="edges" width="1" height="1">
        <defs>${['done', 'running', 'failed', 'stale', 'waiting', 'pending', 'draft', 'skipped', 'sel'].map((k) => html`
          <marker id=${'ar-' + k} viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
            <path d="M0,1 L9,5 L0,9 z" class=${'ah st-' + k} /></marker>`)}</defs>
        ${es.map(([a, b]) => {
          const A = ns[a], B = ns[b], bz = bez(outP(A), inP(B));
          const isSel = s.selEdge && s.selEdge[0] === a && s.selEdge[1] === b;
          const fresh = B.newEdge?.from === a && now - B.newEdge.at < 900 || (B.born && now - B.born < 900);
          const flow = B.status === 'running' && A.status === 'done';
          return html`<g key=${a + '>' + b}>
            <path class="edge-hit" d=${d(bz)} onPointerDown=${(e) => { e.stopPropagation(); set({ selEdge: [a, b], sel: [] }); }} />
            <path class=${`edge st-${A.status}${isSel ? ' sel' : ''}${fresh ? ' fresh' : ''}${flow ? ' flow' : ''}`} d=${d(bz)}
              marker-end=${`url(#ar-${isSel ? 'sel' : A.status})`} pathLength=${fresh ? 1 : undefined} /></g>`;
        })}
        ${link && html`<path class=${'edge live' + (link.target ? ' ok' : '')} d=${d(bez(outP(ns[link.from]), link.target ? inP(ns[link.target]) : [link.x, link.y]))} />`}
        ${snap && html`<path class="edge live ok" d=${d(bez(outP(snap), [s.drag.wx, s.drag.wy + H / 2]))} />`}
      </svg>
      ${Object.values(ns).map((n) => html`<${Card} key=${n.id} n=${n} sel=${s.sel.includes(n.id)} single=${single === n.id}
          target=${link?.target === n.id || snap?.id === n.id} onDown=${cardDown} onPort=${portDown} />`)}
      ${es.map(([a, b]) => {
        const A = ns[a], hot = s.sel.includes(a) || s.sel.includes(b) || (s.selEdge && s.selEdge[0] === a && s.selEdge[1] === b);
        if (!A.artifact || !hot || labelled.has(a + b)) return null;
        labelled.add(a + b);
        const [x, y] = at(bez(outP(A), inP(ns[b])), 0.5);
        return html`<div class="elabel cv-ui" style=${{ left: x, top: y }}>${A.artifact.name} · ${A.artifact.size}</div>`;
      })}
    </div>
    <div class="cv-head cv-ui">
      <span>DAG</span><span>·</span><span>${Object.keys(ns).length} nodes</span><span>·</span>
      ${Object.entries(counts).map(([k, c]) => html`<span class=${'cnt st-' + k}><i />${c} ${STATUS[k]?.label || k}</span>`)}
      <span>·</span><span>critical path ≈ 8m 14s</span>
    </div>
    <div class="cv-zoom cv-ui">
      <button title="Zoom out" onClick=${() => zoomBy(1 / 1.2)}><${Icon} n="minus" s=${12} /></button>
      <span>${Math.round(v.k * 100)}%</span>
      <button title="Zoom in" onClick=${() => zoomBy(1.2)}><${Icon} n="plus" s=${12} /></button>
      <button title="Fit (F)" onClick=${fitView}><${Icon} n="fit" s=${12} /></button>
    </div>
    <div class="cv-hint cv-ui">${s.drag?.over
      ? (snap ? html`release → new node wired after <b>${snap.id}</b>` : 'release to drop · near a card’s output to auto-wire')
      : link ? (link.target ? html`release → <b>${link.from}</b> feeds <b>${link.target}</b>` : 'drop on a card to connect · on empty space to add a node')
      : html`drag a model from <b>Fleet</b> · pull <b>●</b> to wire · double-click to add · <b>⌘K</b> commands`}</div>
  </div>`;
}
