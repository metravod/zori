use super::*;

/// A live run the console can inspect, steer and re-execute. Owns everything
/// static about a run (flow, plan, resolved inputs, directories); the mutable
/// status/transcript lives in the [`Emitter`] passed to each method.
#[derive(Clone)]
pub struct TrackedRun {
    pub flow: Flow,
    pub plan: ExecutionPlan,
    pub inputs: HashMap<String, String>,
    pub default_timeout: u64,
    /// Isolated coddy sessions root (one dir per run → `-sessions-dir`).
    pub sessions_dir: PathBuf,
    /// Where artifacts + `state.json` go.
    pub out_dir: PathBuf,
    /// The run's artifact tree + manifest (attempts under `tasks/<id>/`).
    pub art: RunArtifacts,
    // First-pass nodes currently inside the level barrier. A worker owns its
    // node through all watchdog turns. A manual retry can take ownership
    // after failure and hold the barrier until its corrected output is ready.
    level_nodes: Arc<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    permits: Arc<Semaphore>,
    /// Operator edits applied to individual tasks after boot (the console's
    /// rerun-with-fix): topology stays frozen — an edit may correct leaf
    /// fields (model, prompt, out, cwd, timeout, args, env, optional) but not
    /// ids or edges. Behind an `Arc` so every clone of the driver (each level
    /// runner holds one) sees the same corrections.
    pub(super) overrides: Arc<std::sync::RwLock<HashMap<String, Task>>>,
    /// Wakes waiters after an addressed answer is committed to the snapshot.
    answers: Arc<tokio::sync::Notify>,
    operator_input_closed: Arc<AtomicBool>,
    unattended: bool,
    /// The live canvas graph the operator last sent with a chat turn (steer or
    /// an ASK answer). The planner's next prompt is built on THIS, not on the
    /// stale graph still in its session, so a node the operator dragged or a
    /// model they switched while the agent was thinking is not rolled back on
    /// the next proposal. Behind an `Arc` so the driver clones and the exec
    /// loop (which builds the ASK-answer resume after `answer_question` has
    /// returned) all see the same latest board. `None` until a turn carries
    /// one; only the chat planner populates it.
    pub(super) canvas: Arc<StdMutex<Option<serde_json::Value>>>,
    /// Live graph edits since boot (nodes added or removed by the operator).
    pub(super) topology: Arc<std::sync::RwLock<super::graph::Topology>>,
}

impl TrackedRun {
    /// Build the run and ensure its directories exist. `run_id` is the same id
    /// the caller put in the [`crate::state::RunState`].
    pub fn new(
        flow: Flow,
        plan: ExecutionPlan,
        extra_inputs: &[(String, String)],
        out_dir: PathBuf,
        sessions_dir: PathBuf,
    ) -> Result<Self> {
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating run dir {}", out_dir.display()))?;
        std::fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("creating sessions dir {}", sessions_dir.display()))?;
        // Absolute from here on. The executor hands this path to the agent CLI
        // (coddy `-sessions-dir`), which resolves it against ITS OWN process
        // cwd — and the runs root is often relative (`zori-runs`). A task in
        // its isolated work dir then grew a nested sessions tree under it, and
        // a `workspace: flow` task (a chat planner in the project folder)
        // would drop the sessions INTO the user's project. The dir exists now,
        // so `absolute` also canonicalises it.
        let sessions_dir = artifacts::absolute(&sessions_dir);
        let default_timeout = flow_default_timeout(&flow);
        let inputs = resolved_inputs(&flow, extra_inputs);
        let art = RunArtifacts::adopt(&out_dir, &flow, &plan, &inputs)?;
        let parallelism = flow.max_parallel.unwrap_or(flow.tasks.len())
            .clamp(1, Semaphore::MAX_PERMITS);
        Ok(TrackedRun {
            level_nodes: Default::default(),
            permits: Arc::new(Semaphore::new(parallelism)),
            flow,
            plan,
            inputs,
            default_timeout,
            sessions_dir,
            out_dir,
            art,
            overrides: Arc::new(std::sync::RwLock::new(HashMap::new())),
            answers: Arc::new(tokio::sync::Notify::new()),
            operator_input_closed: Arc::new(AtomicBool::new(false)),
            unattended: false,
            canvas: Arc::new(StdMutex::new(None)),
            topology: Default::default(),
        })
    }

    /// Force unattended supervision when the terminal has no interactive input.
    pub fn with_unattended(mut self, unattended: bool) -> Self {
        self.unattended = unattended;
        self
    }

    /// Wake parked nodes when the terminal can no longer read an answer.
    pub fn close_operator_input(&self) {
        self.operator_input_closed.store(true, Ordering::Relaxed);
        self.answers.notify_waiters();
    }

    /// The task as it should run *now*: the operator's edit if one was
    /// registered, else the boot-time spec. Every execution path (first run,
    /// rerun, rerun-stale) resolves through here, so a fix applied between
    /// attempts is never silently lost.
    pub fn task(&self, id: &str) -> Option<Task> {
        if self.is_removed(id) {
            return None;
        }
        if let Some(t) = self.overrides.read().expect("overrides lock").get(id) {
            return Some(t.clone());
        }
        let topo = self.topology.read().expect("topology lock");
        topo.added.iter().chain(self.flow.tasks.iter()).find(|t| t.id == id).cloned()
    }

    /// Register an operator edit for the task with this id (which must already
    /// exist in the plan — the console validates that before calling).
    pub fn override_task(&self, task: Task) {
        self.overrides
            .write()
            .expect("overrides lock")
            .insert(task.id.clone(), task);
    }

    /// The flow as it runs now (boot spec with all edits applied) — the shape
    /// to persist, so a reloaded run keeps the fixes.
    pub fn effective_flow(&self) -> Flow {
        let mut f = self.flow.clone();
        f.tasks = self.tasks();
        f
    }

    /// The outputs map used to render a node's `in`/`out`: completed nodes give
    /// their stdout; a missing best-effort output resolves to a placeholder.
    /// Other unmet references are render errors.
    pub(super) fn outputs_snapshot(&self, emitter: &Emitter) -> HashMap<String, String> {
        let snap = emitter.snapshot();
        let mut outputs = HashMap::new();
        for (id, node) in &snap.nodes {
            match node.status {
                TaskStatus::Done => {
                    outputs.insert(id.clone(), node.output.clone());
                }
                TaskStatus::Failed | TaskStatus::Skipped | TaskStatus::Stale
                    if node.optional =>
                {
                    outputs.insert(id.clone(), missing_output(id));
                }
                _ => {}
            }
        }
        outputs
    }

    /// True if `task`'s deps are all satisfied (Done) or are optional branches
    /// that fell over. A missing dep counts as blocking (never happens after
    /// `build_plan`).
    pub(super) fn blocked(&self, emitter: &Emitter, task: &Task) -> bool {
        let snap = emitter.snapshot();
        task.depends_on.iter().any(|d| {
            !snap
                .nodes
                .get(d)
                .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
        })
    }

    /// The deps of `task` that are neither Done nor a fallen optional branch
    /// — the same predicate `blocked` tests, kept as a list so diagnostics
    /// and the manifest can name what actually blocked the task.
    fn blocker_names(&self, emitter: &Emitter, task: &Task) -> Vec<String> {
        let snap = emitter.snapshot();
        task.depends_on
            .iter()
            .filter(|d| {
                !snap
                    .nodes
                    .get(d.as_str())
                    .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
            })
            .cloned()
            .collect()
    }

    /// Reserve a failed node in the active level. Taking the guard under the
    /// registry lock makes admission atomic with the scheduler closing the level.
    pub(crate) fn reserve_level_retry(&self, id: &str) -> Result<Option<OwnedMutexGuard<()>>> {
        let nodes = self.level_nodes.lock().expect("level nodes lock");
        nodes.get(id)
            .map(|lock| lock.clone().try_lock_owned()
                .map_err(|_| anyhow!("node '{id}' is still executing")))
            .transpose()
    }

    /// Run every node, level by level, updating `emitter` as it goes. The
    /// nodes of one level run **concurrently**, capped by
    /// `flow.max_parallel` (unset → the whole level at once). Each level
    /// reaches a terminal state before the next spawns. A required node
    /// failure cascades `Skipped` to dependents (D3).
    pub async fn run_all(&self, emitter: &Emitter) {
        install_death_handlers();
        for (li, level) in self.plan.levels.iter().enumerate() {
            emitter.emit(RunEvent::Level(li, level));
            // Determine skips before spawning any siblings.
            let mut runnable: Vec<Task> = Vec::new();
            for id in level {
                if self.is_removed(id) {
                    continue;
                }
                // A dependency wired in live may still be working.
                self.wait_deps_settled(emitter, id).await;
                let Some(task) = self.task(id) else { continue };
                if self.blocked(emitter, &task) {
                    emitter.emit(RunEvent::Finished(id, TaskStatus::Skipped, "(a dependency did not complete)"));
                    let blockers = self.blocker_names(emitter, &task);
                    emitter.mutate(|st| {
                        if let Some(n) = st.nodes.get_mut(id) {
                            n.status = TaskStatus::Skipped;
                            n.finished_at = Some(now_secs());
                        }
                    });
                    // Review (minor): `AttemptRecord.error` promises "which
                    // dependency blocked a skip", but skips were never
                    // recorded — a cascaded task looked like it never
                    // existed in the run index. Record the skip naming its
                    // blockers under a nonce no real attempt can produce.
                    self.art.manifest.record_attempt(
                        &task.id,
                        &task.harness,
                        task.model.clone(),
                        task.optional,
                        "skipped",
                        AttemptRecord {
                            status: TaskStatus::Skipped.label().to_string(),
                            error: Some(format!(
                                "skipped: unfinished dependencies: {}",
                                blockers.join(", ")
                            )),
                            exit_code: None,
                            started_at: None,
                            ended_at: Some(now_secs()),
                            cwd: None,
                            session: None,
                            out: None,
                        },
                    );
                    continue;
                }
                runnable.push(task.clone());
            }
            self.run_level(emitter, runnable, true).await;
            // Close admission first, then join any retry accepted before the
            // boundary. Only then can the next level evaluate dependencies.
            let nodes = std::mem::take(&mut *self.level_nodes.lock().expect("level nodes lock"));
            for lock in nodes.into_values() {
                drop(lock.lock().await);
            }
        }
    }

    /// Run one level's nodes concurrently: one tokio task per node behind a
    /// semaphore sized by `max_parallel` (unset → the whole level). The
    /// upstream outputs map is frozen once, before any node of the level
    /// spawns, and shared by every sibling (see [`ExecMode::FirstRun`]).
    /// Returns only when every node of the level is terminal — the level
    /// barrier (D3).
    pub(super) async fn run_level(&self, emitter: &Emitter, nodes: Vec<Task>, first_pass: bool) {
        if nodes.is_empty() {
            return;
        }
        let outputs = self.outputs_snapshot(emitter);
        let mut handles = Vec::new();
        for task in nodes {
            let owner = first_pass.then(|| {
                let lock = Arc::new(AsyncMutex::new(()));
                let guard = lock.clone().try_lock_owned().expect("new node lock");
                self.level_nodes.lock().expect("level nodes lock").insert(task.id.clone(), lock);
                guard
            });
            let emitter = emitter.clone();
            let outputs = outputs.clone();
            let this = self.clone();
            // Review (minor): keep the id beside the handle — a panicked task
            // never returns, so the JoinError branch below still knows which
            // node to settle.
            let id = task.id.clone();
            handles.push((id, tokio::spawn(async move {
                let _owner = owner;
                let _permit = this.permits.acquire().await.expect("semaphore closed");
                emitter.emit(RunEvent::Started(&task.id, &task.harness));
                if let Err(e) = this.exec(&emitter, &task, ExecMode::FirstRun { outputs }).await {
                    emitter.mutate(|st| {
                        if let Some(n) = st.nodes.get_mut(&task.id) {
                            n.status = TaskStatus::Failed;
                            n.stderr = format!("driver error: {e:#}");
                            n.finished_at = Some(now_secs());
                        }
                    });
                }
                if let Some(n) = emitter.snapshot().nodes.get(&task.id) {
                    let detail = if n.status == TaskStatus::Failed {
                        n.stderr.lines().next().unwrap_or("").to_owned()
                    } else {
                        first_line(&n.output)
                    };
                    emitter.emit(RunEvent::Finished(&n.id, n.status, &detail));
                }
            })));
        }
        // Level barrier: await the whole level before returning. A panicked
        // task (JoinError) never reached a terminal write in exec, so it would
        // sit in Running forever and slip past the external watchdog (which
        // only reads terminal nodes). Settle any node the panic left non-terminal
        // as Failed, and still wait for every sibling — never bail on the first
        // panic.
        for (id, h) in handles {
            if let Err(join_err) = h.await {
                emitter.emit(RunEvent::Finished(&id, TaskStatus::Failed, &format!("task panicked: {join_err}")));
                let id2 = id.clone();
                emitter.mutate(move |st| {
                    if let Some(n) = st.nodes.get_mut(&id2) {
                        if !n.status.is_terminal() {
                            n.status = TaskStatus::Failed;
                            n.stderr = format!("task panicked: {join_err}");
                            n.finished_at = Some(now_secs());
                        }
                    }
                });
            }
        }
    }

    /// Attach saved turns from older runs when this harness has since opted
    /// into transcript chat. Only reuse an unambiguous, completed attempt.
    pub(crate) fn restore_transcript_chats(&self, state: &mut crate::state::RunState) {
        let manifest = self.art.manifest.snapshot();
        for node in state.nodes.values_mut().filter(|n| n.status == TaskStatus::Done && n.session.is_none()) {
            let Some(task) = self.task(&node.id) else { continue };
            if task.harness != node.harness || !connector_for(&self.flow, &task).is_ok_and(|c| c.transcript_chat) {
                continue;
            }
            let Some(entry) = manifest.tasks.get(&node.id) else { continue };
            let digest = artifacts::sha256_hex(node.output.as_bytes());
            let mut matches = entry.attempts.iter().filter(|(_, a)| a.status == "done"
                && a.out.as_ref().is_some_and(|o| o.sha256 == digest));
            if let Some((nonce, _)) = matches.next() {
                if matches.next().is_none() && self.art.dirs.attempt_dir(&node.id, nonce).join("prompt.txt").is_file() {
                    node.session = Some(SessionRef::Transcript { attempt: nonce.clone() });
                }
            }
        }
    }

    /// Steer a finished node: append the human turn, resume the node's session
    /// with it, record the agent's reply, and mark downstream nodes `Stale` so
    /// the human re-runs them (their input just changed). The steering turn is
    /// recorded even if the resume fails, so the transcript tells the truth.
    /// Remember the operator's live canvas, sent with a chat turn. Only an
    /// actual graph overwrites it — a turn that omits the field (an older
    /// client, or a steer of a non-chat node) must not wipe a board we already
    /// know, so the planner keeps seeing the last real state.
    fn set_canvas(&self, graph: Option<serde_json::Value>) {
        if let Some(g) = graph {
            *self.canvas.lock().unwrap_or_else(|p| p.into_inner()) = Some(g);
        }
    }

    /// The canvas block appended to the planner's next prompt, or empty when no
    /// operator has sent a board yet. `pos` is stripped: it is pure layout the
    /// model cannot act on, and dropping it keeps the block small. The wording
    /// says plainly that the operator may have edited this by hand, so the
    /// agent builds on it instead of proposing the graph still in its session.
    fn graph_context(&self) -> String {
        let value = self.canvas.lock().unwrap_or_else(|p| p.into_inner()).clone();
        canvas_prompt(value)
    }

    pub async fn steer(&self, emitter: &Emitter, id: &str, text: &str, graph: Option<serde_json::Value>) -> Result<()> {
        self.steer_with_context(emitter, id, text, graph, "").await
    }

    pub(crate) async fn steer_with_context(&self, emitter: &Emitter, id: &str, text: &str, graph: Option<serde_json::Value>, context: &str) -> Result<()> {
        install_death_handlers();
        let task = self.task(id).ok_or_else(|| anyhow!("no such task '{id}'"))?;

        // Snapshot the session handle before mutating, so we can resume it.
        let session = emitter
            .snapshot()
            .nodes
            .get(id)
            .and_then(|n| n.session.clone())
            .ok_or_else(|| anyhow!("task '{id}' has no resumable session to steer"))?;

        // The operator may have moved a node, switched a model or wired a
        // dependency since the last turn; remember the board so the prompt we
        // send is built on it, not on the graph still stuck in the session.
        self.set_canvas(graph);
        emitter.append_turn(id, Role::Human, text);
        emitter.set_status(id, TaskStatus::Running);

        // The transcript keeps the operator's words verbatim; only the model's
        // prompt gains the canvas block, so what the operator reads back stays
        // exactly what they typed.
        let prompt = format!("{text}{}{context}", self.graph_context());
        let outcome = self
            .exec(emitter, &task, ExecMode::Resume(session.clone(), prompt))
            .await;
        match outcome {
            Ok(()) => {
                self.mark_downstream_stale(emitter, id);
                Ok(())
            }
            Err(e) => {
                emitter.mutate(|st| {
                    if let Some(n) = st.nodes.get_mut(id) {
                        n.status = TaskStatus::Failed;
                        n.stderr = format!("steer error: {e:#}");
                        n.finished_at = Some(now_secs());
                    }
                });
                Err(e)
            }
        }
    }

    /// Re-run a node from scratch (fresh session → fresh context), then mark its
    /// dependents `Stale`. Used by the console's "rerun" button — through
    /// [`TrackedRun::task`], so it picks up any operator fix registered since
    /// the last attempt.
    pub async fn rerun(&self, emitter: &Emitter, id: &str) -> Result<()> {
        let task = self
            .task(id)
            .ok_or_else(|| anyhow!("no such task '{id}'"))?;
        // Review M4: readiness of the dependencies is checked BEFORE any
        // destructive step. The console's claim gate is not the only entrance
        // to this method (rerun_stale and direct calls reach it too), and the
        // old order wiped the node's session and captured output first — so a
        // rerun accepted on a `Skipped` node under a failed parent destroyed
        // the previous transcript and then died on `unknown reference` (or,
        // with plain depends_on, ran happily on an incomplete prompt).
        if self.blocked(emitter, &task) {
            let snap = emitter.snapshot();
            let blockers: Vec<String> = task
                .depends_on
                .iter()
                .filter(|d| {
                    !snap
                        .nodes
                        .get(*d)
                        .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
                })
                .cloned()
                .collect();
            return Err(anyhow!(
                "node '{id}' waits on unfinished dependencies: {}",
                blockers.join(", ")
            ));
        }
        let _permit = self.permits.acquire().await.expect("semaphore closed");
        // Drop the old session so FirstRun mints a new one.
        self.reset_node(emitter, id);
        let outputs = self.outputs_snapshot(emitter);
        if let Err(e) = self.exec(emitter, &task, ExecMode::FirstRun { outputs }).await {
            // A driver-level failure must NOT leave the node zombie-Running:
            // the claim already flipped it to Running, and nothing else will
            // ever touch it again — record the failure honestly (the same
            // shape `run_level` uses for a first-run driver error).
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(id) {
                    n.status = TaskStatus::Failed;
                    n.stderr = format!("driver error: {e:#}");
                    n.finished_at = Some(now_secs());
                }
            });
            return Err(e);
        }
        self.mark_downstream_stale(emitter, id);
        Ok(())
    }

    /// Drop a node's previous attempt (session handle, captured output,
    /// progress) so a FirstRun mints a fresh session and nothing stale leaks
    /// into the transcript or artifacts.
    fn reset_node(&self, emitter: &Emitter, id: &str) {
        emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(id) {
                n.session = None;
                n.output.clear();
                n.stderr.clear();
                n.exit_code = None;
                n.progress = Default::default();
            }
        });
    }

    /// Re-run every `Stale` node in level order (a "refresh downstream"
    /// action). Stale nodes of one level are independent by construction, so
    /// they run concurrently under the same `max_parallel` cap as a first
    /// run; the level barrier keeps the refresh order honest.
    pub async fn rerun_stale(&self, emitter: &Emitter) {
        for level in &self.live_levels() {
            let stale_ids: Vec<String> = level
                .iter()
                .filter(|id| {
                    emitter
                        .snapshot()
                        .nodes
                        .get(*id)
                        .is_some_and(|n| n.status == TaskStatus::Stale)
                })
                .cloned()
                .collect();
            if stale_ids.is_empty() {
                continue;
            }
            let stale: Vec<Task> = stale_ids
                .iter()
                .filter_map(|id| self.task(id))
                .collect();
            // Review M4: a Stale node is only re-runnable when its deps are
            // Done *now* — an earlier level of this very pass may just have
            // refreshed them (topological order makes that the common case),
            // or a refreshed parent may have failed again (then this node must
            // wait, like a first-run node does in `run_level`). Nodes still
            // waiting keep `Stale` — one refresh click later they are picked
            // up — and record why, so the button silently skipping them is
            // not a mystery.
            let (ready, waiting): (Vec<Task>, Vec<Task>) = stale
                .into_iter()
                .partition(|t| !self.blocked(emitter, t));
            for t in &waiting {
                let snap = emitter.snapshot();
                let blockers: Vec<&str> = t
                    .depends_on
                    .iter()
                    .filter(|d| {
                        !snap
                            .nodes
                            .get(*d)
                            .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
                    })
                    .map(|s| s.as_str())
                    .collect();
                let note = format!("rerun-stale held back: unfinished dependencies: {}", blockers.join(", "));
                let id = t.id.clone();
                emitter.mutate(move |st| {
                    if let Some(n) = st.nodes.get_mut(&id) {
                        n.stderr = note.clone();
                    }
                });
            }
            for t in &ready {
                self.reset_node(emitter, &t.id);
            }
            self.run_level(emitter, ready.clone(), false).await;
            // Cascade Stale onto each refreshed node's dependents AFTER the
            // level is terminal (a dependent never shares a level with its
            // producer, so this is the same set the per-node `rerun` marks —
            // and dependents still ahead in this pass get picked up when
            // their level comes around).
            for t in &ready {
                self.mark_downstream_stale(emitter, &t.id);
            }
        }
    }

    /// Mark transitive dependents of `id` `Stale` if they were `Done`. Running or
    /// already-terminal-but-not-Done nodes are left alone.
    pub(super) fn mark_downstream_stale(&self, emitter: &Emitter, id: &str) {
        let mut frontier = vec![id.to_string()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let tasks = self.tasks();
        while let Some(cur) = frontier.pop() {
            for t in &tasks {
                if !t.depends_on.iter().any(|d| d == &cur) {
                    continue;
                }
                if !seen.insert(t.id.clone()) {
                    continue;
                }
                emitter.mutate(|st| {
                    if let Some(n) = st.nodes.get_mut(&t.id) {
                        if n.status == TaskStatus::Done {
                            n.status = TaskStatus::Stale;
                        }
                    }
                });
                frontier.push(t.id.clone());
            }
        }
    }

    /// Execute one node, supervised: after every attempt the watchdog
    /// ([`crate::watchdog`]) inspects the result and may continue the same
    /// session (a nudge for a clipped/empty reply), answer a question
    /// (canned, in unattended mode) or park it for the operator, re-run the
    /// node from scratch (a failed attempt), or accept it. Every automated
    /// step is recorded as a `system` turn, and each attempt lands in its own
    /// artifact dir (`r0`, `r1`, … for fresh attempts, `t<secs>` for resumes).
    ///
    /// Supervision applies to first runs (and reruns) only: a human steer
    /// (`ExecMode::Resume` on entry) that fails stays visible as a failure —
    /// the human is right there to decide. Driver-level errors (bad argv,
    /// unresolvable prompt) are deterministic and propagate unchanged.
    async fn exec(&self, emitter: &Emitter, task: &Task, mode: ExecMode) -> Result<()> {
        let conn = connector_for(&self.flow, task)?;
        // `echo` participates as the scripted test probe (its reply is the
        // rendered prompt / resume text, so the whole supervision loop is
        // exercisable in CI); the `prompt` block is data, not an agent.
        let manual = matches!(mode, ExecMode::Resume(..));
        if !conn.is_agent && conn.id != "echo" {
            let nonce = mode_nonce(&mode, 0);
            return self.exec_once(emitter, task, &mode, &nonce, None, false).await.map(|_| ());
        }
        let settings = supervision_settings(&self.flow, task, self.unattended);
        let mut nudges_left = if manual { 0 } else { settings.nudges };
        let mut retries_left = if manual { 0 } else { settings.retry };
        let mut auto_answers_left = crate::watchdog::AUTO_ANSWERS_MAX;
        let mut attempt_no: u32 = 0;
        let mut mode = mode;

        loop {
            let nonce = mode_nonce(&mode, attempt_no);
            let verdict = self.exec_once(emitter, task, &mode, &nonce, Some(&settings), !manual).await?;
            let node = emitter.snapshot().nodes.get(&task.id).cloned();
            let Some(n) = node else { break };

            // A failed summary must never replay the original task. The last
            // attempt and its partial output are already in the manifest.
            if matches!(mode, ExecMode::CoddySummary { .. }) && verdict != Verdict::Ok {
                break;
            }
            match verdict {
                Verdict::CoddyStepLimit(steps) => {
                    let Some(SessionRef::Coddy { sessions_dir, session_id }) = n.session else { break };
                    if nudges_left == 0 { break; }
                    nudges_left -= 1;
                    Self::watchdog_note(emitter, &task.id, format!(
                        "[watchdog] Coddy hit its limit of {steps} steps. Requesting the final report in the same session."
                    ));
                    mode = ExecMode::CoddySummary { sessions_dir, session_id };
                    continue;
                }
                Verdict::Retry(reason) if retries_left > 0 => {
                    retries_left -= 1;
                    attempt_no += 1;
                    Self::watchdog_note(
                        emitter,
                        &task.id,
                        format!("[watchdog] attempt failed ({reason}) — restarting from scratch, {retries_left} retries left"),
                    );
                    self.reset_node(emitter, &task.id);
                    mode = ExecMode::FirstRun { outputs: self.outputs_snapshot(emitter) };
                    continue;
                }
                Verdict::Ask(_) if settings.unattended
                    && auto_answers_left > 0
                    && resumable(&n.session) =>
                {
                    auto_answers_left -= 1;
                    Self::watchdog_note(
                        emitter,
                        &task.id,
                        format!(
                            "[watchdog] question in unattended mode, auto-answering ({auto_answers_left} auto-answers left):\n{}",
                            crate::watchdog::AUTO_ANSWER
                        ),
                    );
                    let sess = n.session.clone().expect("resumable checked");
                    mode = ExecMode::Resume(sess, crate::watchdog::AUTO_ANSWER.to_string());
                    continue;
                }
                Verdict::Ask(_) if !settings.unattended && resumable(&n.session) => {
                    let Some(answer) = self.wait_for_answers(emitter, &task.id, settings.ask_timeout_secs).await else {
                        break;
                    };
                    // The answer is a fresh planner turn too, so it reasons from
                    // the board the operator holds now (`answer_question` stored
                    // it before we wake here). Empty for a plain node that never
                    // sent a canvas, so this is a no-op off the chat path.
                    let prompt = format!("{answer}{}", self.graph_context());
                    mode = ExecMode::Resume(n.session.expect("resumable checked"), prompt);
                    continue;
                }
                Verdict::Nudge(kind) if nudges_left > 0 && resumable(&n.session) => {
                    nudges_left -= 1;
                    Self::watchdog_note(
                        emitter,
                        &task.id,
                        format!(
                            "[watchdog] the reply looks unfinished, nudging ({nudges_left} nudges left):\n{}",
                            crate::watchdog::nudge_text(kind)
                        ),
                    );
                    let sess = n.session.clone().expect("resumable checked");
                    mode = ExecMode::Resume(sess, crate::watchdog::nudge_text(kind).to_string());
                    continue;
                }
                // Terminal: the agent's last reply is final. For a supermodel
                // chat node this is where a ` ```zori-flow ` block becomes a
                // server-checked proposal — and, if invalid, earns one bounded
                // repair turn. For every other node this is a no-op `break`.
                _ => match self.proposal_gate(emitter, task).await {
                    Some(fix) => {
                        mode = fix;
                        continue;
                    }
                    None => break,
                },
            }
        }
        Ok(())
    }

    fn watchdog_note(emitter: &Emitter, id: &str, text: String) {
        emitter.emit(RunEvent::Watchdog(id, &text));
        emitter.append_turn(id, Role::System, text);
    }

    /// The propose→validate→(fix) gate, run once when a chat node's turn is
    /// final. Reads the model's committed reply, and if it carried a
    /// ` ```zori-flow ` block, validates it with the *same* validator the flow
    /// builder uses and stores the verdict on the node as a [`FlowProposal`].
    /// A valid graph is parked for the operator's Run click; an invalid one
    /// earns a single bounded continuation turn (the [`crate::proposal`]
    /// `fix_instruction`) so the model can repair it before giving up. Returns
    /// the `Resume` to take for a repair turn, or `None` to end the turn.
    ///
    /// A no-op for every non-chat node (`is_chat` false → break unchanged), so
    /// it cannot perturb ordinary flow supervision.
    pub(super) async fn proposal_gate(&self, emitter: &Emitter, task: &Task) -> Option<ExecMode> {
        // Only a supermodel dialogue proposes graphs; a normal flow has nothing
        // to gate here, and a node that never ran (no snapshot) has no reply.
        let snap = emitter.snapshot();
        if !snap.is_chat {
            return None;
        }
        let node = snap.nodes.get(&task.id)?;
        // No block in this reply is a plain conversational turn (phase 1 of the
        // protocol: the model thinks and asks) — keep any prior proposal and end.
        // Review M12: the parse + full builder validation run on a blocking
        // thread, not this runtime worker (an oversized/hostile graph YAML
        // would otherwise stall every other run's event handling), and an
        // oversized block surfaces as an invalid proposal, not silence.
        let proposal = crate::proposal::from_reply_async(node.output.clone()).await?;
        let prior_fix = node.proposal.as_ref().map(|p| p.fix_rounds).unwrap_or(0);
        let resumable_here = resumable(&node.session);
        let session = node.session.clone();
        // The three-way-merge base the console needs: what both sides last
        // agreed on. Prefer the agent's last accepted graph (the previous
        // valid proposal's spec) — the operator's edits made after it was
        // placed, before sending this turn or during the reply, then read as
        // operator-owned, so a reply echoing pre-edit values cannot revert
        // them. Without an earlier accepted proposal the board shipped with
        // this turn is the only common view (there is nothing to protect
        // against: the agent has never spoken in graphs).
        let base = node
            .proposal
            .as_ref()
            .and_then(|p| p.spec.clone())
            .or_else(|| self.canvas.lock().expect("canvas lock").clone());

        if proposal.valid {
            emitter.mutate(move |st| {
                if let Some(n) = st.nodes.get_mut(&task.id) {
                    n.proposal = Some(FlowProposal {
                        raw: proposal.raw,
                        hash: proposal.hash,
                        valid: true,
                        errors: Vec::new(),
                        warnings: proposal.warnings,
                        levels: proposal.levels,
                        spec: proposal.spec,
                        base,
                        fix_rounds: prior_fix,
                        status: ProposalStatus::Proposed,
                        run_id: None,
                        ts: now_secs(),
                    });
                }
            });
            return None;
        }

        // Invalid. Ask the model to repair it — but bounded, so a looping model
        // cannot spin the run the way watchdog nudges are bounded.
        let repair = prior_fix < crate::proposal::MAX_FIX_ROUNDS && resumable_here;
        // Compute the repair text while `proposal.errors` is still ours; the
        // store closure below moves `proposal` into itself.
        let fix_text = repair.then(|| crate::proposal::fix_instruction(&proposal.errors));
        if repair {
            emitter.append_turn(
                &task.id,
                Role::System,
                format!(
                    "[zori] the proposal failed validation — asking the model to fix it (round {}/{})",
                    prior_fix + 1,
                    crate::proposal::MAX_FIX_ROUNDS
                ),
            );
        }
        let task_id = task.id.clone();
        let store = move |st: &mut crate::state::RunState| {
            if let Some(n) = st.nodes.get_mut(&task_id) {
                n.proposal = Some(FlowProposal {
                    raw: proposal.raw,
                    hash: proposal.hash,
                    valid: false,
                    errors: proposal.errors,
                    warnings: Vec::new(),
                    levels: Vec::new(),
                    spec: None,
                    base: None,
                    fix_rounds: if repair { prior_fix + 1 } else { prior_fix },
                    status: ProposalStatus::Proposed,
                    run_id: None,
                    ts: now_secs(),
                });
            }
        };
        emitter.mutate(store);
        if let Some(text) = fix_text {
            return Some(ExecMode::Resume(session.expect("resumable checked"), text));
        }
        None
    }

    async fn wait_for_answers(&self, emitter: &Emitter, id: &str, timeout_secs: u64) -> Option<String> {
        emitter.append_turn(id, Role::System, "[watchdog] waiting for the operator's answers");
        let deadline = tokio::time::Instant::now().checked_add(Duration::from_secs(timeout_secs));
        loop {
            // notify_waiters wakes all registered nodes. Register before checking
            // the durable predicate so a reply cannot fall between check and wait.
            let notified = self.answers.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut response = None;
            let mut failed = false;
            emitter.mutate(|st| {
                let n = st.nodes.get_mut(id).expect("executing node");
                let mut open: Vec<_> = n.questions.iter_mut().filter(|q| !q.closed).collect();
                if let Some(q) = open.iter().find(|q| q.answer.is_none()) {
                    n.ask = Some(format!("ASK: {}", q.text));
                    n.status = TaskStatus::Waiting;
                    let expired = timeout_secs > 0 && deadline.is_some_and(|d| tokio::time::Instant::now() >= d);
                    n.stderr = if self.operator_input_closed.load(Ordering::Relaxed) {
                        "watchdog: operator input closed before an answer".into()
                    } else if expired {
                        format!("watchdog: operator answer timed out after {timeout_secs}s")
                    } else {
                        return;
                    };
                    failed = true;
                    n.status = TaskStatus::Failed;
                    n.finished_at = Some(now_secs());
                } else {
                    response = Some(if open.len() == 1 && !open[0].inline {
                        open[0].answer.clone().expect("answered")
                    } else {
                        open.iter().map(|q| format!("{} — {}", q.text, q.answer.as_deref().unwrap()))
                            .collect::<Vec<_>>().join("\n")
                    });
                    n.status = TaskStatus::Running;
                }
                for q in &mut open { q.closed = true; }
                n.ask = None;
            });
            if failed || response.is_some() { return response; }
            if let Some(d) = deadline.filter(|_| timeout_secs != 0) {
                let _ = tokio::time::timeout_at(d, notified).await;
            } else {
                notified.await;
            }
        }
    }

    /// Legacy clients can answer a single parked question without its ID.
    pub async fn answer(&self, emitter: &Emitter, id: &str, text: &str) -> Result<()> {
        self.answer_question(emitter, id, None, text, None)
    }

    pub fn answer_question(&self, emitter: &Emitter, id: &str, question_id: Option<usize>, text: &str, graph: Option<serde_json::Value>) -> Result<()> {
        anyhow::ensure!(!text.trim().is_empty(), "empty answer");
        // Remember the board BEFORE waking the parked loop: its ASK-answer
        // resume reads `graph_context()` the instant `notify_waiters` lands, so
        // the canvas must already be stored. Same rule as `steer` — an absent
        // graph keeps the last known board rather than clearing it.
        self.set_canvas(graph);
        let mut accepted = false;
        emitter.mutate(|st| {
            let Some(n) = st.nodes.get_mut(id) else { return };
            if !matches!(n.status, TaskStatus::Running | TaskStatus::Waiting) { return; }
            let mut matching = n.questions.iter_mut().filter(|q| !q.closed && q.answer.is_none()
                && question_id.map_or(n.status == TaskStatus::Waiting, |qid| q.id == qid));
            let Some(q) = matching.next() else { return };
            if matching.next().is_some() { return; }
            q.answer = Some(text.to_owned());
            n.turns.push(crate::state::Turn { role: Role::Human, text: text.to_owned(), ts: now_secs() });
            accepted = true;
        });
        anyhow::ensure!(accepted, "question is closed, already answered, or requires a question_id");
        self.answers.notify_waiters();
        Ok(())
    }

    /// Execute ONE attempt in either mode, writing
    /// status/transcript/output/artifacts and returning the recorded verdict. An `Err` is a
    /// driver-level failure (bad argv, unresolvable prompt).
    async fn exec_once(&self, emitter: &Emitter, task: &Task, mode: &ExecMode, nonce: &str, supervision: Option<&Settings>, supervise_failures: bool) -> Result<Verdict> {
        let conn = connector_for(&self.flow, task)?;
        let timeout = timeout_for(task, self.default_timeout);
        let started_at = now_secs();

        // The task's subtree + isolated work dir + attempt dir (r0/r1/… for
        // fresh attempts, t<secs> per steer/watchdog resume — the old flat
        // nonce-suffixed names, now one directory per attempt).
        let task_dir = self.art.dirs.ensure_task(&task.id)?;
        let work_dir = self.art.dirs.work_dir(&task.id);
        let (attempt_dir, nonce) = self.art.dirs.claim_attempt(&task.id, nonce)?;
        let ctx = PromptCtx {
                run_dir: Some(self.art.dirs.root().to_path_buf()),
            task_dir: Some(task_dir.clone()),
            work_dir: Some(work_dir.clone()),
            project_dir: self.art.project_dir.clone(),
        };
        let (cwd, flag_cwd) = effective_cwd(&self.flow.cwd, task, Some(&work_dir))?;

        // The scripted harnesses — `echo` (test probe) and `prompt` (the
        // starting block) — run no process; `prompt` shares the path because
        // its output is likewise just the rendered prompt. The whole steer
        // path stays exercisable in tests without a model, and the scripted
        // reply is recorded like any harness's attempt. Neither gets the
        // ask-protocol preamble: echo's reply is the prompt verbatim (and
        // tests assert byte-exact outputs), the prompt block is data.
        if conn.id == "echo" || conn.id == "prompt" {
            emitter.set_status(task.id.as_str(), TaskStatus::Running);
            // Skills DO ride the echo/prompt path (unlike the preamble): a
            // `prompt` node is the rendered prompt as data, so an attached
            // skill is exactly what should materialise there. No-skill tasks
            // render None, so echo's byte-exact test outputs are unaffected.
            let skills = skill_block(&self.flow, task)?;
            let reply = match mode {
                ExecMode::FirstRun { outputs } => render_prompt(task, &self.inputs, outputs, &ctx, None, skills.as_deref())?,
                ExecMode::Resume(_, human) => human.clone(),
                ExecMode::CoddySummary { .. } => unreachable!("summary belongs to a Coddy task"),
            };
            if supervision.is_some_and(|s| s.ask && !s.unattended) {
                publish_questions(emitter, &task.id, &reply, &mut None);
            }
            let verdict = attempt_verdict(&Attempt { reply: &reply, success: true, timed_out: false, exit_code: Some(0) },
                supervision.filter(|_| supervise_failures), true);
            let verdict = question_verdict(emitter, &task.id, verdict, true, supervision.is_some_and(|s| s.unattended));
            let error = verdict_failure(&verdict);
            let status = if error.is_some() { TaskStatus::Failed } else { TaskStatus::Done };
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(&task.id) {
                    n.session = Some(SessionRef::Echo);
                    n.turns.push(crate::state::Turn { role: Role::Agent, text: reply.clone(), ts: now_secs() });
                    n.output = reply.clone();
                    n.status = question_status(n, status, &verdict);
                    n.stderr = error.clone().unwrap_or_default();
                    n.exit_code = Some(0);
                    n.finished_at = Some(now_secs());
                }
            });
            let _ = tokio::fs::write(attempt_dir.join("prompt.txt"), &reply).await;
            let _ = tokio::fs::write(attempt_dir.join("out.txt"), &reply).await;
            let _ = tokio::fs::write(attempt_dir.join("err.txt"), error.as_deref().unwrap_or("")).await;
            let _ = artifacts::write_meta(
                &attempt_dir,
                &AttemptMeta {
                    harness: conn.id.clone(),
                    model: task.model.clone(),
                    argv: vec![],
                    env_keys: env_keys(&conn, task),
                    status: status.label().to_string(),
                    exit_code: Some(0),
                    started_at: Some(started_at),
                    ended_at: Some(now_secs()),
                    cwd: Some(cwd.clone()),
                    prompt_sha256: artifacts::sha256_hex(reply.as_bytes()),
                },
            );
            self.art.manifest.record_attempt(
                &task.id,
                &conn.id,
                task.model.clone(),
                task.optional,
                &nonce,
                AttemptRecord {
                    status: status.label().to_string(),
                    error,
                    exit_code: Some(0),
                    started_at: Some(started_at),
                    ended_at: Some(now_secs()),
                    cwd: Some(cwd.clone()),
                    session: Some(SessionRef::Echo),
                    out: Some(OutFile {
                        path: self.art.dirs.relative(&attempt_dir.join("out.txt")),
                        sha256: artifacts::sha256_hex(reply.as_bytes()),
                        bytes: reply.len(),
                    }),
                },
            );
            return Ok(verdict);
        }

        // Build the argv: first-run via plan()+track_args, resume via resume_plan.
        // The ask-protocol preamble rides on first-run prompts of real agents
        // only (echo/prompt are handled above).
        let (mut sp, harness_session, prompt) = match &mode {
            ExecMode::FirstRun { outputs } => {
                let preamble = if crate::watchdog::preamble_applies(&conn) {
                    supervision.and_then(crate::watchdog::preamble)
                } else {
                    None
                };
                let skills = skill_block(&self.flow, task)?;
                let prompt = render_prompt(task, &self.inputs, outputs, &ctx, preamble.as_deref(), skills.as_deref())?;
                let mut sp = connectors::plan(&conn, task, &prompt, flag_cwd.as_deref())?;
                // Coddy: mint a fixed session id and isolate it so a sibling
                // task in the same cwd can never be the "most recent".
                // Review M5: merged at the `--` terminator, not appended —
                // a dash-first prompt makes `plan()` close options early, and
                // a blind extend would bury these very flags among positional
                // text (codex/opencode would then never emit their session
                // key, silently killing tracked capture).
                let session = if conn.id == "coddy" {
                    let sid = new_coddy_session_id(&emitter.snapshot().run_id, &task.id);
                    connectors::insert_track_args(
                        &mut sp.argv,
                        connectors::track_args(&conn, &sid, &self.sessions_dir),
                    );
                    Some(SessionRef::Coddy {
                        sessions_dir: self.sessions_dir.clone(),
                        session_id: sid,
                    })
                } else if conn.transcript_chat {
                    Some(SessionRef::Transcript { attempt: nonce.clone() })
                } else {
                    // Codex (`--json`) and OpenCode (`--format json`): stream a
                    // machine format so the reply + session key come off stdout.
                    connectors::insert_track_args(
                        &mut sp.argv,
                        connectors::track_args(&conn, "", &self.sessions_dir),
                    );
                    None
                };
                (sp, session, prompt)
            }
            ExecMode::Resume(SessionRef::Transcript { attempt }, human) => {
                anyhow::ensure!(conn.transcript_chat, "transcript chat disabled for {}", conn.id);
                anyhow::ensure!(Path::new(attempt).file_name() == Some(attempt.as_ref()), "invalid attempt id");
                let previous = self.art.dirs.attempt_dir(&task.id, attempt);
                let prior_prompt = tokio::fs::read_to_string(previous.join("prompt.txt")).await
                    .context("reading the previous conversation prompt")?;
                let reply = tokio::fs::read_to_string(previous.join("out.txt")).await
                    .context("reading the previous agent reply")?;
                let protocol = if prior_prompt.contains("# Operator communication protocol") {
                    None
                } else {
                    supervision.and_then(crate::watchdog::preamble)
                };
                let prompt = format!("{prior_prompt}\n{}\n# Previous assistant reply\n{reply}\n\n# Follow-up message\n{human}\n\nContinue the task from this exchange and the work already in this directory.", protocol.as_deref().unwrap_or(""));
                let mut continued = conn.clone();
                if continued.prompt_file_flag.is_some() && continued.prompt_via == PromptVia::Arg {
                    continued.prompt_via = PromptVia::File;
                }
                let sp = connectors::plan(&continued, task, &prompt, flag_cwd.as_deref())?;
                (sp, Some(SessionRef::Transcript { attempt: nonce.clone() }), prompt)
            }
            ExecMode::Resume(session, human) => {
                let sp = connectors::resume_plan(&conn, session, human, Some(&cwd), &task.args)?;
                // Reuse the stored session handle (context must persist).
                (sp, Some(session.clone()), human.clone())
            }
            ExecMode::CoddySummary { sessions_dir, session_id } => {
                let prompt = crate::watchdog::CODDY_SUMMARY;
                let mut sp = connectors::plan(&conn, task, prompt, flag_cwd.as_deref())?;
                connectors::insert_track_args(&mut sp.argv, connectors::track_args(&conn, session_id, sessions_dir));
                let session = SessionRef::Coddy { sessions_dir: sessions_dir.clone(), session_id: session_id.clone() };
                (sp, Some(session), prompt.to_owned())
            }
        };

        // The attempt's prompt: the rendered prompt on a first run, the human
        // message on a steer. Kept forever (provenance); File-mode harnesses
        // get its path substituted into argv, replacing the deleted temp file.
        tokio::fs::write(attempt_dir.join("prompt.txt"), &prompt).await.with_context(|| "writing prompt file")?;
        if sp.file_prompt.is_some() {
            // The runs root is often relative (`zori-runs`) and the child
            // resolves argv paths against ITS cwd (the task work dir), so the
            // prompt-file path must be absolute — a relative one exits the CLI
            // before it ever reads the prompt.
            let prompt_path = artifacts::absolute(&attempt_dir.join("prompt.txt"));
            for tok in sp.argv.iter_mut() {
                if tok == "__ZORI_PROMPT_FILE__" {
                    *tok = prompt_path.display().to_string();
                }
            }
        }

        // Record the session handle before spawning, so even a crash keeps the
        // id (the transcript still shows captured stdout; the id survives).
        if let Some(sess) = &harness_session {
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(&task.id) {
                    n.session = Some(sess.clone());
                }
            });
        }
        emitter.set_status(task.id.as_str(), TaskStatus::Running);

        // ZORI_* context first, connector/task env can override.
        let mut env = artifacts::task_env(
            self.art.dirs.root(),
            &task_dir,
            &work_dir,
            self.art.project_dir.as_deref(),
        );
        env.extend(env_pairs(&conn, task));

        let summarizing = matches!(mode, ExecMode::CoddySummary { .. });
        let out = spawn_captured(
            &sp.program,
            &sp.argv,
            conn.prompt_via,
            sp.stdin_prompt.as_deref(),
            Some(&cwd),
            &env,
            timeout,
            &task.id,
            emitter,
            supervision.filter(|s| s.extend_on_progress && conn.is_agent)
                .map(|_| Activity::new(harness_session.as_ref())),
            supervision.filter(|s| !summarizing && s.ask && !s.unattended && crate::watchdog::preamble_applies(&conn))
                .map(|_| conn.id.clone()),
        )
        .await;

        let out = out?;

        // Turn the raw capture into (a) what flows downstream and (b) the
        // transcript line. Only assistant text from JSONL goes downstream.
        let mut accepted_warning = None;
        let (agent_reply, final_reply, resolved_session, stream_error) = match conn.id.as_str() {
            "codex" => {
                let mut ev = connectors::parse_codex_jsonl(&out.stdout);
                let error = ev.error.or_else(|| {
                    (!ev.turn_completed && !ev.agent_messages.is_empty())
                        .then(|| "Codex stream ended before turn.completed".to_owned())
                });
                let sess = ev.thread_id.map(|thread_id| SessionRef::Codex { thread_id });
                (ev.agent_messages.join("\n\n"), ev.agent_messages.pop(), sess.or(harness_session), error)
            }
            "opencode" => {
                let ev = connectors::parse_opencode_jsonl(&out.stdout);
                let final_reply = ev.agent_texts[ev.final_step_start..].join("\n\n");
                let facts = Attempt { reply: &final_reply, success: out.success, timed_out: out.timed_out, exit_code: out.exit_code };
                let accept = supervision.is_some_and(|s| s.ask || s.retry > 0 || s.nudges > 0)
                    && crate::watchdog::accept_opencode_report(&facts, &ev);
                let sess = ev.session_id.map(|session_id| SessionRef::Opencode { session_id });
                if accept {
                    accepted_warning = ev.late_input_error;
                    (final_reply.clone(), Some(final_reply), sess.or(harness_session), None)
                } else {
                    let error = ev.error.or(ev.late_input_error).or_else(|| ev.step_open.then(|| "OpenCode stream ended before step_finish".to_owned()));
                    (ev.agent_texts.join("\n\n"), Some(final_reply), sess.or(harness_session), error)
                }
            }
            _ => (out.stdout.clone(), None, harness_session, None),
        };
        let facts = Attempt {
            reply: final_reply.as_deref().unwrap_or(&agent_reply),
            success: (out.success || accepted_warning.is_some()) && !out.timed_out,
            timed_out: out.timed_out,
            exit_code: out.exit_code,
        };
        // Arbitrary command harnesses can legitimately produce files and no stdout.
        let supervision = supervision.filter(|_| crate::watchdog::preamble_applies(&conn));
        let harness_verdict = match conn.id.as_str() {
            "coddy" => crate::watchdog::coddy_verdict(&facts, &out.stderr, summarizing),
            _ if !out.timed_out => stream_error.clone().map(Verdict::Retry),
            _ => None,
        };
        let mut verdict = harness_verdict.unwrap_or_else(|| {
            attempt_verdict(&facts, supervision.filter(|_| supervise_failures), resumable(&resolved_session))
        });
        if conn.id == "devin" && conn.transcript_chat && facts.success && verdict == Verdict::Ok {
            if let Some(settings) = supervision.filter(|s| s.ask) {
                if let Some(text) = crate::watchdog::devin_access_question(&agent_reply, &out.stderr) {
                    if !settings.unattended {
                        publish_questions(emitter, &task.id, &format!("ASK: {text}"), &mut None);
                    }
                    verdict = Verdict::Ask(text.to_owned());
                }
            }
        }
        let verdict = question_verdict(emitter, &task.id, verdict, resumable(&resolved_session), supervision.is_some_and(|s| s.unattended));
        // Review (minor): when the JSONL stream also broke at the moment the
        // timeout fired, only the timeout reached the operator — the truncated
        // stream was visible nowhere but in stdout.log. Keep the timeout as the
        // head (its exact prefix is a stable contract: watchdog retries grep
        // it) and append the stream reason, so the attempt names both causes.
        let fail_reason = if out.timed_out {
            let mut reason = format!("TIMED OUT after {}s (killed with its process group)", out.timeout.as_secs());
            if let Some(se) = &stream_error {
                reason.push_str(&format!("; stream also failed: {se}"));
            }
            Some(reason)
        } else if !facts.success && stream_error.is_none() {
            Some(out.exit_code.map(|c| format!("exited with code {c}"))
                .unwrap_or_else(|| "killed by signal (no exit code)".into()))
        } else {
            verdict_failure(&verdict)
        };
        let status = if fail_reason.is_some() { TaskStatus::Failed } else { TaskStatus::Done };
        let stderr_final = if out.timed_out {
            let mut head = format!("TIMED OUT after {}s", out.timeout.as_secs());
            if let Some(se) = &stream_error {
                head.push_str(&format!(" (stream also failed: {se})"));
            }
            format!("{head}\n{}", out.stderr)
        } else if let Some(reason) = &fail_reason {
            format!("{reason}\n{}", out.stderr)
        } else if let Some(warning) = &accepted_warning {
            format!("{warning}\n{}", out.stderr)
        } else {
            out.stderr.clone()
        };

        emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(&task.id) {
                if resolved_session.is_some() {
                    n.session = resolved_session.clone();
                }
                n.turns.push(crate::state::Turn { role: Role::Agent, text: agent_reply.clone(), ts: now_secs() });
                if let Some(warning) = &accepted_warning {
                    n.turns.push(crate::state::Turn { role: Role::System,
                        text: format!("[watchdog] Final answer accepted. OpenCode reported after the step finished: {warning}"),
                        ts: now_secs() });
                }
                n.output = agent_reply.clone();
                n.stderr = stderr_final.clone();
                n.exit_code = out.exit_code;
                n.status = question_status(n, status, &verdict);
                n.finished_at = Some(now_secs());
                n.progress.bytes = out.stdout.len();
            }
        });

        // Persist the full captured text, including on timeout:
        // one directory per attempt under the task's subtree, plus the
        // manifest record with the session handle a fork would need to resume.
        let ended_at = now_secs();
        let _ = tokio::fs::write(attempt_dir.join("out.txt"), &agent_reply).await;
        let _ = tokio::fs::write(attempt_dir.join("err.txt"), &stderr_final).await;
        if (fail_reason.is_some() || accepted_warning.is_some()) && matches!(conn.id.as_str(), "codex" | "opencode") {
            let _ = tokio::fs::write(attempt_dir.join("stdout.log"), &out.stdout).await;
        }
        let session = emitter
            .snapshot()
            .nodes
            .get(&task.id)
            .and_then(|n| n.session.clone());
        let _ = artifacts::write_meta(
            &attempt_dir,
            &AttemptMeta {
                harness: conn.id.clone(),
                model: task.model.clone(),
                argv: sp.argv.clone(),
                env_keys: env_keys(&conn, task),
                status: status.label().to_string(),
                exit_code: out.exit_code,
                started_at: Some(started_at),
                ended_at: Some(ended_at),
                cwd: Some(cwd.clone()),
                prompt_sha256: artifacts::sha256_hex(prompt.as_bytes()),
            },
        );
        self.art.manifest.record_attempt(
            &task.id,
            &conn.id,
            task.model.clone(),
            task.optional,
            &nonce,
            AttemptRecord {
                status: status.label().to_string(),
                error: fail_reason,
                exit_code: out.exit_code,
                started_at: Some(started_at),
                ended_at: Some(ended_at),
                cwd: Some(cwd.clone()),
                session,
                out: Some(OutFile {
                    path: self.art.dirs.relative(&attempt_dir.join("out.txt")),
                    sha256: artifacts::sha256_hex(agent_reply.as_bytes()),
                    bytes: agent_reply.len(),
                }),
            },
        );
        Ok(verdict)
    }
}
