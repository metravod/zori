use super::*;

/// A built-in harness as the builder sees it: enough to render a select box
/// and decide whether a `model:` field makes sense for it.
#[derive(Serialize)]
pub(super) struct HarnessInfo {
    supports_resume: bool,
    id: String,
    supports_model: bool,
    command: String,
    prompt_via: &'static str,
    is_agent: bool,
}

pub(super) async fn planner_catalog() -> Json<crate::catalog::Catalog> {
    // The personal harness store participates even without a flow: a CLI the
    // operator added to ~/.zori/harnesses.yml is real on this machine.
    Json(crate::catalog::discover(&crate::connectors::personal_harnesses()).await)
}

/// A skill entry for the picker UIs: name, one-line description, and where it
/// came from (so a store file and a built-in look different). The body is
/// omitted — the console fetches it only when a skill is actually inspected.
#[derive(Serialize)]
pub(super) struct SkillInfo {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    source: &'static str,
}

/// The skill list the node config-bar and the start-chat picker render. Lists
/// what the operator can reach right now (project `zori-skills/`, global
/// `~/.zori/skills`, built-ins); a flow's extra `skills_dirs` are honoured at
/// run time by the runner, not here.
pub(super) async fn list_skills() -> Json<Vec<SkillInfo>> {
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

pub(super) async fn harnesses() -> Json<Vec<HarnessInfo>> {
    let mut out = Vec::new();
    for id in crate::connectors::all_ids() {
        if let Ok(c) = crate::connectors::resolve(&id, None) {
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

/// The operator's saved model fleet (`~/.zori/fleet.json`) — what the Fleet
/// picker edits and the planner's brief prefers. GET never fails: no file
/// yet is just an empty fleet.
pub(super) async fn get_fleet() -> Json<serde_json::Value> {
    Json(json!({ "models": crate::fleet::load() }))
}

#[derive(Deserialize)]
pub(super) struct FleetReq {
    models: Vec<crate::fleet::FleetEntry>,
}

/// Save the fleet. Validation is the server's — a pair the runner would
/// refuse at boot (unknown harness, a non-agent, a harness without per-run
/// models) must not get saved — so the picker's answer is either the cleaned
/// list back or a plain error the dialog shows as-is.
pub(super) async fn put_fleet(Json(req): Json<FleetReq>) -> Result<Json<serde_json::Value>, ApiError> {
    crate::fleet::save(&req.models)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "models": crate::fleet::load() })))
}

#[derive(Deserialize)]
pub(super) struct OpenReq {
    path: String,
}

/// Query for `GET /api/harnesses/:id/models`: `?refresh=1` bypasses the TTL
/// cache (the builder's ↻ button).
#[derive(Deserialize)]
pub(super) struct ModelsQuery {
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
pub(super) async fn harness_models(
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
    // A personal harness's models_cmd lives in ~/.zori/harnesses.yml — this
    // route resolves without a flow, so the personal store is the override
    // the listing consults (a flow's own override still wins at run time
    // via catalog --flow paths).
    let personal = crate::connectors::personal_harnesses().get(&id).cloned();
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
    let list = crate::models::list_models(&conn, personal.as_ref())
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    let out = serde_json::to_value(&list)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.models_cache.lock().unwrap().insert(cache_key, (now, list));
    Ok(Json(out))
}

/// List flow files in the builder's flows dir, newest first.
pub(super) async fn list_flows(
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
pub(super) async fn open_flow(Query(q): Query<OpenReq>) -> Result<Json<serde_json::Value>, ApiError> {
    let text = std::fs::read_to_string(&q.path)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("reading {}: {e}", q.path)))?;
    let flow: Flow = serde_yaml::from_str(&text)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("parsing flow: {e}")))?;
    let value = serde_json::to_value(&flow)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "yaml": text, "flow": value })))
}
