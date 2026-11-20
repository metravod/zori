// Test for the console's markdown renderer.
//
// The SPA is one embedded file with no build step, so the renderer is read out
// of web/index.html between its MD markers and evaluated here rather than
// duplicated into a module. Run: node web/md.test.mjs
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const html = readFileSync(join(here, 'index.html'), 'utf8');
const m = /\/\*MD-START\*\/([\s\S]*?)\/\*MD-END\*\//.exec(html);
if (!m) { console.error('FAIL: MD markers not found in index.html'); process.exit(1); }

const MD = eval(m[1] + '\nMD;');

let failed = 0, passed = 0;
function check(name, input, expect) {
  const got = MD.render(input);
  if (got === expect) { passed++; return; }
  failed++;
  console.error(`FAIL ${name}\n  in:   ${JSON.stringify(input)}\n  got:  ${JSON.stringify(got)}\n  want: ${JSON.stringify(expect)}`);
}
function has(name, input, frag) {
  const got = MD.render(input);
  if (got.includes(frag)) { passed++; return; }
  failed++;
  console.error(`FAIL ${name}\n  in:   ${JSON.stringify(input)}\n  got:  ${JSON.stringify(got)}\n  lack: ${JSON.stringify(frag)}`);
}

// ---- nothing typed by the model reaches the page as markup --------------
check('escape', '<script>alert(1)</script>',
  '<div class="md"><p>&lt;script&gt;alert(1)&lt;/script&gt;</p></div>');
has('escape in code', '`<b>x</b>`', '<code>&lt;b&gt;x&lt;/b&gt;</code>');
has('no javascript: link', '[click](javascript:alert(1))', 'click');
has('http link is clickable', '[docs](https://example.com)',
  '<a href="https://example.com" target="_blank" rel="noopener noreferrer">docs</a>');
has('bare url autolinked', 'see https://example.com/a.',
  '<a href="https://example.com/a" target="_blank" rel="noopener noreferrer">https://example.com/a</a>.');
// a sentinel smuggled in by the model must never expand to kept HTML
{
  const out = MD.render('`keepme` \u00000\u0000');
  const hits = out.split('<code>keepme</code>').length - 1;
  if (hits === 1) passed++;
  else { failed++; console.error(`FAIL sentinel smuggle: kept html expanded ${hits} times\n  got: ${JSON.stringify(out)}`); }
}

// ---- url vs. the internal sentinel -------------------------------------
// A code span abutting a bare url is stashed as \u0000N\u0000 right before the
// autolink rule runs; the old url class let NUL through, so the marker was
// swallowed into href and never restored. Exact comparison, not a fragment.
check('code span abutting a url leaves no marker in href',
  'https://a.com`code`',
  '<div class="md"><p><a href="https://a.com" target="_blank" rel="noopener noreferrer">https://a.com</a><code>code</code></p></div>');
{
  // belt-and-braces: the raw NUL must be gone from the output entirely.
  const out = MD.render('go https://a.com`x` end');
  if (!out.includes('\u0000')) passed++;
  else { failed++; console.error(`FAIL no NUL in output\n  got: ${JSON.stringify(out)}`); }
}

// ---- schemes and escaping edge cases -----------------------------------
has('mixed-case scheme is clickable', '[x](HTTPS://example.com)',
  '<a href="HTTPS://example.com" target="_blank" rel="noopener noreferrer">x</a>');
has('data: scheme is not a link', '[x](data:text/html,hi)', '>x<');
has('entities survive a code span literally', '`&amp;`', '<code>&amp;amp;</code>');
has('a single quote passes through', "it's a model's reply", "it's a model's reply");

// ---- inline -------------------------------------------------------------
check('bold', '**hi**', '<div class="md"><p><strong>hi</strong></p></div>');
check('bold underscore', '__hi__', '<div class="md"><p><strong>hi</strong></p></div>');
check('italic', '*hi*', '<div class="md"><p><em>hi</em></p></div>');
check('strike', '~~gone~~', '<div class="md"><p><del>gone</del></p></div>');
check('bold+italic', '***x***', '<div class="md"><p><strong><em>x</em></strong></p></div>');
check('snake_case untouched', 'max_parallel here', '<div class="md"><p>max_parallel here</p></div>');
check('code span hides markup', '`a **b** [c](http://x)`',
  '<div class="md"><p><code>a **b** [c](http://x)</code></p></div>');
check('double newline is one break', 'a\n\nb',
  '<div class="md"><p>a</p><p>b</p></div>');
has('single newline is a break', 'a\nb', 'a<br>b');

// ---- block --------------------------------------------------------------
check('heading', '## Title', '<div class="md"><h2>Title</h2></div>');
check('rule', '---', '<div class="md"><hr></div>');
has('fence', '```rust\nlet x = 1;\n```', '<pre data-lang="rust"><code>let x = 1;</code></pre>');
has('fence keeps markdown raw', '```\n**not bold**\n```', '<code>**not bold**</code>');
has('unterminated fence still shown', '```\ncode here', '<code>code here</code>');
has('bullet list', '- one\n- two', '<ul><li>one</li><li>two</li></ul>');
has('ordered list', '1. a\n2. b', '<ol><li>a</li><li>b</li></ol>');
has('nested list', '- a\n  - b', '<li>a<ul><li>b</li></ul></li>');
has('task done', '- [x] done', '<li class="md-task"><span class="md-box">☑</span> done</li>');
has('task open', '- [ ] todo', '<li class="md-task"><span class="md-box">☐</span> todo</li>');
has('blockquote', '> quoted', '<blockquote><p>quoted</p></blockquote>');
has('table', '| a | b |\n|---|---|\n| 1 | 2 |',
  '<th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr>');
has('table alignment', '| a |\n|:--:|\n| 1 |', '<th style="text-align:center">a</th>');

// a list interrupted by prose must not swallow it
has('list stops at prose', '- one\n\ntail text', '</ul><p>tail text</p>');
// a fence is not a heading/table even when its body looks like one
has('fence shields content', '```\n# not a heading\n```', '<code># not a heading</code>');
// ---- what must never appear in the output ------------------------------
// These are the browser-facing guarantees, asserted on the markup string: an
// injected tag stays text, so nothing can execute or load when the bubble is
// assigned to innerHTML.
const agentAnswer = [
  '## done',
  '<script>window.__pwned=1;<\/script>',
  '<img src=x onerror="window.__pwned=2">',
  '<a href="javascript:alert(3)">x</a>',
  '<iframe src="//evil"></iframe>',
  'text with <b> and & and "quotes"',
  '[a real link](https://example.com/ok)',
].join('\n');
// Only the *executable* shapes are forbidden. The escaped text still contains
// the words onerror= and javascript: as visible text — that is the point — so
// each probe is checked in the form a browser would actually act on.
const out = MD.render(agentAnswer);
for (const forbidden of ['<script', '<img', '<iframe', ' onerror="', ' onload="', 'href="javascript:', '<a href="javascript']) {
  if (out.includes(forbidden)) { failed++; console.error(`FAIL no ${forbidden}\n  got: ${JSON.stringify(out)}`); }
  else passed++;
}
has('injected tags stay text', agentAnswer, '&lt;script&gt;');
has('quotes survive as text', agentAnswer, '&quot;quotes&quot;');
// only our own anchor shape is ever produced
const anchors = out.match(/<a\b[^>]*>/g) || [];
for (const a of anchors) {
  const ok = /^<a href="(?:https?:|mailto:)[^"]*" target="_blank" rel="noopener noreferrer">$/.test(a);
  if (!ok) { failed++; console.error(`FAIL anchor shape: ${a}`); }
  else passed++;
}
if (!anchors.length) { failed++; console.error('FAIL: no anchors produced at all'); }

check('empty', '   ', '');
check('null', null, '');

console.log(failed ? `\n${passed} passed, ${failed} FAILED` : `md renderer: ${passed} cases passed`);
process.exit(failed ? 1 : 0);
