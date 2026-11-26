use super::*;

#[derive(Serialize)]
pub(super) struct RunSummary {
    chat_title: Option<String>,
    launched_run: Option<String>,
    run_id: String,
    flow_name: String,
    status: String,
    updated_at: u64,
    nodes: usize,
}

pub(super) async fn list_runs(State(state): State<std::sync::Arc<AppState>>) -> Json<Vec<RunSummary>> {
    // Review (minor): take the emitter handles out and release the registry
    // guard BEFORE touching any per-run lock. The old shape held the shared
    // `state.runs` mutex across emitter.snapshot() for every run — snapshot
    // takes each run's own mutex, and a panic under that guard (a mutate
    // closure, a future summary-builder regression) would unwind with the
    // registry guard still alive, poisoning the shared mutex so every later
    // handler that locks `state.runs` (lookup, boot, this route) died with
    // it. One run's panic must stay contained to that run.
    let emitters: Vec<Emitter> = runs_guard(&state.runs)
        .values()
        .map(|m| m.emitter.clone())
        .collect();
    let mut out: Vec<RunSummary> = emitters
        .iter()
        .map(|emitter| {
            let s = emitter.snapshot();
            RunSummary {
                launched_run: s.nodes.get("chat").filter(|_| s.is_chat)
                    .and_then(|n| n.proposal.as_ref()?.run_id.clone()),
                chat_title: s.is_chat.then(|| {
                    s.nodes.get("chat").and_then(|n| n.turns.first())
                        .map(|t| t.text.trim().chars().take(48)
                            .map(|c| if c.is_whitespace() { ' ' } else { c }).collect())
                        .unwrap_or_default()
                }),
                run_id: s.run_id,
                flow_name: s.flow_name,
                status: format!("{:?}", s.status).to_lowercase(),
                updated_at: s.updated_at,
                nodes: s.nodes.len(),
            }
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    Json(out)
}

pub(super) async fn boot(
    State(state): State<std::sync::Arc<AppState>>,
    Json(req): Json<BootReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = std::fs::read_to_string(&req.path)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("reading {}: {e}", req.path)))?;
    let inputs = req.inputs.into_iter().collect::<Vec<_>>();
    let run_id = boot_flow(state, &text, Some(req.path.into()), inputs, None)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "run_id": run_id })))
}

pub(super) async fn get_run(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<RunState>, ApiError> {
    let m = lookup(&state, &id)?;
    Ok(Json(m.emitter.snapshot()))
}

pub(super) async fn events(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let m = lookup(&state, &id)?;
    let emitter = m.emitter.clone();
    let rx = emitter.subscribe();
    let current = emitter.snapshot();

    // Fold the broadcast (tokio) into a `futures::Stream` (axum SSE). The
    // channel carries whole snapshots, so each frame is self-contained: the
    // browser re-renders from the latest it holds. A lagged receiver refetches
    // via GET and simply misses a frame; Closed ends the stream.
    let stream = async_stream::stream! {
        yield Ok(snap_event(&current));
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(snap) => yield Ok(snap_event(&snap)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn snap_event(snap: &RunState) -> Event {
    // Whole-state snapshot per frame; the browser is a pure function of this.
    Event::default().event("state").json_data(snap).unwrap_or_default()
}

pub(super) async fn transcript(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath((id, node)): AxPath<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let m = lookup(&state, &id)?;
    let snap = m.emitter.snapshot();
    let n = snap
        .nodes
        .get(&node)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no node '{node}'")))?;
    Ok(Json(json!({
        "node": node,
        "harness": n.harness,
        "model": n.model,
        "status": n.status,
        "turns": n.turns,
    })))
}

/// Spawn a task that writes each snapshot to `<dir>/state.json` until the
/// channel closes (the run's emitter is dropped).
pub(super) fn spawn_persister(mut rx: broadcast::Receiver<RunState>, dir: PathBuf) {
    tokio::spawn(async move {
        let path = dir.join("state.json");
        loop {
            match rx.recv().await {
                Ok(snap) => {
                    let json = serde_json::to_vec_pretty(&snap).unwrap_or_default();
                    // write-then-rename so a crash mid-write never truncates a
                    // previously-good state.json.
                    let tmp = path.with_extension("json.tmp");
                    if tokio::fs::write(&tmp, &json).await.is_ok() {
                        let _ = tokio::fs::rename(&tmp, &path).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// The run's *effective* inputs as recorded in `manifest.json` (flow defaults
/// merged with whatever boot overrode). Re-deriving them from YAML alone loses
/// keys supplied only at launch — a task referencing such a key then fails
/// `unknown reference` after a restart, and an overridden key silently reverts
/// to its default (review M2). A run with no readable manifest returns empty,
/// keeping the old behaviour: `TrackedRun::new` falls back to YAML defaults.
/// A corrupt manifest is also empty-with-a-warning: reloading the run must
/// not hinge on one file, and load_or_init (which refuses to clobber) covers
/// the write path.
pub(super) fn manifest_run_inputs(dir: &Path, run_id: &str) -> Vec<(String, String)> {
    let path = dir.join("manifest.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<crate::artifacts::Manifest>(&text) {
            Ok(m) => m.inputs.into_iter().collect(),
            Err(e) => {
                eprintln!(
                    "warning: run '{run_id}': unreadable manifest.json ({e}) — falling back to flow defaults for inputs"
                );
                Vec::new()
            }
        },
        Err(_) => Vec::new(),
    }
}

/// Reload a run from a `<dir>/state.json` written by a previous server. The
/// transcript, node statuses and session handles survive; any node that was
/// `Running` at shutdown cannot resume (its child is gone) and comes back
/// `Stale` so the human re-runs it. Registered under the stored run id.
pub async fn load_run(
    state: std::sync::Arc<AppState>,
    dir: &Path,
    runs_root: &Path,
) -> anyhow::Result<String> {
    use anyhow::Context;
    let text = std::fs::read_to_string(dir.join("state.json"))
        .with_context(|| format!("reading {}/state.json", dir.display()))?;
    let mut snap: RunState = serde_json::from_str(&text).context("parsing state.json")?;

    let flow_text = std::fs::read_to_string(&snap.flow_path)
        .with_context(|| format!("flow file missing: {}", snap.flow_path.display()))?;
    let flow: Flow = serde_yaml::from_str(&flow_text).context("parsing flow")?;
    let plan = plan(&flow)?;

    // No scheduler survives a restart. Queued nodes also need a manual
    // retry, or they would forever block their failed ancestors from rerunning.
    for n in snap.nodes.values_mut() {
        if matches!(n.status, TaskStatus::Pending | TaskStatus::Running | TaskStatus::Waiting) {
            n.status = TaskStatus::Stale;
            n.ask = None;
            for q in &mut n.questions { q.closed = true; }
        }
    }
    snap.recompute_status();

    let run_dir = runs_root.join(&snap.run_id);
    let sessions_dir = dir.join("sessions");
    let manifest_inputs = manifest_run_inputs(dir, &snap.run_id);
    let tracked = TrackedRun::new(
        flow.clone(),
        plan.clone(),
        &manifest_inputs,
        run_dir.clone(),
        sessions_dir,
    )
    .with_context(|| format!("reinitialising run '{}'", snap.run_id))?;

    tracked.restore_transcript_chats(&mut snap);
    let emitter = Emitter::new(snap);
    let id = emitter.snapshot().run_id.clone();
    runs_guard(&state.runs).insert(
        id.clone(),
        ManagedRun {
            emitter: emitter.clone(),
            tracked,
            op_lock: Arc::new(tokio::sync::Mutex::new(())),
        },
    );
    spawn_persister(emitter.subscribe(), run_dir.clone());

    // Reloaded runs never auto-execute: the human steers or clicks rerun. But a
    // reloaded run can be advanced by retrying interrupted nodes on demand.
    Ok(id)
}

/// Scan `runs_root` for `<id>/state.json` and reload each. A directory whose
/// flow file vanished (or whose JSON is corrupt) is reported and skipped.
pub async fn load_existing_runs(
    state: std::sync::Arc<AppState>,
    runs_root: &Path,
) -> anyhow::Result<Vec<String>> {
    let mut loaded = Vec::new();
    let mut entries = match tokio::fs::read_dir(runs_root).await {
        Ok(e) => e,
        Err(_) => return Ok(loaded),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() && path.join("state.json").is_file() {
            match load_run(state.clone(), &path, runs_root).await {
                Ok(id) => loaded.push(id),
                Err(e) => eprintln!("skipping run {}: {e:#}", path.display()),
            }
        }
    }
    Ok(loaded)
}

// --------------------------------------------------------------------------
// Error / util plumbing
// --------------------------------------------------------------------------
