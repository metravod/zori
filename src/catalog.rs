//! Planner-facing inventory. A listed model, a local executable, and a current
//! account allowance are independent observations.
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdout, Command};

use crate::config::HarnessOverride;
use crate::{connectors, models};

#[derive(Serialize)]
pub struct Catalog {
    pub observed_at: u64,
    pub harnesses: Vec<Harness>,
}

#[derive(Serialize)]
pub struct Harness {
    id: String,
    description: String,
    is_agent: bool,
    supports_model: bool,
    supports_resume: bool,
    executable_found: Option<bool>,
    discovery: &'static str,
    model_source: Option<&'static str>,
    default_model: Option<String>,
    models: Vec<Model>,
    quota_source: Option<&'static str>,
    quota: Option<AccountQuota>,
}

#[derive(Serialize)]
struct Model {
    id: String,
    #[serde(flatten)]
    info: models::ModelInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    operator_note: Option<String>,
}

/// Keep only routing data; the RPC response also contains account identity,
/// credit balances and other fields which do not belong in a model prompt.
#[derive(Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
struct AccountQuota {
    ordinary_usage_allowed: Option<bool>,
    rate_limits: Bucket,
    #[serde(default)]
    rate_limits_by_limit_id: Option<BTreeMap<String, Bucket>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
struct Bucket {
    limit_id: Option<String>,
    normal_model_slug: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
    rate_limit_reached_type: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
struct Window {
    used_percent: f64,
    window_duration_mins: Option<u64>,
    resets_at: Option<i64>,
}

pub async fn discover(overrides: &BTreeMap<String, HarnessOverride>) -> Catalog {
    use futures_util::{stream, StreamExt};
    let mut ids = connectors::builtin_ids().to_vec();
    ids.extend(
        overrides
            .keys()
            .map(String::as_str)
            .filter(|id| !connectors::builtin_ids().contains(id)),
    );
    let probes: Vec<_> = ids
        .into_iter()
        .map(|id| inspect(id, overrides.get(id)))
        .collect();
    let harnesses = stream::iter(probes).buffered(4).collect().await;
    Catalog {
        observed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        harnesses,
    }
}

async fn inspect(id: &str, ov: Option<&HarnessOverride>) -> Harness {
    let description = ov.and_then(|o| o.description.as_deref()).unwrap_or(match id {
        "coddy" => "Coddy CLI coding agent. Selects a configured provider/model and resumes by session ID.",
        "codex" => "Codex CLI coding agent. Runs tasks in a workspace sandbox by default and resumes a thread.",
        "opencode" => "OpenCode CLI coding agent. Uses provider/model IDs and supports session resume.",
        "zcode" => "ZCode print-mode agent. The current connector relies on the model configured in ZCode.",
        "prompt" => "Static input for downstream tasks. Emits the rendered prompt.",
        "echo" => "Echoes its input. Intended for testing flows.",
        _ => "Flow-defined harness.",
    }).to_owned();
    let mut entry = Harness {
        id: id.to_owned(),
        description,
        // Review (minor): an invalid user-defined harness used to report
        // `is_agent: true` with `discovery: invalid_connector`, because this
        // literal defaulted true and the early `return` on a failed resolve
        // never reached the line below that sets it from the connector. An
        // unresolvable harness is not a known agent — default false; it is
        // promoted to the connector's real value only on a successful resolve.
        is_agent: false,
        supports_model: false,
        supports_resume: false,
        executable_found: None,
        discovery: "invalid_connector",
        model_source: None,
        default_model: None,
        models: Vec::new(),
        quota_source: None,
        quota: None,
    };
    let Ok(conn) = connectors::resolve(id, ov) else {
        return entry;
    };
    entry.is_agent = conn.is_agent;
    entry.supports_model = conn.supports_model;
    entry.supports_resume = connectors::steerable(&conn);
    if conn.is_agent {
        entry.executable_found = conn
            .command
            .first()
            .map(|cmd| executable_found(cmd, conn.env.get("PATH")));
        if id == "zcode" && conn.command.first().is_some_and(|s| s == "node") {
            entry.executable_found = Some(
                entry.executable_found == Some(true)
                    && conn
                        .command
                        .get(1)
                        .is_some_and(|script| Path::new(script).is_file()),
            );
        }
    }
    if !conn.supports_model {
        entry.discovery = "fixed";
        return entry;
    }
    // A wrapper/profile may address another account.
    if id == "codex"
        && ov.is_none_or(|o| o.command.is_none() && o.env.is_empty() && o.extra_args.is_empty())
    {
        entry.quota_source = Some("codex account/rateLimits/read");
    }
    let quota = async {
        if entry.quota_source.is_some() {
            let bin = conn.command.first()?;
            tokio::time::timeout(Duration::from_secs(12), codex_quota(bin))
                .await
                .ok()?
                .ok()
        } else {
            None
        }
    };
    let (listing, quota) = tokio::join!(models::list_models(&conn, ov), quota);
    entry.quota = quota;
    match listing {
        Ok(mut list) => {
            entry.discovery = "listed";
            entry.model_source = Some(if ov.is_some_and(|o| !o.models_cmd.is_empty()) {
                "models_cmd"
            } else {
                match id {
                    "coddy" => "coddy config",
                    "codex" => "codex debug models",
                    _ => "opencode models",
                }
            });
            entry.default_model = list.default;
            entry.models = list
                .models
                .into_iter()
                .map(|id| Model {
                    info: list.metadata.remove(&id).unwrap_or_default(),
                    operator_note: ov.and_then(|o| o.model_notes.get(&id)).cloned(),
                    id,
                })
                .collect();
        }
        Err(_) => entry.discovery = "unavailable",
    }
    entry
}

/// Is `program` runnable — a plain PATH search, spawning nothing. Shared with
/// `proposal::harness_brief` (same "installed" verdict as the catalog) and
/// the CLI's `zori harnesses` listing.
pub fn executable_found(program: &str, search_path: Option<&String>) -> bool {
    fn executable(path: &Path) -> bool {
        let Ok(meta) = path.metadata() else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        meta.is_file()
    }
    if program.contains(std::path::MAIN_SEPARATOR) {
        return executable(Path::new(program));
    }
    search_path
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .is_some_and(|p| std::env::split_paths(&p).any(|d| executable(&d.join(program))))
}

async fn rpc_result(
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
) -> Result<serde_json::Value> {
    while let Some(line) = lines.next_line().await? {
        let msg: serde_json::Value = serde_json::from_str(&line)?;
        if msg["id"].as_u64() == Some(id) {
            return msg.get("result").cloned().context("quota RPC failed");
        }
    }
    anyhow::bail!("quota RPC closed")
}

async fn codex_quota(bin: &str) -> Result<AccountQuota> {
    let mut child = Command::new(bin)
        .args(["app-server", "--listen", "stdio://"])
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().context("quota RPC stdin")?;
    let mut lines = BufReader::new(child.stdout.take().context("quota RPC stdout")?).lines();
    input.write_all(b"{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"zori\",\"version\":\"1\"}}}\n").await?;
    rpc_result(&mut lines, 1).await?;
    input.write_all(b"{\"method\":\"initialized\"}\n{\"id\":2,\"method\":\"account/rateLimits/read\",\"params\":null}\n").await?;
    let value = rpc_result(&mut lines, 2).await?;
    Ok(serde_json::from_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Review (minor): an unresolvable harness used to be advertised as an agent
    // (is_agent defaulted true and the early return on a failed resolve never
    // reached the line that promotes it from the connector). A harness zori
    // cannot even build is not a known agent — it must say so.
    #[tokio::test]
    async fn invalid_connector_is_not_advertised_as_an_agent() {
        let h = inspect("no-such-harness", None).await;
        assert_eq!(h.discovery, "invalid_connector");
        assert!(!h.is_agent, "an unresolvable harness must not claim to be an agent");
        assert!(!h.supports_model && !h.supports_resume, "and no capabilities either");
        assert_eq!(h.executable_found, None, "nothing was probed on the failure path");
        assert!(h.models.is_empty());
    }

    #[tokio::test]
    async fn flow_notes_only_enrich_listed_models() {
        let ov = HarnessOverride {
            command: Some(vec!["/bin/sh".into()]),
            model_flag: Some("--model".into()),
            models_cmd: vec!["printf".into(), "small\\nsmall\\n".into()],
            description: Some("Local review worker".into()),
            model_notes: BTreeMap::from([
                ("small".into(), "Short diffs".into()),
                ("retired".into(), "Old".into()),
            ]),
            ..Default::default()
        };
        let h = inspect("reviewer", Some(&ov)).await;
        assert_eq!(h.discovery, "listed");
        assert_eq!(h.models.len(), 1);
        assert_eq!(h.description, "Local review worker");
        assert_eq!(h.models[0].operator_note.as_deref(), Some("Short diffs"));
        assert_eq!(h.executable_found, Some(true));
        assert!(h.quota.is_none());
    }

    #[test]
    fn quota_keeps_windows_and_scope_without_account_identity() {
        let response = serde_json::json!({
            "accountId": "private", "ordinaryUsageAllowed": false,
            "rateLimits": {"limitId":"shared","primary":{"usedPercent":88,"windowDurationMins":300,"resetsAt":111}},
            "rateLimitsByLimitId":{"review":{"limitId":"review","normalModelSlug":"model-a","secondary":null}},
        });
        let quota: AccountQuota = serde_json::from_value(response).unwrap();
        let public = serde_json::to_value(quota).unwrap();
        assert_eq!(public["rate_limits"]["primary"]["used_percent"], 88.0);
        assert_eq!(public["ordinary_usage_allowed"], false);
        assert_eq!(
            public["rate_limits_by_limit_id"]["review"]["normal_model_slug"],
            "model-a"
        );
        assert!(!public.to_string().contains("private"));
    }
}
