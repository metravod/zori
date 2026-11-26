//! zori CLI.
//!
//!   zori run <flow.yml> [--input k=v]... [--dry-run]
//!
//! The flow file is YAML describing tasks wired into a DAG. Each task runs one
//! prompt through one CLI harness (coddy/codex/zcode/opencode/echo) with a
//! chosen model. Run artifacts land under `zori-runs/` (override:
//! `ZORI_RUNS_ROOT`).

use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use zori::config::Flow;
use zori::runner;
use zori::state::{Emitter, RunEvent, RunState, RunStatus, TaskStatus};

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
            let personal_ids = zori::connectors::personal_harnesses();
            for id in zori::connectors::all_ids() {
                let c = zori::connectors::resolve(&id, None)?;
                let src = if personal_ids.contains_key(&id) {
                    if zori::connectors::builtin_ids().contains(&id.as_str()) { "builtin+personal" } else { "personal" }
                } else {
                    "builtin"
                };
                let found = c.command.first()
                    .map(|cmd| zori::catalog::executable_found(cmd, c.env.get("PATH")))
                    .unwrap_or(false);
                println!(
                    "{:<10} {:<18} model-per-run={}  found={}  cmd={}  prompt={:?}",
                    c.id,
                    src,
                    c.supports_model,
                    if found { "yes" } else { "no" },
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
            // Personal harnesses participate even without --flow; a flow's own
            // block wins per id on top of them.
            let mut overrides = zori::connectors::personal_harnesses();
            if let Some(path) = flow {
                let yaml = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                overrides.extend(serde_yaml::from_str::<Flow>(&yaml)?.harnesses);
            }
            let catalog = zori::catalog::discover(&overrides).await;
            println!("{}", serde_json::to_string_pretty(&catalog)?);
            Ok(())
        }
        Command::Models { harness } => {
            let ids: Vec<String> = match harness {
                Some(h) => vec![h],
                // Everything resolvable here that takes a per-run model.
                None => {
                    let mut v = Vec::new();
                    for id in zori::connectors::all_ids() {
                        if let Ok(c) = zori::connectors::resolve(&id, None) {
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
                // A personal harness lists models through its models_cmd.
                let personal = zori::connectors::personal_harnesses().get(&id).cloned();
                match zori::models::list_models(&conn, personal.as_ref()).await {
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
        Command::Run { flow: path, inputs, dry_run } => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading flow {}", path.display()))?;
            let flow: Flow = serde_yaml::from_str(&text)
                .with_context(|| format!("parsing flow {}", path.display()))?;

            let plan = runner::plan(&flow)?;
            let interactive = io::stdin().is_terminal();
            if dry_run {
                return runner::dry_run(&flow, &plan, &inputs, !interactive);
            }

            run_in_terminal(flow, plan, path, &inputs, interactive).await
        }
        Command::Ui { flow, inputs, bind, runs_root, flows_root } => {
            let addr: SocketAddr = bind
                .parse()
                .with_context(|| format!("invalid --bind '{bind}' (want ADDR:PORT, e.g. 127.0.0.1:8787)"))?;
            zori::ui::start(addr, runs_root, flows_root, flow, inputs).await
        }
    }
}

async fn run_in_terminal(
    flow: Flow,
    plan: zori::dag::ExecutionPlan,
    path: PathBuf,
    inputs: &[(String, String)],
    interactive: bool,
) -> Result<()> {
    let wanted = zori::artifacts::new_run_id(&flow.name);
    let (out_dir, run_id) =
        zori::artifacts::reserve_run_dir(&zori::artifacts::runs_root(), &wanted)
            .with_context(|| format!("reserving run dir for '{wanted}'"))?;
    let tasks = flow
        .tasks
        .iter()
        .map(|t| {
            (
                t.id.clone(),
                t.harness.clone(),
                t.model.clone(),
                t.optional,
                t.depends_on.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut state = RunState::new(
        run_id.clone(),
        flow.name.clone(),
        path,
        Some(out_dir.clone()),
        plan.levels.clone(),
        &tasks,
    );
    state.recompute_status();
    let emitter = Emitter::new(state).with_event_handler(|event| match event {
        RunEvent::Notice(..) => eprintln!("{event}"),
        _ => println!("{event}"),
    });
    let tracked = runner::TrackedRun::new(
        flow,
        plan,
        inputs,
        out_dir.clone(),
        out_dir.join("sessions"),
    )
    .with_context(|| format!("initialising run '{run_id}'"))?
    .with_unattended(!interactive);

    if interactive {
        let drive = tracked.run_all(&emitter);
        tokio::pin!(drive);
        tokio::select! {
            () = &mut drive => {}
            result = answer_questions(&tracked, &emitter) => {
                if let Err(e) = result {
                    eprintln!("operator input: {e:#}");
                }
                tracked.close_operator_input();
                drive.await;
            }
        }
    } else {
        tracked.run_all(&emitter).await;
    }

    let state = emitter.snapshot();
    let (mut done, mut failed, mut skipped, mut degraded) = (0, 0, 0, 0);
    for node in state.nodes.values() {
        match node.status {
            TaskStatus::Done => done += 1,
            TaskStatus::Failed if node.optional => degraded += 1,
            TaskStatus::Failed => failed += 1,
            TaskStatus::Skipped => skipped += 1,
            _ => {}
        }
    }
    let degraded_note = if degraded > 0 {
        format!(", {degraded} degraded (best-effort)")
    } else {
        String::new()
    };
    println!(
        "\nflow '{}': {done} done, {failed} failed, {skipped} skipped{degraded_note} → {}",
        state.flow_name,
        out_dir.display(),
    );
    match state.status {
        RunStatus::Completed => Ok(()),
        RunStatus::Failed => std::process::exit(1),
        _ => anyhow::bail!("flow '{}' did not complete", state.flow_name),
    }
}

async fn answer_questions(tracked: &runner::TrackedRun, emitter: &Emitter) -> Result<()> {
    let mut updates = emitter.subscribe();
    let requests = start_terminal_reader()?;
    loop {
        let snapshot = emitter.snapshot();
        let Some((id, question)) = snapshot.nodes.iter().find_map(|(id, node)| {
            matches!(node.status, TaskStatus::Waiting | TaskStatus::Running)
                .then(|| {
                    node.questions
                        .iter()
                        .find(|q| !q.closed && q.answer.is_none())
                })
                .flatten()
                .map(|question| (id, question))
        }) else {
            let _ = updates.recv().await;
            continue;
        };

        {
            let mut stderr = io::stderr().lock();
            writeln!(stderr, "  [{id}] ASK: {}", question.text)?;
            if !question.options.is_empty() {
                writeln!(stderr, "  Options: {}", question.options.join(" / "))?;
            }
            write!(stderr, "  Answer: ")?;
            stderr.flush()?;
        }
        let (reply, mut answer) = tokio::sync::oneshot::channel();
        requests
            .send(reply)
            .ok()
            .context("terminal reader stopped")?;
        loop {
            tokio::select! {
                value = &mut answer => {
                    let line = value.context("terminal reader stopped")??;
                    if line.is_empty() {
                        return Ok(());
                    }
                    let text = line.trim();
                    if !text.is_empty() {
                        if let Err(e) = tracked.answer_question(emitter, id, Some(question.id), text, None) {
                            eprintln!("  [{id}] {e}");
                        }
                    }
                    break;
                }
                _ = updates.recv() => {
                    let current = emitter.snapshot();
                    let node = &current.nodes[id];
                    let still_open = matches!(node.status, TaskStatus::Waiting | TaskStatus::Running)
                        && node.questions.iter().any(|q| q.id == question.id && !q.closed && q.answer.is_none());
                    if !still_open {
                        eprintln!();
                        break;
                    }
                }
            }
        }
    }
}

type AnswerRequest = tokio::sync::oneshot::Sender<io::Result<String>>;

fn start_terminal_reader() -> io::Result<std::sync::mpsc::Sender<AnswerRequest>> {
    #[cfg(unix)]
    let mut input = {
        use std::os::fd::AsFd;
        let stdin = io::stdin().as_fd().try_clone_to_owned()?;
        io::BufReader::new(std::fs::File::from(stdin))
    };
    let (tx, requests) = std::sync::mpsc::channel::<AnswerRequest>();
    // Detached from Tokio: a run can finish while the terminal still has
    // an outstanding read. Dropping the reply receiver cancels that request.
    std::thread::Builder::new()
        .name("zori-terminal-input".into())
        .spawn(move || {
            for reply in requests {
                #[cfg(unix)]
                let result = read_terminal_line(&mut input, &reply);
                #[cfg(not(unix))]
                let result = {
                    let mut line = String::new();
                    io::stdin().read_line(&mut line).map(|_| line)
                };
                let _ = reply.send(result);
            }
        })?;
    Ok(tx)
}

#[cfg(unix)]
fn read_terminal_line(
    input: &mut io::BufReader<std::fs::File>,
    reply: &AnswerRequest,
) -> io::Result<String> {
    use std::io::BufRead;
    use std::os::fd::AsRawFd;

    // Poll before reading so a question that times out releases stdin for
    // the next question even if the operator never presses Enter.
    let mut fd = libc::pollfd {
        fd: input.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    while input.buffer().is_empty() && !reply.is_closed() {
        match unsafe { libc::poll(&mut fd, 1, 100) } {
            0 => continue,
            n if n > 0 => break,
            _ => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }
    }
    let mut line = String::new();
    if !reply.is_closed() {
        input.read_line(&mut line)?;
    }
    Ok(line)
}
