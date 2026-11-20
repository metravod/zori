// Test for the console's single (horizontal) graph layout.
//
// The layout core is DOM-free and lives in web/index.html between the LAYOUT
// markers; it is sliced out and evaluated here rather than duplicated into a
// module, so the test can never drift from what ships. It mirrors
// src/dag.rs::build_plan — whose #[test]s are the contract — and is the reason
// a flow no longer needs the server's plan to be drawn left-to-right.
//
// Run: node web/layout.test.mjs
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(here, 'index.html'), 'utf8');
const m = /\/\*LAYOUT-START\*\/([\s\S]*?)\/\*LAYOUT-END\*\//.exec(html);
if (!m) { console.error('FAIL: LAYOUT markers not found in index.html'); process.exit(1); }

const L = eval(m[1] + `
;({ placeBlocks, levelsOf, isLegacyPile, effDeps, refsIn,
    GRID_X, GRID_Y, STEP_X, STEP_Y });`);

let failed = 0, passed = 0;
const ok = (name, cond, detail) => {
  if (cond) { passed++; return; }
  failed++;
  console.error(`FAIL ${name}${detail ? '\n  ' + detail : ''}`);
};

// task helper: {id, deps?, in?, out?, pos?}
const T = (id, o = {}) => ({
  id, harness: 'echo', in: o.in || '', out: o.out || null,
  depends_on: o.deps || [], pos: o.pos || null,
});
// run placeBlocks on a fresh copy and return id -> {x,y}
const laid = (tasks) => {
  const copy = JSON.parse(JSON.stringify(tasks));
  L.placeBlocks(copy);
  const out = {};
  for (const t of copy) out[t.id] = t.pos;
  return out;
};

// ---- levels mirror the scheduler ---------------------------------------
{
  const lv = L.levelsOf([T('a'), T('b', { deps: ['a'] }), T('c', { deps: ['a'] }), T('d', { deps: ['b', 'c'] })]);
  ok('diamond layers like build_plan',
    JSON.stringify(lv) === JSON.stringify([['a'], ['b', 'c'], ['d']]), JSON.stringify(lv));
}
{
  const lv = L.levelsOf([T('a'), T('b'), T('c')]);
  ok('independent tasks share level 0',
    JSON.stringify(lv) === JSON.stringify([['a', 'b', 'c']]), JSON.stringify(lv));
}
ok('cycle has no layering',
  L.levelsOf([T('a', { deps: ['b'] }), T('b', { deps: ['a'] })]) === null);
// the layout must not be stricter than the UI needs: an unknown dep is left for
// /validate to reject (the server is the authority), and must not collapse the graph
{
  const lv = L.levelsOf([T('a', { deps: ['ghost'] }), T('b', { deps: ['a'] })]);
  ok('unknown dep ignored, not fatal',
    JSON.stringify(lv) === JSON.stringify([['a'], ['b']]), JSON.stringify(lv));
}
// src/dag.rs::self_cycle is the contract: a task on itself is a cycle. The
// canvas must not lay out a flow the runner will refuse as if it were fine.
{
  const lv = L.levelsOf([T('a', { deps: ['a'] })]);
  ok('self-dep is a cycle, as build_plan says', lv === null, JSON.stringify(lv));
  const p = laid([T('a', { deps: ['a'] }), T('b', { deps: ['a'] })]);
  ok('a self-cyclic flow still spreads rather than stacking',
    p.a.x !== p.b.x, JSON.stringify(p));
}
{
  const lv = L.levelsOf([T('a', { deps: ['b', 'b'] }), T('b')]);
  ok('duplicate dep counted once', JSON.stringify(lv) === JSON.stringify([['b'], ['a']]), JSON.stringify(lv));
}
// deps arriving as ${steps.x.output} in the prompt count for the layout too
{
  const lv = L.levelsOf([T('a'), T('b', { in: 'use ${steps.a.output} now' })]);
  ok('prompt ref layers like a dep', JSON.stringify(lv) === JSON.stringify([['a'], ['b']]), JSON.stringify(lv));
}
{
  const lv = L.levelsOf([T('a'), T('b', { out: 'save ${steps.a.output}' })]);
  ok('out ref layers like a dep', JSON.stringify(lv) === JSON.stringify([['a'], ['b']]), JSON.stringify(lv));
}

// ---- the graph is horizontal: that is the whole point -------------------
const chain = [T('a'), T('b', { deps: ['a'] }), T('c', { deps: ['b'] }), T('d', { deps: ['c'] })];
{
  const p = laid(chain);
  ok('chain goes left to right',
    p.a.x < p.b.x && p.b.x < p.c.x && p.c.x < p.d.x,
    JSON.stringify(p));
  // regression for the reported bug: a 4-deep chain was a vertical pile,
  // i.e. every block at the same x with a growing y
  ok('chain is NOT a vertical pile',
    new Set([p.a.x, p.b.x, p.c.x, p.d.x]).size === 4,
    JSON.stringify(p));
}
{
  const p = laid([T('a'), T('b', { deps: ['a'] }), T('c', { deps: ['a'] }), T('d', { deps: ['b', 'c'] })]);
  ok('parallel siblings share a column and differ in y',
    p.b.x === p.c.x && p.b.y !== p.c.y && p.a.x < p.b.x && p.b.x < p.d.x,
    JSON.stringify(p));
}
{
  // a cycle still spreads out rather than stacking
  const p = laid([T('a', { deps: ['b'] }), T('b', { deps: ['a'] }), T('c', { deps: ['b'] })]);
  ok('cyclic flow still spreads left to right',
    new Set([p.a.x, p.b.x, p.c.x]).size === 3,
    JSON.stringify(p));
}
{
  // Three independent tasks ARE one execution level, so they share a column and
  // spread over rows — the shape a DAG viewer draws for a fan-out. Horizontal
  // here means "columns are levels, read left to right", not "everything in a
  // row": forcing parallel tasks apart would misrepresent the schedule.
  const p = laid([T('a'), T('b'), T('c')]);
  ok('independent tasks share a column and spread over rows',
    p.a.x === p.b.x && p.b.x === p.c.x &&
    new Set([p.a.y, p.b.y, p.c.y]).size === 3,
    JSON.stringify(p));
}

// ---- geometry is on the grid -------------------------------------------
{
  const p = laid(chain);
  const xs = [p.a.x, p.b.x, p.c.x, p.d.x];
  ok('columns are evenly spaced',
    xs.every((x) => (x - L.GRID_X) % L.STEP_X === 0) &&
    xs.every((x, i) => i === 0 || x === xs[i - 1] + L.STEP_X),
    JSON.stringify(xs));
  ok('rows are evenly spaced', p.a.y === L.GRID_Y && (p.b.y - L.GRID_Y) % L.STEP_Y === 0);
  ok('step clears a block of NODE_W', L.STEP_X > 184, `STEP_X=${L.STEP_X}`);
}

// ---- the operator's own placement is never moved ------------------------
{
  const custom = [T('a', { pos: { x: 900, y: 700 } }), T('b', { deps: ['a'], pos: { x: 120, y: 340 } })];
  const p = laid(custom);
  ok('placed blocks keep their spot',
    p.a.x === 900 && p.a.y === 700 && p.b.x === 120 && p.b.y === 340,
    JSON.stringify(p));
}
{
  // half placed: only the gap-filler moves, the hand-placed one holds
  const p = laid([T('a', { pos: { x: 900, y: 700 } }), T('b', { deps: ['a'] })]);
  ok('a placed block holds while a new one gets positioned',
    p.a.x === 900 && p.a.y === 700 && Number.isFinite(p.b.x) && p.b.x !== p.a.x,
    JSON.stringify(p));
}
{
  ok('a lone block is not a pile worth re-deriving', !L.isLegacyPile([T('a', { pos: { x: 40, y: 40 } })]));
}

// ---- the legacy pile is re-derived --------------------------------------
// These are the coordinates the old fallback used to bake into the YAML: one
// column at GRID_X, rows stepping by GRID_Y. Opening such a file must come back
// horizontal, not stay vertical forever.
{
  const legacy = [
    T('a', { pos: { x: 40, y: 40 } }),
    T('b', { deps: ['a'], pos: { x: 40, y: 160 } }),
    T('c', { deps: ['b'], pos: { x: 40, y: 280 } }),
  ];
  ok('the legacy pile is recognized', L.isLegacyPile(legacy));
  const p = laid(legacy);
  ok('the legacy pile is re-derived horizontally',
    p.a.x === 40 && p.b.x === 280 && p.c.x === 520 && p.a.y === p.b.y && p.b.y === p.c.y,
    JSON.stringify(p));
}
{
  // A pile of genuinely parallel tasks is already its correct shape (one level,
  // one column), so re-deriving it must be a no-op rather than a sideways
  // shuffle into levels the flow does not have.
  const legacy = [
    T('a', { pos: { x: 40, y: 40 } }),
    T('b', { pos: { x: 40, y: 160 } }),
  ];
  const p = laid(legacy);
  ok('a parallel pile stays the single column it is',
    p.a.x === 40 && p.b.x === 40 && p.a.y === 40 && p.b.y === 160,
    JSON.stringify(p));
}
{
  // a block dragged off the generator's grid marks the flow as hand-placed, so
  // the re-deriver must leave it alone
  const hand = [
    T('a', { pos: { x: 40, y: 40 } }),
    T('b', { deps: ['a'], pos: { x: 41, y: 160 } }),
  ];
  ok('a nudged block means the layout is owned by the operator', !L.isLegacyPile(hand));
  const p = laid(hand);
  ok('a hand-placed pile is left as the operator left it',
    p.a.x === 40 && p.b.x === 41 && p.b.y === 160, JSON.stringify(p));
}
{
  // off-grid y (not a multiple of the step) is likewise not our fingerprint
  ok('off-grid rows are not the fingerprint',
    !L.isLegacyPile([T('a', { pos: { x: 40, y: 40 } }), T('b', { pos: { x: 40, y: 100 } })]));
}

// ---- refsIn / effDeps (shared with drawEdges) ---------------------------
ok('refsIn finds steps refs',
  JSON.stringify(L.refsIn('a ${steps.b.output} b ${steps.c.output}')) === JSON.stringify(['b', 'c']));
ok('refsIn ignores non-output refs',
  JSON.stringify(L.refsIn('${inputs.k} ${steps.d.field}')) === JSON.stringify([]));
ok('refsIn tolerates an unterminated ref',
  JSON.stringify(L.refsIn('${steps.b.output')) === JSON.stringify([]));
ok('effDeps merges declared deps and refs',
  JSON.stringify(L.effDeps(T('x', { deps: ['p'], in: '${steps.q.output}' }))) === JSON.stringify(['p', 'q']));
ok('effDeps does not duplicate',
  JSON.stringify(L.effDeps(T('x', { deps: ['p'], in: '${steps.p.output}' }))) === JSON.stringify(['p']));

// ---- placeBlocks never leaves a task unpositioned -----------------------
{
  const messy = [T('a'), T(''), T('c', { deps: ['a', 'ghost'] }), T('d', { deps: ['d'] })];
  const copy = JSON.parse(JSON.stringify(messy));
  L.placeBlocks(copy);
  ok('every task gets a position, even unnamed/cyclic/dangling ones',
    copy.every(t => t.pos && Number.isFinite(t.pos.x) && Number.isFinite(t.pos.y)),
    JSON.stringify(copy.map(t => [t.id, t.pos])));
}

console.log(failed ? `\n${passed} passed, ${failed} FAILED` : `layout: ${passed} cases passed`);
process.exit(failed ? 1 : 0);
