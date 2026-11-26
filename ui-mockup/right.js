// Right column: attention inbox + final report; agent detail replaces it on click.

export function renderRight(root, run, onSteerSend) {
  const inbox = root.querySelector("#inbox");
  inbox.innerHTML = "";
  for (const item of run.inbox) {
    const el = document.createElement("div");
    el.className = `inbox-item ${item.kind}`;
    el.innerHTML = `
      <div class="inbox-kind">${item.label} · <span class="muted" style="font-weight:400">${item.who}</span></div>
      <div class="inbox-text">${item.text}</div>
      ${item.actions ? `<div class="inbox-actions">${item.actions.map((a) => `<button class="btn ${item.kind === "f" && a.startsWith("Retry") ? "btn-danger" : ""}">${a}</button>`).join("")}</div>` : ""}`;
    inbox.appendChild(el);
  }

  const fr = root.querySelector("#final-report");
  fr.innerHTML = `
    <ol>${run.finalReport.items.map((i) => `<li>${i}</li>`).join("")}</ol>
    <div style="margin-top:10px"><button class="btn btn-primary">Use as result → downstream</button></div>`;
}

const KIND_CLASS = { tool: "k-tool", edit: "k-edit", reason: "k-reason" };
const KIND_LABEL = { tool: "TOOL", edit: "EDIT", reason: "THINK" };

export function renderAgentDetail(container, agent, onClose, onSteerSend) {
  container.innerHTML = "";
  const d = agent.detail;
  const head = document.createElement("div");
  head.innerHTML = `
    <div class="ad-head">
      <h3>${agent.name}</h3>
      <button class="ad-nav" title="previous agent">‹</button>
      <button class="ad-nav" title="next agent">›</button>
      <button class="ad-nav" id="ad-close" title="close">✕</button>
    </div>
    <div class="ad-sub">${agent.harness} · ⏱ ${agent.time} · ${agent.cost} · ${agent.tokens} tok</div>
    <div class="ad-tabs">
      ${(d?.tabs ?? ["Transcript"]).map((t, i) => `<button class="ad-tab ${i === 0 ? "active" : ""}">${t}</button>`).join("")}
    </div>`;
  container.appendChild(head);
  head.querySelector("#ad-close").onclick = onClose;

  if (!d) {
    const empty = document.createElement("div");
    empty.className = "muted";
    empty.textContent = "No transcript sample for this node in the mockup — click agent-ux for the full one.";
    container.appendChild(empty);
    appendSteer(container, agent, onSteerSend);
    return;
  }

  for (const t of d.transcript) {
    const det = document.createElement("details");
    det.className = "t-item";
    const summary = document.createElement("summary");
    summary.innerHTML = `<span class="kind ${KIND_CLASS[t.kind]}">${KIND_LABEL[t.kind]}</span> ${t.label} <span class="muted" style="margin-left:auto">${t.detail ?? ""}</span>`;
    det.appendChild(summary);
    const body = document.createElement("div");
    body.className = "t-body";
    if (t.diff) {
      body.innerHTML = t.diff.map(([k, line]) => `<div class="diff-${k}">${escapeHtml(line)}</div>`).join("");
    } else {
      body.textContent = t.body ?? "";
    }
    det.appendChild(body);
    if (t.kind === "edit") det.open = true;
    container.appendChild(det);
  }

  if (d.report) {
    const rep = document.createElement("div");
    rep.className = "report-block";
    rep.innerHTML = `<h4>${d.report.title}</h4>` + d.report.sections
      .map((s) => `<h5>${s.h}</h5><ul>${s.items.map((i) => `<li>${i}</li>`).join("")}</ul>`)
      .join("");
    container.appendChild(rep);
  }

  appendSteer(container, agent, onSteerSend);
}

function appendSteer(container, agent, onSteerSend) {
  const box = document.createElement("form");
  box.className = "steer-box";
  box.innerHTML = `<input type="text" placeholder="steer ${agent.name}…" aria-label="steer ${agent.name}">
    <button class="btn btn-primary" type="submit">Send &amp; steer</button>`;
  box.onsubmit = (e) => {
    e.preventDefault();
    const input = box.querySelector("input");
    if (input.value.trim()) {
      onSteerSend?.(agent, input.value.trim());
      input.value = "";
    }
  };
  container.appendChild(box);
}

function escapeHtml(s) {
  return s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));
}
