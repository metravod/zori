//! Live graph edits (D12 as amended 2026-11-26): the operator can add a node
//! to a running run, wire or unwire edges and remove a node that is not
//! working, the way they would open another agent session by hand. The boot
//! spec stays in [`TrackedRun::flow`]; edits live in [`Topology`] plus the
//! per-task overrides, and every reader resolves tasks through
//! [`TrackedRun::task`] / [`TrackedRun::tasks`].

use std::collections::HashSet;

use super::*;
use crate::config::TaskPos;
use crate::state::TaskNode;

/// Tasks added after boot, in insertion order, and boot tasks removed since.
#[derive(Default)]
pub struct Topology {
    pub(super) added: Vec<Task>,
    pub(super) removed: HashSet<String>,
}

/// One atomic edit of a live run's graph. Applied all-or-nothing.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct GraphPatch {
    /// New tasks. Each launches as soon as its inputs settle.
    #[serde(default)]
    pub add: Vec<Task>,
    /// Edges `[from, to]`: `to` starts reading `from`'s result.
    #[serde(default)]
    pub wire: Vec<(String, String)>,
    #[serde(default)]
    pub unwire: Vec<(String, String)>,
    /// Nodes to drop; dependents lose the edge.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Canvas positions (layout only, never affects execution).
    #[serde(default)]
    pub pos: BTreeMap<String, TaskPos>,
}

impl GraphPatch {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.wire.is_empty() && self.unwire.is_empty() && self.remove.is_empty() && self.pos.is_empty()
    }
}

fn busy(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Running | TaskStatus::Waiting)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl TrackedRun {
    /// Every live task, boot tasks first, then added ones, edits applied.
    pub fn tasks(&self) -> Vec<Task> {
        let ids: Vec<String> = {
            let topo = self.topology.read().expect("topology lock");
            let mut seen = HashSet::new();
            self.flow.tasks.iter().chain(topo.added.iter()).map(|t| t.id.clone()).filter(|id| seen.insert(id.clone())).collect()
        };
        ids.iter().filter_map(|id| self.task(id)).collect()
    }

    /// Level plan of the live graph (boot plan plus edits).
    pub fn live_levels(&self) -> Vec<Vec<String>> {
        let pairs: Vec<(String, Vec<String>)> = self.tasks().into_iter().map(|t| (t.id, t.depends_on)).collect();
        build_plan(&pairs).map(|p| p.levels).unwrap_or_else(|_| self.plan.levels.clone())
    }

    pub(super) fn is_removed(&self, id: &str) -> bool {
        self.topology.read().expect("topology lock").removed.contains(id)
    }

    /// Wait until every dependency of `id` has settled (is terminal or gone).
    /// Boot-time deps already are when a level starts; a dependency added
    /// live may still be working, and the node must wait for it instead of
    /// being skipped.
    pub(super) async fn wait_deps_settled(&self, emitter: &Emitter, id: &str) {
        let mut rx = emitter.subscribe();
        loop {
            let Some(task) = self.task(id) else { return };
            let snap = emitter.snapshot();
            if task.depends_on.iter().all(|d| snap.nodes.get(d).is_none_or(|n| n.status.is_terminal())) {
                return;
            }
            if let Err(tokio::sync::broadcast::error::RecvError::Closed) = rx.recv().await {
                return;
            }
        }
    }

    /// Validate and apply `patch` to the live graph and the snapshot. Returns
    /// the ids of the added nodes, for the caller to [`launch`](Self::launch).
    pub fn apply_patch(&self, emitter: &Emitter, patch: &GraphPatch) -> std::result::Result<Vec<String>, String> {
        let snap = emitter.snapshot();
        let status = |id: &str| snap.nodes.get(id).map(|n| n.status);
        let mut graph: BTreeMap<String, Task> = self.tasks().into_iter().map(|t| (t.id.clone(), t)).collect();
        let before = graph.clone();

        for id in &patch.remove {
            if graph.remove(id).is_none() {
                return Err(format!("no node '{id}'"));
            }
            if status(id).is_some_and(busy) {
                return Err(format!("node '{id}' is working — wait for it or stop it first"));
            }
            for t in graph.values_mut() {
                t.depends_on.retain(|d| d != id);
            }
        }
        for t in &patch.add {
            let id = t.id.trim();
            if !valid_id(id) {
                return Err(format!("node id '{}' must be letters, digits, '-', '_' or '.'", t.id));
            }
            if graph.contains_key(id) || snap.nodes.contains_key(id) && !patch.remove.iter().any(|r| r == id) {
                return Err(format!("a node named '{id}' already exists"));
            }
            let conn = connectors::resolve(&t.harness, self.flow.harnesses.get(&t.harness)).map_err(|e| format!("node '{id}': {e:#}"))?;
            if t.model.is_some() && !conn.supports_model {
                return Err(format!("node '{id}': harness '{}' cannot set a model per run", t.harness));
            }
            let mut t = t.clone();
            t.id = id.to_string();
            graph.insert(t.id.clone(), t);
        }
        for (from, to) in &patch.wire {
            if !graph.contains_key(from) {
                return Err(format!("no node '{from}'"));
            }
            let t = graph.get_mut(to).ok_or_else(|| format!("no node '{to}'"))?;
            if !t.depends_on.contains(from) {
                t.depends_on.push(from.clone());
            }
        }
        for (from, to) in &patch.unwire {
            let t = graph.get_mut(to).ok_or_else(|| format!("no node '{to}'"))?;
            if t.in_field.contains(&format!("${{steps.{from}.")) {
                return Err(format!("'{to}' quotes ${{steps.{from}…}} in its brief — edit the brief before unwiring"));
            }
            t.depends_on.retain(|d| d != from);
        }
        for (id, t) in &graph {
            if t.depends_on.iter().any(|d| !graph.contains_key(d)) {
                return Err(format!("node '{id}' depends on a node that does not exist"));
            }
            let rewired = before.get(id).is_some_and(|b| b.depends_on != t.depends_on);
            if rewired && status(id).is_some_and(busy) {
                return Err(format!("node '{id}' is working — rewire it when it settles"));
            }
        }
        let pairs: Vec<(String, Vec<String>)> = graph.values().map(|t| (t.id.clone(), t.depends_on.clone())).collect();
        let levels = build_plan(&pairs).map_err(|e| format!("{e}"))?.levels;
        for (id, p) in &patch.pos {
            if let Some(t) = graph.get_mut(id) {
                t.pos = Some(*p);
            }
        }

        // Commit: topology, overrides, then the observable snapshot.
        let added: Vec<String> = patch.add.iter().map(|t| t.id.trim().to_string()).collect();
        let mut rewired: Vec<String> = Vec::new();
        {
            let mut topo = self.topology.write().expect("topology lock");
            for id in &patch.remove {
                topo.added.retain(|t| &t.id != id);
                topo.removed.insert(id.clone());
            }
            for id in &added {
                topo.removed.remove(id);
                topo.added.push(graph[id].clone());
            }
        }
        {
            let mut ov = self.overrides.write().expect("overrides lock");
            for id in &added {
                ov.remove(id);
            }
        }
        for (id, t) in &graph {
            if added.contains(id) {
                continue;
            }
            if before.get(id).is_some_and(|b| b.depends_on != t.depends_on) {
                rewired.push(id.clone());
            }
            if before.get(id).is_some_and(|b| b.depends_on != t.depends_on || b.pos != t.pos) {
                self.override_task(t.clone());
            }
        }
        emitter.mutate(|st| {
            for id in &patch.remove {
                st.nodes.remove(id);
            }
            for id in &added {
                let t = &graph[id];
                st.nodes.insert(id.clone(), TaskNode::new(id, &t.harness, t.model.clone(), t.optional, t.depends_on.clone()));
            }
            for id in &rewired {
                if let Some(n) = st.nodes.get_mut(id) {
                    n.depends_on = graph[id].depends_on.clone();
                    if n.status == TaskStatus::Done {
                        n.status = TaskStatus::Stale;
                    }
                }
            }
            st.levels = levels.clone();
        });
        for id in &rewired {
            self.mark_downstream_stale(emitter, id);
        }
        Ok(added)
    }

    /// Run an added node once its inputs settle: skipped when a required
    /// input did not complete, otherwise a supervised first run.
    pub async fn launch(&self, emitter: &Emitter, id: &str) {
        install_death_handlers();
        self.wait_deps_settled(emitter, id).await;
        let Some(task) = self.task(id) else { return };
        if emitter.snapshot().nodes.get(id).is_none_or(|n| n.status != TaskStatus::Pending) {
            return;
        }
        if self.blocked(emitter, &task) {
            emitter.emit(RunEvent::Finished(id, TaskStatus::Skipped, "(a dependency did not complete)"));
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(id) {
                    n.status = TaskStatus::Skipped;
                    n.finished_at = Some(now_secs());
                }
            });
            return;
        }
        self.run_level(emitter, vec![task], false).await;
    }
}
