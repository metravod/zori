use super::*;

/// One managed run: the observable state, the (immutable) driver that can
/// re-execute it, and a lock serialising user-initiated operations (steer,
/// rerun) so two clicks on the same run can't interleave and fight.
pub(super) struct ManagedRun {
    pub(super) emitter: Emitter,
    pub(super) tracked: TrackedRun,
    pub(super) op_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Model-catalog cache key: the harness's resolved identity (review F11) —
/// `(id, resolved argv, models_cmd override)`; see `AppState::models_cache`.
type ModelsCacheKey = (String, Vec<String>, Vec<String>);

/// Shared server state, cheaply cloned into every handler.
pub struct AppState {
    pub(super) runs: Mutex<HashMap<String, ManagedRun>>,
    /// Root directory under which each run gets `<run-id>/{state.json,sessions}`.
    pub(super) runs_root: PathBuf,
    /// Directory the flow builder saves to (`zori ui --flows-dir`). Saved and
    /// booted-from-spec flows live here; the builder may still *open* a flow
    /// from any readable path, the same trust level as `POST /api/runs`.
    pub(super) flows_root: PathBuf,
    /// Per-harness model catalogs with their fetch time (epoch secs), so the
    /// builder's dropdown does not re-spawn a CLI on every block click.
    /// `?refresh=1` bypasses it; see [`crate::models`].
    /// Keyed (review F11) by the harness id *and* the effective commands the
    /// list was derived from — the resolved argv and any winning `models_cmd`
    /// override. A bare id would let two flows that override the same harness
    /// id with different listing commands share a stale cached list.
    pub(super) models_cache: Mutex<HashMap<ModelsCacheKey, (u64, crate::models::ModelList)>>,
    /// Review F10: the Host/Origin allow-set derived from the bound address
    /// at startup; see [`boundary_guard`].
    pub(super) allowed_hosts: AllowedHosts,
}

/// Lock with explicit poison recovery: a poisoned mutex is entered anyway,
/// loudly. Policy lives here so it is testable without the full registry.
pub(super) fn lock_recovering<'a, T>(m: &'a Mutex<T>, what: &'a str) -> std::sync::MutexGuard<'a, T> {
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
pub(super) fn runs_guard(runs: &Mutex<HashMap<String, ManagedRun>>) -> std::sync::MutexGuard<'_, HashMap<String, ManagedRun>> {
    lock_recovering(runs, "run registry")
}

/// Look up a managed run. Returns a shared clone of the `TrackedRun` and the
/// `Emitter` is fetched via the guard-free clone path (we snapshot here).
pub(super) fn lookup(
    state: &AppState,
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
pub(super) struct RunHandle {
    pub(super) emitter: Emitter,
    pub(super) tracked: TrackedRun,
    pub(super) op_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// An HTTP error with an explicit status code and a message body.
#[derive(Debug)]
pub(super) struct ApiError(pub(super) StatusCode, pub(super) String);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
