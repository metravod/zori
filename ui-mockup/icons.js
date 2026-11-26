import { html } from './vendor/preact-htm.js';

const P = {
  runs: '<path d="M4 6h10M4 12h10M4 18h7"/><path d="M16 14l5 3-5 3z"/>',
  pipelines: '<circle cx="5" cy="6" r="2"/><circle cx="19" cy="18" r="2"/><circle cx="19" cy="6" r="2"/><path d="M7 6h10M5 8v4a6 6 0 0 0 6 6h6"/>',
  fleet: '<rect x="6" y="6" width="12" height="12" rx="2"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/>',
  artifacts: '<path d="M14 3H6v18h12V7z"/><path d="M14 3v4h4"/>',
  timeline: '<path d="M3 6h9M7 12h12M5 18h8"/>',
  logs: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 9l3 3-3 3M12 15h5"/>',
  node: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M14 4v16"/>',
  planner: '<path d="M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8z"/><path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8z"/>',
  inbox: '<path d="M6 16v-5a6 6 0 1 1 12 0v5l2 2H4z"/><path d="M10 21h4"/>',
  play: '<path d="M7 5l11 7-11 7z"/>', stop: '<rect x="6" y="6" width="12" height="12" rx="1"/>',
  refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 4v7h-7"/>',
  compare: '<path d="M8 3v14M5 14l3 3 3-3M16 21V7M13 10l3-3 3 3"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M21 21l-5-5"/>', plus: '<path d="M12 5v14M5 12h14"/>', minus: '<path d="M5 12h14"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>', check: '<path d="M5 12l5 5 9-10"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
  wrench: '<path d="M14.5 6.5a4 4 0 0 0 5 5L12 19a2.1 2.1 0 0 1-3-3z"/>',
  coin: '<circle cx="12" cy="12" r="9"/><path d="M15 9h-4a1.5 1.5 0 0 0 0 3h2a1.5 1.5 0 0 1 0 3H9M12 7v10"/>',
  tokens: '<ellipse cx="12" cy="6" rx="8" ry="3"/><path d="M4 6v6c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12v6c0 1.7 3.6 3 8 3s8-1.3 8-3v-6"/>',
  chevL: '<path d="M15 6l-6 6 6 6"/>', chevR: '<path d="M9 6l6 6-6 6"/>', chevD: '<path d="M6 9l6 6 6-6"/>',
  send: '<path d="M4 12l16-8-6 16-2-6z"/>', dup: '<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V4H4v12h4"/>',
  warn: '<path d="M12 3l10 18H2z"/><path d="M12 10v5M12 18v.01"/>', pause: '<path d="M8 5v14M16 5v14"/>',
  ring: '<circle cx="12" cy="12" r="8"/>', star: '<path d="M12 3l2.7 5.6 6.3.9-4.5 4.4 1 6.1L12 17l-5.5 3 1-6.1L3 9.5l6.3-.9z"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9L7 7M17 17l2.1 2.1M4.9 19.1L7 17M17 7l2.1-2.1"/>',
  branch: '<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="8" r="2"/><path d="M6 7v10M18 10c0 4-6 3-11 7"/>',
  link: '<path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"/>',
  fit: '<path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/>', user: '<circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0 1 16 0"/>',
  steer: '<path d="M4 17l6-6 4 4 6-8"/><path d="M14 7h6v6"/>', grip: '<path d="M9 6h.01M15 6h.01M9 12h.01M15 12h.01M9 18h.01M15 18h.01"/>',
  undo: '<path d="M9 14L4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-3"/>', trash: '<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>',
};
export const Icon = ({ n, s = 14, w = 1.8, cls = '' }) => html`<svg class=${'ic ' + cls} width=${s} height=${s} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width=${w} stroke-linecap="round" stroke-linejoin="round" dangerouslySetInnerHTML=${{ __html: P[n] || '' }} />`;

export const STATUS = {
  done: { label: 'done', icon: 'check' }, running: { label: 'running', icon: 'ring' }, failed: { label: 'failed', icon: 'x' },
  stale: { label: 'stale', icon: 'clock' }, waiting: { label: 'waiting', icon: 'pause' }, pending: { label: 'queued', icon: 'clock' },
  draft: { label: 'draft', icon: 'plus' }, skipped: { label: 'skipped', icon: 'minus' },
};
export const dur = (s) => (s == null || s <= 0 ? '—' : s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`);
export const clock = (s) => `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;
export const money = (c) => `$${(c || 0).toFixed(2)}`;
export const tok = (t) => (t >= 1000 ? `${(t / 1000).toFixed(1)}k` : `${t || 0}`);
export const md = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/\*\*(.+?)\*\*/g, '<b>$1</b>').replace(/`(.+?)`/g, '<code>$1</code>');
