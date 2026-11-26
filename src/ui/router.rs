use super::*;

/// The console: native ES modules with no build step, embedded so the binary
/// has no runtime asset dir. `ZORI_WEB_DIR` serves them from disk instead
/// (front-end development without a rebuild).
const INDEX_HTML: &str = include_str!("../../web/index.html");
const WEB_FILES: &[(&str, &str)] = &[
    ("app.js", include_str!("../../web/app.js")),
    ("canvas.js", include_str!("../../web/canvas.js")),
    ("graph.js", include_str!("../../web/graph.js")),
    ("icons.js", include_str!("../../web/icons.js")),
    ("md.js", include_str!("../../web/md.js")),
    ("panels.js", include_str!("../../web/panels.js")),
    ("store.js", include_str!("../../web/store.js")),
    ("styles.css", include_str!("../../web/styles.css")),
    ("vendor/preact-htm.js", include_str!("../../web/vendor/preact-htm.js")),
];

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
        .route("/web/*file", get(web_file))
        .route("/api/chats", post(start_chat))
        .route("/api/runs", get(list_runs).post(boot))
        .route("/api/runs/:id", get(get_run))
        .route("/api/runs/:id/events", get(events))
        .route("/api/runs/:id/transcript/:node", get(transcript))
        .route("/api/runs/:id/steer", post(steer))
        .route("/api/runs/:id/answer", post(answer))
        .route("/api/runs/:id/rerun", post(rerun))
        .route("/api/runs/:id/rerun-stale", post(rerun_stale))
        .route("/api/runs/:id/patch", post(patch_graph))
        .route("/api/runs/:id/flow", get(run_flow))
        .route("/api/runs/:id/run-proposal", post(run_proposal))
        .route("/api/runs/:id/reject-proposal", post(reject_proposal))
        .route("/api/catalog", get(planner_catalog))
        .route("/api/skills", get(list_skills))
        .route("/api/fleet", get(get_fleet).put(put_fleet))
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
    let body = dev_file("index.html").unwrap_or_else(|| INDEX_HTML.to_string());
    ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
}

fn dev_file(name: &str) -> Option<String> {
    let dir = std::env::var_os("ZORI_WEB_DIR")?;
    std::fs::read_to_string(std::path::Path::new(&dir).join(name)).ok()
}

async fn web_file(AxPath(file): AxPath<String>) -> axum::response::Response {
    let Some((name, embedded)) = WEB_FILES.iter().find(|(name, _)| *name == file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let body = dev_file(&file).unwrap_or_else(|| embedded.to_string());
    let kind = if name.ends_with(".css") { "text/css; charset=utf-8" } else { "text/javascript; charset=utf-8" };
    ([(axum::http::header::CONTENT_TYPE, kind), (axum::http::header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down");
}
