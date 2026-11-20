//! Model discovery: what models each harness can actually take *on this
//! machine, right now* — pulled from the harness itself, never hardcoded.
//!
//! The hardcoded lists that used to live in `.omut/local/instance.md` go stale
//! on the next CLI update; the CLIs themselves already know the answer:
//!
//! - **coddy** — its `~/.coddy/config.yaml` `models:` list is exactly what
//!   `--model` accepts (plus `agent.model` as the default).
//! - **codex** — `codex debug models` prints the live catalog as JSON;
//!   entries with `visibility: "hide"` are internal and not offered;
//!   the default is the top-level `model = "…"` of `~/.codex/config.toml`.
//! - **opencode** — `opencode models` prints one `provider/model` id per line.
//! - **custom harnesses** — a `models_cmd` override (any argv whose stdout is
//!   one model id per line) makes them listable too.
//! - **zcode / echo** — no per-run model (`supports_model: false`), so there
//!   is nothing to list and we say so rather than inventing a list.
//!
//! Everything CLI-backed runs under a hard timeout with `kill_on_drop`, so a
//! hung CLI degrades the builder's dropdown to "type it yourself" instead of
//! wedging the console.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::config::HarnessOverride;
use crate::connectors::Connector;

/// Fields reported by the harness. Missing fields mean unknown.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_levels: Vec<String>,
}

/// A harness's model list as the builder and CLI present it.
#[derive(Debug, Clone, Serialize)]
pub struct ModelList {
    pub harness: String,
    pub models: Vec<String>,
    pub metadata: BTreeMap<String, ModelInfo>,
    /// The harness's own default, when we can see it (coddy: `agent.model`,
    /// codex: config `model`). `None` = unknown, not "none exists".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Human-readable provenance ("coddy config …", "`codex debug models`"),
    /// so the UI can show *where* the list came from.
    pub source: String,
}

/// Hard cap on any discovery subprocess: listing models must never hang the
/// console waiting on a CLI that wedges.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// List the models `conn`'s harness accepts. `ov` is the flow-level override
/// for this harness id (a `models_cmd` there wins over the built-in source).
pub async fn list_models(conn: &Connector, ov: Option<&HarnessOverride>) -> Result<ModelList> {
    // A user-declared listing command beats every built-in source.
    if let Some(cmd) = ov.map(|o| &o.models_cmd).filter(|v| !v.is_empty()) {
        let out = run_capture(cmd, &conn.env).await?;
        return Ok(ModelList {
            harness: conn.id.clone(),
            models: parse_line_list(&out),
            metadata: BTreeMap::new(),
            default: None,
            source: format!("`{}` (models_cmd)", cmd.join(" ")),
        });
    }

    match conn.id.as_str() {
        "coddy" => coddy_models(conn).await,
        "codex" => codex_models(conn).await,
        "opencode" => opencode_models(conn).await,
        _ if !conn.supports_model => bail!(
            "harness '{}' has no per-run model selection, so there is nothing to list",
            conn.id
        ),
        _ => bail!(
            "no model listing for harness '{}' — add models_cmd to its override \
             (an argv whose stdout is one model id per line)",
            conn.id
        ),
    }
}

/// Env var naming an alternative coddy config file (coddy itself takes
/// `--config`; for listing we need only the file).
const ENV_CODDY_CONFIG: &str = "ZORI_CODDY_CONFIG";

fn env_path(conn: &Connector, key: &str) -> Option<std::path::PathBuf> {
    conn.env
        .get(key)
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os(key))
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

async fn coddy_models(conn: &Connector) -> Result<ModelList> {
    let path = env_path(conn, ENV_CODDY_CONFIG)
        .or_else(|| env_path(conn, "HOME").map(|h| h.join(".coddy/config.yaml")))
        .context("cannot locate coddy config")?;
    // Async read: this is an async fn, and a blocking disk read here ties up a
    // runtime worker the console needs for other runs (review, minor). The
    // config may sit on a slow or network mount the operator controls.
    let text = tokio::fs::read_to_string(&path).await.with_context(|| {
        format!(
            "reading coddy config {} (set {ENV_CODDY_CONFIG} to point elsewhere)",
            path.display()
        )
    })?;
    let (models, default, metadata) =
        coddy_entries(&text).with_context(|| format!("parsing coddy config {}", path.display()))?;
    Ok(ModelList {
        harness: "coddy".into(),
        models,
        metadata,
        default,
        source: format!("coddy config {}", path.display()),
    })
}

async fn codex_models(conn: &Connector) -> Result<ModelList> {
    // conn.command is e.g. ["codex","exec"] (or the ZORI_CODEX_BIN override);
    // the listing subcommand hangs off the binary itself.
    let Some(bin) = conn.command.first() else {
        bail!("codex connector has an empty command");
    };
    let argv = vec![bin.clone(), "debug".into(), "models".into()];
    let out = run_capture(&argv, &conn.env).await?;
    let (models, metadata) = codex_entries(&out).context("parsing `codex debug models` output")?;
    // Best-effort default: CODEX_HOME (or ~/.codex) config.toml, top-level key.
    // Async read (review, minor): same blocking-worker concern as coddy_models.
    let default = match codex_config_path(conn) {
        Some(p) => tokio::fs::read_to_string(p).await.ok().and_then(|t| parse_codex_default(&t)),
        None => None,
    };
    Ok(ModelList {
        harness: "codex".into(),
        models,
        metadata,
        default,
        source: format!("`{} debug models`", bin),
    })
}

fn codex_config_path(conn: &Connector) -> Option<std::path::PathBuf> {
    let dir = env_path(conn, "CODEX_HOME")
        .or_else(|| env_path(conn, "HOME").map(|h| h.join(".codex")))?;
    let p = std::path::Path::new(&dir).join("config.toml");
    p.exists().then_some(p)
}

async fn opencode_models(conn: &Connector) -> Result<ModelList> {
    let Some(bin) = conn.command.first() else {
        bail!("opencode connector has an empty command");
    };
    let argv = vec![bin.clone(), "models".into()];
    let out = run_capture(&argv, &conn.env).await?;
    let models = parse_line_list(&out);
    let cache = env_path(conn, "XDG_CACHE_HOME")
        .or_else(|| env_path(conn, "HOME").map(|h| h.join(".cache")));
    let metadata = match cache {
        Some(dir) => tokio::fs::read(dir.join("opencode/models.json"))
            .await
            .ok()
            .and_then(|bytes| opencode_metadata(&bytes, &models).ok())
            .unwrap_or_default(),
        None => BTreeMap::new(),
    };
    Ok(ModelList {
        harness: "opencode".into(),
        models,
        metadata,
        default: None,
        source: format!("`{} models`", bin),
    })
}

fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

/// The local OpenCode registry includes providers that are not configured.
/// Only enrich exact provider/model pairs already returned by the CLI.
fn opencode_metadata(bytes: &[u8], ids: &[String]) -> Result<BTreeMap<String, ModelInfo>> {
    let registry: serde_json::Value = serde_json::from_slice(bytes)?;
    let mut info = BTreeMap::new();
    for id in ids {
        let Some((provider, slug)) = id.split_once('/') else {
            continue;
        };
        let Some(m) = registry[provider]["models"].get(slug) else {
            continue;
        };
        info.insert(
            id.clone(),
            ModelInfo {
                display_name: m["name"].as_str().map(str::to_owned),
                description: m["description"].as_str().map(str::to_owned),
                max_context_tokens: m["limit"]["context"].as_u64(),
                max_output_tokens: m["limit"]["output"].as_u64(),
                reasoning: m["reasoning"].as_bool(),
                tool_call: m["tool_call"].as_bool(),
                input_modalities: strings(&m["modalities"]["input"]),
                ..Default::default()
            },
        );
    }
    Ok(info)
}

/// Per-pipe ceiling for a model-listing command. A hung or misbehaving CLI that
/// floods its terminal could otherwise be buffered into memory without bound —
/// [`FETCH_TIMEOUT`] bounds *time*, not *volume*. Past the cap we stop reading,
/// kill the child, and report the overflow instead of feeding a half-read,
/// possibly-tampered list into the builder's dropdown.
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

/// Drain a child pipe up to [`MAX_CAPTURE_BYTES`]. Returns the bytes collected
/// and whether the cap was exceeded. Reads run concurrently (see [`run_capture`])
/// so neither pipe's kernel buffer can fill and block the child while we drain
/// the other.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut pipe: R) -> Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 8 * 1024];
    loop {
        let n = pipe.read(&mut chunk).await.context("reading model listing output")?;
        if n == 0 {
            break;
        }
        if buf.len() + n > MAX_CAPTURE_BYTES {
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok((buf, false))
}

/// Run argv under [`FETCH_TIMEOUT`], return stdout. A non-zero exit becomes an
/// error naming the command and the first stderr line — the builder shows it
/// instead of silently offering an empty dropdown. Both pipes are read under
/// [`MAX_CAPTURE_BYTES`] (volume, not just time); a chatty CLI is killed and
/// reported. Successful stdout is model ids bound for `-m`, so it is checked
/// for UTF-8 *strictly* — the old lossy decode let a replacement char into a
/// real model id.
async fn run_capture(argv: &[String], env: &BTreeMap<String, String>) -> Result<String> {
    let Some((prog, args)) = argv.split_first() else {
        bail!("empty model listing command");
    };
    let mut cmd = Command::new(prog);
    cmd.args(args).envs(env);
    cmd.kill_on_drop(true);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // `spawn` is synchronous in tokio (it only forks the child); the volume and
    // wall-clock limits are enforced on the reads and the wait below.
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `{prog}` for model listing"))?;

    // Read both pipes concurrently to their EOF, each under the volume cap.
    // Doing it in one pass (stdout then stderr) could dead-lock: the child
    // blocks writing the pipe we are not draining while we wait on the other.
    let (stdout_pipe, stderr_pipe) = (child.stdout.take(), child.stderr.take());
    let read_both = async {
        let (so, se) = tokio::join!(
            read_capped(stdout_pipe.expect("stdout piped")),
            read_capped(stderr_pipe.expect("stderr piped")),
        );
        Ok::<_, anyhow::Error>((so?, se?))
    };
    let ((out_bytes, out_over), (err_bytes, err_over)) = timeout(FETCH_TIMEOUT, read_both)
        .await
        .map_err(|_| {
            anyhow::anyhow!("`{}` timed out after {}s", argv.join(" "), FETCH_TIMEOUT.as_secs())
        })??;
    if out_over || err_over {
        // The CLI printed more than a reasonable model list can be — kill it
        // (kill_on_drop is the backstop) rather than trust the truncation.
        let _ = child.start_kill();
        bail!(
            "`{}` printed more than {} bytes on {} — refusing its model listing",
            argv.join(" "),
            MAX_CAPTURE_BYTES,
            if out_over { "stdout" } else { "stderr" }
        );
    }
    let status = timeout(FETCH_TIMEOUT, child.wait())
        .await
        .map_err(|_| {
            anyhow::anyhow!("`{}` timed out after {}s", argv.join(" "), FETCH_TIMEOUT.as_secs())
        })?
        .with_context(|| format!("waiting for `{prog}`"))?;
    if !status.success() {
        let err = String::from_utf8_lossy(&err_bytes);
        let first = err.trim().lines().next().unwrap_or("no stderr").trim();
        bail!("`{}` failed: {}", argv.join(" "), first);
    }
    // Strict UTF-8: a corrupt byte is a broken model id, not a warning. Refuse
    // the whole listing rather than hand the dropdown a name with U+FFFD in it.
    String::from_utf8(out_bytes)
        .with_context(|| format!("`{}` listed models with non-UTF-8 bytes on stdout", argv.join(" ")))
}

// ---------------------------------------------------------------------------
// Pure parsers (unit-tested without spawning anything)
// ---------------------------------------------------------------------------

/// Deduplicate while preserving order.
fn dedup(mut v: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
    v
}

/// coddy `config.yaml`: `models:` is a list of entries with a `model:` key;
/// `agent.model` is the default. Returns (models, default).
pub fn parse_coddy_config(text: &str) -> Result<(Vec<String>, Option<String>)> {
    let (ids, default, _) = coddy_entries(text)?;
    Ok((ids, default))
}

type CoddyEntries = (Vec<String>, Option<String>, BTreeMap<String, ModelInfo>);

fn coddy_entries(text: &str) -> Result<CoddyEntries> {
    let v: serde_yaml::Value = serde_yaml::from_str(text)?;
    let mut models = Vec::new();
    let mut metadata = BTreeMap::new();
    for item in v
        .get("models")
        .and_then(|m| m.as_sequence())
        .into_iter()
        .flatten()
    {
        let Some(id) = item.get("model").and_then(|m| m.as_str()) else {
            continue;
        };
        models.push(id.to_owned());
        metadata.entry(id.to_owned()).or_insert_with(|| ModelInfo {
            max_context_tokens: item.get("max_context_tokens").and_then(|n| n.as_u64()),
            max_output_tokens: item.get("max_output_tokens").and_then(|n| n.as_u64()),
            description: item
                .get("description")
                .and_then(|s| s.as_str())
                .map(str::to_owned),
            ..Default::default()
        });
    }
    let default = v
        .get("agent")
        .and_then(|a| a.get("model"))
        .and_then(|m| m.as_str())
        .map(str::to_string);
    Ok((dedup(models), default, metadata))
}

/// `codex debug models` JSON: `{ "models": [ { "slug", "visibility", … } ] }`.
/// Anything not explicitly `visibility: "hide"` is offered; hidden entries are
/// the CLI's internal/upcoming models that `-m` may refuse anyway. Tolerates
/// leading non-JSON noise by parsing from the first `{`.
pub fn parse_codex_models(out: &str) -> Result<Vec<String>> {
    Ok(codex_entries(out)?.0)
}

fn codex_entries(out: &str) -> Result<(Vec<String>, BTreeMap<String, ModelInfo>)> {
    let start = out
        .find('{')
        .context("no JSON object in codex model output")?;
    let v: serde_json::Value = serde_json::from_str(&out[start..])?;
    let mut ids = Vec::new();
    let mut metadata = BTreeMap::new();
    for m in v["models"].as_array().context("missing models array")? {
        if m["visibility"].as_str() == Some("hide") {
            continue;
        }
        let Some(id) = m["slug"].as_str() else {
            continue;
        };
        ids.push(id.to_owned());
        metadata.entry(id.to_owned()).or_insert_with(|| ModelInfo {
            display_name: m["display_name"].as_str().map(str::to_owned),
            input_modalities: strings(&m["input_modalities"]),
            description: m["description"].as_str().map(str::to_owned),
            context_tokens: m["context_window"].as_u64(),
            max_context_tokens: m["max_context_window"].as_u64(),
            max_output_tokens: m["max_output_tokens"].as_u64(),
            reasoning_levels: m["supported_reasoning_levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|level| level["effort"].as_str().map(str::to_owned))
                .collect(),
            ..Default::default()
        });
    }
    Ok((dedup(ids), metadata))
}

/// Top-level `model = "…"` from codex's config.toml — only lines *before* the
/// first `[table]` header count, so `[tui] model = …` can never leak in.
/// A hand-rolled scan: the file's full TOML grammar is not worth a dependency
/// for one string, and a wrong read here only mislabels a placeholder.
pub fn parse_codex_default(text: &str) -> Option<String> {
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            break;
        }
        if let Some(rest) = l.strip_prefix("model").map(str::trim_start) {
            let Some(rest) = rest.strip_prefix('=') else {
                continue;
            };
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// One model id per line (opencode's format, and the contract of
/// `models_cmd`): trimmed, empties dropped, order kept.
pub fn parse_line_list(out: &str) -> Vec<String> {
    dedup(
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coddy_config_yields_models_and_default() {
        let yaml = "\
providers:
  - name: neuraldeep
    type: neuraldeep
models:
  - max_context_tokens: 131072
    model: neuraldeep/gpt-oss-120b
  - max_context_tokens: 262144
    model: neuraldeep/kimi-k2.6
  - model: neuraldeep/kimi-k2.6
agent:
  model: neuraldeep/gpt-oss-120b
";
        let (models, default, metadata) = coddy_entries(yaml).unwrap();
        assert_eq!(
            metadata["neuraldeep/kimi-k2.6"].max_context_tokens,
            Some(262144)
        );
        // Duplicates collapse, order is the config's order, entries without a
        // `model:` key are skipped rather than poisoning the list.
        assert_eq!(
            models,
            vec!["neuraldeep/gpt-oss-120b", "neuraldeep/kimi-k2.6"]
        );
        assert_eq!(default.as_deref(), Some("neuraldeep/gpt-oss-120b"));
    }

    #[test]
    fn coddy_config_without_models_is_empty_not_error() {
        let (models, default, metadata) = coddy_entries("agent:\n  model: x/y\n").unwrap();
        assert!(metadata.is_empty());
        assert!(models.is_empty());
        assert_eq!(default.as_deref(), Some("x/y"));
    }

    #[test]
    fn codex_catalog_skips_hidden_and_tolerates_noise() {
        let out = "\
warning: some cli noise on stdout
{\"models\":[
 {\"slug\":\"gpt-6.1-sol\",\"visibility\":\"list\"},
 {\"slug\":\"gpt-6-astra\",\"visibility\":\"list\"},
 {\"slug\":\"gpt-6-chat-thinking\",\"visibility\":\"hide\"},
 {\"slug\":\"gpt-reserve\",\"visibility\":\"hide\"},
 {\"slug\":\"no-visibility-field\"},
 {\"visibility\":\"list\"}
]}";
        assert_eq!(
            parse_codex_models(out).unwrap(),
            vec!["gpt-6.1-sol", "gpt-6-astra", "no-visibility-field"]
        );
    }

    #[test]
    fn opencode_registry_enriches_only_exact_listed_pairs() {
        let raw = br#"{"p":{"models":{"a/b":{"limit":{"context":4096,"output":600},"tool_call":false},"old":{}}}}"#;
        let info = opencode_metadata(raw, &["p/a/b".into(), "q/a/b".into()]).unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info["p/a/b"].max_output_tokens, Some(600));
        assert_eq!(info["p/a/b"].tool_call, Some(false));
    }

    #[test]
    fn codex_separates_context_limits_and_filters_private_catalog_fields() {
        let raw = r#"{"models":[{"slug":"a","description":"fast","context_window":100,"max_context_window":200,
            "base_instructions":"private","supported_reasoning_levels":[{"effort":"medium"}]}]}"#;
        let (_, info) = codex_entries(raw).unwrap();
        assert_eq!(info["a"].context_tokens, Some(100));
        assert_eq!(info["a"].max_context_tokens, Some(200));
        assert_eq!(info["a"].reasoning_levels, ["medium"]);
        assert!(!serde_json::to_string(&info).unwrap().contains("private"));
    }

    #[test]
    fn codex_catalog_garbage_errors() {
        assert!(parse_codex_models("not json").is_err());
        assert!(
            parse_codex_models("{\"models\": \"nope\"}").is_err()
                || parse_codex_models("{\"models\":\"nope\"}")
                    .unwrap()
                    .is_empty()
        );
    }

    #[test]
    fn codex_default_reads_only_top_level() {
        let toml = "\
model = \"gpt-6-astra\"
model_reasoning_effort = \"high\"
sandbox_mode = \"workspace-write\"

[tui.model_availability_nux]
model = \"wrong\"
";
        assert_eq!(parse_codex_default(toml).as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn codex_default_handles_missing_and_single_quotes() {
        assert_eq!(parse_codex_default("[tui]\nx=1\n"), None);
        assert_eq!(parse_codex_default("model = 'x'\n").as_deref(), Some("x"));
        // `model_reasoning_effort` must not parse as `model`.
        assert_eq!(
            parse_codex_default("model_reasoning_effort = \"high\"\n"),
            None
        );
    }

    #[test]
    fn line_list_trims_dedups_and_drops_empties() {
        let out = "opencode/beam-free\n  zai/glm-4.5  \n\nopencode/beam-free\n";
        assert_eq!(
            parse_line_list(out),
            vec!["opencode/beam-free", "zai/glm-4.5"]
        );
    }

    // A custom harness with a models_cmd override is listable through the same
    // entry point the console calls — `printf` stands in for the CLI here.
    #[tokio::test]
    async fn custom_models_cmd_lists_through_list_models() {
        use crate::config::HarnessOverride;
        use crate::connectors;
        let ov = HarnessOverride {
            command: Some(vec!["myagent".into()]),
            model_flag: Some("--model".into()),
            models_cmd: vec!["printf".into(), "a\\nb\\nb\\n".into()],
            ..Default::default()
        };
        let conn = connectors::resolve("myagent", Some(&ov)).unwrap();
        let list = list_models(&conn, Some(&ov)).await.unwrap();
        assert_eq!(list.harness, "myagent");
        assert_eq!(list.models, vec!["a".to_string(), "b".to_string()]);
        assert!(list.source.contains("models_cmd"));
    }
}
