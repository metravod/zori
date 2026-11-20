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
;({ blankTask, blankSpec, normaliseFlow })`);

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

console.log(`spec.test: ${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
