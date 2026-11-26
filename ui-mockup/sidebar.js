// Left sidebar: run history + saved pipelines.

const STATUS_LABEL = {
  running: "running", done: "completed", failed: "failed", waiting: "waiting",
};

export function renderSidebar(root, runs, pipelines, onSelect) {
  const list = root.querySelector("#run-list");
  list.innerHTML = "";
  for (const r of runs) {
    const li = document.createElement("li");
    li.className = r.current ? "selected" : "";
    const top = document.createElement("div");
    top.className = "run-row-top";
    top.innerHTML = `<span class="dot dot-${r.status}"></span><span class="run-name">${r.name}</span>`;
    const meta = document.createElement("div");
    meta.className = "run-meta";
    const parts = [STATUS_LABEL[r.status] ?? r.status, r.progress, r.elapsed];
    if (r.ago) parts.push(r.ago);
    if (r.cost) parts.push(r.cost);
    if (r.tokens) parts.push(r.tokens);
    meta.textContent = parts.join(" · ");
    li.append(top, meta);
    li.onclick = () => onSelect?.(r);
    list.appendChild(li);
  }

  const pipes = root.querySelector("#pipeline-list");
  pipes.innerHTML = "";
  for (const p of pipelines) {
    const li = document.createElement("li");
    li.innerHTML = `<div class="pipe-name">${p.name}</div>
      <div class="pipe-meta">${p.shape}</div>
      <div class="pipe-meta">${p.nodes} · ${p.models}</div>`;
    pipes.appendChild(li);
  }
}
