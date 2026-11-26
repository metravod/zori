// Mockup entry point. Native ES modules, no bundler, no dependencies —
// this is the D14 option A in practice.

import { runs, pipelines, run } from "./data.js";
import { renderSidebar } from "./sidebar.js";
import { renderRunHeader, renderDag, renderAgents, renderTimeline } from "./center.js";
import { renderRight, renderAgentDetail } from "./right.js";

const $ = (sel) => document.querySelector(sel);

const timelineOpts = {
  get attempts() { return $("#opt-attempts").checked; },
  get nudges() { return $("#opt-nudges").checked; },
};

function renderCenter() {
  renderRunHeader($("#center"), run);
  renderDag($("#center"), run.dag);
  renderAgents($("#center"), run.agents, openAgent);
  renderTimeline($("#center"), run.timeline, timelineOpts);
}

for (const id of ["opt-attempts", "opt-nudges"]) {
  $("#" + id).addEventListener("change", () => renderTimeline($("#center"), run.timeline, timelineOpts));
}

renderSidebar($("#sidebar"), runs, pipelines, (r) => {
  if (!r.current) {
    // single-run mockup: only the active run has a detail view
    console.info("mockup: other runs are history rows only —", r.name);
  }
});
renderCenter();
renderRight($("#right"), run);

function openAgent(agent) {
  const inboxView = $("#inbox-view");
  const agentView = $("#agent-view");
  inboxView.hidden = true;
  agentView.hidden = false;
  renderAgentDetail(agentView, agent, () => {
    agentView.hidden = true;
    agentView.innerHTML = "";
    inboxView.hidden = false;
  }, (a, msg) => console.info("mockup: steer →", a.name, ":", msg));
}
