// Round-trip tests for the editor's spec mirror (normaliseFlow).
//
// The browser holds a raw `Flow` mirror; what normaliseFlow drops, Save/Run
// and rerun-with-fix silently lose on disk. The review run (2026-11-19,
// merge) proved all three skill fields — flow.skills, flow.skills_dirs,
// task.skills — vanished through it, stripping the user's instructions from
// every flow that carried them. The functions are sliced out of index.html
// and evaluated here (same no-drift principle as layout.test.mjs), so this
// test guards exactly the code that ships.
//
// Run: node web/spec.test.mjs
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(here, 'index.html'), 'utf8');

const fn = (name) => {
  const re = new RegExp(`function ${name}\\([\\s\\S]*?\\n\\}`);
  const m = re.exec(html);
  if (!m) { console.error(`FAIL: function ${name} not found in index.html`); process.exit(1); }
  return m[0];
};

const F = eval(`
${fn('blankTask')}
${fn('blankSpec')}
${fn('normaliseFlow')}
${fn('sameVal')}
${fn('mergeGraphIntoCanvas')}
;({ blankTask, blankSpec, normaliseFlow, sameVal, mergeGraphIntoCanvas })`);

let failed = 0, passed = 0;
const ok = (name, cond, detail) => {
  if (cond) { passed++; return; }
  failed++;
  console.error(`FAIL ${name}${detail ? '\n  ' + detail : ''}`);
};

// ---- skill fields survive normalisation (the review's M1) ----------------
{
  const flow = {
    name: 's', skills: ['zori-config'], skills_dirs: ['~/x'],
    tasks: [{ id: 'a', harness: 'echo', in: 'hi', skills: ['extra'] }],
  };
  const s = F.normaliseFlow(flow);
  ok('flow.skills survives', JSON.stringify(s.skills) === '["zori-config"]', JSON.stringify(s.skills));
  ok('flow.skills_dirs survives', JSON.stringify(s.skills_dirs) === '["~/x"]', JSON.stringify(s.skills_dirs));
  ok('task.skills survives', JSON.stringify(s.tasks[0].skills) === '["extra"]', JSON.stringify(s.tasks[0].skills));
}
// JSON round-trip is what Save/Run actually send (specForServer deep-copies);
// undefined-valued fields would vanish there — arrays must be real arrays.
{
  const s = F.normaliseFlow({ name: 'x', tasks: [{ id: 'a', harness: 'echo', in: 'hi' }] });
  const wire = JSON.parse(JSON.stringify(s));
  ok('wire keeps skills arrays even when empty',
    Array.isArray(wire.skills) && Array.isArray(wire.skills_dirs) && Array.isArray(wire.tasks[0].skills),
    JSON.stringify(wire));
}
// an opened-then-saved flow keeps every other field it already kept
{
  const flow = {
    name: 'r', cwd: '/tmp', max_parallel: 2, watchdog: { nudge_after_secs: 30 },
    inputs: { k: 'v' }, harnesses: { mine: { cmd: 'cat' } },
    tasks: [{ id: 'a', harness: 'echo', in: 'hi', args: ['-n'], env: { E: '1' },
              cwd: '/w', chat: 'sess_1', workspace: 'flow', pos: { x: 1, y: 2 } }],
  };
  const t = F.normaliseFlow(flow).tasks[0];
  ok('regression: args/env/cwd/chat/workspace/pos still ride',
    JSON.stringify(t.args) === '["-n"]' && t.env.E === '1' && t.cwd === '/w' &&
    t.chat === 'sess_1' && t.workspace === 'flow' && t.pos.x === 1,
    JSON.stringify(t));
  const f = F.normaliseFlow(flow);
  ok('regression: flow meta still rides',
    f.cwd === '/tmp' && f.max_parallel === 2 && f.inputs.k === 'v' &&
    f.harnesses.mine.cmd === 'cat', JSON.stringify(f));
}
// blank flow: an empty tasks array gets the draft block, skills stay arrays
{
  const s = F.normaliseFlow({ name: '' });
  ok('blank flow: draft block + skills arrays',
    s.tasks.length === 1 && s.tasks[0].id === 'draft' &&
    Array.isArray(s.skills) && Array.isArray(s.skills_dirs) &&
    (s.tasks[0].skills === undefined || Array.isArray(s.tasks[0].skills)), JSON.stringify(s));
}

// ---- three-way proposal merge (the canvas is the operator's board) --------
// The sequence the operator asked us to verify: they dragged a node and changed
// a parameter, then asked the agent to add a dependency. Position, parameter AND
// the new edge must come back together. The agent's proposal still carries the
// pre-drag position (it never saw the drag, it reasoned from the base), so a
// naive overwrite would roll the layout back — the three-way merge must not.
const T = (id, over) => Object.assign({ id, harness: 'coddy', model: 'coddy', in: '', depends_on: [] }, over);
const FL = (tasks, over) => Object.assign({ name: 'f', inputs: {}, harnesses: {}, tasks }, over);
{
  const base = FL([                       // what the operator was shown at send time
    T('a', { pos: { x: 0, y: 0 } }),
    T('b', { pos: { x: 240, y: 0 } }),
  ]);
  const canvas = FL([                     // the board now: a dragged, b's model switched
    T('a', { pos: { x: 640, y: 320 } }),
    T('b', { pos: { x: 240, y: 0 }, model: 'gpt-5' }),
  ]);
  const proposal = FL([                   // agent's reply: added a→b, still holds pre-drag pos,
    T('a', { pos: { x: 0, y: 0 } }),      //   and b's model at the base value (unaware of the edit)
    T('b', { pos: { x: 240, y: 0 }, depends_on: ['a'] }),
  ]);
  const m = F.mergeGraphIntoCanvas(base, canvas, proposal);
  const a = m.tasks.find(t => t.id === 'a');
  const b = m.tasks.find(t => t.id === 'b');
  ok('merge: operator-dragged node keeps its spot over the agent position',
    !!a.pos && a.pos.x === 640 && a.pos.y === 320, JSON.stringify(a && a.pos));
  ok('merge: operator-changed model survives the agent (who was unaware)',
    !!b && b.model === 'gpt-5', b && b.model);
  ok('merge: agent-added dependency lands on the board',
    !!b && JSON.stringify(b.depends_on) === '["a"]', b && JSON.stringify(b.depends_on));
}
// Both hands touched the SAME field → the operator's value wins (last writer).
{
  const m = F.mergeGraphIntoCanvas(
    FL([T('a', { in: 'orig' })]),
    FL([T('a', { in: 'OPERATOR' })]),
    FL([T('a', { in: 'AGENT' })]));
  ok('merge: operator + agent both edit a field → operator wins',
    m.tasks[0].in === 'OPERATOR', m.tasks[0].in);
}
// Only the agent changed a field (operator never touched it) → the agent's edit flows in.
{
  const m = F.mergeGraphIntoCanvas(
    FL([T('a', { in: 'orig' })]),
    FL([T('a', { in: 'orig' })]),
    FL([T('a', { in: 'AGENT', model: 'gpt-5' })]));
  ok('merge: an agent-only edit is taken (operator never touched it)',
    m.tasks[0].in === 'AGENT' && m.tasks[0].model === 'gpt-5', JSON.stringify(m.tasks[0]));
}
// A brand-new agent node is appended, unpositioned so the caller auto-places it.
{
  const m = F.mergeGraphIntoCanvas(
    FL([T('a')]),
    FL([T('a')]),
    FL([T('a'), T('b', { depends_on: ['a'] })]));
  const b = m.tasks.find(t => t.id === 'b');
  ok('merge: a brand-new agent node is added', !!b, JSON.stringify(m.tasks.map(t => t.id)));
  ok('merge: a new node arrives unpositioned for the caller to place',
    !!b && b.pos === null, JSON.stringify(b && b.pos));
}
// A node the agent dropped that predated the turn stays dropped; a node the
// operator added this turn (the agent never saw it) is never dropped.
{
  const m = F.mergeGraphIntoCanvas(
    FL([T('a'), T('old')]),                    // base: a + old
    FL([T('a'), T('old'), T('mine')]),         // canvas: operator added `mine` this turn
    FL([T('a')]));                             // agent: dropped old, kept a (never saw `mine`)
  const ids = m.tasks.map(t => t.id).sort();
  ok('merge: agent-dropped node that predated the turn stays dropped',
    !ids.includes('old'), JSON.stringify(ids));
  ok('merge: operator-added-this-turn node is never dropped by the agent',
    ids.includes('a') && ids.includes('mine'), JSON.stringify(ids));
}
// Flow-level meta: once the operator edits the top bar, the agent cannot roll it back.
{
  const m = F.mergeGraphIntoCanvas(
    FL([T('a')], { inputs: { topic: 'x' } }),
    FL([T('a')], { inputs: { topic: 'OPERATOR', keep: 'k' } }),
    FL([T('a')], { inputs: { topic: 'x' } }));
  ok('merge: operator-edited flow inputs are not rolled back by the agent',
    m.inputs.topic === 'OPERATOR' && m.inputs.keep === 'k', JSON.stringify(m.inputs));
}
// sameVal is the linchpin of "did the operator touch this" — pin its contract.
{
  ok('sameVal: object key order is not a change',
    F.sameVal({ a: 1, b: { c: 2 } }, { b: { c: 2 }, a: 1 }), 'reordered keys read as changed');
  ok('sameVal: a value change is a change',
    !F.sameVal({ a: 1 }, { a: 2 }), 'value change missed');
  ok('sameVal: array element order is significant (dependencies)',
    !F.sameVal(['a', 'b'], ['b', 'a']), 'dep reorder should read as a change');
}

// ---- proposal placement keeps the chat alive (repro 2026-11-22) -----------
// The combined verification reproduced three client failures: a valid
// proposal tore the chat down (deselect → leaveChat closed the SSE stream and
// swallowed the launch card, and reopening replayed the close); a revision
// that arrived while the chat was closed lost its new edge on reopen (the
// client had no board history to merge against); and a reply echoing
// pre-edit values reverted a manual edit made before the turn was sent.
// These drive the real openChat / placeProposalOnCanvas wiring with a mock
// EventSource — the same path the browser runs on every SSE tick.
{
  const P = (over) => Object.assign({
    raw: '```zori-flow\n```', hash: 'h1', valid: true, errors: [], warnings: [],
    levels: [['collect'], ['summarize']], status: 'proposed', run_id: null, ts: 0,
  }, over);
  // graph(): the agent's view — two nodes; summarize's input and deps vary.
  const graph = (sumIn, sumDeps) => FL([
    T('collect', { pos: { x: 100, y: 40 } }),
    T('summarize', { pos: { x: 404, y: 216 }, in: sumIn, depends_on: sumDeps }),
  ]);
  const first = P({ spec: graph('amber-canvas-first-617', []) });
  const revised = P({
    hash: 'h2',
    spec: graph('amber-canvas-first-617', ['collect']),
    base: graph('amber-canvas-first-617', []),   // the agent's last accepted graph
  });

  const controls = new Map();
  const $ = (id) => {
    if (!controls.has(id)) controls.set(id, { value: '', focus() {} });
    return controls.get(id);
  };
  const sessionStorage = new Map();
  sessionStorage.setItem = (k, v) => sessionStorage.set(k, v);
  sessionStorage.removeItem = (k) => sessionStorage.delete(k);
  const chat = { run: null, events: null, drafts: new Map() };
  const chatDialog = { open: false };
  const spec = { current: null };
  const state = { runId: null };
  let selId, editTask, editIndex, editEl, rendered;
  class EventSource {
    constructor(url) { this.url = url; }
    addEventListener(name, cb) { this.cb = cb; }
    send(snapshot) {
      if (this.closed) throw new Error('send on a closed SSE stream');
      this.cb({ data: JSON.stringify(snapshot) });
    }
    close() { this.closed = true; }
  }
  const selectTask = () => {};
  const setCfg = () => {};
  const switchNodeInput = () => {};
  const autogrow = () => {};
  const assignAutoPositions = () => {};
  const touch = () => {};
  const renderBuilder = () => {};
  const specForServer = () => structuredClone(spec.current);
  const renderRunSection = () => {
    rendered = { run: chat.run, proposal: chat.node && chat.node.proposal && chat.node.proposal.hash };
  };
  const mod = eval(`
${fn('blankTask')}
${fn('blankSpec')}
${fn('normaliseFlow')}
${fn('sameVal')}
${fn('mergeGraphIntoCanvas')}
${fn('deselect')}
${fn('leaveChat')}
${fn('openChat')}
${fn('placeProposalOnCanvas')}
;({ blankSpec, openChat, leaveChat, deselect, placeProposalOnCanvas })`);
  const node = (p) => ({ id: 'chat', proposal: p });

  // 1. A valid proposal lands with the chat LEFT OPEN: the SSE stream, the
  //    transcript and the launch card all survive the placement.
  spec.current = mod.blankSpec();
  mod.openChat('run-1');
  const es = chat.events;
  es.send({ nodes: { chat: node(revised) } });
  ok('placement: a valid proposal lands on the canvas',
    JSON.stringify(spec.current.tasks.map(t => t.id)) === '["collect","summarize"]',
    JSON.stringify(spec.current.tasks.map(t => t.id)));
  ok('placement: the chat stays attached (not torn down by deselect)',
    chat.run === 'run-1', String(chat.run));
  ok('placement: the SSE subscription stays open', !es.closed, 'stream closed');
  ok('placement: the launch card keeps its proposal',
    rendered.proposal === 'h2', String(rendered.proposal));
  ok('placement: the chat is still marked open for a reopen',
    sessionStorage.has('zori-open-chat'), 'session marker dropped');

  // 2. A same-hash tick (a status flip) never re-merges, so an edit made
  //    after the placement survives later SSE ticks.
  const afterPlace = structuredClone(spec.current);
  afterPlace.tasks[1].in = 'OPERATOR-AFTER';
  spec.current = afterPlace;
  es.send({ nodes: { chat: node(revised) } });
  ok('placement: a same-hash status tick never re-merges over later edits',
    spec.current.tasks[1].in === 'OPERATOR-AFTER', spec.current.tasks[1].in);

  // 3. A reply echoing pre-edit values (the agent repeating the old input in
  //    its revised graph) cannot revert a manual edit made before the turn
  //    was sent: the proposal's base is the agent's last accepted graph, not
  //    the board shipped with the turn (which already contains the edit).
  const edited = structuredClone(afterPlace);
  edited.tasks[1].in = 'operator-summarize-802';
  spec.current = edited;
  const echo = P({
    hash: 'h3',
    spec: graph('amber-canvas-first-617', ['collect']),
    base: graph('amber-canvas-first-617', ['collect']),
  });
  es.send({ nodes: { chat: node(echo) } });
  ok('placement: a reply echoing pre-edit values cannot revert a manual edit',
    spec.current.tasks[1].in === 'operator-summarize-802', spec.current.tasks[1].in);

  // 4. A revision that arrived while the chat was CLOSED: the reopen has no
  //    client-side board history — the proposal's own base must land the new
  //    edge on the existing node (repro: the edge vanished on reopen).
  spec.current = mod.blankSpec();
  mod.openChat('run-2');
  chat.events.send({ nodes: { chat: node(first) } });   // turn 1: edge-less graph placed
  mod.leaveChat();
  mod.openChat('run-2');                                // the operator leaves and reopens
  chat.events.send({ nodes: { chat: node(revised) } }); // the revision arrives now
  ok('placement: a revision that arrived while detached lands its edge on reopen',
    JSON.stringify(spec.current.tasks[1].depends_on) === '["collect"]',
    JSON.stringify(spec.current.tasks[1].depends_on));
  ok('placement: reopen keeps the operator-side layout',
    spec.current.tasks[1].pos.x === 404 && spec.current.tasks[1].pos.y === 216,
    JSON.stringify(spec.current.tasks[1].pos));
  // A proposal arriving while a block is selected must not take the panel
  // away or invalidate the selected task object while the operator edits it.
  editTask = spec.current.tasks[0];
  const subscribed = chat.events;
  $('#planner-msg').value = 'continue this thought';
  chat.events.send({ nodes: { chat: node(P({hash: 'h4', spec: graph('update', [])})) } });
  ok('a planner event keeps the selected block', editTask === spec.current.tasks[0]);
  mod.deselect();
  ok('background returns to the same planner and applies its pending proposal',
    !editTask && chat.events === subscribed && !subscribed.closed && chat.mergedHash === 'h4');
  ok('node navigation preserves the planner draft', $('#planner-msg').value === 'continue this thought');

}

// ---- model fleet helpers (the ⚓ Fleet picker) ------------------------------
{
  const F2 = eval(`
${fn('fleetKey')}
${fn('fleetParsed')}
${fn('fleetWire')}
;({ fleetKey, fleetParsed, fleetWire })`);

  const saved = F2.fleetParsed({
    models: [
      { harness: ' coddy ', model: ' kimi ' },     // trimmed
      { harness: 'codex', model: 'gpt-6.2-sol' },
      { harness: 'coddy', model: 'kimi' },         // duplicate after trim
      { harness: 'coddy', model: '' },             // junk: empty model
      null,                                        // junk row
      { harness: 5, model: 'x' },                  // junk: non-string
    ],
  });
  ok('fleet: trims, drops junk, dedupes, keeps order',
    JSON.stringify(F2.fleetWire(saved)) ===
    JSON.stringify({ models: [
      { harness: 'coddy', model: 'kimi' },
      { harness: 'codex', model: 'gpt-6.2-sol' },
    ]}),
    JSON.stringify(F2.fleetWire(saved)));

  ok('fleet: absent/empty payload is an empty map',
    F2.fleetParsed(null).size === 0 && F2.fleetParsed({}).size === 0);

  // The pair is the identity: the same model id on two harnesses is two picks.
  const both = F2.fleetParsed({ models: [
    { harness: 'coddy', model: 'kimi' },
    { harness: 'codex', model: 'kimi' },
  ]});
  ok('fleet: same model on two harnesses is two picks', both.size === 2);

  // wire → parse round-trips: what Save sends is what a reload reads back.
  const again = F2.fleetParsed(F2.fleetWire(both));
  ok('fleet: wire→parse round-trips',
    again.size === 2 &&
    again.get(F2.fleetKey('coddy', 'kimi')).model === 'kimi' &&
    again.get(F2.fleetKey('codex', 'kimi')).harness === 'codex',
    JSON.stringify(F2.fleetWire(again)));
}

console.log(`spec.test: ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
