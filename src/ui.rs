//! `zori ui` — the interactive console server.
//!
//! Turns zori's one-shot runner into a long-lived, collaborative console: the
//! server owns one or more [`TrackedRun`]s, exposes each as a live [`Emitter`]
//! (authoritative state + a broadcast of whole snapshots), persists every
//! snapshot to `state.json`, and offers HTTP endpoints to read a run, watch it
//! over SSE, steer a finished node (resume its harness session with a human
//! turn), and re-run nodes. The browser SPA is a pure function of the snapshot.
//!
//! Endpoints (JSON in/out unless noted):
//!   GET  /                                  the SPA (embedded HTML)
//!   GET  /api/runs                          list runs (id, name, status, …)
//!   POST /api/runs   {path, inputs?}        boot a flow file, returns run_id
//!   GET  /api/runs/:id                      the full RunState snapshot
//!   GET  /api/runs/:id/events               SSE stream of RunState snapshots
//!   GET  /api/runs/:id/transcript/:node     a node's turn list
//!   POST /api/runs/:id/steer  {node,text}   resume a Done node with a message
//!   POST /api/runs/:id/answer {node,text}   answer a Waiting node's parked question
//!   POST /api/runs/:id/rerun  {node}        re-run a node from scratch
//!   POST /api/runs/:id/rerun-stale          re-run every Stale node
//!
//! Flow-builder endpoints (the "Build" tab of the SPA — click a pipeline
//! together instead of hand-writing YAML; the server is the single authority
//! on what is a valid flow, the browser never re-implements validation):
//!   GET  /api/harnesses                     built-in harness catalog (id, model support, cmd)
//!   GET  /api/harnesses/:id/models          live model list for that harness (?refresh=1)
//!   GET  /api/flows                         list *.yml under the flows dir
//!   GET  /api/flows/open?path=…             read + parse a flow file → {yaml, flow}
//!   POST /api/flows/validate {flow}         check a spec → {ok, levels} or {ok:false, error}
//!   POST /api/flows/save   {flow, path?}    validate, write YAML → {path, yaml}
//!   POST /api/flows/boot   {flow, path?, inputs?}  save then boot → {run_id, path}
//!
//! The `state.json` on disk is the same shape as `GET /api/runs/:id`; a restart
//! can reload a finished run's transcript from it (in-progress children do not
//! survive a restart and come back `Stale`).

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::header::{HOST, ORIGIN};
use axum::http::{Method, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{Flow, Task};
use crate::runner::{plan, TrackedRun};
use crate::state::{Emitter, RunState, TaskStatus};

/// The single-file SPA, embedded so the binary has no runtime asset dir.
const INDEX_HTML: &str = include_str!("../web/index.html");

/// One managed run: the observable state, the (immutable) driver that can
/// re-execute it, and a lock serialising user-initiated operations (steer,
/// rerun) so two clicks on the same run can't interleave and fight.
struct ManagedRun {
    emitter: Emitter,
    tracked: TrackedRun,
    op_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Model-catalog cache key: the harness's resolved identity (review F11) —
/// `(id, resolved argv, models_cmd override)`; see `AppState::models_cache`.
type ModelsCacheKey = (String, Vec<String>, Vec<String>);

/// Shared server state, cheaply cloned into every handler.
pub struct AppState {
    runs: Mutex<HashMap<String, ManagedRun>>,
    /// Root directory under which each run gets `<run-id>/{state.json,sessions}`.
    runs_root: PathBuf,
    /// Directory the flow builder saves to (`zori ui --flows-dir`). Saved and
    /// booted-from-spec flows live here; the builder may still *open* a flow
    /// from any readable path, the same trust level as `POST /api/runs`.
    flows_root: PathBuf,
    /// Per-harness model catalogs with their fetch time (epoch secs), so the
    /// builder's dropdown does not re-spawn a CLI on every block click.
    /// `?refresh=1` bypasses it; see [`crate::models`].
    /// Keyed (review F11) by the harness id *and* the effective commands the
    /// list was derived from — the resolved argv and any winning `models_cmd`
    /// override. A bare id would let two flows that override the same harness
    /// id with different listing commands share a stale cached list.
    models_cache: Mutex<HashMap<ModelsCacheKey, (u64, crate::models::ModelList)>>,
    /// Review F10: the Host/Origin allow-set derived from the bound address
    /// at startup; see [`boundary_guard`].
    allowed_hosts: AllowedHosts,
}

/// Review F10: the `Host`/`Origin` values this console accepts. The server
/// has no auth and its endpoints are powerful (`POST /api/runs/:id/rerun`
/// runs arbitrary CLIs, `POST /api/flows/save` writes YAML), and it binds
/// loopback by default — a malicious local page could DNS-rebind
/// `attacker.com` to 127.0.0.1 and drive the console from the victim's
/// browser. Browsers send `Host` and `Origin` faithfully, so checking both
/// closes that hole without introducing a CORS requirement: absent `Origin`
/// (curl, CLI, same-origin navigations) keeps working; only clearly foreign
/// origins on state-changing (POST) requests are rejected.
///
/// Built once from the bound [`SocketAddr`]: a loopback bind accepts the
/// loopback names (with any port); an explicit non-loopback bind *additionally*
/// accepts exactly that address (any port). An arbitrary `Host` is never
/// accepted — in particular a `--bind 0.0.0.0` wildcard does not become a
/// wildcard Host; bind a concrete address to serve a LAN.
#[derive(Clone, Debug)]
struct AllowedHosts {
    /// Lowercased host names, port already stripped, that may appear in
    /// `Host` (and, on POST, in `Origin`).
    names: Vec<String>,
}

const LOOPBACK_NAMES: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

impl AllowedHosts {
    /// Derive the allow-set from what the server actually bound. Loopback
    /// names are always in the set (even a LAN bind should still answer to
    /// `localhost` on the same machine); a non-loopback bind adds exactly
    /// its own address.
    fn for_bind(bind: SocketAddr) -> Self {
        let mut names: Vec<String> = LOOPBACK_NAMES.iter().map(|s| s.to_string()).collect();
        names.push(bind.ip().to_string());
        names.sort();
        names.dedup();
        AllowedHosts { names }
    }

    /// Test a raw `Host` header value ("127.0.0.1:8787", "[::1]:9000", …).
    fn allows_host(&self, host: &str) -> bool {
        self.names.iter().any(|n| n == &normalize_host(host))
    }

    /// Test a raw `Origin` header value ("http://127.0.0.1:8787"). Only ever
    /// called on POST; `null` (sandboxed/file contexts) is foreign and fails.
    fn allows_origin(&self, origin: &str) -> bool {
        let Some(rest) = origin.split_once("://").map(|(_, r)| r) else {
            return false;
        };
        let authority = rest.split('/').next().unwrap_or("");
        self.names.iter().any(|n| n == &normalize_host(authority))
    }
}

/// Lowercase a host token and strip its port. Handles the bracketed IPv6
/// forms browsers actually send (`[::1]:8787`); an unbracketed multi-colon
/// value is a bare IPv6 literal and has no port to strip.
fn normalize_host(value: &str) -> String {
    let v = value.trim();
    let (name, maybe_port) = match v.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (v, None),
    };
    let bracketed = v.starts_with('[');
    let strip = match maybe_port {
        Some(p) => {
            (bracketed && name.ends_with(']') || !bracketed && !name.contains(':'))
                && p.parse::<u16>().is_ok()
        }
        None => false,
    };
    let name = if strip { name } else { v };
    name.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase()
}

/// Review F10: outermost layer on every route. A `Host` we did not derive
/// from the bind is a 400 (the request is not addressed to us at all —
/// typically a rebinding attempt); a foreign `Origin` on a state-changing
/// POST is a 403. Reads are never Origin-checked, so the SSE event stream
/// (`GET /api/runs/:id/events`) and normal navigation are unaffected.
async fn boundary_guard(
    State(state): State<std::sync::Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match host {
        Some(h) if state.allowed_hosts.allows_host(&h) => {}
        Some(_) => return deny(StatusCode::BAD_REQUEST, "Host not allowed"),
        None => return deny(StatusCode::BAD_REQUEST, "missing Host header"),
    }
    if req.method() == Method::POST {
        if let Some(origin) = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok()) {
            if !state.allowed_hosts.allows_origin(origin) {
                return deny(StatusCode::FORBIDDEN, "Origin not allowed");
            }
        }
    }
    next.run(req).await
}

/// Plain-text rejection with a short reason, per the review: the body must
/// say *why* (a bad Host is a misconfiguration, not a mystery 400).
fn deny(status: StatusCode, reason: &'static str) -> axum::response::Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        reason,
    )
        .into_response()
}

/// A boot request: a flow file path plus optional `--input` overrides.
#[derive(Deserialize)]
struct BootReq {
    path: String,
    #[serde(default)]
    inputs: HashMap<String, String>,
}

#[derive(Deserialize)]
struct AnswerReq {
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
struct SteerReq {
    node: String,
    text: String,
    /// The live canvas graph, sent with the next turn so the planner builds on
    /// the operator's edits (a moved node, a changed model, a hand-wired dep)
    /// instead of the graph still held in the session.
    #[serde(default)]
    graph: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RerunReq {
    node: String,
    /// Optional corrected task spec from the canvas ("rerun with fix"): the
    /// node re-runs with these leaf fields instead of the boot-time ones, so
    /// e.g. a typo'd model name is fixed in Конфиг and the rerun actually uses
    /// the correction. Validated by [`validate_task_edit`] before applying.
    #[serde(default)]
    task: Option<serde_json::Value>,
}

/// A node that may be re-run from scratch. `Done` and `Stale` were always
/// re-runnable; a `Failed` node is the *main* reason to rerun (fix the
/// config, try again), and a `Skipped` node (whose producer died) becomes
/// reachable once its deps are done — rerunning it is the operator's call,
/// not the server's. `Pending`/`Running` are not (the run's own driver owns
/// them), so the UI cannot double-drive a node mid-flight.
fn rerunnable(status: TaskStatus) -> bool {
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
fn validate_task_edit(
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
fn slug(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        s.push(if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' });
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "run".to_string() } else { s }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bind a console server and serve it. `initial_flow`, when given, is booted as
/// the first run so the browser has something to show immediately. Prints the
/// URL (and the first run id) to stdout. `flows_root` is the directory the
/// flow builder saves its YAML to (created if missing).
pub async fn start(
    bind: SocketAddr,
    runs_root: PathBuf,
    flows_root: PathBuf,
    initial_flow: Option<PathBuf>,
    initial_inputs: Vec<(String, String)>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&runs_root)
        .with_context(|| format!("creating runs root {}", runs_root.display()))?;
    std::fs::create_dir_all(&flows_root)
        .with_context(|| format!("creating flows root {}", flows_root.display()))?;

    let state = std::sync::Arc::new(AppState {
        runs: Mutex::new(HashMap::new()),
        runs_root: runs_root.clone(),
        flows_root: flows_root.clone(),
        models_cache: Mutex::new(HashMap::new()),
        allowed_hosts: AllowedHosts::for_bind(bind),
    });

    // Reload any runs left on disk by a previous server (reopen-across-restart):
    // their transcripts and session handles come back; children that were still
    // running come back `Stale` for the human to re-run.
    match load_existing_runs(state.clone(), &runs_root).await {
        Ok(ids) if !ids.is_empty() => println!("reloaded {} prior run(s): {}", ids.len(), ids.join(", ")),
        Ok(_) => {}
        Err(e) => eprintln!("warning: could not scan for prior runs: {e:#}"),
    }

    if let Some(path) = &initial_flow {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading flow {}", path.display()))?;
        match boot_flow(state.clone(), &text, Some(path.clone()), initial_inputs, None).await {
            Ok(id) => println!("booted run '{id}' from {}", path.display()),
            Err(e) => eprintln!("warning: could not boot initial flow: {e:#}"),
        }
    }

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!("zori ui on http://{bind}  (runs under {})", runs_root.display());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn build_router(state: std::sync::Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/chats", post(start_chat))
        .route("/api/runs", get(list_runs).post(boot))
        .route("/api/runs/:id", get(get_run))
        .route("/api/runs/:id/events", get(events))
        .route("/api/runs/:id/transcript/:node", get(transcript))
        .route("/api/runs/:id/steer", post(steer))
        .route("/api/runs/:id/answer", post(answer))
        .route("/api/runs/:id/rerun", post(rerun))
        .route("/api/runs/:id/rerun-stale", post(rerun_stale))
        .route("/api/runs/:id/run-proposal", post(run_proposal))
        .route("/api/runs/:id/reject-proposal", post(reject_proposal))
        .route("/api/catalog", get(planner_catalog))
        .route("/api/skills", get(list_skills))
        .route("/api/harnesses", get(harnesses))
        .route("/api/harnesses/:id/models", get(harness_models))
        .route("/api/flows", get(list_flows))
        .route("/api/flows/open", get(open_flow))
        .route("/api/flows/validate", post(validate_flow))
        .route("/api/flows/save", post(save_flow))
        .route("/api/flows/boot", post(boot_spec))
        .with_state(state.clone())
        // Review F10: the console has no auth and binds loopback by default,
        // so the Host/Origin boundary check runs on *every* route — inside
        // the router's own state so it can read the allow-set — as the outer
        // layer, before any handler sees the request.
        .layer(axum::middleware::from_fn_with_state(state, boundary_guard))
}

// --------------------------------------------------------------------------
// Handlers
// --------------------------------------------------------------------------

async fn index() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML)
}

#[derive(Serialize)]
struct RunSummary {
    chat_title: Option<String>,
    run_id: String,
    flow_name: String,
    status: String,
    updated_at: u64,
    nodes: usize,
}

/// Lock with explicit poison recovery: a poisoned mutex is entered anyway,
/// loudly. Policy lives here so it is testable without the full registry.
fn lock_recovering<'a, T>(m: &'a Mutex<T>, what: &'a str) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|poisoned| {
        eprintln!("  [warning] {what} mutex poisoned by a panicking handler — recovering");
        poisoned.into_inner()
    })
}

/// Lock the run registry with explicit poison recovery (review, minor).
/// The registry's payload is a map of `Arc` handles: a panic that unwinds
/// while the guard is held cannot leave the *map* torn (nothing under this
/// lock computes anything but clones and inserts), and every later handler —
/// `lookup`, boot registration, `list_runs` itself — used to die on the
/// poisoned `unwrap()`, one run's panic turning the whole console off. The
/// per-run state stays exactly as it was; recovery is the honest verdict.
/// The recovery is loud: the warning names what happened, so the poison is
/// never invisible. (The other poisoning paths are removed at the source:
/// `list_runs` no longer holds this guard while touching per-run locks.)
fn runs_guard(runs: &Mutex<HashMap<String, ManagedRun>>) -> std::sync::MutexGuard<'_, HashMap<String, ManagedRun>> {
    lock_recovering(runs, "run registry")
}

async fn list_runs(State(state): State<std::sync::Arc<AppState>>) -> Json<Vec<RunSummary>> {
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

#[derive(Deserialize)]
struct ChatRequest {
    harness: String,
    model: String,
    prompt: String,
    /// Project folder for the planner (the builder's «директория запуска»).
    /// Absent/empty → the classic chat shape: an isolated per-task work dir.
    /// When given, the chat flow gets `cwd:` + `workspace: flow`, so the agent
    /// process runs IN the folder from the very first message and every
    /// continuation resumes there.
    #[serde(default)]
    cwd: Option<String>,
    /// Skills to attach to the chat node. Absent → the default set (see
    /// [`start_chat`], which turns the planner into a config author by
    /// attaching the built-in `zori-config` cheat-sheet). An explicit list is
    /// honoured verbatim — including an empty one, to chat without skills.
    #[serde(default)]
    skills: Option<Vec<String>>,
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
Ты — планировщик пайплайнов zori. Соблюдай протокол ответа буквально.

ФАЗЫ. Сначала обсуждай план обычным текстом: шаги, кто их делает, что откуда \
берётся, где параллельность. YAML показывай только когда оператор согласовал состав.

Границы (не нарушай ни при каких условиях):
- НИЧЕГО не сохраняй: не пиши flow.yaml, не правь файлы проекта. Граф на диск \
кладёт оператор своим запуском.
- НИЧЕГО не запускай: не гоняй пайплайны, CLI, тесты, `zori run`. Запускает \
только оператор кнопкой.
- Каталог харнессов не исследуй запуском CLI — реальное состояние уже приведено ниже.

ФОРМАТ ГРАФА. Когда состав согласован, выдай ПОЛНЫЙ граф одним блоком, помеченным \
`zori-flow` (тройной обратный апостроф + слово zori-flow, внутри только YAML). Один \
такой блок = одно предложение; целиком, не фрагментом и не диффом. Больше ничего \
после блока не добавляй. Блок без этой метки zori не увидит и не предложит запустить.

КАНВАС. Валидный граф из блока zori сам ляжет на канвас рядом с чатом — в файл его \
не пиши. Оператор правит его руками: двигает узлы, меняет модель и параметры, \
дорисовывает зависимости, а запускает кнопкой — запускается ровно то, что на канвасе. \
Если в следующем ходе тебе прислали блок «Текущий граф на канвасе» — это граф перед \
оператором, возможно отредактированный им вручную. Строй новый граф от него и сохрани \
правки оператора (модели, параметры, зависимости); координаты pos не воспроизводи — \
это только раскладка. Ответь снова ПОЛНЫМ графом одним блоком zori-flow, как выше.";

async fn start_chat(
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
    // With a project folder, tell the planner where it IS: the process cwd is
    // already there (see `chat_flow_yaml`), and one factual line keeps the
    // model from `cd`-ing around or inventing absolute paths.
    if let Some(dir) = &project_dir {
        out.push_str("\n\nРабочий каталог этой сессии — ");
        out.push_str(&dir.display().to_string());
        out.push_str(". Ты уже запущен в нём; файлы проекта открывай по относительным путям.");
    }
    let yaml = chat_flow_yaml(&req, &skills, &out, project_dir.as_deref())
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
fn resolve_project_dir(raw: &str) -> Result<std::path::PathBuf, String> {
    let abs = crate::artifacts::absolute(std::path::Path::new(raw));
    match std::fs::metadata(&abs) {
        Ok(m) if m.is_dir() => Ok(abs),
        Ok(_) => Err(format!(
            "проектная папка не каталог: {raw} (→ {})",
            abs.display()
        )),
        Err(e) => Err(format!(
            "проектная папка недоступна: {raw} (→ {}): {e}",
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
fn chat_flow_yaml(
    req: &ChatRequest,
    skills: &[String],
    out: &str,
    project_dir: Option<&std::path::Path>,
) -> Result<String, serde_yaml::Error> {
    let mut flow = json!({
        "name": "chat", "inputs": { "message": req.prompt },
        "tasks": [{"id": "chat", "harness": req.harness, "model": req.model,
                   "in": "${inputs.message}", "out": out, "skills": skills}]
    });
    if let Some(dir) = project_dir {
        flow["cwd"] = json!(dir.display().to_string());
        flow["tasks"][0]["workspace"] = json!("flow");
    }
    serde_yaml::to_string(&flow)
}

async fn boot(
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

async fn get_run(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<RunState>, ApiError> {
    let m = lookup(&state, &id)?;
    Ok(Json(m.emitter.snapshot()))
}

async fn events(
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

async fn transcript(
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

async fn steer(
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
    let emitter = m.emitter.clone();
    claim_node(&emitter, &req.node, &[TaskStatus::Done], false)
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
        if let Err(e) = tracked.steer(&emitter, &node, &text, graph).await {
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
async fn answer(
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

async fn rerun(
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
    // does) and while the first pass still owes work to a descendant.
    claim_node(&emitter, &req.node, from, true)
        .map_err(|msg| ApiError(StatusCode::CONFLICT, msg))?;

    // Claim won: only now apply the spec mutation. The in-memory parts are
    // infallible here (the edit was validated above); persistence is
    // best-effort and logged, never swallowed — the in-memory rerun must not
    // hinge on a disk write succeeding.
    if let Some(incoming) = incoming {
        m.tracked.override_task(incoming.clone());
        // Keep the observable meta honest (the Чат head shows harness·model).
        m.emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(&req.node) {
                n.harness = incoming.harness.clone();
                n.model = incoming.model.clone();
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
        if let Err(e) = handle.await {
            eprintln!("rerun driver for '{watch_node}' died: {e:?}");
            release_claim(&watch, &watch_node, prev, &format!("rerun driver died: {e}"));
        }
    });
    Ok(Json(ack))
}

async fn rerun_stale(
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

/// A built-in harness as the builder sees it: enough to render a select box
/// and decide whether a `model:` field makes sense for it.
#[derive(Serialize)]
struct HarnessInfo {
    supports_resume: bool,
    id: String,
    supports_model: bool,
    command: String,
    prompt_via: &'static str,
    is_agent: bool,
}

async fn planner_catalog() -> Json<crate::catalog::Catalog> {
    Json(crate::catalog::discover(&Default::default()).await)
}

/// A skill entry for the picker UIs: name, one-line description, and where it
/// came from (so a store file and a built-in look different). The body is
/// omitted — the console fetches it only when a skill is actually inspected.
#[derive(Serialize)]
struct SkillInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    source: &'static str,
}

/// The skill list the node config-bar and the start-chat picker render. Lists
/// what the operator can reach right now (project `zori-skills/`, global
/// `~/.zori/skills`, built-ins); a flow's extra `skills_dirs` are honoured at
/// run time by the runner, not here.
async fn list_skills() -> Json<Vec<SkillInfo>> {
    let all = crate::skills::Resolver::standard().list();
    Json(
        all.into_iter()
            .map(|s| SkillInfo {
                name: s.name,
                description: s.description,
                source: match s.source {
                    crate::skills::SkillSource::BuiltIn => "built-in",
                    crate::skills::SkillSource::File(_) => "file",
                },
            })
            .collect(),
    )
}

async fn harnesses() -> Json<Vec<HarnessInfo>> {
    let mut out = Vec::new();
    for id in crate::connectors::builtin_ids() {
        if let Ok(c) = crate::connectors::resolve(id, None) {
            out.push(HarnessInfo {
                supports_resume: crate::connectors::steerable(&c),
                id: c.id,
                supports_model: c.supports_model,
                command: c.command.join(" "),
                prompt_via: match c.prompt_via {
                    crate::config::PromptVia::Arg => "arg",
                    crate::config::PromptVia::Stdin => "stdin",
                    crate::config::PromptVia::File => "file",
                },
                is_agent: c.is_agent,
            });
        }
    }
    Json(out)
}

#[derive(Deserialize)]
struct OpenReq {
    path: String,
}

/// Query for `GET /api/harnesses/:id/models`: `?refresh=1` bypasses the TTL
/// cache (the builder's ↻ button).
#[derive(Deserialize)]
struct ModelsQuery {
    #[serde(default)]
    refresh: Option<String>,
}

/// How long a fetched model catalog stays fresh without `?refresh=1`. Long
/// enough that clicking through ten blocks costs one subprocess, short enough
/// that a just-changed provider config shows up on the next opening anyway.
const MODELS_TTL_SECS: u64 = 60;

/// The live model list for one harness — pulled from the CLI itself (see
/// [`crate::models`]), never a hardcoded table. 404 for an unknown harness,
/// 502 with the CLI's own complaint when discovery fails, so the builder can
/// say *why* a dropdown is empty instead of showing nothing.
async fn harness_models(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Query(q): Query<ModelsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let force = matches!(q.refresh.as_deref(), Some("1") | Some("true"));
    let now = epoch_secs();
    // The cache is keyed by the harness's resolved identity (review F11's
    // half-landed key change, completed mechanically): built-ins share one
    // entry per id, a future override-aware caller gets its own. This
    // endpoint still resolves without a flow, so the built-in table is all
    // we know here.
    let conn = crate::connectors::resolve(&id, None)
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    let cache_key = (id.clone(), conn.extra_args.clone(), Vec::new());
    if !force {
        let fresh = {
            let cache = state.models_cache.lock().unwrap();
            cache
                .get(&cache_key)
                .filter(|(at, _)| now.saturating_sub(*at) <= MODELS_TTL_SECS)
                .map(|(_, list)| serde_json::to_value(list).ok())
        };
        if let Some(Some(v)) = fresh {
            return Ok(Json(v));
        }
    }
    let list = crate::models::list_models(&conn, None)
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let out = serde_json::to_value(&list)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.models_cache.lock().unwrap().insert(cache_key, (now, list));
    Ok(Json(out))
}
/// List flow files in the builder's flows dir, newest first.
async fn list_flows(
    State(state): State<std::sync::Arc<AppState>>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&state.flows_root) {
        Ok(e) => e,
        // A missing dir just means nothing saved yet.
        Err(_) => return Ok(Json(out)),
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let is_yaml = p
            .extension()
            .map(|x| x == "yml" || x == "yaml")
            .unwrap_or(false);
        if !p.is_file() || !is_yaml {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        out.push(json!({
            "name": p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            "path": p.to_string_lossy(),
            "modified": modified,
        }));
    }
    out.sort_by_key(|v| std::cmp::Reverse(v.get("modified").and_then(|x| x.as_u64()).unwrap_or(0)));
    Ok(Json(out))
}

/// Read and parse a flow file for editing. Reading any path is the same trust
/// level as `POST /api/runs {path}` (boot from file): the console is local and
/// unauthenticated, so it is an operator tool, not a public service.
async fn open_flow(Query(q): Query<OpenReq>) -> Result<Json<serde_json::Value>, ApiError> {
    let text = std::fs::read_to_string(&q.path)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("reading {}: {e}", q.path)))?;
    let flow: Flow = serde_yaml::from_str(&text)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("parsing flow: {e}")))?;
    let value = serde_json::to_value(&flow)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "yaml": text, "flow": value })))
}

#[derive(Deserialize)]
struct SpecReq {
    flow: serde_json::Value,
}

#[derive(Deserialize)]
struct SaveReq {
    flow: serde_json::Value,
    /// Optional file name (not a path — writing is confined to the flows dir).
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct BootSpecReq {
    flow: serde_json::Value,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    inputs: HashMap<String, String>,
    /// Run only this task's upstream closure (it + everything it, and they,
    /// depend on) instead of the whole flow — the canvas ▶ button's first
    /// launch, where only this block was asked to run. `None` runs everything.
    #[serde(default)]
    focus: Option<String>,
}

/// Validate a spec the way the runner will, returning its execution levels on
/// success. Shared by /validate, /save and /boot so all three agree.
fn validate_spec(flow: &Flow) -> Result<crate::dag::ExecutionPlan, String> {
    validate_flow_spec(flow).map_err(|errs| errs.join("; "))
}

/// The builder's checks over a [`Flow`], collecting *every* problem into a
/// list rather than bailing at the first — the shape the proposal card wants
/// (show the operator all of what's wrong at once). `validate_spec` is the
/// single-string wrapper the builder's HTTP handlers use; both share this body
/// so a graph the chat can propose is judged exactly as a graph Build accepts.
/// Public so [`crate::proposal::validate`] is the *same* validator, not a
/// lookalike that drifts.
pub fn validate_flow_spec(flow: &Flow) -> Result<crate::dag::ExecutionPlan, Vec<String>> {
    use crate::connectors;
    let mut errors: Vec<String> = Vec::new();
    if flow.name.trim().is_empty() {
        errors.push("flow needs a name".into());
    }
    if flow.tasks.is_empty() {
        errors.push("flow needs at least one task".into());
    }
    for t in &flow.tasks {
        if t.id.trim().is_empty() {
            errors.push("every task needs an id".into());
            continue;
        }
        if t.in_field.trim().is_empty() && t.depends_on.is_empty() {
            errors.push(format!("task '{}': prompt (in) is empty (and no dependencies feed it)", t.id));
        }
        match connectors::resolve(&t.harness, flow.harnesses.get(&t.harness)) {
            Ok(conn) => {
                // Same rule connectors::plan enforces at spawn time; catching
                // it in the builder beats a run that dies at the first task.
                if t.model.is_some() && !conn.supports_model {
                    errors.push(format!(
                        "task '{}': harness '{}' cannot set a model per run — drop the model or add a model_flag override",
                        t.id, t.harness
                    ));
                }
            }
            Err(e) => errors.push(format!("task '{}': {e:#}", t.id)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut normalized = flow.clone();
    merge_ref_deps(&mut normalized);
    crate::runner::plan(&normalized).map_err(|e| vec![format!("{e:#}")])
}

/// Extract `${steps.<id>.output}` references from text.
fn step_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("${steps.") {
        rest = &rest[i + "${steps.".len()..];
        let Some(j) = rest.find('}') else { break };
        let key = &rest[..j];
        if let Some(id) = key.strip_suffix(".output") {
            if !id.is_empty() && !out.iter().any(|x| x == id) {
                out.push(id.to_string());
            }
        }
        rest = &rest[j + 1..];
    }
    out
}

/// Extract `${inputs.<key>}` references from text — the sibling of
/// [`step_refs`]. Mirrors the shape `runner::interpolate` resolves, so what we
/// flag here is exactly what would fail at run time.
fn input_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("${inputs.") {
        rest = &rest[i + "${inputs.".len()..];
        let Some(j) = rest.find('}') else { break };
        let key = &rest[..j];
        if !key.is_empty() && !out.iter().any(|x| x == key) {
            out.push(key.to_string());
        }
        rest = &rest[j + 1..];
    }
    out
}

/// `${inputs.k}` used by some task but supplied by neither the flow's `inputs`
/// block nor the boot request. The runner turns each of these into a hard
/// `unknown reference` failure of the task that uses it, so booting a flow with
/// one guarantees a dead run — which is what happened when the builder lost a
/// multi-line Inputs field. Naming the keys up front is both the gate and the
/// explanation. `${steps.*}` and `${env.*}` resolve later and are not checked.
fn missing_inputs(flow: &Flow, extra: &[String]) -> Vec<String> {
    missing_inputs_filtered(flow, extra, None)
}

/// The `${inputs.k}` keys a proposed graph uses that the flow's own `inputs`
/// block never declares — the proposal card's yellow notes. Unlike a boot, a
/// proposal carries no run-time `--input` values, so we scan against only the
/// flow's declared `inputs`. A key here is not fatal: the operator may well
/// mean to supply it when they hit Run. Public so [`crate::proposal`] shares
/// the exact same reference scan the boot gate uses.
pub fn missing_input_warnings(flow: &Flow) -> Vec<String> {
    missing_inputs_filtered(flow, &[], None)
}

/// Same scan as [`missing_inputs`], but when `only` is given it inspects just
/// the tasks whose id is in the set. A focused run must not be blocked by an
/// `${inputs.k}` in a node it will never execute — only the closure it boots
/// needs its inputs satisfied.
fn missing_inputs_filtered(
    flow: &Flow,
    extra: &[String],
    only: Option<&std::collections::HashSet<String>>,
) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    for t in &flow.tasks {
        if let Some(set) = only {
            if !set.contains(&t.id) {
                continue;
            }
        }
        for text in [t.in_text().to_string(), t.out.clone().unwrap_or_default()] {
            for k in input_refs(&text) {
                let known = flow.inputs.contains_key(&k) || extra.iter().any(|e| e == &k);
                if !known && !missing.iter().any(|m| m == &k) {
                    missing.push(k);
                }
            }
        }
    }
    missing
}

/// The upstream closure of a task: itself plus every task it (transitively)
/// depends on — the set a focused run registers and executes. Deps come from
/// `depends_on`, which [`merge_ref_deps`] has already folded `${steps…}`
/// references into for any flow the builder saved, and `plan()` builds its
/// levels from `depends_on` alone — so this closure is exactly the plan's view
/// of the node's ancestry, and restricting the plan to it keeps every level
/// valid (a downward-closed subset can't introduce a missing dependency). A
/// name matching no task is a caller error: the canvas asked to run a block
/// that isn't in the flow.
fn upstream_closure(flow: &Flow, id: &str) -> Result<std::collections::HashSet<String>, String> {
    if !flow.tasks.iter().any(|t| t.id == id) {
        return Err(format!("no task '{id}' to run"));
    }
    let mut set = std::collections::HashSet::new();
    let mut frontier = vec![id.to_string()];
    while let Some(cur) = frontier.pop() {
        if !set.insert(cur.clone()) {
            continue;
        }
        if let Some(t) = flow.tasks.iter().find(|t| t.id == cur) {
            for d in &t.depends_on {
                if !set.contains(d) {
                    frontier.push(d.clone());
                }
            }
        }
    }
    Ok(set)
}

/// The one wording for a refused boot, shared by `boot_flow` (all boot paths)
/// and `boot_spec` (which refuses before even writing the YAML).
fn missing_inputs_error(missing: &[String]) -> String {    let list = missing
        .iter()
        .map(|k| format!("${{inputs.{k}}}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{list}: no value given — fill the Inputs field, or pass --input {}=...", missing[0])
}

/// Auto-wire the DAG: any `${steps.<id>.output}` referenced in a task's `in`
/// or `out` implies a dependency on `<id>`, so edges in the builder's preview
/// always match data flow even if the operator forgot to tick the box. A
/// self-reference is left in place — it becomes a cycle the plan rejects with
/// a clear message, rather than being silently dropped.
fn merge_ref_deps(flow: &mut Flow) {
    for t in &mut flow.tasks {
        let mut refs: Vec<String> = step_refs(&t.in_field);
        if let Some(o) = &t.out {
            for r in step_refs(o) {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
        for r in refs {
            if !t.depends_on.contains(&r) {
                t.depends_on.push(r);
            }
        }
        // Preserve order, drop dupes.
        let mut seen = std::collections::HashSet::new();
        t.depends_on.retain(|d| seen.insert(d.clone()));
    }
}

/// Serialise a spec to the YAML we write to disk, with ref-implied deps
/// folded in so the file on disk matches the validated plan.
fn spec_to_yaml(flow: &Flow) -> Result<String, ApiError> {
    let mut normalized = flow.clone();
    merge_ref_deps(&mut normalized);
    let mut yaml = serde_yaml::to_string(&normalized)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("serialising flow: {e}")))?;
    // Small header so a saved file is recognisable when reopened later.
    yaml = format!("# zori flow — edited in zori ui (Build)\n{yaml}");
    Ok(yaml)
}

/// Write flow YAML into the flows dir, atomically claiming a name.
/// `name` (or the flow's own name) is slugified; an existing file gets an
/// epoch suffix rather than being clobbered, so "Save" never silently eats
/// an earlier draft.
///
/// Review M8: the old shape picked the path with `candidate.exists()` and
/// then wrote it with a plain `write` — two concurrent saves (or an
/// unrelated file appearing in between) overwrote each other silently.
/// `create_new` claims the name in the same step as the open: exactly one
/// writer wins; the loser retries with the epoch suffix, then a counter,
/// so a same-second double save still yields two distinct files.
async fn write_flow_file(
    state: &AppState,
    flow: &Flow,
    name: Option<&str>,
    yaml: &str,
) -> Result<PathBuf, ApiError> {
    let base = slug(name.unwrap_or(&flow.name));
    claim_flow_file(&state.flows_root, &base, yaml).await
}

/// The atomic core of [`write_flow_file`], split out so tests can drive it
/// with a temp dir instead of a full [`AppState`].
async fn claim_flow_file(flows_root: &Path, base: &str, yaml: &str) -> Result<PathBuf, ApiError> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(flows_root)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("creating {}: {e}", flows_root.display())))?;
    let mut candidate = flows_root.join(format!("{base}.yml"));
    let mut attempt: u32 = 0;
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(mut f) => {
                // Tokio may queue a file write. Finish it before handing the path to the runner.
                let written = async {
                    f.write_all(yaml.as_bytes()).await?;
                    f.flush().await
                }.await;
                if let Err(e) = written {
                    let path = candidate.clone();
                    drop(f);
                    // The claimed file holds a partial write; it is unusable,
                    // and leaving it would poison the name for the retry.
                    let _ = tokio::fs::remove_file(&path).await;
                    return Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("writing {}: {e}", path.display())));
                }
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt += 1;
                candidate = if attempt == 1 {
                    flows_root.join(format!("{base}-{}.yml", epoch_secs()))
                } else {
                    flows_root.join(format!("{base}-{}-{attempt}.yml", epoch_secs()))
                };
                if attempt > 1000 {
                    return Err(ApiError(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("cannot claim a flow file name for '{base}': {attempt} collisions"),
                    ));
                }
            }
            Err(e) => {
                return Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("writing {}: {e}", candidate.display())));
            }
        }
    }
}

async fn validate_flow(Json(req): Json<SpecReq>) -> Result<Json<serde_json::Value>, ApiError> {
    let flow = parse_spec(req.flow)?;
    match validate_spec(&flow) {
        Ok(plan) => {
            // Include the YAML the save would write, so the builder's preview
            // is the server's output rather than a second client-side writer.
            let yaml = spec_to_yaml(&flow)?;
            // A validate request carries no boot inputs, so every `${inputs.k}`
            // is reported here — the editor shows it yellow: legitimate if the
            // operator means to pass `--input`/Inputs at run time, fatal if not.
            let warnings = missing_inputs(&flow, &[]);
            Ok(Json(json!({ "ok": true, "levels": plan.levels, "yaml": yaml, "warnings": warnings })))
        }
        Err(error) => Ok(Json(json!({ "ok": false, "error": error }))),
    }
}

async fn save_flow(
    State(state): State<std::sync::Arc<AppState>>,
    Json(req): Json<SaveReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let flow = parse_spec(req.flow)?;
    validate_spec(&flow).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    // No inputs gate here: saving a flow that uses ${inputs.k} is legitimate —
    // the value may arrive via --input at boot time. The gate belongs to
    // boot_spec, where the actual inputs for the run are known.
    let yaml = spec_to_yaml(&flow)?;
    let path = write_flow_file(&state, &flow, req.name.as_deref(), &yaml).await?;
    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "yaml": yaml,
    })))
}

/// The launch request for a chat proposal: which node carries it, the hash
/// that was on screen when the operator clicked Run, and any `${inputs.k}`
/// values supplied at click time. The hash is a swap guard — the server
/// re-checks it, so a graph validated a moment ago cannot be quietly replaced
/// by a newer one between the render and the click.
#[derive(serde::Deserialize)]
struct RunProposalReq {
    node: String,
    hash: String,
    #[serde(default)]
    inputs: HashMap<String, String>,
    /// The live canvas graph, sent by the card's ▶ so what runs is exactly what
    /// the operator sees and hand-edited (a moved node, a changed model, a
    /// wired dependency), not the YAML the agent first proposed. When present
    /// it is validated the same way the canvas ▶ Run validates, and booted
    /// instead of the stored `raw`; the hash/CAS guard still pins it to the
    /// proposal the operator confirmed. Absent (an older client) → `raw` boots.
    #[serde(default)]
    flow: Option<serde_json::Value>,
}

/// Launch the flow a chat node *proposed* and the operator confirmed — the only
/// way a proposed graph ever executes. The model has no power to run anything;
/// this endpoint (bound to the card's Run button) is the sole execution trigger,
/// which is the whole point of the propose→confirm→run split.
///
/// It is deliberately as strict as a Build boot and no stricter on the model's
/// behalf: re-checks the hash (the swap guard), refuses an invalid or already
/// consumed proposal, applies the SAME `${inputs.k}` gate the builder's boot
/// applies (a proposed graph with a hole nothing supplies is refused, not run
/// broken), writes the approved YAML to `flows/` so what runs has a durable,
/// inspectable home, and boots it as an ordinary flow run (not a chat). On
/// success the card flips to Launched with the child `run_id`.
async fn run_proposal(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RunProposalReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let RunHandle { emitter, .. } = lookup(&state, &id)?;

    // Read the proposal the operator is acting on and verify it is still the
    // one they saw. A newer block would carry a different hash and be refused,
    // so what runs is exactly what was approved.
    let snap = emitter.snapshot();
    if !snap.is_chat {
        return Err(ApiError(StatusCode::BAD_REQUEST, "run has no chat proposals".into()));
    }
    let node = snap
        .nodes
        .get(&req.node)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no node '{}'", req.node)))?;
    let prop = node
        .proposal
        .as_ref()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "node has no proposal".into()))?;
    if prop.hash != req.hash {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "предложение изменилось с тех пор — обнови карточку и подтверди заново".into(),
        ));
    }
    if !prop.valid {
        let why = if prop.errors.is_empty() { "не прошло проверку".to_string() } else { prop.errors.join("; ") };
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("предложение не пригодно к запуску: {why}")));
    }
    if prop.status != crate::state::ProposalStatus::Proposed {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("предложение уже {}", proposal_verb(prop.status)),
        ));
    }

    // Review M12: no proposal saved after this build exceeds the cap, but one
    // persisted by an earlier build is unbounded — and this parse runs on the
    // handler's worker thread. Same ceiling as the proposal path, checked
    // before a byte of YAML is handed to the parser.
    if prop.raw.len() > crate::proposal::MAX_PROPOSAL_BYTES {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "предложение слишком большое: {} байт (максимум {}) — перегенерируй его",
                prop.raw.len(),
                crate::proposal::MAX_PROPOSAL_BYTES
            ),
        ));
    }
    // Launch what the operator actually sees. When the card sends the live
    // canvas graph (`flow`), boot THAT — the operator may have dragged a node,
    // switched a model, or wired a dependency by hand, and ▶ must run the board
    // in front of them, not the YAML the agent first wrote. It is validated
    // exactly like the canvas ▶ Run (parse_spec + validate_spec), so an edited
    // graph with a hole is refused here rather than booted broken. The hash/CAS
    // guard above still pins the launch to the proposal that was confirmed.
    // Without `flow` (an older client) we boot the approved `raw`, re-parsed
    // server-side — never a client copy the operator did not confirm.
    let (flow, yaml) = match req.flow {
        Some(value) => {
            let flow = parse_spec(value)?;
            validate_spec(&flow).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
            let given: Vec<String> = req.inputs.keys().cloned().collect();
            let missing = missing_inputs(&flow, &given);
            if !missing.is_empty() {
                return Err(ApiError(StatusCode::BAD_REQUEST, missing_inputs_error(&missing)));
            }
            let yaml = proposal_yaml(&flow)?;
            (flow, yaml)
        }
        None => {
            // Boot the exact YAML the operator approved — re-parsed here. A
            // proposed graph that reads `${inputs.k}` nothing supplies would
            // die at that node, so refuse the launch and say which input.
            let flow: Flow = serde_yaml::from_str(&prop.raw)
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("flow YAML не разобран: {e}")))?;
            let given: Vec<String> = req.inputs.keys().cloned().collect();
            let missing = missing_inputs(&flow, &given);
            if !missing.is_empty() {
                return Err(ApiError(StatusCode::BAD_REQUEST, missing_inputs_error(&missing)));
            }
            let yaml = proposal_yaml(&flow)?;
            (flow, yaml)
        }
    };

    // CAS claim (Proposed → Launched) so a double-click boots exactly once; the
    // loser sees "already launched" instead of a second identical run.
    if !claim_proposal(&emitter, &req.node, &req.hash) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "предложение уже запускается или запущено".into(),
        ));
    }

    // Give the approved graph a durable home in flows/, then boot it as a real
    // flow run. If the boot fails, revert the claim so the operator can fix
    // inputs and retry — the proposal is not consumed by a failed launch.
    let path = match write_flow_file(&state, &flow, None, &yaml).await {
        Ok(p) => p,
        Err(e) => {
            // Same recovery as a failed boot: without the revert the card would
            // sit at Launched with no run behind it forever (review M6).
            revert_proposal(&emitter, &req.node, &req.hash, &e.1);
            return Err(e);
        }
    };
    let extra: Vec<(String, String)> = req.inputs.into_iter().collect();
    match boot_flow(state.clone(), &yaml, Some(path.clone()), extra, None).await {
        Ok(child) => {
            record_launch(&emitter, &req.node, &child);
            Ok(Json(json!({ "run_id": child, "path": path.to_string_lossy() })))
        }
        Err(e) => {
            revert_proposal(&emitter, &req.node, &req.hash, &format!("{e:#}"));
            Err(ApiError(StatusCode::BAD_REQUEST, format!("запуск не удался: {e:#}")))
        }
    }
}

/// Dismiss a proposed graph without running it. The raw stays on the node (the
/// transcript is the record), only the status flips — a newer block supersedes
/// it anyway, and a rejected card greys out instead of offering Run.
#[derive(serde::Deserialize)]
struct RejectProposalReq {
    node: String,
}

async fn reject_proposal(
    State(state): State<std::sync::Arc<AppState>>,
    AxPath(id): AxPath<String>,
    Json(req): Json<RejectProposalReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let RunHandle { emitter, .. } = lookup(&state, &id)?;
    let node = req.node;
    let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let changed2 = changed.clone();
    emitter.mutate(move |st| {
        if let Some(n) = st.nodes.get_mut(&node) {
            if let Some(p) = n.proposal.as_mut() {
                if p.status == crate::state::ProposalStatus::Proposed {
                    p.status = crate::state::ProposalStatus::Rejected;
                    changed2.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    });
    if !changed.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ApiError(StatusCode::CONFLICT, "предложение уже закрыто".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Serialise an approved proposal to the YAML written to `flows/`: ref-implied
/// deps folded in (so the file on disk matches the validated plan) with a
/// provenance header so a reopened file says where it came from.
fn proposal_yaml(flow: &Flow) -> Result<String, ApiError> {
    let mut normalized = flow.clone();
    merge_ref_deps(&mut normalized);
    let yaml = serde_yaml::to_string(&normalized)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("serialising proposal: {e}")))?;
    Ok(format!("# zori flow — предложен в чате и запущен из консоли\n{yaml}"))
}

/// Human wording for a consumed proposal in a "can't launch" message.
fn proposal_verb(status: crate::state::ProposalStatus) -> &'static str {
    use crate::state::ProposalStatus::*;
    match status {
        Launched => "запущено",
        Rejected => "отклонено",
        Proposed => "ожидает",
    }
}

/// CAS: move a proposal from Proposed to Launched iff its hash still matches,
/// returning whether the caller won. Two racing Run clicks cannot both boot;
/// the loser is told the proposal is already going. Mirrors [`claim_node`].
fn claim_proposal(emitter: &Emitter, node: &str, hash: &str) -> bool {
    let won = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let won2 = won.clone();
    let node = node.to_string();
    let hash = hash.to_string();
    emitter.mutate(move |st| {
        if let Some(n) = st.nodes.get_mut(&node) {
            if let Some(p) = n.proposal.as_mut() {
                if p.status == crate::state::ProposalStatus::Proposed && p.hash == hash {
                    p.status = crate::state::ProposalStatus::Launched;
                    p.ts = epoch_secs();
                    won2.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    });
    won.load(std::sync::atomic::Ordering::Relaxed)
}

/// Stamp the launched child's run id onto the proposal card once the boot
/// succeeds, so the console can link the card to the flow run it produced.
fn record_launch(emitter: &Emitter, node: &str, run_id: &str) {
    let node = node.to_string();
    let run_id = run_id.to_string();
    emitter.mutate(move |st| {
        if let Some(n) = st.nodes.get_mut(&node) {
            if let Some(p) = n.proposal.as_mut() {
                p.run_id = Some(run_id.clone());
                p.ts = epoch_secs();
            }
        }
    });
}

/// Undo a claim whose boot failed: return the proposal to Proposed so the
/// operator can supply missing inputs or fix the graph and retry. Only reverts
/// a still-Launched proposal with the matching hash, so a late report from a
/// launch that actually succeeded cannot clobber a fresh one.
fn revert_proposal(emitter: &Emitter, node: &str, hash: &str, _reason: &str) {
    let node = node.to_string();
    let hash = hash.to_string();
    emitter.mutate(move |st| {
        if let Some(n) = st.nodes.get_mut(&node) {
            if let Some(p) = n.proposal.as_mut() {
                if p.status == crate::state::ProposalStatus::Launched && p.hash == hash && p.run_id.is_none() {
                    p.status = crate::state::ProposalStatus::Proposed;
                    p.ts = epoch_secs();
                }
            }
        }
    });
}

async fn boot_spec(
    State(state): State<std::sync::Arc<AppState>>,
    Json(req): Json<BootSpecReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let flow = parse_spec(req.flow)?;
    validate_spec(&flow).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?;
    // Refuse before writing the YAML: boot_flow gates too (and thus covers
    // /api/runs and the startup flow), but by then the spec file is on disk.
    // A focused boot only needs the inputs of the nodes it will execute — an
    // unsupplied ${inputs.k} in a node outside the closure must not block it.
    let given: Vec<String> = req.inputs.keys().cloned().collect();
    let closure = match req.focus.as_deref() {
        None => None,
        Some(id) => Some(upstream_closure(&flow, id).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?),
    };
    let missing = missing_inputs_filtered(&flow, &given, closure.as_ref());
    if !missing.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, missing_inputs_error(&missing)));
    }
    let yaml = spec_to_yaml(&flow)?;
    // Booting always persists the spec first: runs store their flow_path and
    // the reload-across-restart path re-reads it, so an unsaved spec would
    // leave the run unreopenable after a restart.
    let path = write_flow_file(&state, &flow, req.name.as_deref(), &yaml).await?;
    let inputs = req.inputs.into_iter().collect::<Vec<_>>();
    let run_id = boot_flow(state, &yaml, Some(path.clone()), inputs, req.focus.as_deref())
        .await
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "run_id": run_id, "path": path.to_string_lossy() })))
}

/// Turn a browser spec into a `Flow` with our error shape, so a malformed
/// field comes back as a readable 400 rather than axum's JSON rejection.
fn parse_spec(value: serde_json::Value) -> Result<Flow, ApiError> {
    serde_json::from_value(value)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("invalid flow spec: {e}")))
}

/// Atomically move a node from one of `from` statuses to `Running`, or
/// return the reason it cannot be claimed. Under the emitter's lock the
/// checks and the flip are ONE mutation, so nothing can slip between them
/// (the old order — check outside, flip inside — let a first-pass level
/// launch a descendant between the two and double-drive the subtree).
///
/// Review M3: a node is also refused while any transitive DESCENDANT is
/// still owed work by the first pass (`Pending`/`Running`/`Waiting`).
/// Steering or re-running a Done ancestor while its downstream still
/// executes races that work: the pending level reads the outputs snapshot
/// with or without the fresh value depending on the scheduling, leaving a
/// transcript that matches neither. `Failed`/`Skipped`/`Stale` descendants
/// are terminal — nothing is owed — so a settled run (including a failed
/// one) re-runs freely.
///
/// Review M4: with `require_deps`, the claim also demands every *strict*
/// dependency be `Done` right now (the same rule as `blocked()`), checked
/// BEFORE the flip — the rerun path used to accept a `Skipped` node under a
/// failed parent and wipe its captured output before the dependency check
/// anywhere else could stop it.
fn claim_node(
    emitter: &Emitter,
    node: &str,
    from: &[TaskStatus],
    require_deps: bool,
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
        if require_deps {
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
                if matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::Running | TaskStatus::Waiting
                ) {
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
async fn boot_flow(
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

/// Spawn a task that writes each snapshot to `<dir>/state.json` until the
/// channel closes (the run's emitter is dropped).
fn spawn_persister(mut rx: broadcast::Receiver<RunState>, dir: PathBuf) {
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
fn manifest_run_inputs(dir: &Path, run_id: &str) -> Vec<(String, String)> {
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

    // A node left Running by a restart is unrecoverable (its process is gone);
    // a Waiting node is equally gone (the parked exec died with the server) —
    // both come back Stale so the output is treated as unknown and re-run.
    for n in snap.nodes.values_mut() {
        if matches!(n.status, TaskStatus::Running | TaskStatus::Waiting) {
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
    // reloaded run can be advanced — re-run anything still Pending (e.g. a node
    // never reached) or Stale — on demand via /rerun, not here.
    let _ = &id;
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

/// Look up a managed run. Returns a shared clone of the `TrackedRun` and the
/// `Emitter` is fetched via the guard-free clone path (we snapshot here).
fn lookup(
    state: &std::sync::Arc<AppState>,
    id: &str,
) -> Result<RunHandle, ApiError> {
    let runs = runs_guard(&state.runs);
    let m = runs
        .get(id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no run '{id}'")))?;
    Ok(RunHandle {
        emitter: m.emitter.clone(),
        tracked: m.tracked.clone(),
        op_lock: m.op_lock.clone(),
    })
}

/// A cheap, lock-free handle to a run's pieces, so handlers never hold the map
/// lock across an `.await`.
struct RunHandle {
    emitter: Emitter,
    tracked: TrackedRun,
    op_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// An HTTP error with an explicit status code and a message body.
#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down");
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    // Review (minor): a handler panicking under the registry lock used to
    // poison the shared mutex so every later `lock().unwrap()` in every
    // handler died — one run's panic shut the whole console. The registry
    // only holds Arc handles, so lock_recovering must enter a poisoned lock
    // loudly and hand back the still-valid data.
    #[test]
    fn lock_recovering_enters_a_poisoned_mutex() {
        let m: Mutex<Vec<u32>> = Mutex::new(vec![1, 2]);
        // Poison it: panic while the guard is held.
        std::thread::scope(|s| {
            s.spawn(|| {
                let _g = m.lock().unwrap();
                panic!("handler exploded");
            })
            .join()
            .unwrap_err(); // the spawn thread panicked; scope unwinds here
        });
        assert!(m.is_poisoned());
        // Recovery: the guard comes back and the old contents are intact.
        {
            let g = lock_recovering(&m, "test registry");
            assert_eq!(*g, vec![1, 2]);
        }
    }

    // Review (minor): the Host/Origin boundary (F10) had no direct coverage —
    // the rebinding/CSRF guard was reachable only through a live server. Pin
    // the pure allow-set logic: the address the server bound, IPv6 bracket
    // forms browsers actually send, `null` origins, and foreign hosts.
    #[test]
    fn allowed_hosts_accepts_the_bind_and_rejects_foreign() {
        let loopback = AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap());
        // Loopback names are always in the set, even for a LAN bind.
        assert!(loopback.allows_host("127.0.0.1:8787"), "the bound host+port");
        assert!(loopback.allows_host("localhost:8787"), "localhost always answers");
        assert!(loopback.allows_host("LOCALHOST"), "host is matched case-insensitively");
        assert!(loopback.allows_host("[::1]:9000"), "bracketed IPv6 loopback");
        assert!(!loopback.allows_host("evil.example"), "a foreign host is rejected");
        assert!(!loopback.allows_host(""), "empty host is rejected");

        // A LAN bind adds exactly its own address (any port), nothing more.
        let lan = AllowedHosts::for_bind("192.168.1.5:8787".parse().unwrap());
        assert!(lan.allows_host("192.168.1.5:1234"), "the bind answers any port");
        assert!(lan.allows_host("localhost"), "loopback still served on the same box");
        assert!(!lan.allows_host("192.168.1.6"), "a sibling host is foreign");
    }

    #[test]
    fn allowed_origin_accepts_matching_and_rejects_foreign_and_null() {
        let hosts = AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap());
        assert!(hosts.allows_origin("http://127.0.0.1:8787"), "same-origin POST");
        assert!(hosts.allows_origin("https://localhost:8787"), "https + any scheme");
        assert!(!hosts.allows_origin("http://evil.example"), "a foreign origin is refused");
        // Sandboxed/file contexts send the literal `null` origin: never local.
        assert!(!hosts.allows_origin("null"), "null origin is foreign");
        assert!(!hosts.allows_origin(""), "empty origin is foreign");
        assert!(!hosts.allows_origin("127.0.0.1:8787"), "a bare host is not an origin");
    }

    #[test]
    fn normalize_host_strips_port_and_brackets_and_lowercases() {
        assert_eq!(normalize_host("127.0.0.1:8787"), "127.0.0.1");
        assert_eq!(normalize_host("[::1]:9000"), "::1");
        assert_eq!(normalize_host("LOCALHOST"), "localhost");
        assert_eq!(normalize_host("  Example.COM:80  "), "example.com");
        // A bare (unbracketed) IPv6 literal has no port to strip.
        assert_eq!(normalize_host("::1"), "::1");
        // A trailing colon with no digits is not a port; leave the token intact.
        assert_eq!(normalize_host("host:"), "host:");
    }

    #[test]
    fn manifest_run_inputs_reads_effective_inputs() {
        // Review M2: after a restart the run's inputs must come from the
        // manifest (the effective map), not from YAML defaults alone.
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("zori-ui-inputs-{}-{}", std::process::id(), uniq));
        std::fs::create_dir_all(&dir).unwrap();
        let m = crate::artifacts::Manifest::new(
            "r1",
            "f",
            None,
            &std::collections::BTreeMap::from([
                ("k".to_string(), "override".to_string()),
                ("only_boot".to_string(), "v".to_string()),
            ]),
            &[],
        );
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();
        let mut got = manifest_run_inputs(&dir, "r1");
        got.sort();
        assert_eq!(
            got,
            vec![
                ("k".to_string(), "override".to_string()),
                ("only_boot".to_string(), "v".to_string())
            ]
        );
        // missing manifest → empty (old behaviour: YAML defaults)
        assert!(manifest_run_inputs(&dir.join("nowhere"), "r1").is_empty());
        // corrupt manifest → empty with a warning, never a panic
        std::fs::write(dir.join("manifest.json"), b"{ broken").unwrap();
        assert!(manifest_run_inputs(&dir, "r1").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // Review M8: the old save path checked `exists()` then wrote with a plain
    // write — two concurrent saves clobbered each other. claim_flow_file must
    // hand every writer a distinct file, including same-second saves.
    #[tokio::test]
    async fn claim_flow_file_never_clobbers() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("zori-ui-flows-{}-{}", std::process::id(), uniq));
        // Two writers racing for the same name both succeed, on distinct paths.
        let (a, b) = tokio::join!(
            claim_flow_file(&dir, "review", "name: a\n"),
            claim_flow_file(&dir, "review", "name: b\n")
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_ne!(a, b, "colliding saves must land on distinct files");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "name: a\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "name: b\n");
        // A third save of the same name gets yet another file, with suffix.
        let c = claim_flow_file(&dir, "review", "name: c\n").await.unwrap();
        assert_ne!(c, a);
        assert_ne!(c, b);
        assert_eq!(std::fs::read_to_string(&c).unwrap(), "name: c\n");
        assert!(c.file_name().unwrap().to_string_lossy().starts_with("review-"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn slug_sanitizes_flow_name() {
        assert_eq!(slug("Review Pipeline"), "review-pipeline");
        assert_eq!(slug("a/b c"), "a-b-c");
        assert_eq!(slug(""), "run");
    }

    #[test]
    fn rerunnable_covers_failed_and_skipped_but_not_inflight() {
        assert!(rerunnable(TaskStatus::Done));
        assert!(rerunnable(TaskStatus::Stale));
        // The owner's case: a node died on a typo'd model — rerun must work.
        assert!(rerunnable(TaskStatus::Failed));
        assert!(rerunnable(TaskStatus::Skipped));
        assert!(!rerunnable(TaskStatus::Running));
        assert!(!rerunnable(TaskStatus::Pending));
    }

    #[test]
    fn task_edit_accepts_leaf_fixes_but_not_topology() {
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n    depends_on: [b]\n  - id: b\n    harness: echo\n    in: seed\n");
        let existing = f.tasks.iter().find(|t| t.id == "a").unwrap();

        // Same id + deps, leaf fields corrected (the typo'd-model case): ok.
        let mut fix = existing.clone();
        fix.in_field = "hi, corrected".into();
        fix.out = Some("shorter answer".into());
        fix.model = None;
        assert!(validate_task_edit("a", existing, &fix, &f).is_ok());

        // Dropping an explicit dep = topology change: rejected.
        let mut topo = existing.clone();
        topo.depends_on = vec![];
        assert!(validate_task_edit("a", existing, &topo, &f).is_err());

        // Adding a ${steps.x.output} ref wires a new edge: rejected.
        let mut refed = existing.clone();
        refed.in_field = "take ${steps.c.output}".into();
        assert!(validate_task_edit("a", existing, &refed, &f).is_err());

        // Id must match the node being re-run.
        let mut wrong = existing.clone();
        wrong.id = "zz".into();
        assert!(validate_task_edit("a", existing, &wrong, &f).is_err());

        // An emptied prompt is fine when a dependency feeds it (edges carry
        // data: dep b lands as a `# b` section), but a node with nothing
        // feeding it must keep its text — that would be a guaranteed
        // driver failure.
        let mut empty = existing.clone();
        empty.in_field = "   ".into();
        assert!(validate_task_edit("a", existing, &empty, &f).is_ok());
        let b = f.tasks.iter().find(|t| t.id == "b").unwrap();
        let mut b_empty = b.clone();
        b_empty.in_field = "".into();
        assert!(validate_task_edit("b", b, &b_empty, &f).is_err());

        // Model on a harness that cannot set one: rejected with the reason.
        let mut modelled = existing.clone();
        modelled.model = Some("gpt-9".into());
        let e = validate_task_edit("a", existing, &modelled, &f).unwrap_err();
        assert!(e.contains("model"), "{e}");
    }

    #[test]
    fn task_edit_resolves_custom_harnesses_from_the_flow() {
        // A flow-local harness fix (point the task at a working command) is a
        // legal correction — resolve must consult the run's own table.
        let f = spec(
            "name: x\nharnesses:\n  fine:\n    command: [/bin/echo]\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n",
        );
        let existing = f.tasks.iter().find(|t| t.id == "a").unwrap();
        let mut fix = existing.clone();
        fix.harness = "fine".into();
        assert!(validate_task_edit("a", existing, &fix, &f).is_ok(),);

        // …but an id neither builtin nor in the flow is still unknown.
        let mut ghost = existing.clone();
        ghost.harness = "nosuch".into();
        assert!(validate_task_edit("a", existing, &ghost, &f).is_err());
    }

    #[test]
    fn run_status_summary_is_lowercase_debug() {
        assert_eq!(format!("{:?}", crate::state::RunStatus::Completed).to_lowercase(), "completed");
    }

    fn spec(yaml: &str) -> Flow {
        serde_yaml::from_str(yaml).expect("test spec must parse")
    }

    #[test]
    fn validate_accepts_a_valid_flow_and_returns_levels() {
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hello\n");
        let plan = validate_spec(&f).expect("valid");
        assert_eq!(plan.levels, vec![vec!["a".to_string()]]);
    }

    #[test]
    fn validate_rejects_unknown_harness() {
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: nosuch-cli\n    in: hi\n");
        let e = validate_spec(&f).unwrap_err();
        assert!(e.contains("nosuch-cli"), "{e}");
    }

    #[test]
    fn validate_rejects_model_on_a_harness_without_model_support() {
        // echo never sets a model per run — the builder must say so, not boot a
        // run that dies at the first task.
        assert!(!crate::connectors::resolve("echo", None).unwrap().supports_model);
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    model: gpt-9\n    in: hi\n");
        let e = validate_spec(&f).unwrap_err();
        assert!(e.contains("model"), "{e}");
    }

    #[test]
    fn validate_rejects_a_cycle() {
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n    depends_on: [b]\n  - id: b\n    harness: echo\n    in: hi\n    depends_on: [a]\n");
        assert!(validate_spec(&f).is_err());
    }

    #[test]
    fn step_refs_extracts_referenced_ids() {
        assert_eq!(step_refs("see ${steps.a.output} then ${steps.b.output}"), vec!["a", "b"]);
        // dedup, and non-.output keys are ignored
        assert_eq!(step_refs("${steps.a.output}${steps.a.output}${inputs.x}"), vec!["a"]);
        assert!(step_refs("no refs here").is_empty());
    }

    #[test]
    fn merge_ref_deps_folds_prompt_references_into_edges() {
        let mut f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: \"${steps.a.output}\"\n");
        merge_ref_deps(&mut f);
        let b = f.tasks.iter().find(|t| t.id == "b").unwrap();
        assert_eq!(b.depends_on, vec!["a".to_string()]);
    }

    #[test]
    fn input_refs_extracts_keys() {
        assert_eq!(input_refs("${inputs.a} и ${inputs.тема}"), vec!["a", "тема"]);
        // dedup; ${steps.*} is not an input
        assert_eq!(input_refs("${inputs.a}${inputs.a}${steps.b.output}"), vec!["a"]);
        assert!(input_refs("no refs").is_empty());
    }

    #[test]
    fn missing_inputs_names_unsupplied_keys() {
        // prompt referenced, never supplied by the flow → the lost-inputs bug.
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.prompt}\"\n");
        assert_eq!(missing_inputs(&f, &[]), vec!["prompt".to_string()]);
        // supplied by the flow's inputs block → fine
        let f = spec("name: x\ninputs:\n  prompt: тема\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.prompt}\"\n");
        assert!(missing_inputs(&f, &[]).is_empty());
        // supplied only at boot (--input) → fine
        assert!(missing_inputs(
            &spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.prompt}\"\n"),
            &["prompt".to_string()]
        ).is_empty());
        // referenced in `out` too, not just `in`
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n    out: \"по ${inputs.topic}\"\n");
        assert_eq!(missing_inputs(&f, &[]), vec!["topic".to_string()]);
    }

    #[test]
    fn upstream_closure_is_the_node_plus_all_ancestors() {
        //  a ← b ← c  and an unrelated  d . Running c pulls in b and a, never d.
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: hi\n    depends_on: [a]\n  - id: c\n    harness: echo\n    in: hi\n    depends_on: [b]\n  - id: d\n    harness: echo\n    in: hi\n");
        let c = upstream_closure(&f, "c").unwrap();
        let mut want = std::collections::HashSet::new();
        for id in ["a", "b", "c"] { want.insert(id.to_string()); }
        assert_eq!(c, want);
        // a leaf with no deps closes to itself alone
        assert_eq!(upstream_closure(&f, "a").unwrap().len(), 1);
        // a name the flow does not have is the canvas asking to run a block
        // that isn't there — a caller error, not an empty closure.
        assert!(upstream_closure(&f, "zz").is_err());
    }

    #[test]
    fn missing_inputs_filtered_ignores_nodes_outside_the_closure() {
        // b (outside) references ${inputs.topic}; a (the focused node) is clean.
        // A focused run of `a` must boot despite b's unsupplied input — only
        // the closure it executes needs its inputs satisfied.
        let yaml = "name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: \"${inputs.topic}\"\n    depends_on: [a]\n";
        let f = spec(yaml);
        // unfiltered, the whole flow is blocked by topic…
        assert_eq!(missing_inputs(&f, &[]), vec!["topic".to_string()]);
        // …but a focused run of a only inspects a → nothing missing.
        let only = upstream_closure(&f, "a").unwrap();
        assert!(missing_inputs_filtered(&f, &[], Some(&only)).is_empty());
        // focusing b (which pulls in itself) still needs topic.
        let only_b = upstream_closure(&f, "b").unwrap();
        assert_eq!(missing_inputs_filtered(&f, &[], Some(&only_b)), vec!["topic".to_string()]);
    }

    #[test]
    fn saved_yaml_roundtrips_and_drops_empty_fields() {
        let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: \"${steps.a.output}\"\n");
        let yaml = spec_to_yaml(&f).unwrap();
        // The ref-implied edge is on disk, so the file matches the validated plan.
        assert!(yaml.contains("depends_on"), "{yaml}");
        // skip_serializing_if keeps unset optionals out of the file.
        assert!(!yaml.contains("model:"), "unset model should not appear:\n{yaml}");
        // …and it parses back to the same DAG.
        let reparsed = spec(&yaml);
        assert_eq!(reparsed.tasks.len(), 2);
        assert_eq!(reparsed.tasks[1].depends_on, vec!["a".to_string()]);
    }

    // ---- review M3/M4: the atomic claim gate ------------------------------

    /// An emitter over a chain `a → b → c` (b depends on a, c on b) with the
    /// given statuses; the closure receives them in chain order.
    fn gate_emitter(statuses: [TaskStatus; 3]) -> Emitter {
        let tasks: Vec<crate::state::TaskMeta> = vec![
            ("a".into(), "echo".into(), None, false, vec![]),
            ("b".into(), "echo".into(), None, false, vec!["a".into()]),
            ("c".into(), "echo".into(), None, false, vec!["b".into()]),
        ];
        let mut st = RunState::new(
            "gate-1".into(),
            "gate".into(),
            "gate.yml".into(),
            None,
            vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]],
            &tasks,
        );
        for (node, s) in st.nodes.values_mut().zip(statuses) {
            node.status = s;
        }
        Emitter::new(st)
    }

    #[test]
    fn claim_gate_refuses_while_a_descendant_is_still_owed_work() {
        // M3: the owner's scenario — the first pass is mid-flight: `a`
        // finished, `b` is running, `c` is pending. Steering or re-running
        // `a` now must be refused, naming the in-flight descendant.
        let em = gate_emitter([TaskStatus::Done, TaskStatus::Running, TaskStatus::Pending]);
        let e = claim_node(&em, "a", &[TaskStatus::Done], false).unwrap_err();
        assert!(e.contains("b") && e.contains("c"), "{e}");
        assert!(e.contains("downstream"), "{e}");
        // The refusal must leave the node exactly as it was.
        assert_eq!(em.snapshot().nodes["a"].status, TaskStatus::Done);
    }

    #[test]
    fn claim_gate_blocks_transitively_not_only_children() {
        // `b` is terminal but `c` — two levels down — is still Pending:
        // the walk is transitive, so `a` is still owed.
        let em = gate_emitter([TaskStatus::Done, TaskStatus::Done, TaskStatus::Pending]);
        let e = claim_node(&em, "a", &[TaskStatus::Done], true).unwrap_err();
        assert!(e.contains("c"), "{e}");
    }

    #[test]
    fn claim_gate_allows_a_settled_run_and_flips_the_claim() {
        // A fully settled run (all Done): free to steer/rerun, and the claim
        // flips the node to Running atomically.
        let em = gate_emitter([TaskStatus::Done, TaskStatus::Done, TaskStatus::Done]);
        claim_node(&em, "a", &[TaskStatus::Done], true).unwrap();
        assert_eq!(em.snapshot().nodes["a"].status, TaskStatus::Running);
        // The second click loses the race — status is no longer in `from`.
        let e = claim_node(&em, "a", &[TaskStatus::Done], true).unwrap_err();
        assert!(e.contains("busy"), "{e}");
    }

    #[test]
    fn claim_gate_terminal_descendants_are_not_owed_work() {
        // Failed/Skipped descendants are terminal: a settled-but-failed run
        // must stay re-runnable (the F03 owner's case), even deep.
        let em = gate_emitter([TaskStatus::Done, TaskStatus::Skipped, TaskStatus::Failed]);
        claim_node(&em, "a", &[TaskStatus::Done], false).unwrap();
    }

    #[test]
    fn claim_gate_rerun_requires_done_deps_first() {
        // M4: `b` is Skipped under a Failed `a`. A rerun re-renders its
        // prompt from upstream, so the claim must be refused naming `a` —
        // BEFORE any flip, so nothing gets wiped.
        let mut tasks: Vec<crate::state::TaskMeta> = vec![
            ("a".into(), "echo".into(), None, false, vec![]),
            ("b".into(), "echo".into(), None, false, vec!["a".into()]),
            ("c".into(), "echo".into(), None, false, vec!["b".into()]),
        ];
        tasks[1].4 = vec!["a".into()];
        let st = RunState::new(
            "gate-2".into(),
            "gate".into(),
            "gate.yml".into(),
            None,
            vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]],
            &tasks,
        );
        let em = Emitter::new(st);
        em.mutate(|s| {
            s.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
            s.nodes.get_mut("b").unwrap().status = TaskStatus::Skipped;
        });
        let e = claim_node(&em, "b", &[TaskStatus::Done, TaskStatus::Stale, TaskStatus::Failed, TaskStatus::Skipped], true).unwrap_err();
        assert!(e.contains("unfinished dependencies: a"), "{e}");
        // Refused before the flip: still Skipped, nothing claimed.
        assert_eq!(em.snapshot().nodes["b"].status, TaskStatus::Skipped);
    }

    #[test]
    fn claim_gate_optional_failed_dep_does_not_block_rerun() {
        // An optional (`best-effort`) parent that failed never gates —
        // same rule as the scheduler's `blocked()`.
        let tasks: Vec<crate::state::TaskMeta> = vec![
            ("a".into(), "echo".into(), None, true, vec![]),
            ("b".into(), "echo".into(), None, false, vec!["a".into()]),
            ("c".into(), "echo".into(), None, false, vec!["b".into()]),
        ];
        let st = RunState::new(
            "gate-3".into(),
            "gate".into(),
            "gate.yml".into(),
            None,
            vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]],
            &tasks,
        );
        let em = Emitter::new(st);
        em.mutate(|s| {
            s.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
            s.nodes.get_mut("b").unwrap().status = TaskStatus::Done;
            s.nodes.get_mut("c").unwrap().status = TaskStatus::Done;
        });
        claim_node(&em, "b", &[TaskStatus::Done], true).unwrap();
    }

    #[test]
    fn claim_gate_steer_ignores_upstream_but_not_downstream() {
        // Steer (require_deps=false): an ancestor turned Stale by an earlier
        // steer must not lock a finished node's transcript — but the M3
        // descendant rule still applies. Here `b` (ancestor of c, pending)
        // is Stale and `c` is Done: steering `c` is fine.
        let em = gate_emitter([TaskStatus::Stale, TaskStatus::Stale, TaskStatus::Done]);
        claim_node(&em, "c", &[TaskStatus::Done], false).unwrap();
        // …while steering `b` is refused for the same reason it always was:
        // `c` is not what's owed — nothing downstream is — but `b`'s own
        // status is Stale, outside the steer's from-set.
        let e = claim_node(&em, "b", &[TaskStatus::Done], false).unwrap_err();
        assert!(e.contains("busy"), "{e}");
    }

    // planner-cwd: the project folder a chat is started with must reach the
    // agent process as its cwd. These pin the two pure pieces `start_chat`
    // delegates to; the end-to-end cwd (meta.json of both turns) is exercised
    // on a live console below.
    fn chat_req(cwd: Option<&str>) -> ChatRequest {
        ChatRequest {
            harness: "coddy".into(),
            model: "m".into(),
            prompt: "привет".into(),
            cwd: cwd.map(str::to_string),
            skills: Some(vec![]),
        }
    }

    #[test]
    fn resolve_project_dir_names_the_path_on_every_failure() {
        let tmp = std::env::temp_dir().join(format!(
            "zori-ui-proj-{}-{}",
            std::process::id(),
            epoch_secs()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // A reachable folder resolves to its canonical absolute self.
        let got = resolve_project_dir(tmp.to_str().unwrap()).expect("dir resolves");
        assert!(got.is_absolute(), "{}", got.display());
        assert_eq!(got, tmp.canonicalize().unwrap());
        // A missing path names both the typed value and the resolved one.
        let e = resolve_project_dir(tmp.join("nope").to_str().unwrap()).unwrap_err();
        assert!(e.contains("недоступна") && e.contains("nope"), "{e}");
        // A regular file is not a project folder.
        let f = tmp.join("file.txt");
        std::fs::write(&f, b"x").unwrap();
        let e = resolve_project_dir(f.to_str().unwrap()).unwrap_err();
        assert!(e.contains("не каталог") && e.contains("file.txt"), "{e}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn chat_flow_without_a_folder_keeps_the_classic_shape() {
        let y = chat_flow_yaml(&chat_req(None), &[], "OUT", None).unwrap();
        let f: Flow = serde_yaml::from_str(&y).unwrap();
        assert!(f.cwd.is_none(), "no cwd without a folder: {y}");
        assert!(f.tasks[0].workspace.is_none(), "task keeps its isolated work dir: {y}");
    }

    #[test]
    fn chat_flow_with_a_folder_sets_cwd_and_workspace_flow() {
        let dir = std::env::temp_dir().join(format!(
            "zori-ui-proj2-{}-{}",
            std::process::id(),
            epoch_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let y = chat_flow_yaml(&chat_req(Some(dir.to_str().unwrap())), &[], "OUT", Some(&dir)).unwrap();
        let f: Flow = serde_yaml::from_str(&y).unwrap();
        assert_eq!(f.cwd.as_deref(), Some(dir.as_path()));
        assert_eq!(f.tasks[0].workspace, Some(crate::config::Workspace::Flow));
        // And that combination is exactly what puts the process in the folder:
        let (proc_cwd, _flag) = crate::runner::effective_cwd(&f.cwd, &f.tasks[0], None).expect("cwd");
        assert_eq!(proc_cwd, dir.canonicalize().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
