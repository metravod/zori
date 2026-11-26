// Run: node web/graph.test.mjs
import { levels, layout, wouldCycle, uniqueId, serverTask, nearestOut, freeSpot, W, H, STEP_X, STEP_Y } from './graph.js';

let failed = 0, passed = 0;
const ok = (name, cond, detail = '') => { if (cond) passed++; else { failed++; console.error(`FAIL ${name} ${detail}`); } };
const eq = (name, a, b) => ok(name, JSON.stringify(a) === JSON.stringify(b), `\n  got  ${JSON.stringify(a)}\n  want ${JSON.stringify(b)}`);

const fan = [
  { id: 'a' }, { id: 'b' },
  { id: 'join', depends_on: ['a', 'b'] },
  { id: 'final', depends_on: ['join', 'a'] },
];
eq('levels are longest paths', Object.fromEntries(levels(fan)), { a: 0, b: 0, join: 1, final: 2 });
eq('cycles do not hang', Object.fromEntries(levels([{ id: 'x', depends_on: ['y'] }, { id: 'y', depends_on: ['x'] }])), { x: 1, y: 0 });
eq('unknown deps are ignored', Object.fromEntries(levels([{ id: 'x', depends_on: ['ghost'] }])), { x: 0 });

const pos = layout([...fan, { id: 'pinned', pos: { x: 5, y: 7 } }]);
eq('layout columns by level, rows by order', [pos.a, pos.b, pos.join, pos.final], [{ x: 0, y: 0 }, { x: 0, y: STEP_Y }, { x: STEP_X, y: 0 }, { x: 2 * STEP_X, y: 0 }]);
eq('saved positions win', pos.pinned, { x: 5, y: 7 });

const edges = [['a', 'join'], ['b', 'join'], ['join', 'final']];
ok('self edge is a cycle', wouldCycle(edges, 'a', 'a'));
ok('back edge is a cycle', wouldCycle(edges, 'final', 'a'));
ok('transitive back edge is a cycle', wouldCycle(edges, 'final', 'b'));
ok('forward edge is fine', !wouldCycle(edges, 'a', 'final'));
ok('sibling edge is fine', !wouldCycle(edges, 'a', 'b'));

eq('uniqueId slugs', uniqueId('Codex GPT/6', new Set()), 'codex-gpt-6');
eq('uniqueId counts up', uniqueId('x', new Set(['x', 'x-2'])), 'x-3');

eq('serverTask keeps unknown fields and drops canvas keys',
  serverTask({ id: 'n', harness: 'coddy', model: '', in: 'hi', depends_on: [], skills: ['s'], env: { A: '1' }, feeds: ['z'], born: 1, x: 10.4, y: 20.6 }),
  { id: 'n', harness: 'coddy', in: 'hi', skills: ['s'], env: { A: '1' }, pos: { x: 10, y: 21 } });

const cards = [{ id: 'a', x: 0, y: 0 }, { id: 'b', x: 0, y: 400 }];
eq('drop right of a card wires to it', nearestOut(cards, W + 80, H / 2 + 30)?.id, 'a');
eq('drop far away wires to nothing', nearestOut(cards, W + 900, 0), null);
eq('free spot skips an occupied slot', freeSpot(cards, 0, 0), STEP_Y);

if (failed) { console.error(`graph: ${failed} failed, ${passed} passed`); process.exit(1); }
console.log(`graph: ${passed} cases passed`);
