//! zori CLI.
//!
//!   zori run <flow.yml> [--input k=v]... [--dry-run]
//!
//! The flow file is YAML describing tasks wired into a DAG. Each task runs one
//! prompt through one CLI harness (coddy/codex/zcode/opencode/echo) with a
//! chosen model. Run artifacts land under `zori-runs/` (override:
//! `ZORI_RUNS_ROOT`).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use zori::config::Flow;
use zori::runner;

#[derive(Parser)]
#[command(name = "zori", version, about = "Run a task across several CLI coding agents as a small DAG")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a flow defined in a YAML file.
    Run {
        /// Path to the flow YAML.
        flow: PathBuf,
        /// Override/add a flow input, e.g. --input prompt="fix the bug".
        #[arg(long = "input", value_parser = parse_kv)]
        inputs: Vec<(String, String)>,
        /// Print the plan and the command each task would run; spawn nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Emit the planner's JSON inventory of harnesses, models, limits and operator notes.
    Catalog {
        /// Include overrides and routing notes from this trusted flow.
        #[arg(long)]
        flow: Option<PathBuf>,
    },
    /// Print the built-in harnesses and whether each supports per-run model.
    Harnesses,
    /// List the skills available to attach to a task: built-ins plus every
    /// `<name>.md` in the per-project `zori-skills/` and the global
    /// `~/.zori/skills`. Pass a flow to include its `skills_dirs` too.
    Skills {
        /// Also search this flow's `skills_dirs`.
        #[arg(long)]
        flow: Option<PathBuf>,
    },
    /// List the models a harness actually accepts on this machine, pulled
    /// live from its CLI (coddy config, `codex debug models`, `opencode
    /// models`). With no harness, lists every model-capable built-in.
    Models {
        /// Harness id (coddy, codex, opencode, …). Omit for all.
        harness: Option<String>,
    },
    /// Serve the interactive console: run a flow (or none) and steer its
    /// agents live from a browser at the printed URL.
    Ui {
        /// Optional flow YAML to boot immediately; prior runs are reloaded
        /// regardless, and new flows can be booted from the UI.
        flow: Option<PathBuf>,
        /// Override/add a flow input for the initial flow, e.g. --input prompt="…".
        #[arg(long = "input", value_parser = parse_kv)]
        inputs: Vec<(String, String)>,
        /// Address:port to bind. Local only by default; the console has no auth.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,
        /// Directory holding each run's state.json, artifacts and sessions.
        #[arg(long, default_value = "zori-runs")]
        runs_root: PathBuf,
        /// Directory the flow builder saves its YAML to (created if missing).
        #[arg(long, default_value = "zori-flows")]
        flows_root: PathBuf,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got '{s}'"))?;
    Ok((k.to_string(), v.to_string()))
}

#[tokio::main]
async fn main() {
    if let Err(e) = real().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn real() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Harnesses => {
            for id in zori::connectors::builtin_ids() {
                let c = zori::connectors::resolve(id, None)?;
                println!(
                    "{:<6} model-per-run={}  cmd={}  prompt={:?}",
                    c.id,
                    c.supports_model,
                    c.command.join(" "),
                    c.prompt_via
                );
            }
            Ok(())
        }
        Command::Skills { flow } => {
            // With a flow we honour its extra `skills_dirs` too; without one
            // we list what the operator can reach right now (project + global
            // + built-ins), so a skill can be picked before any flow exists.
            let resolver = match flow {
                Some(path) => {
                    let yaml = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    let flow: Flow = serde_yaml::from_str(&yaml)
                        .with_context(|| format!("parsing flow {}", path.display()))?;
                    zori::skills::Resolver::for_flow(&flow)
                }
                None => zori::skills::Resolver::standard(),
            };
            println!("skill stores searched:");
            for d in resolver.dirs() {
                println!("  {}", d.display());
            }
            println!("  (built-ins compiled into zori)");
            let all = resolver.list();
            if all.is_empty() {
                println!("\n(no skills found — add <name>.md to a store above)");
                return Ok(());
            }
            println!("\n{} skill(s):", all.len());
            for s in &all {
                let src = match &s.source {
                    zori::skills::SkillSource::BuiltIn => "built-in".to_string(),
                    zori::skills::SkillSource::File(p) => p.display().to_string(),
                };
                match &s.description {
                    Some(d) => println!("  {:<18} {d}", s.name),
                    None => println!("  {:<18}", s.name),
                }
                println!("      {src}");
            }
            Ok(())
        }
        Command::Catalog { flow } => {
            let overrides = match flow {
                Some(path) => {
                    let yaml = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                    serde_yaml::from_str::<Flow>(&yaml)?.harnesses
                }
                None => Default::default(),
            };
            let catalog = zori::catalog::discover(&overrides).await;
            println!("{}", serde_json::to_string_pretty(&catalog)?);
            Ok(())
        }
        Command::Models { harness } => {
            let ids: Vec<String> = match harness {
                Some(h) => vec![h],
                // All built-ins that take a per-run model, in catalog order.
                None => {
                    let mut v = Vec::new();
                    for id in zori::connectors::builtin_ids() {
                        if let Ok(c) = zori::connectors::resolve(id, None) {
                            if c.supports_model {
                                v.push(c.id);
                            }
                        }
                    }
                    v
                }
            };
            let mut failed = false;
            for id in ids {
                // resolve fails loudly for an unknown id — the user's typo
                // should say so, not silently print nothing.
                let conn = zori::connectors::resolve(&id, None)?;
                match zori::models::list_models(&conn, None).await {
                    Ok(list) => {
                        println!("{} ({}):", list.harness, list.source);
                        if let Some(d) = &list.default {
                            println!("  default: {d}");
                        }
                        for m in &list.models {
                            println!("  {m}");
                        }
                        if list.models.is_empty() {
                            println!("  (none reported)");
                        }
                    }
                    Err(e) => {
                        failed = true;
                        println!("{id}: error: {e:#}");
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Run { flow, inputs, dry_run } => {
            let text = std::fs::read_to_string(&flow)
                .with_context(|| format!("reading flow {}", flow.display()))?;
            let flow: Flow = serde_yaml::from_str(&text)
                .with_context(|| format!("parsing flow {}", flow.display()))?;

            let plan = runner::plan(&flow)?;
            if dry_run {
                return runner::dry_run(&flow, &plan, &inputs);
            }

            let report = runner::run(&flow, &plan, &inputs, &zori::artifacts::runs_root()).await?;
            let degraded = report.degraded();
            let degraded_note = if degraded > 0 {
                format!(", {degraded} degraded (best-effort)")
            } else {
                String::new()
            };
            println!(
                "\nflow '{}': {} done, {} failed, {} skipped{degraded_note} → {}",
                flow.name,
                report.succeeded(),
                report.failed(),
                report.skipped(),
                report
                    .out_dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            );
            if report.failed() > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Ui { flow, inputs, bind, runs_root, flows_root } => {
            let addr: SocketAddr = bind
                .parse()
                .with_context(|| format!("invalid --bind '{bind}' (want ADDR:PORT, e.g. 127.0.0.1:8787)"))?;
            zori::ui::start(addr, runs_root, flows_root, flow, inputs).await
        }
    }
}
