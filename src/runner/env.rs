use super::*;

/// Env var names a child receives, for `meta.json` — names only, never values.
pub(super) fn env_keys(conn: &Connector, task: &Task) -> Vec<String> {
    let mut keys: Vec<String> = conn.env.keys().cloned().collect();
    for k in task.env.keys() {
        if !keys.contains(k) {
            keys.push(k.clone());
        }
    }
    for k in ["ZORI_RUN_DIR", "ZORI_TASK_DIR", "ZORI_TASK_WORK", "ZORI_PROJECT_DIR"] {
        if !keys.iter().any(|e| e == k) {
            keys.push(k.to_string());
        }
    }
    keys
}

/// Merge flow inputs with CLI overrides (CLI wins).
pub(super) fn resolved_inputs(flow: &Flow, extra: &[(String, String)]) -> HashMap<String, String> {
    let mut inputs: HashMap<String, String> =
        flow.inputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in extra {
        inputs.insert(k.clone(), v.clone());
    }
    inputs
}

pub(super) fn connector_for(flow: &Flow, task: &Task) -> Result<Connector> {
    let ov = flow.harnesses.get(&task.harness);
    connectors::resolve(&task.harness, ov)
}

/// Merge connector-level and task-level env into spawn pairs (task wins).
pub(super) fn env_pairs(conn: &Connector, task: &Task) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        conn.env.iter().map(|(k, val)| (k.clone(), val.clone())).collect();
    for (k, val) in &task.env {
        v.retain(|(ek, _)| ek != k);
        v.push((k.clone(), val.clone()));
    }
    v
}
