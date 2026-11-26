// Center column: run header, DAG, agent cards, timeline.

const st = (s) => ({ running: "st-running", done: "st-done", failed: "st-failed",
  waiting: "st-waiting", queued: "st-queued", approval: "st-approval" }[s] ?? "st-waiting");

export function renderRunHeader(root, run) {
  const h = root.querySelector("#run-header");
  h.innerHTML = `
    <div class="rh-top">
      <span class="dot dot-${run.status === "running" ? "running" : run.status}"></span>
      <h2>${run.name}</h2>
      <span class="muted mono">${run.cwd}</span>
    </div>
    <div class="rh-meta">
      <span>${run.status}</span><span>${run.elapsed}</span><span>${run.cost}</span><span>${run.tokens}</span>
    </div>
    ${run.question ? `<div class="question-banner">
        <span class="who">${run.question.from} asks:</span>
        <span>${run.question.text}</span>
        <button class="btn" style="margin-left:auto">reply →</button>
      </div>` : ""}`;
}

export function renderDag(root, dag) {
  root.querySelector("#dag-meta").textContent = dag.meta;
  const box = root.querySelector("#dag");
  box.innerHTML = "";

  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  box.appendChild(svg);

  const els = new Map();
  for (const n of dag.nodes) {
    const el = document.createElement("div");
    el.className = `dag-node ${st(n.status)}`;
    el.style.left = `${n.x}%`;
    el.style.top = `${n.y}%`;
    el.style.borderLeftColor = `var(--st-${n.status === "approval" ? "approval" : n.status})`;
    el.innerHTML = `<div class="n-name"><span class="n-mini dot-${n.status === "approval" ? "waiting" : n.status}" style="background:var(--st-${n.status})"></span>${n.id}</div>
      <div class="n-harness">${n.harness}</div>
      <div class="n-status">${n.note}</div>`;
    els.set(n.id, el);
    box.appendChild(el);
  }

  // edges drawn after layout so coordinates exist
  requestAnimationFrame(() => {
    const b = box.getBoundingClientRect();
    for (const [a, bId] of dag.edges) {
      const ea = els.get(a).getBoundingClientRect();
      const eb = els.get(bId).getBoundingClientRect();
      const x1 = ea.right - b.left, y1 = ea.top + ea.height / 2 - b.top;
      const x2 = eb.left - b.left, y2 = eb.top + eb.height / 2 - b.top;
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", `M${x1},${y1} C${x1 + 30},${y1} ${x2 - 30},${y2} ${x2},${y2}`);
      path.setAttribute("stroke", "#2a3340");
      path.setAttribute("fill", "none");
      svg.appendChild(path);
    }
  });
}

export function renderAgents(root, agents, onOpen) {
  const grid = root.querySelector("#agent-cards");
  grid.innerHTML = "";
  for (const a of agents) {
    const card = document.createElement("div");
    card.className = "agent-card";
    card.style.borderTopColor = `var(--st-${a.status === "approval" ? "approval" : a.status})`;
    card.innerHTML = `
      <div class="ac-top">
        <span class="ac-name">${a.name}</span>
        <span class="ac-status ${st(a.status)}">${a.status.toUpperCase()}${a.stale ? " · STALE" : ""}</span>
      </div>
      <div class="ac-harness">${a.harness}</div>
      <div class="ac-activity">${a.activity}</div>
      <div class="ac-meta">
        <span>⏱ ${a.time}</span><span>#${a.calls} calls</span><span>${a.cost}</span><span>${a.tokens} tok</span>
      </div>
      ${a.actions ? `<div class="ac-actions">${a.actions.map((x) => `<button class="btn ${x === "Retry with other model" ? "btn-danger" : ""}">${x}</button>`).join("")}</div>` : ""}`;
    card.onclick = () => onOpen?.(a);
    grid.appendChild(card);
  }
}

function fmtTick(sec) {
  const m = Math.floor(sec / 60), s = sec % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

export function renderTimeline(root, tl, opts) {
  const box = root.querySelector("#timeline");
  box.innerHTML = "";
  const pct = (sec) => `${(sec / tl.total) * 100}%`;

  for (const lane of tl.lanes) {
    const row = document.createElement("div");
    row.className = "tl-row";
    row.innerHTML = `<span class="tl-label">${lane.node}</span><div class="tl-track"></div>`;
    const track = row.querySelector(".tl-track");
    for (const s of lane.segments) {
      if (!opts.attempts && s.kind === "retry") continue;
      const bar = document.createElement("div");
      bar.className = `tl-bar ${s.kind}`;
      bar.style.left = pct(s.from);
      bar.style.width = pct(s.to - s.from);
      bar.title = `${lane.node} ${fmtTick(s.from)}–${fmtTick(s.to)}`;
      track.appendChild(bar);
    }
    if (opts.nudges) {
      for (const n of lane.nudges ?? []) {
        const tick = document.createElement("div");
        tick.className = "tl-nudge";
        tick.style.left = pct(n);
        tick.title = "watchdog nudge";
        track.appendChild(tick);
      }
    }
    box.appendChild(row);
  }

  const axis = document.createElement("div");
  axis.className = "tl-axis";
  for (let i = 0; i <= tl.ticks; i++) {
    const span = document.createElement("span");
    span.textContent = fmtTick(Math.round((tl.total / tl.ticks) * i));
    axis.appendChild(span);
  }
  box.appendChild(axis);
}
