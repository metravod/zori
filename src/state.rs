//! The run's observable state — the single source of truth that the `zori ui`
//! server owns, persists to `state.json`, and streams to the browser.
//!
//! This is the "state spine" that turns zori from a print-and-forget batch
//! runner into a collaborative console: every node carries not just its result
//! but a *session handle* (so it can be resumed/steered) and a `turns`
//! transcript (what the agent said and what the human wrote). The classic
//! `zori run` path does not touch this module at all — it only runs when a run
//! is driven through [`crate::ui`].
//!
//! Design notes for the MVP:
//!   * Flows are tiny (a handful of nodes), so instead of fine-grained events
//!     we broadcast a whole [`RunState`] snapshot on every mutation. The SSE
//!     layer just forwards the latest snapshot and the SPA re-renders. Cheap,
//!     correct, and it makes the browser a pure function of the state.
//!   * `RunState` is `Serialize`/`Deserialize` so the exact same shape feeds
//!     `GET /api/runs/:id`, the SSE stream, and `state.json` on disk.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Lifecycle of a whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    /// Created but not yet executing.
    Created,
    /// A level is executing right now.
    Running,
    /// Every node reached a terminal state and none hard-failed.
    Completed,
    /// Execution finished with at least one hard failure.
    Failed,
}

/// Lifecycle of one DAG node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    /// Not started (its level has not been reached, or it waits on a dep).
    Pending,
    /// Child process is running.
    Running,
    /// Completed successfully; `output` holds its stdout.
    Done,
    /// Failed (non-zero exit, timeout, or spawn error).
    Failed,
    /// Skipped because a dependency did not complete.
    Skipped,
    /// Completed earlier, but an upstream node was steered since, so its
    /// captured output is no longer derived from the current upstream values.
    /// The human re-runs it (or the run re-runs all stale nodes) to refresh.
    Stale,
    /// The agent asked the operator a question (`ASK:`) and the run is
    /// parked waiting for the answer (`ask` holds it). Not terminal: the
    /// answer resumes the same session and the node finishes. The level
    /// barrier holds the rest of the flow meanwhile (D3), so a parked
    /// question pauses exactly its own downstream, nothing else.
    Waiting,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Skipped | TaskStatus::Stale
        )
    }
    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Stale => "stale",
            TaskStatus::Waiting => "waiting",
        }
    }
}

/// Who authored a transcript turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Agent,
    Human,
    /// zori itself: a watchdog nudge, a canned unattended answer, a
    /// "waiting for the operator" note. Recorded so the transcript tells
    /// the truth about who said what — an automated action must never
    /// masquerade as the human.
    System,
}

/// One message in a node's conversation: an agent turn or a human steering turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    /// Unix seconds when the turn was recorded.
    pub ts: u64,
}

/// How to resume this node's agent session, per harness family.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionRef {
    /// coddy: a print-run under an isolated sessions dir, reopened by id.
    Coddy {
        /// The `-sessions-dir` root that isolates this run.
        sessions_dir: PathBuf,
        /// The fixed `-session-id` this node's session was created under.
        session_id: String,
    },
    /// codex: a thread captured from `codex exec --json`'s `thread.started`.
    Codex { thread_id: String },
    /// opencode: the `ses_…` id captured from `opencode run --format json`'s
    /// `sessionID`; `opencode run -s <id>` continues it with full context.
    Opencode { session_id: String },
    /// The `echo` harness: a scripted fake session, keyed by task id. Steering
    /// it just appends turns and replies with a canned echo — no model, which
    /// is what lets the whole steering path be tested in CI.
    Echo,
    /// A harness zori does not know how to resume; steering is disabled.
    Opaque { detail: String },
}

/// Progress counters for a node, surfaced as the card's progress bar. Bytes/
/// lines come from the captured output streams (see `SharedBuf`), elapsed from
/// the wall clock between start and now/finish.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub bytes: usize,
    pub lines: usize,
    pub elapsed_secs: u64,
}

/// A question stays in the snapshot after delivery, timeout, or restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorQuestion {
    pub id: usize,
    pub text: String,
    pub options: Vec<String>,
    pub inline: bool,
    pub answer: Option<String>,
    pub closed: bool,
}

/// Lifecycle of a flow the supermodel *proposed* from a chat — the artifact
/// that carries the "propose → confirm → run" gate. The model only ever
/// **proposes** a graph (a fenced ` ```zori-flow ` block in its reply); zori
/// validates it server-side and only launches it when the operator clicks Run.
/// The model can never start it itself, which is the whole point of the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProposalStatus {
    /// Just extracted from the model's reply, awaiting the operator.
    Proposed,
    /// The operator launched it: `run_id` names the child flow run.
    Launched,
    /// The operator dismissed it; a newer block supersedes it anyway.
    Rejected,
}

/// One proposed flow, extracted from a chat node's latest agent reply and
/// validated by zori. Lives on the chat node so the console renders it as a
/// card bound to the transcript (a proposal is *part of the dialogue*, not a
/// loose file), and so a re-render is a pure function of the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowProposal {
    /// The exact YAML the model wrote inside the fence (what the card shows).
    pub raw: String,
    /// FNV-1a hash of `raw` (trimmed). The launch endpoint re-checks it so a
    /// proposal validated a moment ago can't be swapped under the Run click.
    pub hash: String,
    /// Server verdict from [`crate::proposal::validate`]: does zori accept it.
    pub valid: bool,
    /// Human-readable validation failures (empty when `valid`).
    pub errors: Vec<String>,
    /// Non-fatal notes — e.g. `${inputs.k}` nothing supplies yet (legitimate
    /// if the operator means to pass `--input` at run time).
    pub warnings: Vec<String>,
    /// The DAG level plan when valid — the console can sketch it without
    /// re-reading the YAML.
    #[serde(default)]
    pub levels: Vec<Vec<String>>,
    /// The parsed graph as JSON when `valid` — the same shape `/api/flows/open`
    /// returns, so the console can put the proposal straight onto the canvas
    /// without re-parsing the YAML. `None` on an invalid proposal (there is no
    /// graph to show) and for proposals stored by older builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
    /// The graph this proposal merges against on the canvas: the agent's last
    /// accepted graph (the previous valid proposal's `spec`), or — for a first
    /// proposal — the board the operator shipped with the turn. The console's
    /// three-way merge needs the common ancestor: with the raw turn board as
    /// the base, an edit made BEFORE sending would sit inside the base and a
    /// reply echoing the pre-edit value could revert it; after a chat reopen
    /// the client has no board history at all. `None` on invalid proposals and
    /// for proposals stored by older builds (the client falls back to its
    /// send-time snapshot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<serde_json::Value>,
    /// How many auto-fix continuation turns zori has spent asking the model to
    /// repair an invalid block. Bounded so a looping model can't spin the run.
    pub fix_rounds: u32,
    pub status: ProposalStatus,
    /// The child flow run once launched (`run_id`), else `None`.
    pub run_id: Option<String>,
    /// Unix seconds when this proposal was last extracted/updated.
    pub ts: u64,
}

/// One DAG node: its identity, status, session handle, transcript and result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,
    pub harness: String,
    pub model: Option<String>,
    pub optional: bool,
    /// Ids this node waits for — copied from the flow so the UI can draw the
    /// DAG edges straight from a snapshot, without re-reading the YAML.
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub status: TaskStatus,
    /// Present once the node has run far enough to record a session.
    pub session: Option<SessionRef>,
    pub turns: Vec<Turn>,
    pub output: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub progress: Progress,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    /// The parked question while the node is `Waiting` (the `ASK:` line),
    /// cleared when the answer arrives. Mirrors the last agent turn, but as
    /// a field so the UI/status can show it without re-parsing the
    /// transcript.
    #[serde(default)]
    pub ask: Option<String>,
    #[serde(default)]
    pub questions: Vec<OperatorQuestion>,
    /// The flow this chat node's agent last *proposed* (a ` ```zori-flow `
    /// block), validated server-side. Only ever present on a chat node; the
    /// gate that keeps the model from launching its own graph — the operator's
    /// Run click is the only thing that boots it. `None` = nothing proposed.
    #[serde(default)]
    pub proposal: Option<FlowProposal>,
}

impl TaskNode {
    /// A fresh node from a flow task's metadata, in `Pending`.
    pub fn new(id: &str, harness: &str, model: Option<String>, optional: bool, depends_on: Vec<String>) -> Self {
        TaskNode {
            id: id.to_string(),
            harness: harness.to_string(),
            model,
            optional,
            depends_on,
            status: TaskStatus::Pending,
            session: None,
            turns: Vec::new(),
            output: String::new(),
            stderr: String::new(),
            exit_code: None,
            progress: Progress::default(),
            started_at: None,
            finished_at: None,
            ask: None,
            questions: Vec::new(),
            proposal: None,
        }
    }
}

/// The whole run: metadata, the level plan, and a node per task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    /// Stable id: `<flow>-<timestamp>` (also the `zori-runs/` dir name).
    pub run_id: String,
    pub flow_name: String,
    #[serde(default)]
    pub is_chat: bool,
    pub flow_path: PathBuf,
    pub out_dir: Option<PathBuf>,
    pub status: RunStatus,
    /// The DAG level plan (each inner Vec is one level, lexicographically).
    pub levels: Vec<Vec<String>>,
    /// Nodes keyed by id (BTreeMap so JSON order is deterministic).
    pub nodes: BTreeMap<String, TaskNode>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// One flow task's metadata as passed to [`RunState::new`]:
/// (id, harness, model, optional, depends_on).
pub type TaskMeta = (String, String, Option<String>, bool, Vec<String>);

impl RunState {
    /// Build the initial state from the flow's tasks and a computed level plan.
    pub fn new(
        run_id: String,
        flow_name: String,
        flow_path: PathBuf,
        out_dir: Option<PathBuf>,
        levels: Vec<Vec<String>>,
        tasks: &[TaskMeta], // (id, harness, model, optional, deps)
    ) -> Self {
        let now = now_secs();
        let mut nodes = BTreeMap::new();
        for (id, harness, model, optional, deps) in tasks {
            nodes.insert(
                id.clone(),
                TaskNode::new(id, harness, model.clone(), *optional, deps.clone()),
            );
        }
        RunState {
            run_id,
            flow_name,
            is_chat: false,
            flow_path,
            out_dir,
            status: RunStatus::Created,
            levels,
            nodes,
            created_at: now,
            updated_at: now,
        }
    }

    /// Recompute the overall run status from node statuses. A run is `Completed`
    /// only when every node is terminal and no *strict* (non-optional) node
    /// failed or was skipped-by-failure; optional failures are `degraded`, not
    /// failed, matching `RunReport`'s accounting. A `Waiting` node keeps the
    /// run `Running`: its child is gone but the run has not moved on.
    pub fn recompute_status(&mut self) {
        let any_alive = self
            .nodes
            .values()
            .any(|n| matches!(n.status, TaskStatus::Running | TaskStatus::Waiting));
        if any_alive {
            self.status = RunStatus::Running;
            return;
        }
        let all_terminal = self.nodes.values().all(|n| n.status.is_terminal());
        if !all_terminal {
            // Some nodes still pending but none running: created or partially
            // loaded. Leave the status as-is.
            return;
        }
        let hard_failed = self.nodes.values().any(|n| {
            !n.optional && matches!(n.status, TaskStatus::Failed | TaskStatus::Skipped)
        });
        self.status = if hard_failed {
            RunStatus::Failed
        } else {
            RunStatus::Completed
        };
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The shared, observable handle to a run: the authoritative `RunState` behind a
/// lock, plus a broadcast channel carrying whole snapshots for SSE subscribers.
///
/// `Clone` is cheap (both fields are `Arc`). The executing task, the axum
/// handlers and the persistence layer all hold clones of the same `Emitter`.
#[derive(Clone)]
pub struct Emitter {
    state: Arc<Mutex<RunState>>,
    tx: broadcast::Sender<RunState>,
}

impl Emitter {
    /// Start an emitter from an initial state. Capacity is bounded; lagging SSE
    /// subscribers that miss snapshots simply refetch via `GET /api/runs/:id`.
    pub fn new(initial: RunState) -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Emitter {
            state: Arc::new(Mutex::new(initial)),
            tx,
        }
    }

    /// The current snapshot (cloned out of the lock).
    pub fn snapshot(&self) -> RunState {
        self.state.lock().unwrap().clone()
    }

    /// Subscribe to snapshots (each SSE client gets one receiver).
    pub fn subscribe(&self) -> broadcast::Receiver<RunState> {
        self.tx.subscribe()
    }

    /// Mutate the state under the lock, stamp `updated_at`, recompute the run
    /// status, then broadcast the fresh snapshot. The closure runs while the
    /// lock is held, so keep it cheap.
    pub fn mutate<F: FnOnce(&mut RunState)>(&self, f: F) {
        let mut st = self.state.lock().unwrap();
        f(&mut st);
        st.recompute_status();
        st.updated_at = now_secs();
        // A broadcast error only means there are no subscribers right now;
        // the state is still correct and persisted by the caller.
        let _ = self.tx.send(st.clone());
    }

    /// Mark a node's status, keeping `updated_at`/status bookkeeping centralised.
    pub fn set_status(&self, id: &str, status: TaskStatus) {
        self.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(id) {
                n.status = status;
                if status == TaskStatus::Running {
                    n.started_at = Some(now_secs());
                }
            }
        });
    }

    /// Record a transcript turn on a node.
    pub fn append_turn(&self, id: &str, role: Role, text: impl Into<String>) {
        let text = text.into();
        self.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(id) {
                n.turns.push(Turn { role, text, ts: now_secs() });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, optional: bool) -> (String, String, Option<String>, bool, Vec<String>) {
        (id.into(), "echo".into(), None, optional, vec![])
    }

    fn state() -> RunState {
        RunState::new(
            "r1".into(),
            "demo".into(),
            "demo.yml".into(),
            None,
            vec![vec!["a".into()], vec!["b".into()]],
            &[node("a", false), node("b", false)],
        )
    }

    #[test]
    fn new_starts_created_with_pending_nodes() {
        let st = state();
        assert_eq!(st.status, RunStatus::Created);
        assert_eq!(st.nodes.len(), 2);
        assert_eq!(st.nodes["a"].status, TaskStatus::Pending);
        assert_eq!(st.nodes["a"].harness, "echo");
    }

    #[test]
    fn completed_only_when_all_done() {
        let mut st = state();
        st.nodes.get_mut("a").unwrap().status = TaskStatus::Done;
        st.recompute_status();
        assert_eq!(st.status, RunStatus::Created, "b still pending → not complete");

        st.nodes.get_mut("b").unwrap().status = TaskStatus::Done;
        st.recompute_status();
        assert_eq!(st.status, RunStatus::Completed);
    }

    #[test]
    fn strict_failure_makes_run_failed() {
        let mut st = state();
        st.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
        st.nodes.get_mut("b").unwrap().status = TaskStatus::Done;
        st.recompute_status();
        assert_eq!(st.status, RunStatus::Failed);
    }

    #[test]
    fn optional_failure_is_not_a_hard_failure() {
        // `a` optional: its failure must not make the run Failed (degraded).
        let mut st = RunState::new(
            "r".into(),
            "d".into(),
            "d.yml".into(),
            None,
            vec![vec!["a".into(), "b".into()]],
            &[
                ("a".into(), "echo".into(), None, true, vec![]),
                ("b".into(), "echo".into(), None, false, vec![]),
            ],
        );
        st.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
        st.nodes.get_mut("b").unwrap().status = TaskStatus::Done;
        st.recompute_status();
        assert_eq!(st.status, RunStatus::Completed);
    }

    #[test]
    fn running_any_node_marks_run_running() {
        let mut st = state();
        st.nodes.get_mut("a").unwrap().status = TaskStatus::Running;
        st.recompute_status();
        assert_eq!(st.status, RunStatus::Running);
    }

    #[tokio::test]
    async fn emitter_mutates_and_broadcasts_snapshot() {
        let em = Emitter::new(state());
        let mut rx = em.subscribe();
        em.set_status("a", TaskStatus::Running);
        let snap = rx.recv().await.expect("a snapshot after set_status");
        assert_eq!(snap.nodes["a"].status, TaskStatus::Running);
        assert!(snap.nodes["a"].started_at.is_some(), "started_at stamped on Running");

        em.append_turn("a", Role::Human, "clarify X");
        let snap = rx.recv().await.expect("a snapshot after append_turn");
        assert_eq!(snap.nodes["a"].turns.len(), 1);
        assert_eq!(snap.nodes["a"].turns[0].role, Role::Human);
        assert_eq!(snap.nodes["a"].turns[0].text, "clarify X");
    }

    #[test]
    fn state_roundtrips_through_json() {
        // The same shape feeds state.json and the API — prove it survives.
        let mut st = state();
        st.nodes.get_mut("a").unwrap().status = TaskStatus::Done;
        st.nodes.get_mut("a").unwrap().output = "hello".into();
        st.nodes.get_mut("a").unwrap().session = Some(SessionRef::Codex {
            thread_id: "tid-1".into(),
        });
        let json = serde_json::to_string(&st).unwrap();
        let back: RunState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.nodes["a"].output, "hello");
        assert_eq!(back.status, st.status);
        match back.nodes["a"].session.as_ref().unwrap() {
            SessionRef::Codex { thread_id } => assert_eq!(thread_id, "tid-1"),
            other => panic!("wrong session kind: {other:?}"),
        }
    }
}
