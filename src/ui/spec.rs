use super::*;

#[derive(Deserialize)]
pub(super) struct SpecReq {
    flow: serde_json::Value,
}

#[derive(Deserialize)]
pub(super) struct SaveReq {
    flow: serde_json::Value,
    /// Optional file name (not a path — writing is confined to the flows dir).
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct BootSpecReq {
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
pub(super) fn validate_spec(flow: &Flow) -> Result<crate::dag::ExecutionPlan, String> {
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
pub(super) fn step_refs(text: &str) -> Vec<String> {
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
pub(super) fn input_refs(text: &str) -> Vec<String> {
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
pub(super) fn missing_inputs(flow: &Flow, extra: &[String]) -> Vec<String> {
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
pub(super) fn missing_inputs_filtered(
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
pub(super) fn upstream_closure(flow: &Flow, id: &str) -> Result<std::collections::HashSet<String>, String> {
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
pub(super) fn missing_inputs_error(missing: &[String]) -> String {    let list = missing
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
pub(super) fn merge_ref_deps(flow: &mut Flow) {
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
pub(super) fn spec_to_yaml(flow: &Flow) -> Result<String, ApiError> {
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
pub(super) async fn write_flow_file(
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
pub(super) async fn claim_flow_file(flows_root: &Path, base: &str, yaml: &str) -> Result<PathBuf, ApiError> {
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

pub(super) async fn validate_flow(Json(req): Json<SpecReq>) -> Result<Json<serde_json::Value>, ApiError> {
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

pub(super) async fn save_flow(
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

pub(super) async fn boot_spec(
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
pub(super) fn parse_spec(value: serde_json::Value) -> Result<Flow, ApiError> {
    serde_json::from_value(value)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("invalid flow spec: {e}")))
}
