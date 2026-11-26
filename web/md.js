// Markdown renderer for transcripts and reports (moved verbatim from the old
// single-file console). HTML-escapes first; only its own tags are introduced.
// ---- markdown ------------------------------------------------------------
//
// The harnesses answer in markdown; the console used to print the raw text, so
// an agent's **bold** arrived as asterisks. This is a small, dependency-free
// renderer for the transcript — the whole SPA is one embedded file with no
// build step and no network, so a library is not an option (a CDN would break
// the console for anyone running zori offline).
//
// Safety: text is HTML-escaped *first*, and only this renderer's own tags are
// ever introduced. Code spans and links are stashed behind sentinels while the
// remaining rules run, so no emphasis or autolink rule reaches inside them,
// and a URL is only made clickable for http(s)/mailto.

const MD = (() => {
  const FENCE = /^(\s*)(`{3,}|~{3,})[ \t]*([^\s`]*)[ \t]*$/;
  const HEAD = /^(#{1,6})[ \t]+(.*)$/;
  const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
  const ITEM = /^(\s*)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;
  const TASK = /^\[([ xX])\][ \t]+/;

  const esc = (s) => (s == null ? '' : String(s))
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  const indentOf = (l) => (l.match(/^[ \t]*/)[0] || '').length;

  // links and code spans are pulled out of the text and put back last, so the
  // other inline rules can never rewrite their insides
  function inline(text) {
    const keep = [];
    const stash = (html) => { keep.push(html); return '\u0000' + (keep.length - 1) + '\u0000'; };
    let s = esc(text).replace(/\u0000/g, '');

    s = s.replace(/(`+)([\s\S]*?[^`])\1(?!`)/g,
      (m, ticks, code) => stash('<code>' + code.replace(/^ | $/g, '') + '</code>'));

    // an image is a link with its alt text: the console serves no assets, and
    // a remote <img> would simply fail to load for an offline operator
    const anchor = (label, url) => {
      const raw = url.replace(/&amp;/g, '&');
      if (!/^(https?:|mailto:)/i.test(raw)) return label;
      return stash('<a href="' + esc(raw) + '" target="_blank" rel="noopener noreferrer">' + label + '</a>');
    };
    s = s.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (m, alt, url) => anchor(alt || url, url));
    s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, txt, url) => anchor(txt, url));

    s = s
      .replace(/\*\*\*(?=\S)([\s\S]*?\S)\*\*\*/g, '<strong><em>$1</em></strong>')
      .replace(/\*\*(?=\S)([\s\S]*?\S)\*\*/g, '<strong>$1</strong>')
      .replace(/(^|[^\w*])__(?=\S)([\s\S]*?\S)__(?![\w*])/g, '$1<strong>$2</strong>')
      .replace(/~~(?=\S)([\s\S]*?\S)~~/g, '<del>$1</del>')
      .replace(/\*(?=\S)([^*\n]*?\S)\*/g, '<em>$1</em>')
      .replace(/(^|[^\w_])_(?=\S)([^_\n]*?\S)_(?![\w_])/g, '$1<em>$2</em>');

    // bare urls (markdown's own link form above is already stashed). The
    // sentinel `\u0000` is excluded from the url class: an adjacent code span
    // is stashed behind it just before this rule runs, and leaving NUL in the
    // class let a real code span abut a url and smuggle its internal marker
    // into the href (the marker is never re-restored once the anchor is stashed).
    s = s.replace(/(^|[\s(])((?:https?:\/\/|mailto:)[^\s<>"')\]\u0000]+)/g, (m, pre, url) => {
      const trail = /[.,;:!?]$/.test(url) ? url.slice(-1) : '';
      const href = trail ? url.slice(0, -1) : url;
      return pre + stash('<a href="' + href + '" target="_blank" rel="noopener noreferrer">' + href + '</a>') + trail;
    });

    // a newline inside a paragraph is a line break the model actually typed
    s = s.replace(/\n/g, '<br>');
    return s.replace(/\u0000(\d+)\u0000/g, (m, i) => keep[+i] || '');
  }

  function splitRow(l) {
    return l.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim());
  }
  function isTable(lines, i) {
    if (i + 1 >= lines.length || !lines[i].includes('|')) return false;
    if (/^ {0,3}>/.test(lines[i]) || FENCE.test(lines[i])) return false;
    const sep = splitRow(lines[i + 1]);
    return sep.length >= 1 && sep.every((c) => /^:?-{1,}:?$/.test(c));
  }
  function table(lines, i) {
    const head = splitRow(lines[i]);
    const align = splitRow(lines[i + 1]).map((c) =>
      c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : c.startsWith(':') ? 'left' : '');
    const al = (a) => (a ? ' style="text-align:' + a + '"' : '');
    const rows = [];
    let j = i + 2;
    while (j < lines.length && lines[j].trim() && lines[j].includes('|')) { rows.push(splitRow(lines[j])); j++; }
    let html = '<table><thead><tr>';
    head.forEach((c, k) => { html += '<th' + al(align[k]) + '>' + inline(c) + '</th>'; });
    html += '</tr></thead><tbody>';
    for (const r of rows) {
      html += '<tr>';
      head.forEach((_, k) => { html += '<td' + al(align[k]) + '>' + inline(r[k] == null ? '' : r[k]) + '</td>'; });
      html += '</tr>';
    }
    return [html + '</tbody></table>', j];
  }

  function listBlock(lines, start) {
    const rows = [];
    let i = start;
    while (i < lines.length) {
      const line = lines[i];
      if (!line.trim()) {                       // a blank line only ends the list
        let j = i;                               // when the next text is not part of it
        while (j < lines.length && !lines[j].trim()) j++;
        if (j < lines.length && (ITEM.test(lines[j]) || /^ {2,}\S/.test(lines[j]))) { i = j; continue; }
        break;
      }
      const m = ITEM.exec(line);
      if (m) { rows.push({ indent: m[1].length, item: true, ordered: /\d/.test(m[2]), text: m[3] }); i++; continue; }
      if (/^ {2,}\S/.test(line) && rows.length) { rows.push({ indent: indentOf(line), item: false, text: line.trim() }); i++; continue; }
      break;
    }
    return [renderList(rows), i];
  }

  function renderList(rows) {
    if (!rows.length) return '';
    const base = rows[0].indent;
    const ordered = rows[0].ordered;
    let out = '';
    let i = 0;
    while (i < rows.length) {
      if (rows[i].indent < base) break;
      if (rows[i].indent > base) { out += '<li>' + renderList(rows.slice(i)) + '</li>'; break; }
      const content = [rows[i].text];
      i++;
      const inner = [];
      while (i < rows.length && rows[i].indent > base) { inner.push(rows[i]); i++; }
      let innerHtml = '';
      if (inner.length && inner[0].item) innerHtml = renderList(inner);
      else for (const r of inner) content.push(r.text);
      let task = '';
      content[0] = content[0].replace(TASK, (m, box) => { task = box.toLowerCase(); return ''; });
      const body = blocks(content.join('\n')).replace(/^<p>(?:(?!<\/p>)[\s\S])*<\/p>$/, (m) => m.slice(3, -4));
      out += task === ''
        ? '<li>' + body + innerHtml + '</li>'
        : '<li class="md-task"><span class="md-box">' + (task === 'x' ? '☑' : '☐') + '</span> ' + body + innerHtml + '</li>';
    }
    return '<' + (ordered ? 'ol' : 'ul') + '>' + out + '</' + (ordered ? 'ol' : 'ul') + '>';
  }

  function blocks(text) {
    const lines = text.replace(/\r\n?/g, '\n').split('\n');
    let out = '', para = [], fence = null, fenceLang = '', fenceBody = [];
    const flush = () => { if (para.length) { out += '<p>' + inline(para.join('\n')) + '</p>'; para = []; } };
    const code = () => '<pre' + (fenceLang ? ' data-lang="' + esc(fenceLang) + '"' : '') +
      '><code>' + esc(fenceBody.join('\n')) + '</code></pre>';
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      if (fence !== null) {
        const e = FENCE.exec(line);
        if (e && e[2][0] === fence[0] && e[2].length >= fence.length) { out += code(); fence = null; fenceLang = ''; fenceBody = []; }
        else fenceBody.push(line);
        continue;
      }
      const f = FENCE.exec(line);
      if (f) { flush(); fence = f[2]; fenceLang = f[3] || ''; fenceBody = []; continue; }
      if (!line.trim()) { flush(); continue; }
      if (HR.test(line)) { flush(); out += '<hr>'; continue; }
      const h = HEAD.exec(line);
      if (h) { flush(); out += '<h' + h[1].length + '>' + inline(h[2]) + '</h' + h[1].length + '>'; continue; }
      if (/^ {0,3}>/.test(line)) {
        flush();
        const body = [];
        while (i < lines.length && /^ {0,3}>/.test(lines[i])) { body.push(lines[i].replace(/^ {0,3}> ?/, '')); i++; }
        out += '<blockquote>' + blocks(body.join('\n')) + '</blockquote>';
        i--; continue;
      }
      if (isTable(lines, i)) { const [html, next] = table(lines, i); out += html; i = next - 1; continue; }
      if (ITEM.test(line)) { const [html, next] = listBlock(lines, i); out += html; i = next - 1; continue; }
      para.push(line);
    }
    if (fence !== null) out += code();          // unterminated fence: still show the code
    flush();
    return out;
  }

  return {
    render(text) {
      const t = text == null ? '' : String(text);
      if (!t.trim()) return '';
      return '<div class="md">' + blocks(t) + '</div>';
    },
  };
})();

export default MD;
