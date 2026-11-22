//! The operator's model fleet — the curated (harness, model) pairs the
//! console's Fleet picker saves to `~/.zori/fleet.json`.
//!
//! The model dropdowns and the chat picker list *everything* a CLI advertises;
//! the fleet is the operator's answer to "which of those do I actually run".
//! [`brief`] renders it as the planner-facing section appended after
//! `harness_brief` (see `start_chat`), so a proposal draws its `model:` values
//! from the operator's roster instead of the whole catalog.
//!
//! The file is machine-personal state, so it lives in the zori home next to
//! the global skills store, never in a project checkout (D11). It is small,
//! human-editable JSON:
//!
//! ```json
//! { "models": [ { "harness": "coddy", "model": "neuraldeep/kimi-k2.6" } ] }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One pick: a model on a harness. The *pair* is the identity — the same
/// model id on two harnesses (a proxy and a native CLI) is two picks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FleetEntry {
    pub harness: String,
    pub model: String,
}

/// Where the fleet lives: `~/.zori/fleet.json` (see [`crate::skills::personal_dir`]).
pub fn path() -> PathBuf {
    crate::skills::personal_dir().join("fleet.json")
}

#[derive(Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    models: Vec<FleetEntry>,
}

/// The saved fleet, de-duplicated, order preserved. A missing file is an
/// empty fleet (fresh install); a *malformed* file is also an empty fleet
/// rather than a dead console — the picker rewrites it on the next Save, and
/// an unparseable fleet must not take the planner's prompt down with it.
pub fn load() -> Vec<FleetEntry> {
    load_from(&path())
}

fn load_from(path: &Path) -> Vec<FleetEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<FileShape>(&text) {
        Ok(shape) => dedupe(shape.models),
        Err(e) => {
            // Loud but non-fatal, the same contract as an unreadable skill:
            // the operator sees why the fleet is empty and one Save fixes it.
            eprintln!(
                "  [warning] fleet file {} unparseable ({e}); ignoring it",
                path.display()
            );
            Vec::new()
        }
    }
}

/// Validate and persist atomically (tmp + rename, like the run manifest):
/// a crash mid-write leaves the previous fleet intact.
pub fn save(entries: &[FleetEntry]) -> anyhow::Result<()> {
    let clean = validate(entries)?;
    let dir = crate::skills::personal_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
    let path = path();
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(&FileShape { models: clean })
        .map_err(|e| anyhow::anyhow!("serialising the fleet: {e}"))?;
    std::fs::write(&tmp, body.as_bytes())
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| anyhow::anyhow!("renaming {} → {}: {e}", tmp.display(), path.display()))?;
    Ok(())
}

/// Check every entry against the connector table and de-duplicate. An entry
/// must name a resolve-able **agent** harness that honours per-run `model:`
/// — anything else would let the picker save a pair the runner refuses at
/// boot. The harness's *live* model list is deliberately NOT checked:
/// catalogs are slow, flaky and TTL-cached, and the Model field already lets
/// an operator type an id the CLI has not advertised.
pub fn validate(entries: &[FleetEntry]) -> anyhow::Result<Vec<FleetEntry>> {
    let mut out: Vec<FleetEntry> = Vec::new();
    for entry in entries {
        let harness = entry.harness.trim();
        let model = entry.model.trim();
        if harness.is_empty() || model.is_empty() {
            anyhow::bail!("a fleet entry needs both a harness and a model (got '{harness}' / '{model}')");
        }
        let conn = crate::connectors::resolve(harness, None)
            .map_err(|e| anyhow::anyhow!("harness '{harness}': {e:#}"))?;
        if !conn.is_agent {
            anyhow::bail!("harness '{harness}' is not an agent");
        }
        if !conn.supports_model {
            anyhow::bail!("harness '{harness}' cannot take a per-run model");
        }
        let clean = FleetEntry { harness: harness.to_string(), model: model.to_string() };
        if !out.contains(&clean) {
            out.push(clean);
        }
    }
    Ok(out)
}

fn dedupe(entries: Vec<FleetEntry>) -> Vec<FleetEntry> {
    let mut out: Vec<FleetEntry> = Vec::new();
    for e in entries {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// The planner-facing fleet section, rendered from the saved file. `None`
/// when no fleet exists — no section, no behaviour change.
pub fn brief() -> Option<String> {
    brief_for(&load())
}

/// The pure half of [`brief`], so the rendering is testable without touching
/// the operator's real `~/.zori`. Picks on a harness whose CLI has vanished
/// stay listed but marked NOT installed: the planner avoids them instead of
/// silently planning around the operator's roster.
pub fn brief_for(fleet: &[FleetEntry]) -> Option<String> {
    if fleet.is_empty() {
        return None;
    }
    let mut by_harness: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for e in fleet {
        by_harness.entry(e.harness.as_str()).or_default().push(e.model.as_str());
    }
    let rows: Vec<String> = by_harness
        .iter()
        .map(|(h, models)| {
            let installed = crate::connectors::resolve(h, None).ok().and_then(|c| {
                c.command.first().map(|cmd| (cmd.clone(), c.env.get("PATH").cloned()))
            }).map(|(cmd, path)| crate::catalog::executable_found(&cmd, path.as_ref()))
                .unwrap_or(false);
            let marker = if installed { "" } else { " — NOT installed on this machine; do not use" };
            format!("- **{h}**{marker}: {}", models.join(", "))
        })
        .collect();
    Some(format!(
        "# Model fleet (the operator's picks)\n\n\
         The operator chose these models for this machine. Pick `model:` values from this list when a task fits one; a model outside it needs a reason the operator would accept.\n\n{}",
        rows.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(h: &str, m: &str) -> FleetEntry {
        FleetEntry { harness: h.into(), model: m.into() }
    }

    #[test]
    fn validate_keeps_order_dedupes_and_trims() {
        let got = validate(&[
            pick(" coddy ", " neuraldeep/kimi-k2.6 "),
            pick("codex", "gpt-6.2-sol"),
            pick("coddy", "neuraldeep/kimi-k2.6"),
        ])
        .unwrap();
        assert_eq!(got, vec![pick("coddy", "neuraldeep/kimi-k2.6"), pick("codex", "gpt-6.2-sol")]);
    }

    #[test]
    fn validate_rejects_empty_and_non_agent() {
        assert!(validate(&[pick("coddy", "")]).is_err(), "empty model");
        assert!(validate(&[pick("", "m")]).is_err(), "empty harness");
        assert!(validate(&[pick("echo", "x")]).is_err(), "echo is not an agent");
        assert!(validate(&[pick("definitely-not-a-connector", "x")]).is_err(), "unknown harness");
        // zcode is an agent but has no per-run model flag (D5): the fleet is
        // exactly the wrong place for it.
        assert!(validate(&[pick("zcode", "x")]).is_err(), "zcode cannot take a model");
    }

    // The save/load round-trip must never touch the operator's real ~/.zori:
    // an explicit path is the test seam, same principle as Resolver::with_dirs.
    fn tempdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zori-fleet-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn save_and_load_round_trip_atomically() {
        let dir = tempdir("roundtrip");
        let file = dir.join("fleet.json");
        std::fs::write(
            &file,
            serde_json::to_string_pretty(&FileShape {
                models: vec![pick("coddy", "neuraldeep/kimi-k2.6"), pick("codex", "gpt-6.2-sol")],
            })
            .unwrap(),
        )
        .unwrap();
        let loaded = load_from(&file);
        assert_eq!(loaded.len(), 2);
        // no tmp file left behind by a hypothetical rewrite
        assert!(!dir.join("fleet.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty_malformed_is_empty_not_a_panic() {
        let dir = tempdir("malformed");
        assert!(load_from(&dir.join("absent.json")).is_empty());
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        assert!(load_from(&bad).is_empty(), "malformed fleet is ignored, not fatal");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn brief_lists_models_by_harness_and_marks_missing_clis() {
        let fleet = vec![
            pick("definitely-not-a-cli", "some/model"),
            pick("definitely-not-a-cli", "other/model"),
        ];
        let text = brief_for(&fleet).unwrap();
        assert!(text.contains("# Model fleet"), "carries its heading");
        assert!(text.contains("**definitely-not-a-cli** — NOT installed"), "flags a vanished CLI: {text}");
        assert!(text.contains("some/model, other/model"), "groups one harness's models on one row");
        assert!(brief_for(&[]).is_none(), "no fleet → no section at all");
    }
}
