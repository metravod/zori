use super::*;

/// The launch request for a chat proposal: which node carries it, the hash
/// that was on screen when the operator clicked Run, and any `${inputs.k}`
/// values supplied at click time. The hash is a swap guard — the server
/// re-checks it, so a graph validated a moment ago cannot be quietly replaced
/// by a newer one between the render and the click.
#[derive(serde::Deserialize)]
pub(super) struct RunProposalReq {
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
pub(super) async fn run_proposal(
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
            "the proposal changed since then — refresh the card and confirm again".into(),
        ));
    }
    if !prop.valid {
        let why = if prop.errors.is_empty() { "failed validation".to_string() } else { prop.errors.join("; ") };
        return Err(ApiError(StatusCode::BAD_REQUEST, format!("the proposal is not runnable: {why}")));
    }
    if prop.status != crate::state::ProposalStatus::Proposed {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("the proposal is already {}", proposal_verb(prop.status)),
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
                "proposal too large: {} bytes (maximum {}) — regenerate it",
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
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("flow YAML failed to parse: {e}")))?;
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
            "the proposal is already launching or launched".into(),
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
            Err(ApiError(StatusCode::BAD_REQUEST, format!("launch failed: {e:#}")))
        }
    }
}

/// Dismiss a proposed graph without running it. The raw stays on the node (the
/// transcript is the record), only the status flips — a newer block supersedes
/// it anyway, and a rejected card greys out instead of offering Run.
#[derive(serde::Deserialize)]
pub(super) struct RejectProposalReq {
    node: String,
}

pub(super) async fn reject_proposal(
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
        return Err(ApiError(StatusCode::CONFLICT, "the proposal is already closed".into()));
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
    Ok(format!("# zori flow — proposed in chat, launched from the console\n{yaml}"))
}

/// Human wording for a consumed proposal in a "can't launch" message.
fn proposal_verb(status: crate::state::ProposalStatus) -> &'static str {
    use crate::state::ProposalStatus::*;
    match status {
        Launched => "launched",
        Rejected => "rejected",
        Proposed => "awaiting confirmation",
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
