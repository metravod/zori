// Pure graph helpers for the canvas (no DOM): levels, auto-layout, cycle
// checks and the server-facing task shape. Covered by graph.test.mjs.

export const W = 236, H = 134, STEP_X = 300, STEP_Y = 160;

// Longest-path level of every task; ids in a cycle get the level they reached.
export function levels(tasks) {
  const deps = new Map(tasks.map((t) => [t.id, (t.depends_on || []).filter((d) => d !== t.id)]));
  const memo = new Map(), stack = new Set();
  const lvl = (id) => {
    if (memo.has(id)) return memo.get(id);
    if (stack.has(id) || !deps.has(id)) return -1;
    stack.add(id);
    const v = Math.max(-1, ...deps.get(id).map(lvl)) + 1;
    stack.delete(id);
    memo.set(id, v);
    return v;
  };
  return new Map(tasks.map((t) => [t.id, lvl(t.id)]));
}

// Positions for every task: its saved `pos`, else a slot in its level column.
export function layout(tasks) {
  const lv = levels(tasks), rows = new Map(), out = {};
  for (const t of tasks) {
    const l = lv.get(t.id);
    const row = rows.get(l) || 0;
    rows.set(l, row + 1);
    out[t.id] = t.pos && Number.isFinite(t.pos.x) ? { x: t.pos.x, y: t.pos.y } : { x: l * STEP_X, y: row * STEP_Y };
  }
  return out;
}

// Would the edge from → to (to reads from) close a cycle in `edges` ([from,to] pairs)?
export function wouldCycle(edges, from, to) {
  if (from === to) return true;
  const up = new Map();
  for (const [a, b] of edges) up.set(b, [...(up.get(b) || []), a]);
  const seen = new Set(), stack = [from];
  while (stack.length) {
    const cur = stack.pop();
    if (cur === to) return true;
    for (const p of up.get(cur) || []) if (!seen.has(p)) seen.add(p), stack.push(p);
  }
  return false;
}

// A unique, slug-safe id derived from `base`.
export function uniqueId(base, taken) {
  const b = String(base).toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^-+|-+$/g, '') || 'node';
  if (!taken.has(b)) return b;
  for (let i = 2; ; i++) if (!taken.has(`${b}-${i}`)) return `${b}-${i}`;
}

// The task as the server's `Task` expects it: canvas-only keys dropped,
// everything else (skills, env, args, cwd, …) kept verbatim.
export function serverTask(t) {
  const { feeds, born, newEdge, x, y, ...task } = t;
  if (x != null && y != null) task.pos = { x: Math.round(x), y: Math.round(y) };
  if (task.model == null || task.model === '') delete task.model;
  if (!task.depends_on?.length) delete task.depends_on;
  if (task.in == null) task.in = '';
  return task;
}

// The upstream that a node dropped at (wx, wy) should auto-wire to: a card whose
// output port sits just left of the drop point.
export function nearestOut(nodes, wx, wy) {
  let best = null, bd = Infinity;
  for (const n of nodes) {
    const dx = wx - (n.x + W), dy = wy - (n.y + H / 2);
    if (dx < 30 || dx > 320 || Math.abs(dy) > 160) continue;
    const d = Math.hypot(dx, dy * 1.6);
    if (d < bd) bd = d, best = n;
  }
  return best;
}

// A free y near `y` in the column at `x` so a new card does not land on another.
export function freeSpot(nodes, x, y) {
  const hit = (yy) => nodes.some((n) => Math.abs(n.x - x) < W + 20 && Math.abs(n.y - yy) < H + 16);
  for (let i = 0; i < 16; i++) for (const yy of [y + i * STEP_Y, y - i * STEP_Y]) if (!hit(yy)) return yy;
  return y;
}
