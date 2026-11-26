use super::*;

/// A boot request: a flow file path plus optional `--input` overrides.
#[derive(Deserialize)]
pub(super) struct BootReq {
    pub(super) path: String,
    #[serde(default)]
    pub(super) inputs: HashMap<String, String>,
}

#[derive(Deserialize)]
pub(super) struct AnswerReq {
    node: String,
    text: String,
    question_id: Option<usize>,
    /// The live canvas graph, sent with the answer so the planner's next turn
    /// sees what the operator actually has on the board (positions are UI-only
    /// and stripped; deps and leaf params are what the model must build on).
    #[serde(default)]
    graph: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct SteerReq {
    attached_run: Option<String>,
    node: String,
    text: String,
    /// The live canvas graph, sent with the next turn so the planner builds on
    /// the operator's edits (a moved node, a changed model, a hand-wired dep)
    /// instead of the graph still held in the session.
    #[serde(default)]
    graph: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct RerunReq {
    pub(super) node: String,
    /// Optional corrected task spec from the canvas ("rerun with fix"): the
    /// node re-runs with these leaf fields instead of the boot-time ones, so
    /// e.g. a typo'd model name is fixed in Config and the rerun actually uses
    /// the correction. Validated by [`validate_task_edit`] before applying.
    #[serde(default)]
    pub(super) task: Option<serde_json::Value>,
}

/// A node that may be re-run from scratch. `Done` and `Stale` were always
/// re-runnable; a `Failed` node is the *main* reason to rerun (fix the
/// config, try again), and a `Skipped` node (whose producer died) becomes
/// reachable once its deps are done — rerunning it is the operator's call,
/// not the server's. `Pending`/`Running` are not (the run's own driver owns
/// them), so the UI cannot double-drive a node mid-flight.
pub(super) fn rerunnable(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Done | TaskStatus::Stale | TaskStatus::Failed | TaskStatus::Skipped
    )
}

/// Validate an operator task edit arriving with a rerun: id and topology must
/// stay identical (edges are the plan; changing them mid-run is a new run),
/// the prompt may not be emptied, and the harness/model pair must resolve.
/// Everything else (model, in, out, cwd, timeout, args, env, optional) is a
/// legal correction.
pub(super) fn validate_task_edit(
    node: &str,
    existing: &Task,
    incoming: &Task,
    flow: &Flow,
) -> Result<(), String> {
    if incoming.id.trim() != node {
        return Err(format!(
            "task id '{}' does not match the re-run node '{node}'",
            incoming.id
        ));
    }
    // Empty `in` is allowed when deps feed the prompt (edge-carried data);
    // an edit must still keep the node able to produce a prompt at all.
    if incoming.in_field.trim().is_empty() && incoming.depends_on.is_empty() {
        return Err(format!("task '{node}': prompt (in) is empty (and no dependencies feed it)"));
    }
    // Effective deps must not move: explicit depends_on plus the ${steps…}
    // references (the save path auto-wires the same refs into edges).
    let mut deps: std::collections::BTreeSet<String> =
        incoming.depends_on.iter().cloned().collect();
    for r in step_refs(&incoming.in_field)
        .into_iter()
        .chain(step_refs(incoming.out.as_deref().unwrap_or("")))
    {
        deps.insert(r);
    }
    let existing_deps: std::collections::BTreeSet<String> =
        existing.depends_on.iter().cloned().collect();
    if deps != existing_deps {
        return Err(format!(
            "task '{node}': changing dependencies on a live run is not supported — save it as a new flow instead"
        ));
    }
    // Resolve against the run's own harness table: a custom harness defined
    // in the flow is a legal fix (e.g. pointing a task at a working command),
    // and the model-support rule is the same one boot enforces.
    let conn = crate::connectors::resolve(&incoming.harness, flow.harnesses.get(&incoming.harness))
        .map_err(|e| format!("task '{node}': {e:#}"))?;
    if incoming.model.is_some() && !conn.supports_model {
        return Err(format!(
            "task '{node}': harness '{}' cannot set a model per run",
            incoming.harness
        ));
    }
    Ok(())
}

/// Filesystem-safe run-id slug from a flow name.
pub(super) fn slug(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        s.push(if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' });
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "run".to_string() } else { s }
}

pub(super) fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) async fn steer(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<SteerReq>,
) -> Result<Json<RunState>, ApiError> {
    let m = lookup(&state, &id)?;
    // Gate on status so a node cannot be steered mid-flight; the tracked engine
    // also refuses without a session, but this makes the intent explicit.
    // A Waiting node is not steered — it is ANSWERED (the parked watchdog
    // loop resumes the session itself; /steer would double-drive it).
    let status = m.emitter.snapshot().nodes.get(&req.node).map(|n| n.status);
    match status {
        Some(TaskStatus::Done) => {}
        Some(TaskStatus::Waiting) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!(
                    "node '{}' is waiting for an answer to its question — use POST /api/runs/:id/answer",
                    req.node
                ),
            ))
        }
        Some(other) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("node '{}' is {}, not steerable", req.node, other.label()),
            ))
        }
        None => return Err(ApiError(StatusCode::NOT_FOUND, format!("no node '{}'", req.node))),
    }
    if req.text.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "empty message".into()));
    }

    // Atomically claim the node (Done → Running) so a second click cannot
    // double-steer: whoever flips it first wins, the loser sees Running and is
    // rejected. `steer` re-sets Running internally (idempotent) and the worker
    // holds `op_lock`, serialising this against other ops on the same run.
    // Review M3: the gate also refuses while the first DAG pass still owes
    // work to any descendant of this node — steering an ancestor mid-pass
    // races that work. Deps are NOT checked here (require_deps=false): a
    // steer resumes the existing session and re-renders nothing from
    // upstream, so an ancestor that went Stale after this node finished must
    // not lock the transcript.
    let context = planner_run_context(&state, req.attached_run.as_deref())?;
    let emitter = m.emitter.clone();
    claim_node(&emitter, &req.node, &[TaskStatus::Done], NodeAction::Steer)
        .map_err(|msg| ApiError(StatusCode::CONFLICT, msg))?;
    let node = req.node.clone();
    let text = req.text.clone();
    // The live canvas the operator sent with this turn (may be absent from an
    // older client). The planner's next prompt is built on it — see
    // `TrackedRun::steer`.
    let graph = req.graph.clone();
    let lock = m.op_lock.clone();
    let tracked = m.tracked.clone();
    let ack = emitter.snapshot();
    // (cloned before the move into the worker below — the F09 watcher needs it)
    let watch = emitter.clone();
    let handle = tokio::spawn(async move {
        let _guard = lock.lock().await;
        if let Err(e) = tracked.steer_with_context(&emitter, &node, &text, graph, &context).await {
            eprintln!("steer '{node}' failed: {e:#}");
        }
    });
    // Review F09: never drop the JoinHandle. `TrackedRun::steer` reports its
    // own errors, but a panic inside this task would leave the node `Running`
    // with nothing left to move it. On a JoinError, restore it to `Done` —
    // the only status the gate above admits, so the honest pre-steer value —
    // with the failure recorded, so the console shows why.
    let watch_node = req.node.clone();
    tokio::spawn(async move {
        if let Err(e) = handle.await {
            eprintln!("steer driver for '{watch_node}' died: {e:?}");
            release_claim(
                &watch,
                &watch_node,
                Some(TaskStatus::Done),
                &format!("steer driver died: {e}"),
            );
        }
    });
    Ok(Json(ack))
}

/// Answer a parked (`Waiting`) node: the watchdog parked it on an `ASK:`
/// question; this records the human turn and wakes the parked exec, which
/// resumes the same session with the text and finishes the node.
pub(super) async fn answer(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<AnswerReq>,
) -> Result<Json<RunState>, ApiError> {
    let m = lookup(&state, &id)?;
    if req.text.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "empty answer".into()));
    }
    m.tracked
        .answer_question(&m.emitter, &req.node, req.question_id, &req.text, req.graph)
        .map_err(|e| ApiError(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(m.emitter.snapshot()))
}

pub(super) async fn rerun(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RerunReq>,
) -> Result<Json<RunState>, ApiError> {
    let m = lookup(&state, &id)?;
    let prev = m.emitter.snapshot().nodes.get(&req.node).map(|n| n.status);
    match prev {
        Some(s) if rerunnable(s) => {}
        Some(other) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("node '{}' is {}, not re-runnable", req.node, other.label()),
            ))
        }
        None => return Err(ApiError(StatusCode::NOT_FOUND, format!("no node '{}'", req.node))),
    }

    // A rerun may carry the corrected task from the canvas. Validate against
    // the run's current spec BEFORE claiming the node — this is pure reading,
    // so a rejected edit leaves everything untouched and the error names the
    // reason.
    let incoming: Option<crate::config::Task> = match &req.task {
        None => None,
        Some(tval) => {
            let incoming: crate::config::Task = serde_json::from_value(tval.clone())
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("invalid task spec: {e}")))?;
            let existing = m
                .tracked
                .task(&req.node)
                .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no node '{}'", req.node)))?;
            validate_task_edit(&req.node, &existing, &incoming, &m.tracked.flow)
                .map_err(|msg| ApiError(StatusCode::BAD_REQUEST, msg))?;
            Some(incoming)
        }
    };

    let emitter = m.emitter.clone();
    // Done/Stale/Failed/Skipped → Running, atomically (same double-click guard
    // as steer).
    let from = &[
        TaskStatus::Done,
        TaskStatus::Stale,
        TaskStatus::Failed,
        TaskStatus::Skipped,
    ];
    // Review F09: claim FIRST, then mutate. The old order applied the
    // corrected spec (and possibly rewrote the flow YAML on disk) before
    // attempting the claim, so a request that lost the race returned 409
    // *after* having changed the run's spec. Now the loser touches nothing.
    // Review M3+M4: the gate is one atomic mutation that also refuses while a
    // strict dependency is not `Done` (require_deps=true — a rerun re-renders
    // its prompt from upstream, so it must wait for them like a first run
    // does). A failed node in the active level can reserve the barrier
    // so pending consumers will see its corrected output.
    let level_retry = if prev == Some(TaskStatus::Failed) {
        m.tracked.reserve_level_retry(&req.node)
            .map_err(|e| ApiError(StatusCode::CONFLICT, e.to_string()))?
    } else {
        None
    };
    let (from, action): (&[_], _) = if level_retry.is_some() {
        (&[TaskStatus::Failed], NodeAction::RetryInLevel)
    } else {
        (from, NodeAction::Rerun)
    };
    claim_node(&emitter, &req.node, from, action)
        .map_err(|msg| ApiError(StatusCode::CONFLICT, msg))?;

    // Claim won: only now apply the spec mutation. The in-memory parts are
    // infallible here (the edit was validated above); persistence is
    // best-effort and logged, never swallowed — the in-memory rerun must not
    // hinge on a disk write succeeding.
    if let Some(incoming) = incoming {
        m.tracked.override_task(incoming.clone());
        // Keep the observable meta honest (the Chat head shows harness·model).
        m.emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(&req.node) {
                n.harness = incoming.harness.clone();
                n.model = incoming.model.clone();
                n.optional = incoming.optional;
            }
        });
        // Persist the corrected flow so a server restart doesn't lose the fix.
        // Only for flows that live in the builder's dir: rewriting an arbitrary
        // path (e.g. a repo example) from a live run would be a surprise write.
        let snap = m.emitter.snapshot();
        let in_flows_dir = snap
            .flow_path
            .canonicalize()
            .ok()
            .zip(state.flows_root.canonicalize().ok())
            .is_some_and(|(f, root)| f.starts_with(&root));
        if in_flows_dir {
            let persisted = spec_to_yaml(&m.tracked.effective_flow())
                .map_err(|e| format!("serializing flow: {}", e.1))
                .and_then(|yaml| {
                    std::fs::write(&snap.flow_path, yaml)
                        .map_err(|e| format!("writing {}: {e}", snap.flow_path.display()))
                });
            if let Err(e) = persisted {
                eprintln!("warning: rerun '{id}' could not persist the flow fix: {e}");
            }
        }
    }

    let node = req.node.clone();
    let lock = m.op_lock.clone();
    let tracked = m.tracked.clone();
    let ack = emitter.snapshot();
    // (cloned before the move into the worker below — the F09 watcher needs it)
    let watch = emitter.clone();
    let handle = tokio::spawn(async move {
        let _guard = lock.lock().await;
        if let Err(e) = tracked.rerun(&emitter, &node).await {
            eprintln!("rerun '{node}' failed: {e:#}");
            // Review M4: `TrackedRun::rerun` refuses a node whose strict
            // deps are not Done *now* — a TOCTOU backstop behind the claim
            // gate (an op that grabbed the lock first can have reset a
            // dependency in between). That error arrives before any exec,
            // with the node still `Running` from the claim: release it back
            // to its pre-rerun status with the reason, or the node becomes a
            // zombie. Driver-level failures already recorded `Failed` inside
            // `rerun`, and `release_claim` only touches a still-`Running`
            // node, so this is a no-op for them.
            release_claim(&emitter, &node, prev, &format!("rerun refused: {e}"));
        }
    });
    // Review F09: never drop the JoinHandle. `TrackedRun::rerun` records a
    // driver error itself, but a *panic* inside the task would leave the node
    // `Running` forever — nothing else ever touches it again. Watch the
    // handle: on a JoinError, restore the node to its pre-rerun status with
    // the failure recorded (keeping the previous status rather than Failed
    // when we had one is honest: this rerun never really started).
    let watch_node = req.node.clone();
    tokio::spawn(async move {
        // Keep the level barrier through panic recovery as well as execution.
        let _level_retry = level_retry;
        if let Err(e) = handle.await {
            eprintln!("rerun driver for '{watch_node}' died: {e:?}");
            release_claim(&watch, &watch_node, prev, &format!("rerun driver died: {e}"));
        }
    });
    Ok(Json(ack))
}

pub(super) async fn rerun_stale(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<RunState>, ApiError> {
    let m = lookup(&state, &id)?;
    let emitter = m.emitter.clone();
    let lock = m.op_lock.clone();
    let tracked = m.tracked.clone();
    let ack = emitter.snapshot();
    // Review F09: the node ids `rerun_stale` can claim are exactly the ones
    // Stale right now (run_all's in-flight nodes are Running, not Stale, so a
    // later revert can never steal a node another driver owns).
    let candidates: Vec<String> = ack
        .nodes
        .iter()
        .filter(|(_, n)| n.status == TaskStatus::Stale)
        .map(|(id, _)| id.clone())
        .collect();
    // (cloned before the move into the worker below — the F09 watcher needs it)
    let watch = emitter.clone();
    let handle = tokio::spawn(async move {
        let _guard = lock.lock().await;
        tracked.rerun_stale(&emitter).await;
    });
    // A panic mid-sweep would leave already-claimed nodes `Running` forever;
    // on a JoinError, roll those back to `Stale` — where they started.
    tokio::spawn(async move {
        if let Err(e) = handle.await {
            eprintln!("rerun_stale driver died: {e:?}");
            for node in candidates {
                release_claim(
                    &watch,
                    &node,
                    Some(TaskStatus::Stale),
                    &format!("stale-sweep driver died: {e}"),
                );
            }
        }
    });
    Ok(Json(ack))
}

// --------------------------------------------------------------------------
// Flow builder
// --------------------------------------------------------------------------
//
// The "Build" tab of the SPA lets the operator click a pipeline together
// instead of hand-writing YAML. The browser holds an editable spec (a JSON
// mirror of `Flow`); the server is the single authority on validity: the same
// code path (`plan` + `connectors::resolve`) that a real run will use decides
// here whether the spec is runnable, so the builder can never save a flow the
// runner would reject.

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeAction {
    Steer,
    Rerun,
    /// The scheduler will await this retry before launching pending descendants.
    RetryInLevel,
}

/// Atomically move a node from one of `from` statuses to `Running`, or
/// return the reason it cannot be claimed. Under the emitter's lock the
/// checks and the flip are ONE mutation, so nothing can slip between them
/// (the old order — check outside, flip inside — let a first-pass level
/// launch a descendant between the two and double-drive the subtree).
///
/// Active descendants prevent changes to a producer whose output they use.
/// A failed producer reserved in the current level can safely retry with
/// pending descendants because the scheduler holds those at the barrier.
/// Reruns also require their strict dependencies to be done before replacing
/// the previous session and output.
pub(super) fn claim_node(
    emitter: &Emitter,
    node: &str,
    from: &[TaskStatus],
    action: NodeAction,
) -> Result<(), String> {
    let from = from.to_vec();
    let node = node.to_string();
    let mut reason: Option<String> = None;
    emitter.mutate(|st| {
        let Some(n) = st.nodes.get(&node) else {
            reason = Some(format!("no node '{node}'"));
            return;
        };
        if !from.contains(&n.status) {
            reason = Some(format!(
                "node '{node}' is busy (now {}), try again",
                n.status.label()
            ));
            return;
        }
        if action != NodeAction::Steer {
            let blockers: Vec<&String> = n
                .depends_on
                .iter()
                .filter(|d| {
                    !st.nodes
                        .get(*d)
                        .is_some_and(|p| p.status == TaskStatus::Done || p.optional)
                })
                .collect();
            if !blockers.is_empty() {
                let list: Vec<&str> = blockers.iter().map(|s| s.as_str()).collect();
                reason = Some(format!(
                    "node '{node}' waits on unfinished dependencies: {}",
                    list.join(", ")
                ));
                return;
            }
        }
        // Transitively: is any descendant still owed work? (edges point
        // child → parent, so walk children; the DAG guarantees no cycles.)
        let mut frontier = vec![node.clone()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut busy: Vec<&str> = Vec::new();
        while let Some(cur) = frontier.pop() {
            for (id, t) in &st.nodes {
                if !t.depends_on.iter().any(|d| d == &cur) || !seen.insert(id.clone()) {
                    continue;
                }
                if matches!(t.status, TaskStatus::Running | TaskStatus::Waiting)
                    || (t.status == TaskStatus::Pending && action != NodeAction::RetryInLevel)
                {
                    busy.push(id.as_str());
                }
                frontier.push(id.clone());
            }
        }
        if !busy.is_empty() {
            reason = Some(format!(
                "the run is still working downstream of '{node}': {} — wait for the flow to settle",
                busy.join(", ")
            ));
            return;
        }
        if let Some(n) = st.nodes.get_mut(&node) {
            n.status = TaskStatus::Running;
        }
    });
    reason.map_or(Ok(()), Err)
}

/// Review F09: undo a claim whose driver died (a `JoinError` on the spawned
/// task — a panic would otherwise leave the node `Running` forever, with
/// nothing ever touching it again). The node returns to `prev` — its status
/// before the claim, which is the honest value to restore since the rerun
/// never really ran — or `Failed` when no previous status was known, with the
/// reason recorded in `stderr` rather than swallowed. Only a node still
/// marked `Running` is rolled back: if a fresh claim has since flipped it,
/// that driver owns the status now and this stale report must not steal it.
fn release_claim(
    emitter: &Emitter,
    node: &str,
    prev: Option<TaskStatus>,
    reason: &str,
) {
    let reason = reason.to_string();
    let restore = prev.unwrap_or(TaskStatus::Failed);
    let node = node.to_string();
    emitter.mutate(move |st| {
        if let Some(n) = st.nodes.get_mut(&node) {
            if n.status == TaskStatus::Running {
                n.status = restore;
                n.stderr = reason.clone();
                n.finished_at = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                );
            }
        }
    });
}

// --------------------------------------------------------------------------
// Boot / persistence helpers
// --------------------------------------------------------------------------

/// Parse flow text, mint a run id, register it, spawn its execution and its
/// persistence writer. Returns the new `run_id`.
pub(super) async fn boot_flow(
    state: std::sync::Arc<AppState>,
    flow_text: &str,
    flow_path: Option<PathBuf>,
    extra_inputs: Vec<(String, String)>,
    focus: Option<&str>,
) -> anyhow::Result<String> {
    use anyhow::Context;
    let flow: Flow = serde_yaml::from_str(flow_text).context("parsing flow YAML")?;
    let full_plan = plan(&flow)?;

    // A focused run (the canvas ▶ button's first launch) executes only one
    // node's upstream closure: the node plus everything it, transitively,
    // depends on. `TrackedRun` still carries the FULL flow — so a later rerun
    // of any node resolves its task and persists `effective_flow` without
    // clobbering nodes this run never touched — while its *plan* and the
    // registered `RunState` nodes are restricted to the closure. The
    // restriction is what lets a focused run reach `Completed`: the emitter
    // recomputes status on every mutation and only calls a run done when
    // *every registered node* is terminal, so the untouched nodes must not be
    // registered (they would sit `Pending` and keep the run `Running`
    // forever). The canvas still draws the whole graph, because it lays out
    // the flow file at `snap.flow_path`, independently of registered nodes.
    let closure: Option<std::collections::HashSet<String>> = match focus {
        None => None,
        Some(id) => Some(upstream_closure(&flow, id).map_err(anyhow::Error::msg)?),
    };
    let run_plan = match &closure {
        None => full_plan.clone(),
        Some(set) => {
            let levels: Vec<Vec<String>> = full_plan
                .levels
                .iter()
                .map(|l| -> Vec<String> {
                    l.iter().filter(|x| set.contains(*x)).cloned().collect()
                })
                .filter(|l| !l.is_empty())
                .collect();
            crate::dag::ExecutionPlan { levels }
        }
    };

    // Gate here, not in the handlers: every boot path comes through this
    // function — the initial flow at startup, /api/runs booting a saved file,
    // and /api/flows/boot from a builder spec. A flow whose ${inputs.k} nothing
    // supplies produces a run in which every task using it dies with `unknown
    // reference` (the lost-inputs bug); the runner would fail them anyway, so
    // refusing before the run exists is free and leaves no half-dead run. A
    // focused run only checks the nodes it will actually execute.
    let given: Vec<String> = extra_inputs.iter().map(|(k, _)| k.clone()).collect();
    let missing = match &closure {
        None => missing_inputs(&flow, &given),
        Some(set) => missing_inputs_filtered(&flow, &given, Some(set)),
    };
    if !missing.is_empty() {
        anyhow::bail!("{}", missing_inputs_error(&missing));
    }

    // Millisecond, process-monotonic ids: whole-second stamps collided for
    // back-to-back boots, and the run dir name must be unique.
    // Review M9: the id is only process-monotonic, so a second zori process
    // (or a crash-recovery boot beside a live one) could mint the same id and
    // the two would silently share one run dir. reserve_run_dir claims the
    // folder with create_dir; a loser lands on `<id>-2` and the run keeps
    // that effective id everywhere below.
    let wanted = crate::artifacts::new_run_id(&flow.name);
    let (run_dir, run_id) = crate::artifacts::reserve_run_dir(&state.runs_root, &wanted)
        .with_context(|| format!("reserving run dir for '{wanted}'"))?;
    let sessions_dir = run_dir.join("sessions");

    let tracked = TrackedRun::new(
        flow.clone(),
        run_plan.clone(),
        &extra_inputs,
        run_dir.clone(),
        sessions_dir,
    )
    .with_context(|| format!("initialising run '{run_id}'"))?;

    let tasks_meta: Vec<crate::state::TaskMeta> = flow
        .tasks
        .iter()
        .filter(|t| closure.as_ref().is_none_or(|set| set.contains(&t.id)))
        .map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone()))
        .collect();
    let is_chat = flow_path.is_none();
    let flow_path = match flow_path {
        Some(path) => path,
        None => {
            let path = run_dir.join("chat.yml");
            tokio::fs::write(&path, flow_text).await.context("saving the chat session")?;
            path
        }
    };
    let mut run_state = RunState::new(
        run_id.clone(),
        flow.name.clone(),
        flow_path,
        Some(run_dir.clone()),
        run_plan.levels.clone(),
        &tasks_meta,
    );
    run_state.is_chat = is_chat;
    let emitter = Emitter::new(run_state);
    if is_chat {
        emitter.append_turn("chat", crate::state::Role::Human, flow.inputs["message"].clone());
    }

    // Register BEFORE spawning execution so a fast client can GET immediately.
    runs_guard(&state.runs).insert(
        run_id.clone(),
        ManagedRun {
            emitter: emitter.clone(),
            tracked: tracked.clone(),
            op_lock: Arc::new(tokio::sync::Mutex::new(())),
        },
    );

    // Persistence: every broadcast snapshot → state.json. A lagged writer just
    // skips a frame and writes the next; the last write is authoritative.
    spawn_persister(emitter.subscribe(), run_dir);
    // Execution: run every node, level by level (siblings within a level run
    // concurrently, capped by the flow's max_parallel). The op_lock is NOT
    // held here — steering a *done* node must be possible while later levels
    // still execute. Two drivers can never touch one node: a node that is
    // currently running is refused by the status gate above, and a node is
    // terminal once its own exec finishes.
    let exec_emitter = emitter.clone();
    let exec_node_report = emitter.clone();
    let handle = tokio::spawn(async move {
        tracked.run_all(&exec_emitter).await;
    });
    // Review F09: a panic in the execution driver would leave every not-yet
    // finished node stuck in whatever status it had — a `Running` one stuck
    // forever. Surface the JoinError and mark still-Running nodes Failed so
    // the console reflects reality instead of hanging on a phantom task.
    tokio::spawn(async move {
        if let Err(e) = handle.await {
            eprintln!("run_all driver died: {e:?}");
            let snap = exec_node_report.snapshot();
            for (node, n) in snap.nodes {
                if n.status == TaskStatus::Running {
                    release_claim(
                        &exec_node_report,
                        &node,
                        Some(TaskStatus::Failed),
                        &format!("run driver died: {e}"),
                    );
                }
            }
        }
    });

    Ok(run_id)
}
