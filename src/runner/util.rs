use super::*;

/// Unix seconds — for transcript timestamps and the coddy session nonce.
pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keep only id-safe characters (coddy/codex ids must be shell- and FS-safe).
fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect()
}

/// A fresh coddy session id, unique per (run,node,attempt). Steer reuses the id
/// already stored on the node; only a first-run/rerun mints a new one (new nonce
/// → a fresh coddy session, which is what a from-scratch rerun must be).
pub(super) fn new_coddy_session_id(run_id: &str, node: &str) -> String {
    format!("sess_{}_{}", sanitize_id(&format!("{run_id}-{node}")), artifacts::next_millis())
}
