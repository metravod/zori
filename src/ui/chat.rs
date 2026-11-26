use super::*;

#[derive(Deserialize)]
pub(super) struct ChatRequest {
    pub(super) graph: Option<serde_json::Value>,
    pub(super) attached_run: Option<String>,
    pub(super) harness: String,
    pub(super) model: String,
    pub(super) prompt: String,
    /// Project folder for the planner (the builder's "launch directory").
    /// Absent/empty → the classic chat shape: an isolated per-task work dir.
    /// When given, the chat flow gets `cwd:` + `workspace: flow`, so the agent
    /// process runs IN the folder from the very first message and every
    /// continuation resumes there.
    #[serde(default)]
    pub(super) cwd: Option<String>,
    /// Skills to attach to the chat node. Absent → the default set (see
    /// [`start_chat`], which turns the planner into a config author by
    /// attaching the built-in `zori-config` cheat-sheet). An explicit list is
    /// honoured verbatim — including an empty one, to chat without skills.
    #[serde(default)]
    pub(super) skills: Option<Vec<String>>,
}

/// The hard response protocol a supermodel chat node is held to, injected as
/// the node's `out:` so the runner renders it as the closing `# Output` block
/// on **every** attempt. It lives here, in the server, not in the model's own
/// words, because "the model proposed and then launched a 25-minute pipeline
/// itself" is exactly the failure the propose→confirm→run split forbids. The
/// two inviolable lines — *never save, never run* — are the operator's Run
/// button's reason to exist; the model has no launch authority at all.
///
/// Kept terse and imperative: it is the last thing the model reads before it
/// answers, so it must say what a valid reply *is* (plain text to agree on,
/// then exactly one ` ```zori-flow ` block once agreed), not merely hint.
const CHAT_PROTOCOL: &str = "\
You are the zori supermodel. Discuss the graph and answer questions about the current run, checking your conclusions against the artifacts. When planning a pipeline, follow the protocol below.

PHASES. First discuss the plan in plain text: the steps, who runs each one, what feeds into what, where the parallelism is. Show YAML only once the operator has agreed on the composition.

Boundaries (never break these under any circumstances):
- SAVE NOTHING: do not write flow.yaml, do not edit project files. The graph reaches the disk only when the operator launches it.
- RUN NOTHING: do not run pipelines, CLIs, tests, `zori run`. Only the operator runs things, with the button.
- Do not probe the harness catalog by running CLIs — the real state is already listed below.

GRAPH FORMAT. Once the composition is agreed, emit the FULL graph as a single block tagged `zori-flow` (triple backtick + the word zori-flow, YAML only inside). One such block = one proposal; complete, not a fragment and not a diff. Add nothing after the block. A block without this tag zori will not see and will not offer to run.

CANVAS. A valid graph from a zori block lands on the canvas next to the chat by itself — do not write it to a file. The operator edits it by hand: moves nodes, changes models and parameters, draws more dependencies, and runs it with the button — exactly what is on the canvas is what runs. If the next turn sends you a \"The current graph on the canvas\" block — that is the graph in front of the operator, possibly edited by hand. Build the new graph from it and preserve the operator's edits (models, parameters, dependencies); do not reproduce pos coordinates — they are layout only. When a graph change is agreed, return the FULL graph as one zori-flow block. Answer a question about the run in plain text.";

/// Point the planner to the selected run even when its working directory is
/// another project. Read fresh artifacts when answering follow-up questions.
pub(super) fn planner_run_context(state: &AppState, id: Option<&str>) -> Result<String, ApiError> {
    let Some(id) = id else { return Ok(String::new()); };
    let snap = lookup(state, id)?.emitter.snapshot();
    let dir = crate::artifacts::absolute(&state.runs_root.join(id));
    let nodes: Vec<_> = snap.nodes.values().map(|n| json!({
        "node": n.id, "status": n.status, "exit": n.exit_code,
        "diagnostic": n.stderr.chars().take(320).collect::<String>(),
        "output_chars": n.output.chars().count(),
    })).collect();
    Ok(format!("\n\nOpen run {id}. Node states: {}. Artifact directory: {}. \
        To analyze, read manifest.json and the files of the relevant attempt in this directory.",
        json!(nodes), dir.display()))
}

pub(super) async fn start_chat(
    State(state): State<std::sync::Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let conn = crate::connectors::resolve(&req.harness, None)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if !conn.is_agent || !conn.supports_model || !crate::connectors::steerable(&conn) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "chat needs a resumable agent with model selection".into()));
    }
    if req.prompt.trim().is_empty() || req.model.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "chat needs a prompt and a model".into()));
    }
    // Interpolate once from an input. References typed in a chat message are literal text.
    // By default the chat node carries BOTH built-in planner skills: the
    // `zori-config` cheat-sheet (the flow schema, so the graph the model emits
    // is syntactically real) and `zori-planner` (the propose→confirm→run
    // discipline — phases, when to fan out, harness choice, «never save, never
    // run»). An explicit `skills` list from the UI — including an empty one —
    // overrides the default set, so a plain chat without them is reachable.
    // (skills cloned, not moved: `chat_flow_yaml` below borrows the whole req)
    let skills = req.skills.clone().unwrap_or_else(|| vec!["zori-config".to_string(), "zori-planner".to_string()]);
    // The project folder resolves BEFORE anything is booted: a typo'd path
    // must come back as one clear error, not as a run whose agent quietly
    // spawns somewhere odd.
    let project_dir = match req.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(raw) => Some(
            resolve_project_dir(raw).map_err(|msg| ApiError(StatusCode::BAD_REQUEST, msg))?,
        ),
    };
    // The hard response protocol lives in `out:`, which the runner renders as
    // the closing `# Output` block on every attempt — so it re-arms each turn,
    // and a steered continuation is held to it as firmly as the first reply.
    // It is server-authored, never the model's own words, because a model left
    // to itself in a repo will save and boot its own pipeline. Alongside it we
    // bake a cheap inventory of the harnesses actually installed here, so the
    // model plans against reality (the bug that listed a model no CLI has ever
    // seen). `harness_brief` only checks PATH — it spawns nothing, so it is
    // safe to call on every chat boot.
    let mut out = CHAT_PROTOCOL.to_string();
    if let Some(brief) = crate::proposal::harness_brief() {
        out.push_str("\n\n");
        out.push_str(&brief);
    }
    // The operator's model fleet, when one is saved: the planner proposes
    // `model:` values from the operator's own roster instead of inventing ids
    // (the same class of dead-node bug the harness brief already kills).
    if let Some(brief) = crate::fleet::brief() {
        out.push_str("\n\n");
        out.push_str(&brief);
    }
    // With a project folder, tell the planner where it IS: the process cwd is
    // already there (see `chat_flow_yaml`), and one factual line keeps the
    // model from `cd`-ing around or inventing absolute paths.
    if let Some(dir) = &project_dir {
        out.push_str("\n\nThis session's working directory — ");
        out.push_str(&dir.display().to_string());
        out.push_str(". You are already running in it; open project files by relative paths.");
    }
    let context = planner_run_context(&state, req.attached_run.as_deref())?;
    let yaml = chat_flow_yaml(&req, &skills, &out, project_dir.as_deref(), &context)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let id = boot_flow(state, &yaml, None, Vec::new(), None).await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({"run_id": id})))
}

/// Resolve the operator-typed project folder to an absolute, reachable
/// directory. `~` expands and an existing path canonicalises
/// ([`crate::artifacts::absolute`]); the metadata check is what actually
/// proves the folder is usable, and its error names both the typed value and
/// the resolved one so a typo is obvious in the console.
pub(super) fn resolve_project_dir(raw: &str) -> Result<std::path::PathBuf, String> {
    let abs = crate::artifacts::absolute(std::path::Path::new(raw));
    match std::fs::metadata(&abs) {
        Ok(m) if m.is_dir() => Ok(abs),
        Ok(_) => Err(format!(
            "project folder is not a directory: {raw} (→ {})",
            abs.display()
        )),
        Err(e) => Err(format!(
            "project folder unavailable: {raw} (→ {}): {e}",
            abs.display()
        )),
    }
}

/// The synthetic chat flow's YAML, pure so its shape is directly testable.
/// Without a project folder it is exactly the classic chat flow (no `cwd`, no
/// `workspace` — the task keeps its isolated work dir). With one, the flow
/// carries `cwd:` and the chat task `workspace: flow`: `effective_cwd` then
/// makes the project folder the process cwd on the first run AND on every
/// resume (a steer recomputes it from the same stored flow), and
/// `${project.dir}` / `ZORI_PROJECT_DIR` point at it too.
pub(super) fn chat_flow_yaml(
    req: &ChatRequest,
    skills: &[String],
    out: &str,
    project_dir: Option<&std::path::Path>,
    run_context: &str,
) -> Result<String, serde_yaml::Error> {
    let mut flow = json!({
        "name": "chat", "inputs": { "message": req.prompt },
        "tasks": [{"id": "chat", "harness": req.harness, "model": req.model,
                   "in": "${inputs.message}", "out": out, "skills": skills}]
    });
    let context = format!("{}{run_context}", crate::runner::canvas_prompt(req.graph.clone()));
    if !context.is_empty() {
        flow["inputs"]["context"] = json!(context);
        flow["tasks"][0]["in"] = json!("${inputs.message}${inputs.context}");
    }
    if let Some(dir) = project_dir {
        flow["cwd"] = json!(dir.display().to_string());
        flow["tasks"][0]["workspace"] = json!("flow");
    }
    serde_yaml::to_string(&flow)
}
