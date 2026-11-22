//! The driver: run a [`Flow`]'s DAG by executing each task as a child process
//! of its harness, level by level, and feeding each task's stdout to whoever
//! depends on it.
//!
//! This is the "own driver" half of D1: zymi's executor is not reusable, so we
//! write the ~300 lines that shell out to a CLI, capture its output, and wire
//! outputs into downstream prompts. Semantics we deliberately *fix* relative to
//! zymi (where docs and code disagreed — see the explorer report):
//!   * A failed task cascades: tasks that (transitively) depend on it are
//!     skipped. Independent branches still run. zymi only cascaded on
//!     `when:false`, not on failure; for a review fan-out the intuitive
//!     behaviour is to not run a joiner whose input died.
//!   * We keep zymi's level barrier (a level's slowest task gates the level)
//!     because it is simple and deterministic; parallelism inside a level is
//!     capped by `max_parallel`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, Semaphore};

use crate::artifacts::{self, AttemptMeta, AttemptRecord, ManifestStore, OutFile, RunDirs};
use crate::config::{Flow, PromptVia, Task, Workspace};
use crate::connectors::{self, Connector};
use crate::dag::{build_plan, ExecutionPlan};
use crate::state::{Emitter, FlowProposal, ProposalStatus, Role, SessionRef, TaskStatus};
use crate::watchdog::{Attempt, Settings, Verdict};

/// Default per-task timeout when neither the flow nor the task sets one.
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// The flow-level timeout, falling back to [`DEFAULT_TIMEOUT_SECS`] when the
/// flow omits `default_timeout_secs`. Single home for the flow default so the
/// classic and tracked drivers cannot drift on what "no flow timeout" means.
fn flow_default_timeout(flow: &Flow) -> u64 {
    flow.default_timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// The timeout that actually applies to a task: its own `timeout_secs` when
/// set, else the already-resolved flow default. Task wins over flow — this
/// precedence is the whole contract, so it lives in exactly one place.
fn timeout_for(task: &Task, flow_default_secs: u64) -> Duration {
    Duration::from_secs(task.timeout_secs.unwrap_or(flow_default_secs))
}

/// Seconds between heartbeat lines for a still-running task. A run that looks
/// dead for minutes (the first real run hung on a deadlock and looked the same
/// as one that was merely slow) is impossible to tell apart without this.
const HEARTBEAT_SECS: u64 = 30;

/// Process groups of currently-spawned children (each == that child's pid,
/// because we spawn with `process_group(0)`). The signal handler walks this to
/// kill the whole tree on Ctrl-C/SIGTERM; a 12-minute orphan burning tokens
/// after the runner died was the motivator.
static RUNNING_GROUPS: StdMutex<Vec<i32>> = StdMutex::new(Vec::new());

/// Register the current child group and unregister it on drop (which fires
/// even on the `?`/panic paths, so a reaped pid never lingers in the list and
/// a later `killpg` never targets a recycled pid).
pub(crate) struct GroupRegistration {
    pgid: Option<i32>,
}

impl GroupRegistration {
    pub(crate) fn register(pid: Option<u32>) -> Self {
        let pgid = pid.map(|p| p as i32);
        if let Some(pg) = pgid {
            RUNNING_GROUPS.lock().unwrap().push(pg);
        }
        Self { pgid }
    }

    /// SIGTERM the group, give it a moment to exit on its own, then SIGKILL.
    /// Reaching the whole group (not just the direct child) is the point: the
    /// agent CLIs spawn their own tool subprocesses.
    async fn kill_group(&self) {
        let Some(pg) = self.pgid else { return };
        #[cfg(unix)]
        {
            // ESRCH (already gone) is fine to ignore here.
            unsafe { libc::killpg(pg, libc::SIGTERM) };
            tokio::time::sleep(Duration::from_millis(1500)).await;
            unsafe { libc::killpg(pg, libc::SIGKILL) };
        }
    }
}

impl Drop for GroupRegistration {
    fn drop(&mut self) {
        if let Some(pg) = self.pgid {
            let mut g = RUNNING_GROUPS.lock().unwrap();
            if let Some(pos) = g.iter().position(|&x| x == pg) {
                g.remove(pos);
            }
        }
    }
}

/// Install Ctrl-C / SIGTERM handling. Without this, tearing down the tokio
/// runtime does not run `Drop` on the in-flight `Command`s, so `kill_on_drop`
/// never fires and every agent CLI becomes an orphan. On a signal we kill all
/// live child groups and exit — we do not try to unwind the run.
pub fn install_death_handlers() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tokio::spawn(async {
            #[cfg(unix)]
            let sig = {
                use tokio::signal::unix::{signal, SignalKind};
                let mut term = match signal(SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            };
            #[cfg(not(unix))]
            let sig = {
                tokio::signal::ctrl_c().await.ok();
                "SIGINT"
            };
            // die() exits the process, so there is nothing to re-arm: one
            // wait, then every live child group dies with us.
            die(sig);
        });
    });
}

/// Kill every registered child group and exit with a signal-appropriate code.
fn die(sig: &str) -> ! {
    let pgs: Vec<i32> = RUNNING_GROUPS.lock().unwrap().clone();
    if !pgs.is_empty() {
        eprintln!(
            "zori: {sig} received — killing {} running child group(s)",
            pgs.len()
        );
        #[cfg(unix)]
        {
            for &pg in &pgs {
                unsafe { libc::killpg(pg, libc::SIGTERM) };
            }
            std::thread::sleep(Duration::from_millis(1500));
            for &pg in &pgs {
                unsafe { libc::killpg(pg, libc::SIGKILL) };
            }
        }
    }
    // 128 + signal: 130 for SIGINT, 143 for SIGTERM (conventional).
    std::process::exit(if sig == "SIGTERM" { 143 } else { 130 });
}

/// Result of one executed task.
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub id: String,
    pub status: Status,
    /// Captured stdout (the value referenced by `${steps.<id>.output}`).
    pub output: String,
    /// Captured stderr, kept for diagnostics and shown on failure.
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// Mirrors `Task::optional`. A failed OPTIONAL branch neither blocks
    /// downstream nor fails the flow: its `${steps.<id>.output}` feeds a
    /// "did not complete" placeholder instead. See D3.
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Done,
    Failed,
    Skipped,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
        }
    }
}

/// Outcome of a whole flow run.
pub struct RunReport {
    pub results: HashMap<String, TaskResult>,
    pub out_dir: Option<PathBuf>,
}

impl RunReport {
    pub fn succeeded(&self) -> usize {
        self.results.values().filter(|r| r.status == Status::Done).count()
    }
    /// Hard failures: tasks that must complete and didn't. Drives the exit
    /// code. Best-effort (optional) branches are excluded — see `degraded`.
    pub fn failed(&self) -> usize {
        self.results
            .values()
            .filter(|r| r.status == Status::Failed && !r.optional)
            .count()
    }
    /// Best-effort (`optional: true`) branches that failed. They neither block
    /// downstream nor fail the flow; count them so they don't go silent.
    pub fn degraded(&self) -> usize {
        self.results
            .values()
            .filter(|r| r.status == Status::Failed && r.optional)
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.results.values().filter(|r| r.status == Status::Skipped).count()
    }
}

/// Build the execution plan for a flow, surfacing DAG errors as `anyhow`.
pub fn plan(flow: &Flow) -> Result<ExecutionPlan> {
    let pairs: Vec<(String, Vec<String>)> = flow
        .tasks
        .iter()
        .map(|t| (t.id.clone(), t.depends_on.clone()))
        .collect();
    let plan = build_plan(&pairs).map_err(|e| anyhow!("flow '{}': {e}", flow.name))?;
    Ok(plan)
}

/// Print the plan and the argv each task would run, without spawning anything.
/// Used by `zori run --dry-run` and by the echo smoke tests. Honours `--input`
/// overrides so the printed argv is what a real run would execute.
pub fn dry_run(
    flow: &Flow,
    plan: &ExecutionPlan,
    extra_inputs: &[(String, String)],
) -> Result<()> {
    println!("flow '{}' — {} task(s) in {} level(s)", flow.name, plan.step_count(), plan.levels.len());
    for (li, level) in plan.levels.iter().enumerate() {
        println!("  level {li}: {}", level.join(", "));
    }
    let inputs = resolved_inputs(flow, extra_inputs);
    let mut outputs: HashMap<String, String> = HashMap::new();
    for level in &plan.levels {
        for id in level {
            let task = flow.tasks.iter().find(|t| &t.id == id).unwrap();
            let conn = connector_for(flow, task)?;
            // M10: preview paths (placeholders for the not-yet-created tree)
            // and the same cwd choice the real drivers make, so the printed
            // argv is honest: path refs interpolate instead of erroring, and
            // a workspace:task codex task shows its real `-C`.
            let (ctx, flag_cwd) = dry_preview(flow, task);
            let preamble = crate::watchdog::preamble_for_classic(flow, task)
                .filter(|_| crate::watchdog::preamble_applies(&conn));
            let skills = skill_block(flow, task)?;
            let prompt = render_prompt(task, &inputs, &outputs, &ctx, preamble.as_deref(), skills.as_deref())?;
            let mut sp = connectors::plan(&conn, task, &prompt, flag_cwd.as_deref())?;
            if let Some(f) = &sp.file_prompt {
                let _ = f;
                for tok in sp.argv.iter_mut() {
                    if tok == "__ZORI_PROMPT_FILE__" {
                        *tok = "<prompt-file>".into();
                    }
                }
            }
            if task.cwd.is_none() && task.workspace.unwrap_or(Workspace::Task) == Workspace::Task {
                println!("         (isolated cwd: <run>/tasks/{id}/work — created at run time)");
            }
            // sp.argv already begins with the program, so print it alone.
            println!(
                "  [{}] {}  (harness={}, model={:?})",
                id,
                render_argv(&sp.argv),
                task.harness,
                task.model
            );
            // so downstream dry-run renders see a placeholder
            outputs.insert(id.clone(), format!("<output of {id}>"));
        }
    }
    Ok(())
}

/// Render an argv for display and diagnostics — **not** a byte-exact command
/// to re-run. Tokens that are unsafe to show bare (anything outside a small
/// shell-safe set: a shell metacharacter, a quote, whitespace, or empty) are
/// single-quoted, with an embedded `'` closed and escaped as the POSIX `'\''`
/// idiom, so `$(rm -rf)`, backticks, and `"` can never look like live shell
/// syntax. Control characters are shown as `\n`/`\r`/`\t` to keep one line per
/// task; that substitution is exactly why this is diagnostic rather than
/// copy-pasteable — the earlier version left `$(rm -rf)` unquoted and did not
/// escape `$`/backtick inside its double quotes, promising a copy-paste line
/// that could run something other than the argv it displayed.
fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|tok| {
            // The set POSIX leaves expandable-free when unquoted (plus `-`).
            let safe = !tok.is_empty()
                && tok.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(c, '_' | '@' | '%' | '+' | '=' | ':' | ',' | '.' | '-')
                });
            if safe {
                return tok.clone();
            }
            // Single quotes: nothing is expanded inside them, so `$`, backtick
            // and `"` stay literal; only the quote itself needs the close/escape
            // dance, and backslash/newline are escaped so the line stays one
            // physical line.
            let mut s = String::with_capacity(tok.len() + 2);
            s.push('\'');
            for c in tok.chars() {
                match c {
                    '\'' => s.push_str("'\\''"),
                    '\\' => s.push_str("\\\\"),
                    '\n' => s.push_str("\\n"),
                    '\r' => s.push_str("\\r"),
                    '\t' => s.push_str("\\t"),
                    _ => s.push(c),
                }
            }
            s.push('\'');
            s
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Everything the artifact layer needs while a run executes: the run's
/// directory tree, its manifest, and the project dir agents are pointed at.
/// Bundled once per run and cloned into each spawned task.
#[derive(Clone)]
pub struct RunArtifacts {
    pub dirs: RunDirs,
    pub manifest: ManifestStore,
    pub project_dir: Option<PathBuf>,
}

impl RunArtifacts {
    /// A fresh run tree + manifest (classic `zori run`).
    fn create(
        runs_root: &Path,
        flow: &Flow,
        plan: &ExecutionPlan,
        inputs: &HashMap<String, String>,
    ) -> Result<Self> {
        let run_id = artifacts::new_run_id(&flow.name);
        let dirs = RunDirs::create(runs_root, &run_id)?;
        // M9: create() may have claimed a suffixed name; the manifest must
        // carry the id the folder actually landed on.
        let run_id = dirs.run_id();
        let manifest = Self::init_manifest(&dirs, &run_id, flow, plan, inputs)?;
        let project_dir = project_dir_of(flow);
        Ok(RunArtifacts { dirs, manifest, project_dir })
    }

    /// Adopt an existing run directory (the tracked engine, incl. reloads):
    /// the manifest is loaded when present, initialised when the run predates
    /// manifests — old flat artifacts stay untouched on disk.
    fn adopt(
        out_dir: &Path,
        flow: &Flow,
        plan: &ExecutionPlan,
        inputs: &HashMap<String, String>,
    ) -> Result<Self> {
        let dirs = RunDirs::open(out_dir.to_path_buf());
        let run_id = out_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let manifest = Self::init_manifest(&dirs, &run_id, flow, plan, inputs)?;
        let project_dir = project_dir_of(flow);
        Ok(RunArtifacts { dirs, manifest, project_dir })
    }

    fn init_manifest(
        dirs: &RunDirs,
        run_id: &str,
        flow: &Flow,
        plan: &ExecutionPlan,
        inputs: &HashMap<String, String>,
    ) -> Result<ManifestStore> {
        let inputs_btree: BTreeMap<String, String> =
            inputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let m = artifacts::Manifest::new(
            run_id,
            &flow.name,
            flow.cwd.as_deref().map(artifacts::absolute),
            &inputs_btree,
            &plan.levels,
        );
        // Fresh runs rewrite; reloaded runs keep what previous attempts wrote.
        if dirs.manifest_path().is_file() {
            ManifestStore::load_or_init(dirs, m)
        } else {
            ManifestStore::init(dirs, m)
        }
    }
}

/// Resolve where a task's process runs and what its harness cwd flag says:
/// an explicit `task.cwd` wins (flag mirrors it, as always); otherwise
/// `workspace: task` (the default) runs in the task's isolated work dir and
/// tells the harness so; `workspace: flow` keeps the pre-isolation behaviour
/// (the flow cwd / zori cwd, no flag).
///
/// Both returned paths are ABSOLUTE. The child is spawned with its process
/// cwd already set to this directory, and harnesses resolve their cwd flag
/// (`-C` for codex, `--cwd` for coddy) relative to their own process cwd —
/// a relative flag therefore doubles the path and the CLI dies instantly with
/// ENOENT (the live zori-review failure of 2026-11-17: codex was pointed at
/// `<work>/zori-runs/<run>/tasks/<id>/work`, which never exists).
pub(crate) fn effective_cwd(
    flow_cwd: &Option<PathBuf>,
    task: &Task,
    work: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    if let Some(c) = &task.cwd {
        let abs = artifacts::absolute(c);
        return Ok((abs.clone(), Some(abs)));
    }
    match task.workspace.unwrap_or(Workspace::Task) {
        Workspace::Task => {
            let w = work.context("workspace: task needs the run's work dir")?;
            let abs = artifacts::absolute(w);
            Ok((abs.clone(), Some(abs)))
        }
        Workspace::Flow => {
            let base = flow_cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            // Same tilde/relative treatment as the other branches (review F05):
            // a flow with `cwd: ~/x` and `workspace: flow` otherwise spawns in
            // the literal `<zori>/~/x`.
            Ok((artifacts::absolute(&base), None))
        }
    }
}

/// The project dir agents are pointed at: the flow's `cwd`, else zori's own
/// working directory — what the README promises for `${project.dir}` ("the
/// flow `cwd`, or zori's own"). Review M10: the real driver left this empty
/// whenever `flow.cwd` was unset, so the documented fallback never happened
/// and `${project.dir}`/`ZORI_PROJECT_DIR` silently disappeared.
fn project_dir_of(flow: &Flow) -> Option<PathBuf> {
    match flow.cwd.as_deref() {
        Some(c) => Some(artifacts::absolute(c)),
        None => std::env::current_dir().ok().as_deref().map(artifacts::absolute),
    }
}

/// Review M10: the preview ctx and the `-C`/`--cwd` flag [`dry_run`] would
/// show. A dry run has no tree, so the paths are built in the *shape* the
/// real run gets (`<runs-root>/<run-id>/tasks/<id>[/work]`, the run id left
/// as a literal placeholder), and the cwd choice goes through the very same
/// [`effective_cwd`] the drivers use. Previously dry-run interpolated with an
/// empty ctx — any `${task.work}` / `${run.dir}` / `${task.dir}` /
/// `${project.dir}` died with "unknown reference" — and passed only
/// `task.cwd` as the flag, so a default codex task's printed argv silently
/// lost its `-C`: the preview claimed a command that would never run.
fn dry_preview(flow: &Flow, task: &Task) -> (PromptCtx, Option<PathBuf>) {
    let run_dir = artifacts::runs_root().join("<run-id>");
    let task_dir = run_dir.join("tasks").join(artifacts::sanitize_component(&task.id));
    let work_dir = task_dir.join("work");
    // The task-workspace branch (the only one that can error) gets its work
    // dir, so the Err arm is unreachable by construction.
    let (_cwd, flag_cwd) = effective_cwd(&flow.cwd, task, Some(&work_dir))
        .expect("dry preview always supplies a work dir");
    (
        PromptCtx {
            run_dir: Some(run_dir),
            task_dir: Some(task_dir),
            work_dir: Some(work_dir),
            project_dir: project_dir_of(flow),
        },
        flag_cwd,
    )
}

/// Env var names a child receives, for `meta.json` — names only, never values.
fn env_keys(conn: &Connector, task: &Task) -> Vec<String> {
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

/// Execute the flow. `extra_inputs` (from `--input k=v`) override flow inputs;
/// `runs_root` is where the run tree is created (`artifacts::runs_root()`).
pub async fn run(
    flow: &Flow,
    plan: &ExecutionPlan,
    extra_inputs: &[(String, String)],
    runs_root: &Path,
) -> Result<RunReport> {
    // Ctrl-C/SIGTERM to zori must take the agent CLIs down with it, not leave
    // them orphaned (one ran 12 minutes past a dead parent on the first real
    // run). Idempotent, so calling it from every run() entry is safe.
    install_death_handlers();

    let inputs = resolved_inputs(flow, extra_inputs);
    let art = RunArtifacts::create(runs_root, flow, plan, &inputs)?;
    let out_dir = Some(art.dirs.root().to_path_buf());
    let default_timeout = flow_default_timeout(flow);

    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut results: HashMap<String, TaskResult> = HashMap::new();

    for (li, level) in plan.levels.iter().enumerate() {
        println!("── level {li}: {}", level.join(", "));
        // Partition this level into runnable vs skipped-because-a-dep-died.
        let mut runnable: Vec<&Task> = Vec::new();
        for id in level {
            let task = flow.tasks.iter().find(|t| &t.id == id).unwrap();
            // A dependency blocks this task unless it completed, or it is a
            // best-effort (`optional: true`) branch that fell over — optional
            // producers never block a join (see Task::optional, D3). A missing
            // dep is treated as blocking; build_plan guarantees deps exist.
            let blocked = task.depends_on.iter().any(|d| {
                !results
                    .get(d)
                    .is_some_and(|r| r.status == Status::Done || r.optional)
            });
            if blocked {
                println!("  [{}] skipped (a dependency did not complete)", id);
                // Review (minor): record the skip in the manifest naming its
                // blockers — `AttemptRecord.error` promised this but skips
                // were never written, so a cascaded task looked like it never
                // existed. (Mirror of the tracked driver's skip recording.)
                let blockers: Vec<String> = task
                    .depends_on
                    .iter()
                    .filter(|d| {
                        !results
                            .get(d.as_str())
                            .is_some_and(|r| r.status == Status::Done || r.optional)
                    })
                    .cloned()
                    .collect();
                art.manifest.record_attempt(
                    id,
                    &task.harness,
                    task.model.clone(),
                    task.optional,
                    "skipped",
                    AttemptRecord {
                        status: Status::Skipped.label().to_string(),
                        error: Some(format!(
                            "skipped: unfinished dependencies: {}",
                            blockers.join(", ")
                        )),
                        exit_code: None,
                        started_at: None,
                        ended_at: Some(now_secs()),
                        cwd: None,
                        session: None,
                        out: None,
                    },
                );
                results.insert(
                    id.clone(),
                    TaskResult { id: id.clone(), status: Status::Skipped, output: String::new(), stderr: String::new(), exit_code: None, optional: task.optional },
                );
                // A skipped best-effort branch must still not break a
                // downstream join's `${steps.<id>.output}`.
                if task.optional {
                    outputs.insert(id.clone(), missing_output(id));
                }
            } else {
                runnable.push(task);
            }
        }
        if runnable.is_empty() {
            continue;
        }

        let permits = Arc::new(Semaphore::new(
            flow.max_parallel.unwrap_or(runnable.len()).max(1),
        ));
        let mut handles = Vec::new();
        for task in runnable {
            let permits = Arc::clone(&permits);
            let inputs = inputs.clone();
            let outputs = outputs.clone();
            let art = art.clone();
            let flow_cwd = flow.cwd.clone();
            let flow = flow.clone();
            let task = task.clone();
            // Review (minor): keep the id and the optional flag beside the
            // handle. A panicked task never returns its tuple, so the id must
            // be known before the spawn for the JoinError branch below.
            let (id, optional) = (task.id.clone(), task.optional);
            handles.push((id, optional, tokio::spawn(async move {
                let _permit = permits.acquire_owned().await.expect("semaphore closed");
                run_task(&flow, &task, &flow_cwd, default_timeout, &inputs, &outputs, Some(&art)).await
            })));
        }

        // Level barrier: await the whole level before starting the next. A
        // panicked task (JoinError) becomes a Failed result for its id and the
        // barrier keeps waiting for the siblings — the old `.context(..)?`
        // bailed on the FIRST panic, leaving the rest of the level detached
        // (orphaned agents) and skipping their `Failed` bookkeeping.
        for (id, optional, h) in handles {
            let result = match h.await {
                Ok(res) => match res {
                    Ok(r) => r,
                    Err(e) => {
                        println!("  [{id}] failed: {e:#}");
                        TaskResult { id: id.clone(), status: Status::Failed, output: String::new(), stderr: format!("{e:#}"), exit_code: None, optional }
                    }
                },
                Err(join_err) => {
                    // Review (minor): the task future panicked. Record it as a
                    // failed result (so the DAG and the report tell the truth)
                    // rather than aborting the whole run().
                    println!("  [{id}] panicked: {join_err}");
                    TaskResult { id: id.clone(), status: Status::Failed, output: String::new(), stderr: format!("task panicked: {join_err}"), exit_code: None, optional }
                }
            };
            println!("  [{id}] {} {}", result.status.label(), first_line(&result.output));
            if result.status == Status::Done {
                outputs.insert(id.clone(), result.output.clone());
            } else if optional {
                // Best-effort branch that fell: downstream still runs and sees
                // an honest gap, not a hard `${steps.<id>.output}` error.
                outputs.insert(id.clone(), missing_output(&id));
            }
            results.insert(id, result);
        }
    }

    Ok(RunReport { results, out_dir })
}

/// Merge flow inputs with CLI overrides (CLI wins).
fn resolved_inputs(flow: &Flow, extra: &[(String, String)]) -> HashMap<String, String> {
    let mut inputs: HashMap<String, String> =
        flow.inputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in extra {
        inputs.insert(k.clone(), v.clone());
    }
    inputs
}

fn connector_for(flow: &Flow, task: &Task) -> Result<Connector> {
    let ov = flow.harnesses.get(&task.harness);
    connectors::resolve(&task.harness, ov)
}

/// Path context available to `${...}` interpolation. A dry run has no run
/// tree, so its ctx carries no paths and the path refs render placeholders.
#[derive(Clone, Default)]
struct PromptCtx {
    run_dir: Option<PathBuf>,
    task_dir: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    project_dir: Option<PathBuf>,
}

/// Render the final prompt: interpolate `${...}` in `in`, then append the plain
/// `out` description as an explicit "# Output" section (the brief's "out as
/// part of the starting prompt"), and finally the watchdog's protocol section
/// (see [`crate::watchdog::preamble`]) when one applies to this task.
///
/// `skills` is the pre-rendered `# Skill:` block (see [`crate::skills`]),
/// placed after the dependency sections and before `# Output`: a skill is
/// context about *how* to do the task, so the deliverable spec (`# Output`) and
/// then the protocol preamble stay the closing instructions the agent reads
/// last. Computed by [`skill_block`] from the flow's and task's `skills`.
fn render_prompt(
    task: &Task,
    inputs: &HashMap<String, String>,
    outputs: &HashMap<String, String>,
    ctx: &PromptCtx,
    preamble: Option<&str>,
    skills: Option<&str>,
) -> Result<String> {
    let mut s = interpolate(task.in_text(), inputs, outputs, ctx)
        .with_context(|| format!("task '{}': unresolved reference in `in`", task.id))?;
    // Dependency edges carry data: every upstream task this one waits for,
    // whose result its `in` does not already place itself via
    // `${steps.<id>.…}`, is appended as a `# <id>` section. A drawn edge is
    // then enough to feed a result downstream — no hand-written joins
    // (the owner's ask: "if there is an edge, the prompt already gets the
    // result with a comment naming its node"). Deps referenced
    // explicitly are skipped so their output lands exactly once.
    let mut fed = String::new();
    for dep in &task.depends_on {
        if task.in_text().contains(&format!("${{steps.{dep}.")) {
            continue; // the prompt places this dependency's result itself
        }
        if let Some(out) = outputs.get(dep) {
            fed.push_str("\n\n# ");
            fed.push_str(dep);
            fed.push('\n');
            fed.push_str(out);
        }
    }
    if !fed.is_empty() {
        if s.trim().is_empty() {
            s = fed.trim_start_matches('\n').to_string();
        } else {
            s.push_str(&fed);
        }
    }
    // Skills are context handed to the node from the shared store — placed
    // after the dependency results but before the deliverable spec, so `#
    // Output` (what to produce) and the protocol preamble (how to behave) read
    // as the closing instructions. The block is `# Skill: <name>\n<body>`.
    if let Some(sk) = skills.map(str::trim).filter(|p| !p.is_empty()) {
        if s.trim().is_empty() {
            s = sk.to_string();
        } else {
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str("\n\n");
            s.push_str(sk);
            s.push('\n');
        }
    }
    if let Some(out) = task.out.as_ref().map(|o| o.trim()).filter(|o| !o.is_empty()) {
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("\n# Output\n");
        s.push_str(out);
        s.push('\n');
    }
    if let Some(pre) = preamble.map(str::trim).filter(|p| !p.is_empty()) {
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s.push('\n');
        s.push_str(pre);
        s.push('\n');
    }
    Ok(s)
}

/// The `# Skill:` block for a task: its effective skills (flow default + the
/// task's own) resolved from the store and rendered. `Ok(None)` when the task
/// has none. An unresolvable name is a hard error here — surfaced at run/plan
/// time (like an unknown `${...}`), so a typo in `skills:` fails the boot
/// before any process is spawned rather than handing a node a mystery.
fn skill_block(flow: &Flow, task: &Task) -> Result<Option<String>> {
    let names = crate::skills::effective(flow, task);
    if names.is_empty() {
        return Ok(None);
    }
    crate::skills::Resolver::for_flow(flow).sections(&names)
}

/// Replace `${inputs.k}`, `${steps.id.output}`, `${steps.id.dir}`, `${env.VAR}`
/// and the path refs (`${run.dir}`, `${project.dir}`, `${task.dir}`,
/// `${task.work}`). Unknown references are an error (no silent empty strings).
fn interpolate(
    tpl: &str,
    inputs: &HashMap<String, String>,
    outputs: &HashMap<String, String>,
    ctx: &PromptCtx,
) -> Result<String> {
    let mut out = String::with_capacity(tpl.len());
    let bytes = tpl.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // find closing }
            if let Some(end) = tpl[i + 2..].find('}') {
                let rel = i + 2;
                let key = &tpl[rel..rel + end];
                let value = resolve_ref(key, inputs, outputs, ctx)
                    .ok_or_else(|| anyhow!("unknown reference '${{{key}}}'"))?;
                out.push_str(&value);
                i = rel + end + 1;
                continue;
            }
        }
        // push the char at i (handle multibyte by slicing at char boundaries)
        let ch = tpl[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn resolve_ref(
    key: &str,
    inputs: &HashMap<String, String>,
    outputs: &HashMap<String, String>,
    ctx: &PromptCtx,
) -> Option<String> {
    if let Some(k) = key.strip_prefix("inputs.") {
        return inputs.get(k).cloned();
    }
    if let Some(rest) = key.strip_prefix("steps.") {
        // steps.<id>.dir — the upstream task's subtree, so a join can read the
        // files its branches wrote into their work dirs. Same availability
        // rule as `.output` (done, or an optional branch that fell over).
        if let Some(id) = rest.strip_suffix(".dir") {
            if !outputs.contains_key(id) {
                return None;
            }
            return Some(match &ctx.run_dir {
                Some(rd) => rd
                    .join("tasks")
                    .join(artifacts::sanitize_component(id))
                    .display()
                    .to_string(),
                None => format!("<dir of {id}>"),
            });
        }
        // steps.<id>.output
        let id = rest.strip_suffix(".output").unwrap_or(rest);
        return outputs.get(id).cloned();
    }
    if let Some(v) = key.strip_prefix("env.") {
        return std::env::var(v).ok();
    }
    match key {
        "run.dir" => ctx.run_dir.as_ref().map(|p| p.display().to_string()),
        "project.dir" => ctx.project_dir.as_ref().map(|p| p.display().to_string()),
        "task.dir" => ctx.task_dir.as_ref().map(|p| p.display().to_string()),
        "task.work" => ctx.work_dir.as_ref().map(|p| p.display().to_string()),
        _ => None,
    }
}

/// Text fed downstream for a non-completing best-effort (`optional: true`)
/// branch, so a join sees an honest gap rather than the branch's output or a
/// hard "unknown reference" error. Kept in one place so both the skip and the
/// failure path agree. See `Task::optional` / D3.
fn missing_output(id: &str) -> String {
    format!(
        "<step '{id}' did not complete: this best-effort branch produced no output; proceed without it>"
    )
}

/// Spawn one task's harness and capture output. When `art` is present the
/// task gets its own subtree (`tasks/<id>/work` + the `r0` attempt dir) and
/// its prompt/argv/output land in it as provenance.
#[allow(clippy::too_many_arguments)]
async fn run_task(
    flow: &Flow,
    task: &Task,
    flow_cwd: &Option<PathBuf>,
    default_timeout: u64,
    inputs: &HashMap<String, String>,
    outputs: &HashMap<String, String>,
    art: Option<&RunArtifacts>,
) -> Result<TaskResult> {
    let conn = connector_for(flow, task)?;
    let started_at = now_secs();

    // The task's subtree and (by default) its isolated work dir.
    let task_dir = match art {
        Some(a) => Some(a.dirs.ensure_task(&task.id)?),
        None => None,
    };
    let work_dir = art.map(|a| a.dirs.work_dir(&task.id));
    let ctx = PromptCtx {
        run_dir: art.map(|a| a.dirs.root().to_path_buf()),
        task_dir: task_dir.clone(),
        work_dir: work_dir.clone(),
        project_dir: art.and_then(|a| a.project_dir.clone()),
    };
    let preamble = crate::watchdog::preamble_for_classic(flow, task)
        .filter(|_| crate::watchdog::preamble_applies(&conn));
    let skills = skill_block(flow, task)?;
    let prompt = render_prompt(task, inputs, outputs, &ctx, preamble.as_deref(), skills.as_deref())?;
    let (cwd, flag_cwd) = effective_cwd(flow_cwd, task, work_dir.as_deref())?;
    let mut sp = connectors::plan(&conn, task, &prompt, flag_cwd.as_deref())?;

    let timeout = timeout_for(task, default_timeout);

    // The first-run attempt dir; the fully rendered prompt is kept in it as
    // provenance forever (and doubles as the PromptVia::File carrier, which is
    // why it is no longer a deleted temp file).
    let attempt_dir = match art {
        Some(a) => Some(a.dirs.ensure_attempt(&task.id, "r0")?),
        None => None,
    };
    if let Some(dir) = &attempt_dir {
        let _ = tokio::fs::write(dir.join("prompt.txt"), &prompt).await;
    }

    // The `prompt` harness is the flow's starting block: there is no CLI to
    // run — the rendered prompt IS the node's output, recorded like any
    // attempt so dependents (and the manifest) see it identically.
    if conn.id == "prompt" {
        let ended_at = now_secs();
        if let (Some(a), Some(adir)) = (art, &attempt_dir) {
            let _ = tokio::fs::write(adir.join("out.txt"), &prompt).await;
            let _ = tokio::fs::write(adir.join("err.txt"), "").await;
            let _ = artifacts::write_meta(
                adir,
                &AttemptMeta {
                    harness: conn.id.clone(),
                    model: task.model.clone(),
                    argv: vec![],
                    env_keys: env_keys(&conn, task),
                    status: Status::Done.label().to_string(),
                    exit_code: Some(0),
                    started_at: Some(started_at),
                    ended_at: Some(ended_at),
                    cwd: Some(cwd.clone()),
                    prompt_sha256: artifacts::sha256_hex(prompt.as_bytes()),
                },
            );
            a.manifest.record_attempt(
                &task.id,
                &conn.id,
                task.model.clone(),
                task.optional,
                "r0",
                AttemptRecord {
                    status: Status::Done.label().to_string(),
                    error: None, // scripted: only ever Done
                    exit_code: Some(0),
                    started_at: Some(started_at),
                    ended_at: Some(ended_at),
                    cwd: Some(cwd.clone()),
                    session: None,
                    out: Some(OutFile {
                        path: a.dirs.relative(&adir.join("out.txt")),
                        sha256: artifacts::sha256_hex(prompt.as_bytes()),
                        bytes: prompt.len(),
                    }),
                },
            );
        }
        return Ok(TaskResult {
            id: task.id.clone(),
            status: Status::Done,
            output: prompt.clone(),
            stderr: String::new(),
            exit_code: Some(0),
            optional: task.optional,
        });
    }

    if sp.file_prompt.is_some() {
        let path = match &attempt_dir {
            Some(dir) => dir.join("prompt.txt"),
            None => {
                let t = std::env::temp_dir().join(format!(
                    "{}.prompt.txt",
                    artifacts::sanitize_component(&task.id)
                ));
                std::fs::create_dir_all(std::env::temp_dir()).ok();
                t
            }
        };
        if attempt_dir.is_none() {
            tokio::fs::write(&path, &prompt).await.with_context(|| "writing prompt file")?;
        }
        for tok in sp.argv.iter_mut() {
            if tok == "__ZORI_PROMPT_FILE__" {
                *tok = path.display().to_string();
            }
        }
    }

    let mut cmd = Command::new(&sp.program);
    cmd.args(&sp.argv[1..]); // argv[0] is program
    cmd.current_dir(&cwd);
    // ZORI_* context first, so a connector/task env can still override it.
    if let (Some(a), Some(tdir)) = (art, &task_dir) {
        for (k, v) in artifacts::task_env(
            a.dirs.root(),
            tdir,
            &work_dir.clone().unwrap_or_else(|| tdir.join("work")),
            a.project_dir.as_deref(),
        ) {
            cmd.env(k, v);
        }
    }
    for (k, v) in &conn.env {
        cmd.env(k, v);
    }
    for (k, v) in &task.env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    // Put the child in its own process group so a timeout or a Ctrl-C reaches
    // the whole tree (the agent CLIs fork their own tool subprocesses). With
    // pgid == child pid, killpg(child_pid) covers everyone below it.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning '{}': {}", sp.program, render_argv(&sp.argv)))?;
    // Capture the pid now: after we hand `child` to `wait()` below it is
    // borrowed and we can no longer call id() on it.
    let group = GroupRegistration::register(child.id());

    // Spawn the readers before feeding stdin: if the child writes a lot to
    // stdout while we block writing a big prompt to its stdin, the pipes
    // deadlock. Draining stdout/stderr first removes that ordering hazard.
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout handle"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr handle"))?;

    // Readers append to shared buffers so a timeout (where we kill the child
    // and may abort the reader) still keeps whatever it had already read.
    let out_buf = SharedBuf::default();
    let err_buf = SharedBuf::default();
    let mut stdout_task = tokio::spawn(read_stream(stdout, out_buf.clone()));
    let mut stderr_task = tokio::spawn(read_stream(stderr, err_buf.clone()));

    // The clock starts BEFORE stdin is fed: a child that never reads a big
    // stdin used to sit outside the deadline forever, because the unbounded
    // write_all below gated everything after it (review F01).
    let start = Instant::now();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut timed_out = false;

    // Feed stdin, raced against the same deadline so a wedged writer cannot
    // extend the budget.
    {
        let mut si = child.stdin.take().ok_or_else(|| anyhow!("no stdin handle"))?;
        match conn.prompt_via {
            PromptVia::Stdin => {
                let text = sp.stdin_prompt.as_deref().unwrap_or("");
                tokio::select! {
                    biased;
                    r = si.write_all(text.as_bytes()) => {
                        // Broken pipe is the normal case: the child exited
                        // (or ignores stdin) before draining it. Anything
                        // else deserves a diagnostic line.
                        if let Err(e) = r {
                            if e.kind() != std::io::ErrorKind::BrokenPipe {
                                eprintln!("  [{}] stdin write error: {e}", task.id);
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        // The child never drained the prompt within budget.
                        // Kill the group via the timeout path below; a
                        // partial prompt plus exit-0 must not read as Done.
                        eprintln!(
                            "  [{}] TIMED OUT feeding stdin ({}s)",
                            task.id,
                            timeout.as_secs()
                        );
                        timed_out = true;
                    }
                }
            }
            // Arg/File: send nothing but close stdin so the child doesn't wait.
            PromptVia::Arg | PromptVia::File => {}
        }
        // Dropping si closes stdin (also unwedges a timed-out writer).
    }

    // Await child exit with a deadline and a heartbeat. The previous version
    // awaited the reader tasks before inspecting the timeout — so on a hung
    // child the readers never returned and the timeout never fired. Here the
    // only thing we wait on is `child.wait()`, racing a deadline and a ticker.
    // If stdin feeding already consumed the budget, `timed_out` is set and the
    // sleep_until below is already expired, so we fall straight into the kill.
    let wait = child.wait();
    tokio::pin!(wait);
    let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // consume the interval's immediate first tick

    let mut exit_res: Option<std::io::Result<std::process::ExitStatus>> = None;
    loop {
        tokio::select! {
            biased;
            r = &mut wait => { exit_res = Some(r); break; }
            _ = tokio::time::sleep_until(deadline) => { timed_out = true; break; }
            _ = ticker.tick() => {
                println!(
                    "  [{}] … running {}s, {} bytes out so far",
                    task.id,
                    start.elapsed().as_secs(),
                    out_buf.bytes(),
                );
            }
        }
    }

    // On timeout, kill the whole group FIRST, then let the readers finish —
    // killing closes the pipes so they unblock. Awaiting them before the kill
    // was the deadlock that made the first real run hang forever.
    if timed_out {
        eprintln!("  [{}] TIMED OUT after {}s — killing group", task.id, timeout.as_secs());
        group.kill_group().await;
        // Reap the killed child so it doesn't linger as a zombie; the kill
        // closed its run so this returns promptly with the signal status.
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut wait).await;
    }

    // Give readers a bounded grace to flush whatever the killed child left in
    // the pipes, then snapshot. Aborting after the (possibly expired) grace
    // guarantees we never block on a reader that won't finish.
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = (&mut stdout_task).await;
        let _ = (&mut stderr_task).await;
    })
    .await;
    stdout_task.abort();
    stderr_task.abort();

    let stdout_text = out_buf.snapshot();
    let stderr_text = err_buf.snapshot();
    let ended_at = now_secs();

    let (status, exit_code, stderr_final) = if timed_out {
        (Status::Failed, None, format!("TIMED OUT after {}s\n{stderr_text}", timeout.as_secs()))
    } else {
        let st = exit_res
            .unwrap()
            .with_context(|| format!("waiting on '{}'", task.id))?;
        // `group` drop deregisters; no kill needed since the child exited.
        let code = st.code();
        if st.success() {
            (Status::Done, code, stderr_text.clone())
        } else {
            (Status::Failed, code, stderr_text.clone())
        }
    };

    // The honest reason a failed attempt failed, in the manifest (postmortem
    // bug 1: the live zori-review run showed two `failed` nodes with
    // `error: null` because nothing recorded the cause). Wording matches the
    // `TIMED OUT` prefix already written into stderr, so the watchdog's
    // timeout classification and any grep-based tooling stay consistent.
    let fail_reason = match status {
        Status::Done => None,
        _ => Some(if timed_out {
            format!("TIMED OUT after {}s (killed with its process group)", timeout.as_secs())
        } else {
            match exit_code {
                Some(c) => format!("exited with code {c}"),
                None => "killed by signal (no exit code)".to_string(),
            }
        }),
    };

    // Persist artifacts on every terminal path, including timeout — the whole
    // point of capturing partial output from a killed or hung run. err.txt is
    // written always now: stderr of a *successful* agent is often where the
    // interesting diagnostics went.
    if let (Some(a), Some(adir)) = (art, &attempt_dir) {
        let _ = tokio::fs::write(adir.join("out.txt"), &stdout_text).await;
        let _ = tokio::fs::write(adir.join("err.txt"), &stderr_final).await;
        let meta = AttemptMeta {
            harness: conn.id.clone(),
            model: task.model.clone(),
            argv: sp.argv.clone(),
            env_keys: env_keys(&conn, task),
            status: status.label().to_string(),
            exit_code,
            started_at: Some(started_at),
            ended_at: Some(ended_at),
            cwd: Some(cwd.clone()),
            prompt_sha256: artifacts::sha256_hex(prompt.as_bytes()),
        };
        let _ = artifacts::write_meta(adir, &meta);
        a.manifest.record_attempt(
            &task.id,
            &conn.id,
            task.model.clone(),
            task.optional,
            "r0",
            AttemptRecord {
                status: status.label().to_string(),
                error: fail_reason,
                exit_code,
                started_at: Some(started_at),
                ended_at: Some(ended_at),
                cwd: Some(cwd.clone()),
                session: None,
                out: Some(OutFile {
                    path: a.dirs.relative(&adir.join("out.txt")),
                    sha256: artifacts::sha256_hex(stdout_text.as_bytes()),
                    bytes: stdout_text.len(),
                }),
            },
        );
    }

    Ok(TaskResult {
        id: task.id.clone(),
        status,
        output: stdout_text,
        stderr: stderr_final,
        exit_code,
        optional: task.optional,
    })
}

/// Append-only line buffer shared between a reader task and `run_task`, so a
/// killed child still leaves its partial output for the artifact files.
#[derive(Clone, Default)]
struct SharedBuf {
    text: Arc<StdMutex<String>>,
    nbytes: Arc<AtomicUsize>,
    nlines: Arc<AtomicUsize>,
}

impl SharedBuf {
    fn push_line(&self, line: &str) {
        self.nbytes.fetch_add(line.len() + 1, Ordering::Relaxed);
        self.nlines.fetch_add(1, Ordering::Relaxed);
        let mut g = self.text.lock().unwrap();
        g.push_str(line);
        g.push('\n');
    }
    fn bytes(&self) -> usize {
        self.nbytes.load(Ordering::Relaxed)
    }
    fn lines(&self) -> usize {
        self.nlines.load(Ordering::Relaxed)
    }
    fn snapshot(&self) -> String {
        self.text.lock().unwrap().clone()
    }
}

/// Read a child stream line by line, appending each line to the shared buffer.
/// Output is not echoed line-by-line (parallel tasks interleave); the per-task
/// status line summarises and the run dir keeps the full text.
async fn read_stream<S>(stream: S, buf: SharedBuf)
where
    S: tokio::io::AsyncRead + Unpin,
{
    read_stream_with(stream, buf, |_| {}).await;
}

async fn read_stream_with<S: tokio::io::AsyncRead + Unpin>(
    stream: S, buf: SharedBuf, mut on_line: impl FnMut(&str),
) {
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        buf.push_line(&line);
        on_line(&line);
    }
}

fn first_line(s: &str) -> String {
    let line = s.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "(empty)".to_string()
    } else {
        // take() over chars: byte-slicing can split a multibyte UTF-8 char.
        let mut short: String = line.chars().take(80).collect();
        if line.chars().count() > 80 {
            short.push('…');
        }
        short
    }
}

// ===========================================================================
// Tracked / interactive driver — the engine behind `zori ui`.
//
// This is deliberately SEPARATE from `run()`/`run_task` above, which is the
// fully-tested print-and-forget path (16 tests lean on its exact stdout,
// timeout ordering and artifact layout) and is left untouched. The tracked
// path reuses the private primitives (`SharedBuf`, `read_stream`,
// `GroupRegistration`, `connector_for`, `render_prompt`, …) but keeps its own
// execution loop so a change here can never regress the classic path.
//
// The point of the split: `run()` spawns a child, captures stdout, done — the
// agent is gone and un-steerable. `TrackedRun` records, for every node, the
// harness *session handle* (`SessionRef`) and a `turns` transcript, so the
// console can resume a finished node with a fresh human message (steer) and
// flow the corrected output downstream. Resume mechanics are per the spike:
// coddy reopens a print-run by `-session-id` under an isolated `-sessions-dir`
// (deterministic, no cross-talk); codex resumes `--json <thread_id>`.
// ===========================================================================

/// Unix seconds — for transcript timestamps and the coddy session nonce.
fn now_secs() -> u64 {
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
fn new_coddy_session_id(run_id: &str, node: &str) -> String {
    format!("sess_{}_{}", sanitize_id(&format!("{run_id}-{node}")), artifacts::next_millis())
}

/// How to drive one node: a first run (build from scratch) or a resume (steer).
enum ExecMode {
    /// Build from scratch. Carries the upstream outputs map frozen *before
    /// this node's level started* — the same per-level snapshot the classic
    /// `run()` clones into its children — so same-level siblings running
    /// concurrently can never observe each other's output: a
    /// `${steps.x.output}` without a declared dep fails deterministically
    /// instead of resolving by scheduler luck.
    FirstRun { outputs: HashMap<String, String> },
    /// Resume an existing session with this human/system message (the
    /// watchdog's nudges and canned answers take the same path as a steer).
    Resume(SessionRef, String),
    /// One report-only turn after Coddy exhausted its step budget.
    CoddySummary { sessions_dir: PathBuf, session_id: String },
}

/// A live run the console can inspect, steer and re-execute. Owns everything
/// static about a run (flow, plan, resolved inputs, directories); the mutable
/// status/transcript lives in the [`Emitter`] passed to each method.
#[derive(Clone)]
pub struct TrackedRun {
    pub flow: Flow,
    pub plan: ExecutionPlan,
    pub inputs: HashMap<String, String>,
    pub default_timeout: u64,
    /// Isolated coddy sessions root (one dir per run → `-sessions-dir`).
    pub sessions_dir: PathBuf,
    /// Where artifacts + `state.json` go.
    pub out_dir: PathBuf,
    /// The run's artifact tree + manifest (attempts under `tasks/<id>/`).
    pub art: RunArtifacts,
    // First-pass nodes currently inside the level barrier. A worker owns its
    // node through all watchdog turns. A manual retry can take ownership
    // after failure and hold the barrier until its corrected output is ready.
    level_nodes: Arc<StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    permits: Arc<Semaphore>,
    /// Operator edits applied to individual tasks after boot (the console's
    /// rerun-with-fix): topology stays frozen — an edit may correct leaf
    /// fields (model, prompt, out, cwd, timeout, args, env, optional) but not
    /// ids or edges. Behind an `Arc` so every clone of the driver (each level
    /// runner holds one) sees the same corrections.
    overrides: Arc<std::sync::RwLock<HashMap<String, Task>>>,
    /// Wakes waiters after an addressed answer is committed to the snapshot.
    answers: Arc<tokio::sync::Notify>,
    /// The live canvas graph the operator last sent with a chat turn (steer or
    /// an ASK answer). The planner's next prompt is built on THIS, not on the
    /// stale graph still in its session, so a node the operator dragged or a
    /// model they switched while the agent was thinking is not rolled back on
    /// the next proposal. Behind an `Arc` so the driver clones and the exec
    /// loop (which builds the ASK-answer resume after `answer_question` has
    /// returned) all see the same latest board. `None` until a turn carries
    /// one; only the chat planner populates it.
    canvas: Arc<StdMutex<Option<serde_json::Value>>>,
}

impl TrackedRun {
    /// Build the run and ensure its directories exist. `run_id` is the same id
    /// the caller put in the [`crate::state::RunState`].
    pub fn new(
        flow: Flow,
        plan: ExecutionPlan,
        extra_inputs: &[(String, String)],
        out_dir: PathBuf,
        sessions_dir: PathBuf,
    ) -> Result<Self> {
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating run dir {}", out_dir.display()))?;
        std::fs::create_dir_all(&sessions_dir)
            .with_context(|| format!("creating sessions dir {}", sessions_dir.display()))?;
        // Absolute from here on. The executor hands this path to the agent CLI
        // (coddy `-sessions-dir`), which resolves it against ITS OWN process
        // cwd — and the runs root is often relative (`zori-runs`). A task in
        // its isolated work dir then grew a nested sessions tree under it, and
        // a `workspace: flow` task (a chat planner in the project folder)
        // would drop the sessions INTO the user's project. The dir exists now,
        // so `absolute` also canonicalises it.
        let sessions_dir = artifacts::absolute(&sessions_dir);
        let default_timeout = flow_default_timeout(&flow);
        let inputs = resolved_inputs(&flow, extra_inputs);
        let art = RunArtifacts::adopt(&out_dir, &flow, &plan, &inputs)?;
        let parallelism = flow.max_parallel.unwrap_or(flow.tasks.len())
            .clamp(1, Semaphore::MAX_PERMITS);
        Ok(TrackedRun {
            level_nodes: Default::default(),
            permits: Arc::new(Semaphore::new(parallelism)),
            flow,
            plan,
            inputs,
            default_timeout,
            sessions_dir,
            out_dir,
            art,
            overrides: Arc::new(std::sync::RwLock::new(HashMap::new())),
            answers: Arc::new(tokio::sync::Notify::new()),
            canvas: Arc::new(StdMutex::new(None)),
        })
    }

    /// The task as it should run *now*: the operator's edit if one was
    /// registered, else the boot-time spec. Every execution path (first run,
    /// rerun, rerun-stale) resolves through here, so a fix applied between
    /// attempts is never silently lost.
    pub fn task(&self, id: &str) -> Option<Task> {
        if let Some(t) = self.overrides.read().expect("overrides lock").get(id) {
            return Some(t.clone());
        }
        self.flow.tasks.iter().find(|t| t.id == id).cloned()
    }

    /// Register an operator edit for the task with this id (which must already
    /// exist in the plan — the console validates that before calling).
    pub fn override_task(&self, task: Task) {
        self.overrides
            .write()
            .expect("overrides lock")
            .insert(task.id.clone(), task);
    }

    /// The flow as it runs now (boot spec with all edits applied) — the shape
    /// to persist, so a reloaded run keeps the fixes.
    pub fn effective_flow(&self) -> Flow {
        let ov = self.overrides.read().expect("overrides lock");
        let mut f = self.flow.clone();
        for t in f.tasks.iter_mut() {
            if let Some(new) = ov.get(&t.id) {
                *t = new.clone();
            }
        }
        f
    }

    /// The outputs map used to render a node's `in`/`out`: completed nodes give
    /// their stdout; a *missing* value for a best-effort (`optional`) node
    /// resolves to an honest gap rather than a hard `${steps…}` error (mirrors
    /// `run()`'s `missing_output`). Any other unmet reference is an error at
    /// render time, exactly as in the classic path.
    fn outputs_snapshot(&self, emitter: &Emitter) -> HashMap<String, String> {
        let snap = emitter.snapshot();
        let mut outputs = HashMap::new();
        for (id, node) in &snap.nodes {
            match node.status {
                TaskStatus::Done => {
                    outputs.insert(id.clone(), node.output.clone());
                }
                TaskStatus::Failed | TaskStatus::Skipped | TaskStatus::Stale
                    if node.optional =>
                {
                    outputs.insert(id.clone(), missing_output(id));
                }
                _ => {}
            }
        }
        outputs
    }

    /// True if `task`'s deps are all satisfied (Done) or are optional branches
    /// that fell over. A missing dep counts as blocking (never happens after
    /// `build_plan`).
    fn blocked(&self, emitter: &Emitter, task: &Task) -> bool {
        let snap = emitter.snapshot();
        task.depends_on.iter().any(|d| {
            !snap
                .nodes
                .get(d)
                .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
        })
    }

    /// The deps of `task` that are neither Done nor a fallen optional branch
    /// — the same predicate `blocked` tests, kept as a list so diagnostics
    /// and the manifest can name what actually blocked the task.
    fn blocker_names(&self, emitter: &Emitter, task: &Task) -> Vec<String> {
        let snap = emitter.snapshot();
        task.depends_on
            .iter()
            .filter(|d| {
                !snap
                    .nodes
                    .get(d.as_str())
                    .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
            })
            .cloned()
            .collect()
    }

    /// Reserve a failed node in the active level. Taking the guard under the
    /// registry lock makes admission atomic with the scheduler closing the level.
    pub(crate) fn reserve_level_retry(&self, id: &str) -> Result<Option<OwnedMutexGuard<()>>> {
        let nodes = self.level_nodes.lock().expect("level nodes lock");
        nodes.get(id)
            .map(|lock| lock.clone().try_lock_owned()
                .map_err(|_| anyhow!("node '{id}' is still executing")))
            .transpose()
    }

    /// Run every node, level by level, updating `emitter` as it goes. The
    /// nodes of one level run **concurrently**, capped by
    /// `flow.max_parallel` (unset → the whole level at once — the same
    /// contract as the classic `run()`); the level barrier is kept, so the
    /// whole level is terminal before the next one spawns. A node failure
    /// marks it `Failed` and cascades `Skipped` to dependents; best-effort
    /// (`optional`) failures never block a join. Mirrors `run()`'s cascade
    /// (D3). Returns when the whole plan has reached a terminal state.
    pub async fn run_all(&self, emitter: &Emitter) {
        install_death_handlers();
        for (li, level) in self.plan.levels.iter().enumerate() {
            println!("── level {li}: {}", level.join(", "));
            // Partition this level into runnable vs skipped-because-a-dep-died
            // (same predicate as the classic path) BEFORE spawning anything.
            let mut runnable: Vec<Task> = Vec::new();
            for id in level {
                let task = self.task(id).unwrap_or_else(|| {
                    panic!("task '{id}' is in the plan but missing from the flow")
                });
                if self.blocked(emitter, &task) {
                    println!("  [{id}] skipped (a dependency did not complete)");
                    let blockers = self.blocker_names(emitter, &task);
                    emitter.mutate(|st| {
                        if let Some(n) = st.nodes.get_mut(id) {
                            n.status = TaskStatus::Skipped;
                            n.finished_at = Some(now_secs());
                        }
                    });
                    // Review (minor): `AttemptRecord.error` promises "which
                    // dependency blocked a skip", but skips were never
                    // recorded — a cascaded task looked like it never
                    // existed in the run index. Record the skip naming its
                    // blockers under a nonce no real attempt can produce.
                    self.art.manifest.record_attempt(
                        &task.id,
                        &task.harness,
                        task.model.clone(),
                        task.optional,
                        "skipped",
                        AttemptRecord {
                            status: TaskStatus::Skipped.label().to_string(),
                            error: Some(format!(
                                "skipped: unfinished dependencies: {}",
                                blockers.join(", ")
                            )),
                            exit_code: None,
                            started_at: None,
                            ended_at: Some(now_secs()),
                            cwd: None,
                            session: None,
                            out: None,
                        },
                    );
                    continue;
                }
                runnable.push(task.clone());
            }
            self.run_level(emitter, runnable, true).await;
            // Close admission first, then join any retry accepted before the
            // boundary. Only then can the next level evaluate dependencies.
            let nodes = std::mem::take(&mut *self.level_nodes.lock().expect("level nodes lock"));
            for lock in nodes.into_values() {
                drop(lock.lock().await);
            }
        }
    }

    /// Run one level's nodes concurrently: one tokio task per node behind a
    /// semaphore sized by `max_parallel` (unset → the whole level). The
    /// upstream outputs map is frozen once, before any node of the level
    /// spawns, and shared by every sibling (see [`ExecMode::FirstRun`]).
    /// Returns only when every node of the level is terminal — the level
    /// barrier (D3).
    async fn run_level(&self, emitter: &Emitter, nodes: Vec<Task>, first_pass: bool) {
        if nodes.is_empty() {
            return;
        }
        let outputs = self.outputs_snapshot(emitter);
        let mut handles = Vec::new();
        for task in nodes {
            let owner = first_pass.then(|| {
                let lock = Arc::new(AsyncMutex::new(()));
                let guard = lock.clone().try_lock_owned().expect("new node lock");
                self.level_nodes.lock().expect("level nodes lock").insert(task.id.clone(), lock);
                guard
            });
            let emitter = emitter.clone();
            let outputs = outputs.clone();
            let this = self.clone();
            // Review (minor): keep the id beside the handle — a panicked task
            // never returns, so the JoinError branch below still knows which
            // node to settle.
            let id = task.id.clone();
            handles.push((id, tokio::spawn(async move {
                let _owner = owner;
                let _permit = this.permits.acquire().await.expect("semaphore closed");
                match this.exec(&emitter, &task, ExecMode::FirstRun { outputs }).await {
                    Ok(()) => {
                        let st = emitter.snapshot();
                        if let Some(n) = st.nodes.get(&task.id) {
                            println!("  [{}] {} {}", n.id, n.status.label(), first_line(&n.output));
                        }
                    }
                    Err(e) => {
                        println!("  [{}] error: {e:#}", task.id);
                        emitter.mutate(|st| {
                            if let Some(n) = st.nodes.get_mut(&task.id) {
                                n.status = TaskStatus::Failed;
                                n.stderr = format!("driver error: {e:#}");
                                n.finished_at = Some(now_secs());
                            }
                        });
                    }
                }
            })));
        }
        // Level barrier: await the whole level before returning. A panicked
        // task (JoinError) never reached a terminal write in exec, so it would
        // sit in Running forever and slip past the external watchdog (which
        // only reads terminal nodes). Settle any node the panic left non-terminal
        // as Failed, and still wait for every sibling — never bail on the first
        // panic.
        for (id, h) in handles {
            if let Err(join_err) = h.await {
                println!("  [{id}] panicked: {join_err}");
                let id2 = id.clone();
                emitter.mutate(move |st| {
                    if let Some(n) = st.nodes.get_mut(&id2) {
                        if !n.status.is_terminal() {
                            n.status = TaskStatus::Failed;
                            n.stderr = format!("task panicked: {join_err}");
                            n.finished_at = Some(now_secs());
                        }
                    }
                });
            }
        }
    }

    /// Steer a finished node: append the human turn, resume the node's session
    /// with it, record the agent's reply, and mark downstream nodes `Stale` so
    /// the human re-runs them (their input just changed). The steering turn is
    /// recorded even if the resume fails, so the transcript tells the truth.
    /// Remember the operator's live canvas, sent with a chat turn. Only an
    /// actual graph overwrites it — a turn that omits the field (an older
    /// client, or a steer of a non-chat node) must not wipe a board we already
    /// know, so the planner keeps seeing the last real state.
    fn set_canvas(&self, graph: Option<serde_json::Value>) {
        if let Some(g) = graph {
            *self.canvas.lock().unwrap_or_else(|p| p.into_inner()) = Some(g);
        }
    }

    /// The canvas block appended to the planner's next prompt, or empty when no
    /// operator has sent a board yet. `pos` is stripped: it is pure layout the
    /// model cannot act on, and dropping it keeps the block small. The wording
    /// says plainly that the operator may have edited this by hand, so the
    /// agent builds on it instead of proposing the graph still in its session.
    fn graph_context(&self) -> String {
        let value = self.canvas.lock().unwrap_or_else(|p| p.into_inner()).clone();
        canvas_prompt(value)
    }

    pub async fn steer(&self, emitter: &Emitter, id: &str, text: &str, graph: Option<serde_json::Value>) -> Result<()> {
        self.steer_with_context(emitter, id, text, graph, "").await
    }

    pub(crate) async fn steer_with_context(&self, emitter: &Emitter, id: &str, text: &str, graph: Option<serde_json::Value>, context: &str) -> Result<()> {
        install_death_handlers();
        let task = self.task(id).ok_or_else(|| anyhow!("no such task '{id}'"))?;

        // Snapshot the session handle before mutating, so we can resume it.
        let session = emitter
            .snapshot()
            .nodes
            .get(id)
            .and_then(|n| n.session.clone())
            .ok_or_else(|| anyhow!("task '{id}' has no resumable session to steer"))?;

        // The operator may have moved a node, switched a model or wired a
        // dependency since the last turn; remember the board so the prompt we
        // send is built on it, not on the graph still stuck in the session.
        self.set_canvas(graph);
        emitter.append_turn(id, Role::Human, text);
        emitter.set_status(id, TaskStatus::Running);

        // The transcript keeps the operator's words verbatim; only the model's
        // prompt gains the canvas block, so what the operator reads back stays
        // exactly what they typed.
        let prompt = format!("{text}{}{context}", self.graph_context());
        let outcome = self
            .exec(emitter, &task, ExecMode::Resume(session.clone(), prompt))
            .await;
        match outcome {
            Ok(()) => {
                self.mark_downstream_stale(emitter, id);
                Ok(())
            }
            Err(e) => {
                emitter.mutate(|st| {
                    if let Some(n) = st.nodes.get_mut(id) {
                        n.status = TaskStatus::Failed;
                        n.stderr = format!("steer error: {e:#}");
                        n.finished_at = Some(now_secs());
                    }
                });
                Err(e)
            }
        }
    }

    /// Re-run a node from scratch (fresh session → fresh context), then mark its
    /// dependents `Stale`. Used by the console's "rerun" button — through
    /// [`TrackedRun::task`], so it picks up any operator fix registered since
    /// the last attempt.
    pub async fn rerun(&self, emitter: &Emitter, id: &str) -> Result<()> {
        let task = self
            .task(id)
            .ok_or_else(|| anyhow!("no such task '{id}'"))?;
        // Review M4: readiness of the dependencies is checked BEFORE any
        // destructive step. The console's claim gate is not the only entrance
        // to this method (rerun_stale and direct calls reach it too), and the
        // old order wiped the node's session and captured output first — so a
        // rerun accepted on a `Skipped` node under a failed parent destroyed
        // the previous transcript and then died on `unknown reference` (or,
        // with plain depends_on, ran happily on an incomplete prompt).
        if self.blocked(emitter, &task) {
            let snap = emitter.snapshot();
            let blockers: Vec<String> = task
                .depends_on
                .iter()
                .filter(|d| {
                    !snap
                        .nodes
                        .get(*d)
                        .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
                })
                .cloned()
                .collect();
            return Err(anyhow!(
                "node '{id}' waits on unfinished dependencies: {}",
                blockers.join(", ")
            ));
        }
        let _permit = self.permits.acquire().await.expect("semaphore closed");
        // Drop the old session so FirstRun mints a new one.
        self.reset_node(emitter, id);
        let outputs = self.outputs_snapshot(emitter);
        if let Err(e) = self.exec(emitter, &task, ExecMode::FirstRun { outputs }).await {
            // A driver-level failure must NOT leave the node zombie-Running:
            // the claim already flipped it to Running, and nothing else will
            // ever touch it again — record the failure honestly (the same
            // shape `run_level` uses for a first-run driver error).
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(id) {
                    n.status = TaskStatus::Failed;
                    n.stderr = format!("driver error: {e:#}");
                    n.finished_at = Some(now_secs());
                }
            });
            return Err(e);
        }
        self.mark_downstream_stale(emitter, id);
        Ok(())
    }

    /// Drop a node's previous attempt (session handle, captured output,
    /// progress) so a FirstRun mints a fresh session and nothing stale leaks
    /// into the transcript or artifacts.
    fn reset_node(&self, emitter: &Emitter, id: &str) {
        emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(id) {
                n.session = None;
                n.output.clear();
                n.stderr.clear();
                n.exit_code = None;
                n.progress = Default::default();
            }
        });
    }

    /// Re-run every `Stale` node in level order (a "refresh downstream"
    /// action). Stale nodes of one level are independent by construction, so
    /// they run concurrently under the same `max_parallel` cap as a first
    /// run; the level barrier keeps the refresh order honest.
    pub async fn rerun_stale(&self, emitter: &Emitter) {
        for level in &self.plan.levels {
            let stale_ids: Vec<String> = level
                .iter()
                .filter(|id| {
                    emitter
                        .snapshot()
                        .nodes
                        .get(*id)
                        .is_some_and(|n| n.status == TaskStatus::Stale)
                })
                .cloned()
                .collect();
            if stale_ids.is_empty() {
                continue;
            }
            let stale: Vec<Task> = stale_ids
                .iter()
                .filter_map(|id| self.task(id))
                .collect();
            // Review M4: a Stale node is only re-runnable when its deps are
            // Done *now* — an earlier level of this very pass may just have
            // refreshed them (topological order makes that the common case),
            // or a refreshed parent may have failed again (then this node must
            // wait, like a first-run node does in `run_level`). Nodes still
            // waiting keep `Stale` — one refresh click later they are picked
            // up — and record why, so the button silently skipping them is
            // not a mystery.
            let (ready, waiting): (Vec<Task>, Vec<Task>) = stale
                .into_iter()
                .partition(|t| !self.blocked(emitter, t));
            for t in &waiting {
                let snap = emitter.snapshot();
                let blockers: Vec<&str> = t
                    .depends_on
                    .iter()
                    .filter(|d| {
                        !snap
                            .nodes
                            .get(*d)
                            .is_some_and(|n| n.status == TaskStatus::Done || n.optional)
                    })
                    .map(|s| s.as_str())
                    .collect();
                let note = format!("rerun-stale held back: unfinished dependencies: {}", blockers.join(", "));
                let id = t.id.clone();
                emitter.mutate(move |st| {
                    if let Some(n) = st.nodes.get_mut(&id) {
                        n.stderr = note.clone();
                    }
                });
            }
            for t in &ready {
                self.reset_node(emitter, &t.id);
            }
            self.run_level(emitter, ready.clone(), false).await;
            // Cascade Stale onto each refreshed node's dependents AFTER the
            // level is terminal (a dependent never shares a level with its
            // producer, so this is the same set the per-node `rerun` marks —
            // and dependents still ahead in this pass get picked up when
            // their level comes around).
            for t in &ready {
                self.mark_downstream_stale(emitter, &t.id);
            }
        }
    }

    /// Mark transitive dependents of `id` `Stale` if they were `Done`. Running or
    /// already-terminal-but-not-Done nodes are left alone.
    fn mark_downstream_stale(&self, emitter: &Emitter, id: &str) {
        let mut frontier = vec![id.to_string()];
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(cur) = frontier.pop() {
            for t in &self.flow.tasks {
                if !t.depends_on.iter().any(|d| d == &cur) {
                    continue;
                }
                if !seen.insert(t.id.clone()) {
                    continue;
                }
                emitter.mutate(|st| {
                    if let Some(n) = st.nodes.get_mut(&t.id) {
                        if n.status == TaskStatus::Done {
                            n.status = TaskStatus::Stale;
                        }
                    }
                });
                frontier.push(t.id.clone());
            }
        }
    }

    /// Execute one node, supervised: after every attempt the watchdog
    /// ([`crate::watchdog`]) inspects the result and may continue the same
    /// session (a nudge for a clipped/empty reply), answer a question
    /// (canned, in unattended mode) or park it for the operator, re-run the
    /// node from scratch (a failed attempt), or accept it. Every automated
    /// step is recorded as a `system` turn, and each attempt lands in its own
    /// artifact dir (`r0`, `r1`, … for fresh attempts, `t<secs>` for resumes).
    ///
    /// Supervision applies to first runs (and reruns) only: a human steer
    /// (`ExecMode::Resume` on entry) that fails stays visible as a failure —
    /// the human is right there to decide. Driver-level errors (bad argv,
    /// unresolvable prompt) are deterministic and propagate unchanged.
    async fn exec(&self, emitter: &Emitter, task: &Task, mode: ExecMode) -> Result<()> {
        let conn = connector_for(&self.flow, task)?;
        // `echo` participates as the scripted test probe (its reply is the
        // rendered prompt / resume text, so the whole supervision loop is
        // exercisable in CI); the `prompt` block is data, not an agent.
        let manual = matches!(mode, ExecMode::Resume(..));
        if !conn.is_agent && conn.id != "echo" {
            let nonce = mode_nonce(&mode, 0);
            return self.exec_once(emitter, task, &mode, &nonce, None, false).await.map(|_| ());
        }
        let settings = Settings::for_task(&self.flow, task);
        let mut nudges_left = if manual { 0 } else { settings.nudges };
        let mut retries_left = if manual { 0 } else { settings.retry };
        let mut auto_answers_left = crate::watchdog::AUTO_ANSWERS_MAX;
        let mut attempt_no: u32 = 0;
        let mut mode = mode;

        loop {
            let nonce = mode_nonce(&mode, attempt_no);
            let verdict = self.exec_once(emitter, task, &mode, &nonce, Some(&settings), !manual).await?;
            let node = emitter.snapshot().nodes.get(&task.id).cloned();
            let Some(n) = node else { break };

            // A failed summary must never replay the original task. The last
            // attempt and its partial output are already in the manifest.
            if matches!(mode, ExecMode::CoddySummary { .. }) && verdict != Verdict::Ok {
                break;
            }
            match verdict {
                Verdict::CoddyStepLimit(steps) => {
                    let Some(SessionRef::Coddy { sessions_dir, session_id }) = n.session else { break };
                    if nudges_left == 0 { break; }
                    nudges_left -= 1;
                    emitter.append_turn(&task.id, Role::System, format!(
                        "[watchdog] Coddy hit its limit of {steps} steps. Requesting the final report in the same session."
                    ));
                    mode = ExecMode::CoddySummary { sessions_dir, session_id };
                    continue;
                }
                Verdict::Retry(reason) if retries_left > 0 => {
                    retries_left -= 1;
                    attempt_no += 1;
                    emitter.append_turn(
                        &task.id,
                        Role::System,
                        format!("[watchdog] attempt failed ({reason}) — restarting from scratch, {retries_left} retries left"),
                    );
                    self.reset_node(emitter, &task.id);
                    mode = ExecMode::FirstRun { outputs: self.outputs_snapshot(emitter) };
                    continue;
                }
                Verdict::Ask(_) if settings.unattended
                    && auto_answers_left > 0
                    && resumable(&n.session) =>
                {
                    auto_answers_left -= 1;
                    emitter.append_turn(
                        &task.id,
                        Role::System,
                        format!(
                            "[watchdog] question in unattended mode, auto-answering ({auto_answers_left} auto-answers left):\n{}",
                            crate::watchdog::AUTO_ANSWER
                        ),
                    );
                    let sess = n.session.clone().expect("resumable checked");
                    mode = ExecMode::Resume(sess, crate::watchdog::AUTO_ANSWER.to_string());
                    continue;
                }
                Verdict::Ask(_) if !settings.unattended && resumable(&n.session) => {
                    let Some(answer) = self.wait_for_answers(emitter, &task.id, settings.ask_timeout_secs).await else {
                        break;
                    };
                    // The answer is a fresh planner turn too, so it reasons from
                    // the board the operator holds now (`answer_question` stored
                    // it before we wake here). Empty for a plain node that never
                    // sent a canvas, so this is a no-op off the chat path.
                    let prompt = format!("{answer}{}", self.graph_context());
                    mode = ExecMode::Resume(n.session.expect("resumable checked"), prompt);
                    continue;
                }
                Verdict::Nudge(kind) if nudges_left > 0 && resumable(&n.session) => {
                    nudges_left -= 1;
                    emitter.append_turn(
                        &task.id,
                        Role::System,
                        format!(
                            "[watchdog] the reply looks unfinished, nudging ({nudges_left} nudges left):\n{}",
                            crate::watchdog::nudge_text(kind)
                        ),
                    );
                    let sess = n.session.clone().expect("resumable checked");
                    mode = ExecMode::Resume(sess, crate::watchdog::nudge_text(kind).to_string());
                    continue;
                }
                // Terminal: the agent's last reply is final. For a supermodel
                // chat node this is where a ` ```zori-flow ` block becomes a
                // server-checked proposal — and, if invalid, earns one bounded
                // repair turn. For every other node this is a no-op `break`.
                _ => match self.proposal_gate(emitter, task).await {
                    Some(fix) => {
                        mode = fix;
                        continue;
                    }
                    None => break,
                },
            }
        }
        Ok(())
    }

    /// The propose→validate→(fix) gate, run once when a chat node's turn is
    /// final. Reads the model's committed reply, and if it carried a
    /// ` ```zori-flow ` block, validates it with the *same* validator the flow
    /// builder uses and stores the verdict on the node as a [`FlowProposal`].
    /// A valid graph is parked for the operator's Run click; an invalid one
    /// earns a single bounded continuation turn (the [`crate::proposal`]
    /// `fix_instruction`) so the model can repair it before giving up. Returns
    /// the `Resume` to take for a repair turn, or `None` to end the turn.
    ///
    /// A no-op for every non-chat node (`is_chat` false → break unchanged), so
    /// it cannot perturb ordinary flow supervision.
    async fn proposal_gate(&self, emitter: &Emitter, task: &Task) -> Option<ExecMode> {
        // Only a supermodel dialogue proposes graphs; a normal flow has nothing
        // to gate here, and a node that never ran (no snapshot) has no reply.
        let snap = emitter.snapshot();
        if !snap.is_chat {
            return None;
        }
        let node = snap.nodes.get(&task.id)?;
        // No block in this reply is a plain conversational turn (phase 1 of the
        // protocol: the model thinks and asks) — keep any prior proposal and end.
        // Review M12: the parse + full builder validation run on a blocking
        // thread, not this runtime worker (an oversized/hostile graph YAML
        // would otherwise stall every other run's event handling), and an
        // oversized block surfaces as an invalid proposal, not silence.
        let proposal = crate::proposal::from_reply_async(node.output.clone()).await?;
        let prior_fix = node.proposal.as_ref().map(|p| p.fix_rounds).unwrap_or(0);
        let resumable_here = resumable(&node.session);
        let session = node.session.clone();
        // The three-way-merge base the console needs: what both sides last
        // agreed on. Prefer the agent's last accepted graph (the previous
        // valid proposal's spec) — the operator's edits made after it was
        // placed, before sending this turn or during the reply, then read as
        // operator-owned, so a reply echoing pre-edit values cannot revert
        // them. Without an earlier accepted proposal the board shipped with
        // this turn is the only common view (there is nothing to protect
        // against: the agent has never spoken in graphs).
        let base = node
            .proposal
            .as_ref()
            .and_then(|p| p.spec.clone())
            .or_else(|| self.canvas.lock().expect("canvas lock").clone());

        if proposal.valid {
            emitter.mutate(move |st| {
                if let Some(n) = st.nodes.get_mut(&task.id) {
                    n.proposal = Some(FlowProposal {
                        raw: proposal.raw,
                        hash: proposal.hash,
                        valid: true,
                        errors: Vec::new(),
                        warnings: proposal.warnings,
                        levels: proposal.levels,
                        spec: proposal.spec,
                        base,
                        fix_rounds: prior_fix,
                        status: ProposalStatus::Proposed,
                        run_id: None,
                        ts: now_secs(),
                    });
                }
            });
            return None;
        }

        // Invalid. Ask the model to repair it — but bounded, so a looping model
        // cannot spin the run the way watchdog nudges are bounded.
        let repair = prior_fix < crate::proposal::MAX_FIX_ROUNDS && resumable_here;
        // Compute the repair text while `proposal.errors` is still ours; the
        // store closure below moves `proposal` into itself.
        let fix_text = repair.then(|| crate::proposal::fix_instruction(&proposal.errors));
        if repair {
            emitter.append_turn(
                &task.id,
                Role::System,
                format!(
                    "[zori] the proposal failed validation — asking the model to fix it (round {}/{})",
                    prior_fix + 1,
                    crate::proposal::MAX_FIX_ROUNDS
                ),
            );
        }
        let task_id = task.id.clone();
        let store = move |st: &mut crate::state::RunState| {
            if let Some(n) = st.nodes.get_mut(&task_id) {
                n.proposal = Some(FlowProposal {
                    raw: proposal.raw,
                    hash: proposal.hash,
                    valid: false,
                    errors: proposal.errors,
                    warnings: Vec::new(),
                    levels: Vec::new(),
                    spec: None,
                    base: None,
                    fix_rounds: if repair { prior_fix + 1 } else { prior_fix },
                    status: ProposalStatus::Proposed,
                    run_id: None,
                    ts: now_secs(),
                });
            }
        };
        emitter.mutate(store);
        if let Some(text) = fix_text {
            return Some(ExecMode::Resume(session.expect("resumable checked"), text));
        }
        None
    }

    async fn wait_for_answers(&self, emitter: &Emitter, id: &str, timeout_secs: u64) -> Option<String> {
        emitter.append_turn(id, Role::System, "[watchdog] waiting for the operator's answers");
        let deadline = tokio::time::Instant::now().checked_add(Duration::from_secs(timeout_secs));
        loop {
            // notify_waiters wakes all registered nodes. Register before checking
            // the durable predicate so a reply cannot fall between check and wait.
            let notified = self.answers.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut response = None;
            let mut expired = false;
            emitter.mutate(|st| {
                let n = st.nodes.get_mut(id).expect("executing node");
                let mut open: Vec<_> = n.questions.iter_mut().filter(|q| !q.closed).collect();
                if let Some(q) = open.iter().find(|q| q.answer.is_none()) {
                    n.ask = Some(format!("ASK: {}", q.text));
                    n.status = TaskStatus::Waiting;
                    expired = timeout_secs > 0 && deadline.is_some_and(|d| tokio::time::Instant::now() >= d);
                    if !expired { return; }
                    n.status = TaskStatus::Failed;
                    n.stderr = format!("watchdog: operator answer timed out after {timeout_secs}s");
                } else {
                    response = Some(if open.len() == 1 && !open[0].inline {
                        open[0].answer.clone().expect("answered")
                    } else {
                        open.iter().map(|q| format!("{} — {}", q.text, q.answer.as_deref().unwrap()))
                            .collect::<Vec<_>>().join("\n")
                    });
                    n.status = TaskStatus::Running;
                }
                for q in &mut open { q.closed = true; }
                n.ask = None;
            });
            if expired || response.is_some() { return response; }
            if let Some(d) = deadline.filter(|_| timeout_secs != 0) {
                let _ = tokio::time::timeout_at(d, notified).await;
            } else {
                notified.await;
            }
        }
    }

    /// Legacy clients can answer a single parked question without its ID.
    pub async fn answer(&self, emitter: &Emitter, id: &str, text: &str) -> Result<()> {
        self.answer_question(emitter, id, None, text, None)
    }

    pub fn answer_question(&self, emitter: &Emitter, id: &str, question_id: Option<usize>, text: &str, graph: Option<serde_json::Value>) -> Result<()> {
        anyhow::ensure!(!text.trim().is_empty(), "empty answer");
        // Remember the board BEFORE waking the parked loop: its ASK-answer
        // resume reads `graph_context()` the instant `notify_waiters` lands, so
        // the canvas must already be stored. Same rule as `steer` — an absent
        // graph keeps the last known board rather than clearing it.
        self.set_canvas(graph);
        let mut accepted = false;
        emitter.mutate(|st| {
            let Some(n) = st.nodes.get_mut(id) else { return };
            if !matches!(n.status, TaskStatus::Running | TaskStatus::Waiting) { return; }
            let mut matching = n.questions.iter_mut().filter(|q| !q.closed && q.answer.is_none()
                && question_id.map_or(n.status == TaskStatus::Waiting, |qid| q.id == qid));
            let Some(q) = matching.next() else { return };
            if matching.next().is_some() { return; }
            q.answer = Some(text.to_owned());
            n.turns.push(crate::state::Turn { role: Role::Human, text: text.to_owned(), ts: now_secs() });
            accepted = true;
        });
        anyhow::ensure!(accepted, "question is closed, already answered, or requires a question_id");
        self.answers.notify_waiters();
        Ok(())
    }

    /// Execute ONE attempt in either mode, writing
    /// status/transcript/output/artifacts and returning the recorded verdict. An `Err` is a
    /// driver-level failure (bad argv, unresolvable prompt).
    async fn exec_once(&self, emitter: &Emitter, task: &Task, mode: &ExecMode, nonce: &str, supervision: Option<&Settings>, supervise_failures: bool) -> Result<Verdict> {
        let conn = connector_for(&self.flow, task)?;
        let timeout = timeout_for(task, self.default_timeout);
        let started_at = now_secs();

        // The task's subtree + isolated work dir + attempt dir (r0/r1/… for
        // fresh attempts, t<secs> per steer/watchdog resume — the old flat
        // nonce-suffixed names, now one directory per attempt).
        let task_dir = self.art.dirs.ensure_task(&task.id)?;
        let work_dir = self.art.dirs.work_dir(&task.id);
        let (attempt_dir, nonce) = self.art.dirs.claim_attempt(&task.id, nonce)?;
        let ctx = PromptCtx {
                run_dir: Some(self.art.dirs.root().to_path_buf()),
            task_dir: Some(task_dir.clone()),
            work_dir: Some(work_dir.clone()),
            project_dir: self.art.project_dir.clone(),
        };
        let (cwd, flag_cwd) = effective_cwd(&self.flow.cwd, task, Some(&work_dir))?;

        // The scripted harnesses — `echo` (test probe) and `prompt` (the
        // starting block) — run no process; `prompt` shares the path because
        // its output is likewise just the rendered prompt. The whole steer
        // path stays exercisable in tests without a model, and the scripted
        // reply is recorded like any harness's attempt. Neither gets the
        // ask-protocol preamble: echo's reply is the prompt verbatim (and
        // tests assert byte-exact outputs), the prompt block is data.
        if conn.id == "echo" || conn.id == "prompt" {
            emitter.set_status(task.id.as_str(), TaskStatus::Running);
            // Skills DO ride the echo/prompt path (unlike the preamble): a
            // `prompt` node is the rendered prompt as data, so an attached
            // skill is exactly what should materialise there. No-skill tasks
            // render None, so echo's byte-exact test outputs are unaffected.
            let skills = skill_block(&self.flow, task)?;
            let reply = match mode {
                ExecMode::FirstRun { outputs } => render_prompt(task, &self.inputs, outputs, &ctx, None, skills.as_deref())?,
                ExecMode::Resume(_, human) => human.clone(),
                ExecMode::CoddySummary { .. } => unreachable!("summary belongs to a Coddy task"),
            };
            if supervision.is_some_and(|s| s.ask && !s.unattended) {
                publish_questions(emitter, &task.id, &reply, &mut None);
            }
            let verdict = attempt_verdict(&Attempt { reply: &reply, success: true, timed_out: false, exit_code: Some(0) },
                supervision.filter(|_| supervise_failures), true);
            let verdict = question_verdict(emitter, &task.id, verdict, true, supervision.is_some_and(|s| s.unattended));
            let error = verdict_failure(&verdict);
            let status = if error.is_some() { TaskStatus::Failed } else { TaskStatus::Done };
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(&task.id) {
                    n.session = Some(SessionRef::Echo);
                    n.turns.push(crate::state::Turn { role: Role::Agent, text: reply.clone(), ts: now_secs() });
                    n.output = reply.clone();
                    n.status = question_status(n, status, &verdict);
                    n.stderr = error.clone().unwrap_or_default();
                    n.exit_code = Some(0);
                    n.finished_at = Some(now_secs());
                }
            });
            let _ = tokio::fs::write(attempt_dir.join("prompt.txt"), &reply).await;
            let _ = tokio::fs::write(attempt_dir.join("out.txt"), &reply).await;
            let _ = artifacts::write_meta(
                &attempt_dir,
                &AttemptMeta {
                    harness: conn.id.clone(),
                    model: task.model.clone(),
                    argv: vec![],
                    env_keys: env_keys(&conn, task),
                    status: status.label().to_string(),
                    exit_code: Some(0),
                    started_at: Some(started_at),
                    ended_at: Some(now_secs()),
                    cwd: Some(cwd.clone()),
                    prompt_sha256: artifacts::sha256_hex(reply.as_bytes()),
                },
            );
            self.art.manifest.record_attempt(
                &task.id,
                &conn.id,
                task.model.clone(),
                task.optional,
                &nonce,
                AttemptRecord {
                    status: status.label().to_string(),
                    error,
                    exit_code: Some(0),
                    started_at: Some(started_at),
                    ended_at: Some(now_secs()),
                    cwd: Some(cwd.clone()),
                    session: Some(SessionRef::Echo),
                    out: Some(OutFile {
                        path: self.art.dirs.relative(&attempt_dir.join("out.txt")),
                        sha256: artifacts::sha256_hex(reply.as_bytes()),
                        bytes: reply.len(),
                    }),
                },
            );
            return Ok(verdict);
        }

        // Build the argv: first-run via plan()+track_args, resume via resume_plan.
        // The ask-protocol preamble rides on first-run prompts of real agents
        // only (echo/prompt are handled above).
        let (mut sp, harness_session, prompt) = match &mode {
            ExecMode::FirstRun { outputs } => {
                let preamble = if crate::watchdog::preamble_applies(&conn) {
                    crate::watchdog::preamble(&crate::watchdog::Settings::for_task(&self.flow, task))
                } else {
                    None
                };
                let skills = skill_block(&self.flow, task)?;
                let prompt = render_prompt(task, &self.inputs, outputs, &ctx, preamble.as_deref(), skills.as_deref())?;
                let mut sp = connectors::plan(&conn, task, &prompt, flag_cwd.as_deref())?;
                // Coddy: mint a fixed session id and isolate it so a sibling
                // task in the same cwd can never be the "most recent".
                // Review M5: merged at the `--` terminator, not appended —
                // a dash-first prompt makes `plan()` close options early, and
                // a blind extend would bury these very flags among positional
                // text (codex/opencode would then never emit their session
                // key, silently killing tracked capture).
                let session = if conn.id == "coddy" {
                    let sid = new_coddy_session_id(&emitter.snapshot().run_id, &task.id);
                    connectors::insert_track_args(
                        &mut sp.argv,
                        connectors::track_args(&conn, &sid, &self.sessions_dir),
                    );
                    Some(SessionRef::Coddy {
                        sessions_dir: self.sessions_dir.clone(),
                        session_id: sid,
                    })
                } else {
                    // Codex (`--json`) and OpenCode (`--format json`): stream a
                    // machine format so the reply + session key come off stdout.
                    connectors::insert_track_args(
                        &mut sp.argv,
                        connectors::track_args(&conn, "", &self.sessions_dir),
                    );
                    None
                };
                (sp, session, prompt)
            }
            ExecMode::Resume(session, human) => {
                let sp = connectors::resume_plan(&conn, session, human, Some(&cwd))?;
                // Reuse the stored session handle (context must persist).
                (sp, Some(session.clone()), human.clone())
            }
            ExecMode::CoddySummary { sessions_dir, session_id } => {
                let prompt = crate::watchdog::CODDY_SUMMARY;
                let mut sp = connectors::plan(&conn, task, prompt, flag_cwd.as_deref())?;
                connectors::insert_track_args(&mut sp.argv, connectors::track_args(&conn, session_id, sessions_dir));
                let session = SessionRef::Coddy { sessions_dir: sessions_dir.clone(), session_id: session_id.clone() };
                (sp, Some(session), prompt.to_owned())
            }
        };

        // The attempt's prompt: the rendered prompt on a first run, the human
        // message on a steer. Kept forever (provenance); File-mode harnesses
        // get its path substituted into argv, replacing the deleted temp file.
        tokio::fs::write(attempt_dir.join("prompt.txt"), &prompt).await.with_context(|| "writing prompt file")?;
        if sp.file_prompt.is_some() {
            for tok in sp.argv.iter_mut() {
                if tok == "__ZORI_PROMPT_FILE__" {
                    *tok = attempt_dir.join("prompt.txt").display().to_string();
                }
            }
        }

        // Record the session handle before spawning, so even a crash keeps the
        // id (the transcript still shows captured stdout; the id survives).
        if let Some(sess) = &harness_session {
            emitter.mutate(|st| {
                if let Some(n) = st.nodes.get_mut(&task.id) {
                    n.session = Some(sess.clone());
                }
            });
        }
        emitter.set_status(task.id.as_str(), TaskStatus::Running);

        // ZORI_* context first, connector/task env can override.
        let mut env = artifacts::task_env(
            self.art.dirs.root(),
            &task_dir,
            &work_dir,
            self.art.project_dir.as_deref(),
        );
        env.extend(env_pairs(&conn, task));

        let summarizing = matches!(mode, ExecMode::CoddySummary { .. });
        let out = spawn_captured(
            &sp.program,
            &sp.argv,
            conn.prompt_via,
            sp.stdin_prompt.as_deref(),
            Some(&cwd),
            &env,
            timeout,
            &task.id,
            Some(emitter),
            supervision.filter(|s| !summarizing && s.ask && !s.unattended && crate::watchdog::preamble_applies(&conn))
                .map(|_| conn.id.clone()),
            &task.id,
        )
        .await;

        let out = out?;

        // Turn the raw capture into (a) what flows downstream and (b) the
        // transcript line. Only assistant text from JSONL goes downstream.
        let mut accepted_warning = None;
        let (agent_reply, final_reply, resolved_session, stream_error) = match conn.id.as_str() {
            "codex" => {
                let mut ev = connectors::parse_codex_jsonl(&out.stdout);
                let error = ev.error.or_else(|| {
                    (!ev.turn_completed && !ev.agent_messages.is_empty())
                        .then(|| "Codex stream ended before turn.completed".to_owned())
                });
                let sess = ev.thread_id.map(|thread_id| SessionRef::Codex { thread_id });
                (ev.agent_messages.join("\n\n"), ev.agent_messages.pop(), sess.or(harness_session), error)
            }
            "opencode" => {
                let ev = connectors::parse_opencode_jsonl(&out.stdout);
                let final_reply = ev.agent_texts[ev.final_step_start..].join("\n\n");
                let facts = Attempt { reply: &final_reply, success: out.success, timed_out: out.timed_out, exit_code: out.exit_code };
                let accept = supervision.is_some_and(|s| s.ask || s.retry > 0 || s.nudges > 0)
                    && crate::watchdog::accept_opencode_report(&facts, &ev);
                let sess = ev.session_id.map(|session_id| SessionRef::Opencode { session_id });
                if accept {
                    accepted_warning = ev.late_input_error;
                    (final_reply.clone(), Some(final_reply), sess.or(harness_session), None)
                } else {
                    let error = ev.error.or(ev.late_input_error).or_else(|| ev.step_open.then(|| "OpenCode stream ended before step_finish".to_owned()));
                    (ev.agent_texts.join("\n\n"), Some(final_reply), sess.or(harness_session), error)
                }
            }
            _ => (out.stdout.clone(), None, harness_session, None),
        };
        let facts = Attempt {
            reply: final_reply.as_deref().unwrap_or(&agent_reply),
            success: (out.success || accepted_warning.is_some()) && !out.timed_out,
            timed_out: out.timed_out,
            exit_code: out.exit_code,
        };
        // Arbitrary command harnesses can legitimately produce files and no stdout.
        let supervision = supervision.filter(|_| crate::watchdog::preamble_applies(&conn));
        let harness_verdict = match conn.id.as_str() {
            "coddy" => crate::watchdog::coddy_verdict(&facts, &out.stderr, summarizing),
            _ if !out.timed_out => stream_error.clone().map(Verdict::Retry),
            _ => None,
        };
        let verdict = harness_verdict.unwrap_or_else(|| {
            attempt_verdict(&facts, supervision.filter(|_| supervise_failures), resumable(&resolved_session))
        });
        let verdict = question_verdict(emitter, &task.id, verdict, resumable(&resolved_session), supervision.is_some_and(|s| s.unattended));
        // Review (minor): when the JSONL stream also broke at the moment the
        // timeout fired, only the timeout reached the operator — the truncated
        // stream was visible nowhere but in stdout.log. Keep the timeout as the
        // head (its exact prefix is a stable contract: watchdog retries grep
        // it) and append the stream reason, so the attempt names both causes.
        let fail_reason = if out.timed_out {
            let mut reason = format!("TIMED OUT after {}s (killed with its process group)", timeout.as_secs());
            if let Some(se) = &stream_error {
                reason.push_str(&format!("; stream also failed: {se}"));
            }
            Some(reason)
        } else if !facts.success && stream_error.is_none() {
            Some(out.exit_code.map(|c| format!("exited with code {c}"))
                .unwrap_or_else(|| "killed by signal (no exit code)".into()))
        } else {
            verdict_failure(&verdict)
        };
        let status = if fail_reason.is_some() { TaskStatus::Failed } else { TaskStatus::Done };
        let stderr_final = if out.timed_out {
            let mut head = format!("TIMED OUT after {}s", timeout.as_secs());
            if let Some(se) = &stream_error {
                head.push_str(&format!(" (stream also failed: {se})"));
            }
            format!("{head}\n{}", out.stderr)
        } else if let Some(reason) = &fail_reason {
            format!("{reason}\n{}", out.stderr)
        } else if let Some(warning) = &accepted_warning {
            format!("{warning}\n{}", out.stderr)
        } else {
            out.stderr.clone()
        };

        emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(&task.id) {
                if resolved_session.is_some() {
                    n.session = resolved_session.clone();
                }
                n.turns.push(crate::state::Turn { role: Role::Agent, text: agent_reply.clone(), ts: now_secs() });
                if let Some(warning) = &accepted_warning {
                    n.turns.push(crate::state::Turn { role: Role::System,
                        text: format!("[watchdog] Final answer accepted. OpenCode reported after the step finished: {warning}"),
                        ts: now_secs() });
                }
                n.output = agent_reply.clone();
                n.stderr = stderr_final.clone();
                n.exit_code = out.exit_code;
                n.status = question_status(n, status, &verdict);
                n.finished_at = Some(now_secs());
                n.progress.bytes = out.stdout.len();
            }
        });

        // Persist artifacts, mirroring run_task (full text even on timeout):
        // one directory per attempt under the task's subtree, plus the
        // manifest record with the session handle a fork would need to resume.
        let ended_at = now_secs();
        let _ = tokio::fs::write(attempt_dir.join("out.txt"), &agent_reply).await;
        let _ = tokio::fs::write(attempt_dir.join("err.txt"), &stderr_final).await;
        if (fail_reason.is_some() || accepted_warning.is_some()) && matches!(conn.id.as_str(), "codex" | "opencode") {
            let _ = tokio::fs::write(attempt_dir.join("stdout.log"), &out.stdout).await;
        }
        let session = emitter
            .snapshot()
            .nodes
            .get(&task.id)
            .and_then(|n| n.session.clone());
        let _ = artifacts::write_meta(
            &attempt_dir,
            &AttemptMeta {
                harness: conn.id.clone(),
                model: task.model.clone(),
                argv: sp.argv.clone(),
                env_keys: env_keys(&conn, task),
                status: status.label().to_string(),
                exit_code: out.exit_code,
                started_at: Some(started_at),
                ended_at: Some(ended_at),
                cwd: Some(cwd.clone()),
                prompt_sha256: artifacts::sha256_hex(prompt.as_bytes()),
            },
        );
        self.art.manifest.record_attempt(
            &task.id,
            &conn.id,
            task.model.clone(),
            task.optional,
            &nonce,
            AttemptRecord {
                status: status.label().to_string(),
                error: fail_reason,
                exit_code: out.exit_code,
                started_at: Some(started_at),
                ended_at: Some(ended_at),
                cwd: Some(cwd.clone()),
                session,
                out: Some(OutFile {
                    path: self.art.dirs.relative(&attempt_dir.join("out.txt")),
                    sha256: artifacts::sha256_hex(agent_reply.as_bytes()),
                    bytes: agent_reply.len(),
                }),
            },
        );
        Ok(verdict)
    }
}

pub(crate) fn canvas_prompt(value: Option<serde_json::Value>) -> String {
    let Some(mut v) = value else { return String::new(); };
    if let Some(tasks) = v.get_mut("tasks").and_then(|t| t.as_array_mut()) {
        for t in tasks.iter_mut() {
            if let Some(obj) = t.as_object_mut() {
                obj.remove("pos");
            }
        }
    }
    match serde_json::to_string_pretty(&v) {
        Ok(json) => format!(
            "\n\nThe current graph on the canvas (the operator may have edited it by hand — \
             moved nodes, changed a model, added a dependency). Rely on it when discussing and \
             changing the graph:\n\
             ```json\n{json}\n```"
        ),
        Err(_) => String::new(),
    }
}

fn publish_questions(emitter: &Emitter, id: &str, text: &str, fence: &mut Option<char>) {
    for mut question in text.lines().filter_map(|l| crate::watchdog::protocol_line(l, fence))
        .filter_map(crate::watchdog::question_line) {
        emitter.mutate(|st| {
            let Some(n) = st.nodes.get_mut(id) else { return };
            if n.questions.iter().any(|q| !q.closed && q.text == question.text && q.options == question.options) {
                return;
            }
            question.id = n.questions.len() + 1;
            n.questions.push(question);
        });
    }
}

fn question_verdict(emitter: &Emitter, id: &str, verdict: Verdict, can_resume: bool, unattended: bool) -> Verdict {
    if matches!(verdict, Verdict::Retry(_) | Verdict::Incomplete) { return verdict; }
    if let Some(q) = emitter.snapshot().nodes[id].questions.iter().find(|q| !q.closed) {
        if can_resume { return Verdict::Ask(q.text.clone()); }
        return Verdict::Retry("operator question without a resumable session".into());
    }
    if !unattended && matches!(verdict, Verdict::Ask(_)) { Verdict::Ok } else { verdict }
}

fn question_status(node: &mut crate::state::TaskNode, status: TaskStatus, verdict: &Verdict) -> TaskStatus {
    if matches!(verdict, Verdict::Ask(_)) && node.questions.iter().any(|q| !q.closed) {
        return TaskStatus::Running;
    }
    if status == TaskStatus::Failed {
        for q in &mut node.questions { q.closed = true; }
    }
    status
}

/// With supervision disabled only process failures count. A non-resumable
/// agent gets a fresh attempt when a nudge would otherwise be needed.
fn attempt_verdict(facts: &Attempt, settings: Option<&Settings>, can_resume: bool) -> Verdict {
    if facts.success && settings.is_none_or(|s| !s.ask && s.nudges == 0 && s.retry == 0) {
        return Verdict::Ok;
    }
    let verdict = crate::watchdog::judge(facts, settings.is_some_and(|s| s.ask));
    match verdict {
        Verdict::Nudge(kind) if !can_resume => Verdict::Retry(format!("unfinished reply ({kind:?}), no session to resume")),
        other => other,
    }
}

fn verdict_failure(verdict: &Verdict) -> Option<String> {
    match verdict {
        Verdict::CoddyStepLimit(n) => Some(format!("Coddy reached agent.max_turns ({n} steps)")),
        Verdict::Incomplete => Some("Coddy summary reports unfinished work".into()),
        Verdict::Nudge(kind) => Some(format!("watchdog: unfinished reply ({kind:?})")),
        Verdict::Retry(reason) => Some(reason.clone()),
        _ => None,
    }
}

/// A nonce distinguishing artifact files per attempt: a first run is `r0`,
/// its watchdog retries `r1`, `r2`, …, and every resume (steer, nudge, ask
/// answer) is the current second — so artifacts never overwrite each other.
fn mode_nonce(mode: &ExecMode, attempt: u32) -> String {
    match mode {
        ExecMode::FirstRun { .. } => format!("r{attempt}"),
        ExecMode::Resume(..) | ExecMode::CoddySummary { .. } => format!("t{}", now_secs()),
    }
}

/// True when the session handle can carry a resume (the watchdog's nudges,
/// answers and parks all continue the same session; an `Opaque`/absent
/// session can only be re-run from scratch or left alone).
fn resumable(session: &Option<SessionRef>) -> bool {
    matches!(
        session,
        Some(
            SessionRef::Coddy { .. }
                | SessionRef::Codex { .. }
                | SessionRef::Opencode { .. }
                | SessionRef::Echo
        )
    )
}

/// Merge connector-level and task-level env into spawn pairs (task wins).
fn env_pairs(conn: &Connector, task: &Task) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> =
        conn.env.iter().map(|(k, val)| (k.clone(), val.clone())).collect();
    for (k, val) in &task.env {
        v.retain(|(ek, _)| ek != k);
        v.push((k.clone(), val.clone()));
    }
    v
}

/// Spawn a child and capture it, reusing `run_task`'s hard-won discipline:
/// readers started before stdin (no pipe deadlock), `wait()` raced against a
/// deadline and a heartbeat ticker, kill-the-whole-group *before* draining on
/// timeout, a bounded reader grace, and partial-output snapshots so a killed
/// run still yields text. `emitter`/`node_id` (when both `Some`) get progress
/// updates on every heartbeat and after the final drain.
// Mirrors `run_task`'s parameter list deliberately: it is the same spawn
// contract plus two progress args; folding them into a struct here would make
// the two loops harder to diff against each other.
#[allow(clippy::too_many_arguments)]
async fn spawn_captured(
    program: &str,
    argv: &[String],
    prompt_via: PromptVia,
    stdin_prompt: Option<&str>,
    cwd: Option<&Path>,
    env: &[(String, String)],
    timeout: Duration,
    label: &str,
    emitter: Option<&Emitter>,
    question_harness: Option<String>,
    node_id: &str,
) -> Result<TrackedOutcome> {
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]); // argv[0] is the program
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning '{program}': {}", render_argv(argv)))?;
    let group = GroupRegistration::register(child.id());

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout handle"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr handle"))?;
    let out_buf = SharedBuf::default();
    let err_buf = SharedBuf::default();
    let observer = emitter.cloned().zip(question_harness);
    let question_node = node_id.to_owned();
    let mut plain_text_fence = None;
    let mut stdout_task = tokio::spawn(read_stream_with(stdout, out_buf.clone(), move |line| {
        let Some((emitter, harness)) = &observer else { return };
        match harness.as_str() {
            "codex" => {
                for text in connectors::parse_codex_jsonl(line).agent_messages {
                    publish_questions(emitter, &question_node, &text, &mut None);
                }
            }
            "opencode" => {
                for text in connectors::parse_opencode_jsonl(line).agent_texts {
                    publish_questions(emitter, &question_node, &text, &mut None);
                }
            }
            _ => publish_questions(emitter, &question_node, line, &mut plain_text_fence),
        }
    }));
    let mut stderr_task = tokio::spawn(read_stream(stderr, err_buf.clone()));

    // Clock starts before stdin is fed (review F01), same discipline as
    // run_task: a child that never drains a big prompt must not sit outside
    // the deadline.
    let start = Instant::now();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut timed_out = false;

    {
        let mut si = child.stdin.take().ok_or_else(|| anyhow!("no stdin handle"))?;
        if matches!(prompt_via, PromptVia::Stdin) {
            let text = stdin_prompt.unwrap_or("");
            tokio::select! {
                biased;
                r = si.write_all(text.as_bytes()) => {
                    if let Err(e) = r {
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            eprintln!("  [{label}] stdin write error: {e}");
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    eprintln!("  [{label}] TIMED OUT feeding stdin ({}s)", timeout.as_secs());
                    timed_out = true;
                }
            }
        }
    }

    let wait = child.wait();
    tokio::pin!(wait);
    let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    let push_progress = |bytes: usize, lines: usize, secs: u64| {
        if let Some(em) = emitter {
            let id = node_id.to_string();
            em.mutate(move |st| {
                if let Some(n) = st.nodes.get_mut(&id) {
                    n.progress.bytes = bytes;
                    n.progress.lines = lines;
                    n.progress.elapsed_secs = secs;
                }
            });
        }
    };

    let mut exit_res: Option<std::io::Result<std::process::ExitStatus>> = None;
    loop {
        tokio::select! {
            biased;
            r = &mut wait => { exit_res = Some(r); break; }
            _ = tokio::time::sleep_until(deadline) => { timed_out = true; break; }
            _ = ticker.tick() => {
                let secs = start.elapsed().as_secs();
                println!("  [{label}] … running {secs}s, {} bytes out so far", out_buf.bytes());
                push_progress(out_buf.bytes(), out_buf.lines(), secs);
            }
        }
    }

    if timed_out {
        eprintln!("  [{label}] TIMED OUT after {}s — killing group", timeout.as_secs());
        group.kill_group().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut wait).await;
    }

    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = (&mut stdout_task).await;
        let _ = (&mut stderr_task).await;
    })
    .await;
    stdout_task.abort();
    stderr_task.abort();

    push_progress(out_buf.bytes(), out_buf.lines(), start.elapsed().as_secs());

    let stdout_text = out_buf.snapshot();
    let stderr_text = err_buf.snapshot();

    let (success, exit_code) = if timed_out {
        (false, None)
    } else {
        let st = exit_res.unwrap().with_context(|| format!("waiting on '{label}'"))?;
        (st.success(), st.code())
    };

    Ok(TrackedOutcome {
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code,
        timed_out,
        success,
    })
}

/// Result of one tracked child execution.
pub(crate) struct TrackedOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub success: bool,
}

#[cfg(test)]
mod tracked_tests {
    use super::*;
    use crate::config::Task;
    use crate::state::RunState;
    use std::collections::BTreeMap;

    fn echo_task(id: &str, deps: &[&str]) -> Task {
        Task {
           skills: vec![],
            watchdog: None,
            id: id.into(),
            harness: "echo".into(),
            model: None,
            in_field: format!("say {id}"),
            out: None,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: BTreeMap::new(),
            args: vec![],
            timeout_secs: None,
            optional: false,
            pos: None, workspace: None,
            chat: None,
        }
    }

    fn codex_task(id: &str, in_field: &str) -> Task {
        Task {
            skills: vec![],
            watchdog: None,
            id: id.into(),
            harness: "codex".into(),
            model: None,
            in_field: in_field.into(),
            out: None,
            depends_on: vec![],
            cwd: None,
            env: BTreeMap::new(),
            args: vec![],
            timeout_secs: None,
            optional: false,
            pos: None,
            workspace: None,
            chat: None,
        }
    }

    fn preview_flow(task: Task) -> Flow {
        Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "preview".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: None,
            max_parallel: None,
            tasks: vec![task],
        }
    }

    // Review M10: dry-run interpolated with an empty ctx, so any documented
    // path ref died with "unknown reference" — and it printed the codex argv
    // without its `-C`, claiming a command that would never have run. The
    // preview must resolve every path ref and reuse the drivers' cwd choice.
    #[test]
    fn dry_preview_resolves_path_refs_and_the_cwd_flag() {
        let flow = preview_flow(codex_task("rev", "read ${run.dir}/x ${task.dir} ${task.work} ${project.dir}"));
        let task = &flow.tasks[0];
        let (ctx, flag) = dry_preview(&flow, task);
        assert!(ctx.run_dir.is_some() && ctx.task_dir.is_some() && ctx.work_dir.is_some());
        assert!(ctx.project_dir.is_some(), "README: project.dir = flow cwd or zori's own");
        let empty: HashMap<String, String> = HashMap::new();
        for key in ["${run.dir}", "${task.dir}", "${task.work}", "${project.dir}"] {
            let r = interpolate(key, &empty, &empty, &ctx).unwrap_or_else(|e| panic!("{key}: {e}"));
            assert!(!r.is_empty(), "{key} rendered empty");
        }
        let flag = flag.expect("workspace: task passes the work dir as a flag");
        assert!(flag.ends_with("tasks/rev/work"), "flag path: {}", flag.display());
        let conn = connectors::resolve("codex", None).unwrap();
        let sp = connectors::plan(&conn, task, "x", Some(&flag)).unwrap();
        assert!(sp.argv.windows(2).any(|w| w[0] == "-C"), "codex argv must carry -C: {:?}", sp.argv);
        // A dry run over the same flow must not error on the path refs (the
        // old shape returned Err("unknown reference '${task.work}'")).
        let plan = plan(&flow).unwrap();
        dry_run(&flow, &plan, &[]).expect("dry_run survives path refs");
    }

    // Review M10: the README promises `${project.dir}` = "the flow cwd, or
    // zori's own" — the drivers left it empty whenever flow.cwd was unset.
    #[test]
    fn project_dir_falls_back_to_zoris_own_dir() {
        let mut flow = preview_flow(codex_task("rev", "x"));
        let own = project_dir_of(&flow).expect("no cwd → zori's own dir");
        assert_eq!(own, artifacts::absolute(&std::env::current_dir().unwrap()));
        flow.cwd = Some(std::path::PathBuf::from("somewhere"));
        let p = project_dir_of(&flow).unwrap();
        assert!(p.to_string_lossy().ends_with("somewhere"), "flow cwd wins: {p:?}");
    }

    /// A two-level echo flow: a → b. Build TrackedRun + initial RunState.
    async fn tracked_fixture(dir: &std::path::Path) -> (TrackedRun, Emitter) {
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "tracked".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![echo_task("a", &[]), echo_task("b", &["a"])],
        };
        let plan = plan(&flow).unwrap();
        let out_dir = dir.join("run");
        let sessions_dir = dir.join("sessions");
        let tr = TrackedRun::new(
            flow.clone(),
            plan.clone(),
            &[],
            out_dir.clone(),
            sessions_dir.clone(),
        )
        .unwrap();
        let state = RunState::new(
            "tracked-1".into(),
            flow.name.clone(),
            "tracked.yml".into(),
            Some(out_dir),
            plan.levels.clone(),
            &flow.tasks.iter().map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone())).collect::<Vec<_>>(),
        );
        (tr, Emitter::new(state))
    }

    #[tokio::test]
    async fn run_all_completes_nodes_in_order() {
        let dir = std::env::temp_dir().join(format!("zori-trk-all-{}", now_secs()));
        let (tr, em) = tracked_fixture(&dir).await;
        tr.run_all(&em).await;
        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done);
        assert_eq!(s.nodes["b"].status, TaskStatus::Done);
        assert_eq!(s.status, crate::state::RunStatus::Completed);
        // Echo replies with its prompt; b's transcript has one agent turn.
        assert_eq!(s.nodes["a"].turns.len(), 1);
        assert_eq!(s.nodes["a"].turns[0].role, Role::Agent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- watchdog: the supervision loop, exercised on the echo probe ------
    //
    // Echo participates in supervision (its reply IS the rendered prompt /
    // resume text), so the whole loop — verdict → nudge/auto-answer/park →
    // resume → re-verdict — runs in CI without a model.

    /// Build a TrackedRun + Emitter for an arbitrary flow (the watchdog
    /// fixtures below need custom prompts/settings per test).
    async fn supervised_fixture(dir: &std::path::Path, flow: Flow) -> (TrackedRun, Emitter) {
        let plan = plan(&flow).unwrap();
        let out_dir = dir.join("run");
        let tr = TrackedRun::new(flow.clone(), plan.clone(), &[], out_dir.clone(), dir.join("sessions"))
            .unwrap();
        let state = RunState::new(
            "sup-1".into(),
            flow.name.clone(),
            "sup.yml".into(),
            Some(out_dir),
            plan.levels.clone(),
            &flow.tasks.iter().map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone())).collect::<Vec<_>>(),
        );
        (tr, Emitter::new(state))
    }

    /// Real child processes emit harness events but deliberately exit zero.
    /// Inspect both the DAG outcome and the persisted attempt reasons.
    #[tokio::test]
    async fn watchdog_checks_streams_before_unblocking_dependents() {
        let cases = [
            ("codex", r#"{"type":"thread.started","thread_id":"tid"}"#, "Empty", true),
            ("codex", r#"{"type":"item.completed","item":{"type":"agent_message","text":"working"}} "#,
                "before turn.completed", false),
            ("codex", r#"{"type":"turn.failed","error":{"message":"usage limit exceeded"}}"#,
                "usage limit exceeded", false),
            ("codex", r#"{"type":"item.completed","item":{"type":"agent_message","text":"Starting"}}
{"type":"item.completed","item":{"type":"agent_message","text":"connection lost"}}
{"type":"turn.completed"}"#, "connection lost", false),
            ("opencode", r#"{"type":"step_start","sessionID":"sid"}
{"type":"text","part":{"type":"text","text":"Partial result"}}"#, "before step_finish", false),
            ("opencode", r#"{"type":"error","error":{"data":{"message":"connection lost"}}}"#,
                "connection lost", false),
        ];
        for (harness, stream, reason, resume) in cases {
            let dir = std::env::temp_dir().join(artifacts::new_run_id("zori-watchdog-stream"));
            let command = vec!["/bin/sh".into(), "-c".into(), "echo \"$1\"".into(), "test".into(), stream.into()];
            let flow = Flow {
                skills: vec![],
                skills_dirs: vec![],
                name: "stream".into(), inputs: BTreeMap::new(), cwd: None,
                default_timeout_secs: Some(10), max_parallel: None, watchdog: None,
                harnesses: BTreeMap::from([(harness.into(), crate::config::HarnessOverride {
                    command: Some(command), ..Default::default()
                })]),
                tasks: vec![Task { harness: harness.into(), ..echo_task("a", &[]) }, echo_task("b", &["a"])],
            };
            let (tr, em) = supervised_fixture(&dir, flow).await;
            tr.run_all(&em).await;
            let state = em.snapshot();
            assert_eq!(state.nodes["a"].exit_code, Some(0));
            assert_eq!(state.nodes["a"].status, TaskStatus::Failed);
            assert!(state.nodes["a"].stderr.contains(reason));
            assert_eq!(state.nodes["b"].status, TaskStatus::Skipped);
            let manifest: artifacts::Manifest = serde_json::from_slice(
                &std::fs::read(dir.join("run/manifest.json")).unwrap()).unwrap();
            let attempts = &manifest.tasks["a"].attempts;
            assert_eq!(attempts.len(), 2);
            assert_eq!(attempts.contains_key("r1"), !resume);
            for attempt in attempts.values() {
                assert_eq!(attempt.status, "failed");
                assert!(attempt.error.as_deref().unwrap().contains(reason));
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn finished_opencode_report_reaches_the_join_on_first_attempt() {
        let dir = std::env::temp_dir().join(artifacts::new_run_id("zori-opencode-report"));
        let flow: Flow = serde_yaml::from_str(r#"
name: report
harnesses:
  opencode:
    command:
      - /bin/sh
      - -c
      - |
        cat <<'EVENTS'
        {"type":"step_start","sessionID":"sid"}
        {"type":"text","part":{"type":"text","text":"Getting started"}}
        {"type":"step_finish","part":{"reason":"tool-calls"}}
        {"type":"step_start","sessionID":"sid"}
        {"type":"text","part":{"type":"text","text":"Final research report"}}
        {"type":"step_finish","part":{"reason":"stop"}}
        {"type":"error","error":{"message":"Cancelled a request for input this run cannot answer: Web Search"}}
        EVENTS
        exit 1
tasks:
  - { id: science, harness: opencode, in: Research }
  - { id: summary, harness: echo, in: Summarize, depends_on: [science] }
"#).unwrap();
        let (tr, em) = supervised_fixture(&dir, flow).await;
        tr.run_all(&em).await;
        let state = em.snapshot();
        let science = &state.nodes["science"];
        assert_eq!(science.status, TaskStatus::Done);
        assert_eq!(science.exit_code, Some(1));
        assert_eq!(science.output, "Final research report");
        assert!(science.stderr.contains("Web Search"));
        assert_eq!(state.nodes["summary"].status, TaskStatus::Done);
        assert!(state.nodes["summary"].output.contains(&science.output));
        let manifest: artifacts::Manifest = serde_json::from_slice(
            &std::fs::read(dir.join("run/manifest.json")).unwrap()).unwrap();
        let attempts = &manifest.tasks["science"].attempts;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts["r0"].status, "done");
        assert_eq!(attempts["r0"].exit_code, Some(1));
        assert!(dir.join("run/tasks/science/r0/stdout.log").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn coddy_step_cap_requests_one_summary_before_releasing_dependents() {
        let script = r#"
while [ "$#" -gt 0 ]; do
    case "$1" in
        -p) prompt=$2; shift ;;
        -session-id) session=$2; shift ;;
        -sessions-dir) sessions_dir=$2; shift ;;
    esac
    shift
done
if [ -f session ]; then
    test "$(cat session)" = "$sessions_dir/$session" || exit 7
    case "$prompt" in *ZORI_INCOMPLETE*) ;; *) exit 8 ;; esac
    case "$SUMMARY" in
        ok) echo 'final report'; exit 0 ;;
        partial) printf 'ZORI_INCOMPLETE\nTests still pending\n'; exit 0 ;;
        crash) exit 9 ;;
    esac
fi
echo "$sessions_dir/$session" > session
echo 'I have edited the file and will run tests.'
echo 'Stopped after 165 steps, the step limit set by agent.max_turns. The task may be unfinished: send a message to let the agent continue, or raise the limit.' >&2
"#;
        for (summary, nudges) in [("ok", 1), ("partial", 2), ("cap_again", 2), ("crash", 2), ("off", 0)] {
            let dir = std::env::temp_dir().join(artifacts::new_run_id("coddy-step-limit"));
            let mut flow: Flow = serde_yaml::from_str(r#"
name: step-limit
tasks:
  - {id: a, harness: coddy, in: implement}
  - {id: b, harness: echo, in: report, depends_on: [a]}
"#).unwrap();
            flow.tasks[0].env.insert("SUMMARY".into(), summary.into());
            flow.watchdog = Some(crate::config::Watchdog { nudges: Some(nudges), ..Default::default() });
            flow.harnesses.insert("coddy".into(), crate::config::HarnessOverride {
                command: Some(vec!["sh".into(), "-c".into(), script.into(), "coddy".into()]),
                ..Default::default()
            });
            let (tr, em) = supervised_fixture(&dir, flow).await;
            tr.run_all(&em).await;
            let state = em.snapshot();
            if summary == "ok" {
                assert_eq!(state.nodes["a"].status, TaskStatus::Done);
                assert!(state.nodes["b"].output.contains("final report"));
                assert!(!state.nodes["b"].output.contains("will run tests"));
            } else {
                assert_eq!(state.nodes["a"].status, TaskStatus::Failed, "{summary}");
                assert_eq!(state.nodes["b"].status, TaskStatus::Skipped);
                if summary == "partial" {
                    assert!(state.nodes["a"].output.contains("Tests still pending"));
                }
            }
            let manifest: artifacts::Manifest = serde_json::from_slice(
                &std::fs::read(dir.join("run/manifest.json")).unwrap()).unwrap();
            let attempts = &manifest.tasks["a"].attempts;
            assert_eq!(attempts["r0"].exit_code, Some(0));
            assert!(attempts["r0"].error.as_deref().unwrap().contains("165"));
            assert_eq!(attempts.len(), if nudges == 0 { 1 } else { 2 });
            assert!(!attempts.contains_key("r1"));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn inline_questions_arrive_live_and_resume_with_every_answer() {
        let dir = std::env::temp_dir().join(artifacts::new_run_id("zori-inline"));
        let gate = dir.join("finish-turn");
        let script = r#"
for arg do
    if [ "$arg" = resume ]; then resumed=yes; fi
    last=$arg
done
echo '{"type":"thread.started","thread_id":"my-session"}'
if [ "$resumed" = yes ]; then
    case "$last" in
        *"Format? — CSV"*"Version? — second"*) ;;
        *) exit 4 ;;
    esac
    echo '{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}'
else
    echo '{"type":"item.completed","item":{"type":"command_execution","text":"ASK_INLINE: tool noise"}}'
    echo '{"type":"item.completed","item":{"type":"agent_message","text":"ASK_INLINE: {\"question\":\"Format?\",\"options\":[\"CSV\",\"JSON\"]}"}}'
    echo '{"type":"item.completed","item":{"type":"agent_message","text":"ASK_INLINE: Version?"}}'
    while [ ! -e "$GATE" ]; do sleep 0.02; done
    echo '{"type":"item.completed","item":{"type":"agent_message","text":"Finished the independent part"}}'
fi
echo '{"type":"turn.completed"}'
"#;
        let harness = crate::config::HarnessOverride {
            command: Some(vec!["/bin/sh".into(), "-c".into(), script.into(), "probe".into()]),
            ..Default::default()
        };
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "inline".into(), inputs: BTreeMap::new(), cwd: None,
            watchdog: None, default_timeout_secs: Some(10), max_parallel: None,
            harnesses: BTreeMap::from([("codex".into(), harness)]),
            tasks: vec![Task { harness: "codex".into(), env: BTreeMap::from([
                ("GATE".into(), gate.display().to_string())
            ]), ..echo_task("a", &[]) }, echo_task("b", &["a"])],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        let operator = async {
            let mut snapshots = em.subscribe();
            let live = loop {
                let s = em.snapshot();
                if s.nodes["a"].questions.len() == 2 { break s; }
                snapshots.recv().await.unwrap();
            };
            let q = &live.nodes["a"].questions;
            assert_eq!(live.nodes["a"].status, TaskStatus::Running);
            assert_eq!(q[0].options, ["CSV", "JSON"]);
            tr.answer_question(&em, "a", Some(q[1].id), "second", None).unwrap();
            assert!(tr.answer_question(&em, "a", Some(q[1].id), "duplicate", None).is_err());
            std::fs::write(&gate, "").unwrap();
            while em.snapshot().nodes["a"].status != TaskStatus::Waiting {
                snapshots.recv().await.unwrap();
            }
            // A subscriber arriving after the stream events still gets the pending question.
            let snap = em.snapshot();
            assert_eq!(snap.nodes["a"].questions[1].answer.as_deref(), Some("second"));
            assert_eq!(snap.nodes["b"].status, TaskStatus::Pending);
            tr.answer_question(&em, "a", Some(q[0].id), "CSV", None).unwrap();
        };
        tokio::time::timeout(Duration::from_secs(8), async {
            tokio::join!(tr.run_all(&em), operator);
        }).await.unwrap();
        let final_state = em.snapshot();
        assert_eq!(final_state.nodes["a"].output, "Done");
        assert!(final_state.nodes["a"].questions.iter().all(|q| q.closed));
        assert_eq!(final_state.nodes["b"].status, TaskStatus::Done);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn watchdog_delivers_concurrent_answers_to_the_right_node() {
        let dir = std::env::temp_dir().join(artifacts::new_run_id("zori-park-two"));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "questions".into(), harnesses: BTreeMap::new(), inputs: BTreeMap::new(),
            cwd: None, default_timeout_secs: None, max_parallel: Some(2), watchdog: None,
            tasks: ["a", "b"].into_iter().map(|id| Task {
                skills: vec![],
                in_field: "ASK: format?".into(), ..echo_task(id, &[])
            }).collect(),
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        let operator = async {
            while em.snapshot().nodes.values().any(|n| n.status != TaskStatus::Waiting) {
                tokio::task::yield_now().await;
            }
            tr.answer(&em, "b", "json").await.unwrap();
            while em.snapshot().nodes["b"].status != TaskStatus::Done {
                tokio::task::yield_now().await;
            }
            assert_eq!(em.snapshot().nodes["a"].status, TaskStatus::Waiting);
            tr.answer(&em, "a", "yaml").await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(tr.run_all(&em), operator);
        }).await.unwrap();
        assert_eq!(em.snapshot().nodes["a"].output, "yaml");
        assert_eq!(em.snapshot().nodes["b"].output, "json");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A clipped reply is auto-nudged in the same session: the transcript
    /// shows the [watchdog] system turn, the final output is the nudge reply,
    /// and both attempts land in the manifest (`r0` + a `t<secs>` resume).
    #[tokio::test]
    async fn watchdog_nudges_a_clipped_reply() {
        let dir = std::env::temp_dir().join(format!("zori-wd-clip-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "clipped".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            watchdog: None,
            tasks: vec![Task {
                // echo replies with its prompt → the reply carries the
                // clip marker → verdict Nudge → resume with the nudge
                // text → echo replies with THAT (clean) → Done.
                in_field: "working\n<response clipped>".into(),
                ..echo_task("a", &[])
            }],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done, "nudge rescued the reply");
        let roles: Vec<Role> = s.nodes["a"].turns.iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![Role::Agent, Role::System, Role::Agent], "nudge is a system turn");
        assert!(s.nodes["a"].turns[1].text.contains("[watchdog]"), "{}", s.nodes["a"].turns[1].text);
        assert!(s.nodes["a"].output.contains("cut off"), "final output is the nudge reply");

        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(dir.join("run/manifest.json")).unwrap(),
        )
        .unwrap();
        let nonces: Vec<&String> = manifest.tasks["a"].attempts.keys().collect();
        assert_eq!(nonces.len(), 2, "both attempts recorded: {nonces:?}");
        assert!(nonces.iter().any(|n| n.starts_with('r')), "{nonces:?}");
        assert!(nonces.iter().any(|n| n.starts_with('t')), "the nudge is a resume attempt: {nonces:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An ASK parks the node (`waiting` + the question on `ask`), the level
    /// barrier holds the dependent, and the operator's answer resumes the
    /// session and completes the flow.
    #[tokio::test]
    async fn watchdog_parks_ask_and_answer_resumes() {
        let dir = std::env::temp_dir().join(format!("zori-wd-ask-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "asks".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            watchdog: None,
            tasks: vec![
                Task { in_field: "starting\nASK: which report format?".into(), ..echo_task("a", &[]) },
                echo_task("b", &["a"]),
            ],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;

        let driver = tokio::spawn({
            let tr = tr.clone();
            let em = em.clone();
            async move { tr.run_all(&em).await }
        });

        // Wait for the park: status waiting, question published, dependent held.
        let parked = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let s = em.snapshot();
                if s.nodes["a"].status == TaskStatus::Waiting {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("node parks on ASK");
        assert_eq!(parked.nodes["a"].ask.as_deref(), Some("ASK: which report format?"));
        assert_eq!(parked.nodes["b"].status, TaskStatus::Pending, "barrier holds the dependent");
        assert_eq!(parked.status, crate::state::RunStatus::Running);

        // Answer from the "operator": the human turn lands, the session
        // resumes with the answer text (echo replies with it) → Done → b runs.
        tr.answer(&em, "a", "format: a short list").await.unwrap();
        driver.await.unwrap();

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done);
        assert_eq!(s.nodes["a"].ask, None, "question cleared");
        assert_eq!(s.nodes["a"].output, "format: a short list");
        let roles: Vec<Role> = s.nodes["a"].turns.iter().map(|t| t.role).collect();
        assert_eq!(roles, vec![Role::Agent, Role::System, Role::Human, Role::Agent], "{roles:?}");
        assert_eq!(s.nodes["b"].status, TaskStatus::Done, "dependent ran after the answer");
        assert_eq!(s.status, crate::state::RunStatus::Completed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unattended mode: no parking — the question gets the canned auto-answer
    /// as a system turn, the session continues, the run completes alone.
    #[tokio::test]
    async fn watchdog_unattended_auto_answers() {
        let dir = std::env::temp_dir().join(format!("zori-wd-night-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "night".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            watchdog: Some(crate::config::Watchdog {
                unattended: Some(true),
                ..Default::default()
            }),
            tasks: vec![Task { in_field: "asking\nASK: what should I do?".into(), ..echo_task("a", &[]) }],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done, "no parking overnight");
        assert_eq!(s.nodes["a"].ask, None);
        let sys: Vec<&str> = s.nodes["a"].turns.iter().filter(|t| t.role == Role::System).map(|t| t.text.as_str()).collect();
        assert!(sys.iter().any(|t| t.contains("auto-answering")), "{sys:?}");
        assert_eq!(s.nodes["a"].output, crate::watchdog::AUTO_ANSWER, "echo replied with the canned answer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed attempt is retried from scratch within budget: attempts r0
    /// AND r1 land in the manifest, the [watchdog] retry turn is recorded,
    /// and the node ends Failed once the budget is spent.
    #[tokio::test]
    async fn watchdog_retries_a_failed_attempt_from_scratch() {
        let dir = std::env::temp_dir().join(format!("zori-wd-retry-{}", now_secs()));
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "always-false".to_string(),
            crate::config::HarnessOverride {
                command: Some(vec!["/usr/bin/false".to_string()]),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "retry".into(),
            harnesses,
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            watchdog: None, // defaults: retry 1
            tasks: vec![Task {
                harness: "always-false".into(),
                in_field: "do something".into(),
                ..echo_task("a", &[])
            }],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Failed);
        assert!(s.nodes["a"].turns.iter().any(|t| t.role == Role::System && t.text.contains("exit 1")), "{:?}",
            s.nodes["a"].turns.iter().map(|t| t.text.clone()).collect::<Vec<_>>());

        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(dir.join("run/manifest.json")).unwrap(),
        )
        .unwrap();
        let mut nonces: Vec<String> = manifest.tasks["a"].attempts.keys().cloned().collect();
        nonces.sort();
        assert_eq!(nonces, vec!["r0".to_string(), "r1".to_string()], "exactly one retry: {nonces:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Postmortem bug 1 (tracked driver): both dead zori-review nodes showed
    /// `error: null` with empty stderr. A failed tracked attempt now names
    /// its cause in the manifest, on the node, and in `err.txt`.
    #[tokio::test]
    async fn tracked_failed_attempt_records_the_reason() {
        let dir = std::env::temp_dir().join(format!("zori-trk-fail-{}", now_secs()));
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "exit3".to_string(),
            crate::config::HarnessOverride {
                command: Some(vec!["sh".to_string(), "-c".to_string(), "echo boom >&2; exit 3".to_string()]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: Some(crate::config::Watchdog {
                nudges: Some(0),
                retry: Some(0),
                ask: Some(false),
                ..Default::default()
            }),
            name: "trk-fail".into(),
            harnesses,
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![Task { harness: "exit3".into(), in_field: "get to work".into(), ..echo_task("a", &[]) }],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Failed);
        assert!(s.nodes["a"].stderr.contains("boom"), "node stderr kept: {:?}", s.nodes["a"].stderr);

        let manifest: artifacts::Manifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join("run/manifest.json")).unwrap()).unwrap();
        let att = &manifest.tasks["a"].attempts["r0"];
        assert_eq!(att.error.as_deref(), Some("exited with code 3"), "manifest names the exit code");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tracked timeout must look like the classic one: a `TIMED OUT` line
    /// on the node and in `err.txt` (the watchdog classifies retries by
    /// grepping stderr), and the reason in the manifest `error`.
    #[tokio::test]
    async fn tracked_timeout_records_the_reason() {
        let dir = std::env::temp_dir().join(format!("zori-trk-timeout-{}", now_secs()));
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "hanger".to_string(),
            crate::config::HarnessOverride {
                command: Some(vec![
                    "bash".to_string(),
                    "-c".to_string(),
                    "exec -a zori-probe-trk-hanger sleep 300".to_string(),
                ]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: Some(crate::config::Watchdog {
                nudges: Some(0),
                retry: Some(0),
                ask: Some(false),
                ..Default::default()
            }),
            name: "trk-timeout".into(),
            harnesses,
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(2),
            max_parallel: Some(1),
            tasks: vec![Task { harness: "hanger".into(), in_field: "sleepy".into(), ..echo_task("a", &[]) }],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        let finished = tokio::time::timeout(Duration::from_secs(20), tr.run_all(&em)).await;
        assert!(finished.is_ok(), "run_all must return after its own timeout");

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Failed);
        assert!(
            s.nodes["a"].stderr.starts_with("TIMED OUT after 2s"),
            "node stderr carries the timeout reason: {:?}",
            s.nodes["a"].stderr
        );

        let manifest: artifacts::Manifest =
            serde_json::from_str(&std::fs::read_to_string(dir.join("run/manifest.json")).unwrap()).unwrap();
        let att = &manifest.tasks["a"].attempts["r0"];
        assert_eq!(
            att.error.as_deref(),
            Some("TIMED OUT after 2s (killed with its process group)"),
            "manifest names the timeout"
        );
        let err = std::fs::read_to_string(dir.join("run/tasks/a/r0/err.txt")).unwrap();
        assert!(err.starts_with("TIMED OUT after 2s"), "err.txt carries the reason: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ask_timeout_secs` bounds the park: no answer in time → the node fails
    /// honestly (and the cascade would skip its dependents).
    #[tokio::test]
    async fn watchdog_ask_timeout_fails_the_node() {
        let dir = std::env::temp_dir().join(format!("zori-wd-tmo-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            name: "tmo".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            watchdog: Some(crate::config::Watchdog {
                ask_timeout_secs: Some(1),
                ..Default::default()
            }),
            tasks: vec![
                Task { in_field: "ASK: so what?".into(), ..echo_task("a", &[]) },
                echo_task("b", &["a"]),
            ],
        };
        let (tr, em) = supervised_fixture(&dir, flow).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Failed, "timeout fails the parked node");
        assert!(s.nodes["a"].stderr.contains("watchdog"), "{}", s.nodes["a"].stderr);
        assert_eq!(s.nodes["b"].status, TaskStatus::Skipped, "cascade skips the dependent");
        assert_eq!(s.status, crate::state::RunStatus::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `prompt` harness in the tracked driver: the starting block runs
    /// scripted (no process), its text becomes the node output, and the
    /// dependent receives it as a `# prompt` section via the dep edge.
    #[tokio::test]
    async fn prompt_block_runs_scripted_and_feeds_dependents() {
        let dir = std::env::temp_dir().join(format!("zori-trk-prompt-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "prompted".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![
                Task { harness: "prompt".into(), in_field: "Shared task text".into(), ..echo_task("prompt", &[]) },
                Task { depends_on: vec!["prompt".into()], ..echo_task("worker", &["prompt"]) },
            ],
        };
        let plan = plan(&flow).unwrap();
        let out_dir = dir.join("run");
        let tr = TrackedRun::new(flow.clone(), plan.clone(), &[], out_dir.clone(), dir.join("sessions"))
            .unwrap();
        let state = RunState::new(
            "prompted-1".into(),
            flow.name.clone(),
            "prompted.yml".into(),
            Some(out_dir),
            plan.levels.clone(),
            &flow.tasks.iter().map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone())).collect::<Vec<_>>(),
        );
        let em = Emitter::new(state);
        tr.run_all(&em).await;
        let s = em.snapshot();
        assert_eq!(s.nodes["prompt"].status, TaskStatus::Done);
        assert_eq!(s.nodes["prompt"].output, "Shared task text");
        assert_eq!(s.nodes["worker"].status, TaskStatus::Done);
        // The worker's echo reply is its prompt: `in` + the fed section.
        assert!(
            s.nodes["worker"].output.contains("# prompt\nShared task text"),
            "dep edge fed the prompt text: {:?}",
            s.nodes["worker"].output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A flow of independent bash "sleepers" (all in level 0): each reads its
    /// tag from stdin, appends `start-<tag>` to a shared log, sleeps a second,
    /// then appends `end-<tag>`. The log's shape is the concurrency probe —
    /// no wall-clock timing to go flaky.
    async fn sleeper_fixture(
        dir: &std::path::Path,
        ids: &[&str],
        max_parallel: Option<usize>,
    ) -> (TrackedRun, Emitter, std::path::PathBuf) {
        use crate::config::HarnessOverride;
        let log = dir.join("log.txt");
        let script = format!(
            "read tag; echo \"start-$tag\" >> \"{log}\"; sleep 1; echo \"end-$tag\" >> \"{log}\"",
            log = log.display()
        );
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "sleeper".to_string(),
            HarnessOverride {
                command: Some(vec!["bash".to_string(), "-c".to_string(), script]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        let tasks: Vec<Task> = ids
            .iter()
            .map(|id| Task {
                skills: vec![],
                id: id.to_string(),
                harness: "sleeper".into(),
                in_field: id.to_string(),
                depends_on: vec![],
                ..echo_task(id, &[])
            })
            .collect();
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "sleepers".into(),
            harnesses,
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel,
            tasks,
        };
        let plan = plan(&flow).unwrap();
        let out_dir = dir.join("run");
        let sessions_dir = dir.join("sessions");
        let tr = TrackedRun::new(flow.clone(), plan.clone(), &[], out_dir.clone(), sessions_dir)
            .unwrap();
        let state = RunState::new(
            "sleepers-1".into(),
            flow.name.clone(),
            "sleepers.yml".into(),
            Some(out_dir),
            plan.levels.clone(),
            &flow
                .tasks
                .iter()
                .map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone()))
                .collect::<Vec<_>>(),
        );
        (tr, Emitter::new(state), log)
    }

    /// Same-level siblings must run concurrently: with no `max_parallel`, all
    /// starts land in the log before any end — impossible if `run_all` still
    /// awaited each node before spawning the next.
    #[tokio::test]
    async fn run_all_runs_level_siblings_concurrently() {
        let dir = std::env::temp_dir().join(format!("zori-trk-par-{}", now_secs()));
        let (tr, em, log) = sleeper_fixture(&dir, &["a", "b"], None).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done);
        assert_eq!(s.nodes["b"].status, TaskStatus::Done);

        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "two starts + two ends, got {lines:?}");
        assert!(lines[0].starts_with("start-"), "{lines:?}");
        assert!(lines[1].starts_with("start-"), "{lines:?}");
        assert!(lines[2].starts_with("end-"), "{lines:?}");
        assert!(lines[3].starts_with("end-"), "{lines:?}");
        let mut starts: Vec<&str> = lines
            .iter()
            .take(2)
            .map(|l| l.trim_start_matches("start-"))
            .collect();
        starts.sort_unstable();
        assert_eq!(starts, vec!["a", "b"], "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `max_parallel: 1` serializes the level: every `end-<tag>` immediately
    /// follows its own `start-<tag>` — the cap is honoured, not advisory.
    #[tokio::test]
    async fn run_all_respects_max_parallel_one() {
        let dir = std::env::temp_dir().join(format!("zori-trk-cap-{}", now_secs()));
        let (tr, em, log) = sleeper_fixture(&dir, &["a", "b"], Some(1)).await;
        tr.run_all(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done);
        assert_eq!(s.nodes["b"].status, TaskStatus::Done);

        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "two starts + two ends, got {lines:?}");
        assert!(lines[0].starts_with("start-") && lines[1].starts_with("end-"), "{lines:?}");
        assert!(lines[2].starts_with("start-") && lines[3].starts_with("end-"), "{lines:?}");
        assert_eq!(
            lines[0].trim_start_matches("start-"),
            lines[1].trim_start_matches("end-"),
            "first end must belong to the first start, got {lines:?}"
        );
        assert_eq!(
            lines[2].trim_start_matches("start-"),
            lines[3].trim_start_matches("end-"),
            "second end must belong to the second start, got {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn steer_updates_output_and_marks_downstream_stale() {
        let dir = std::env::temp_dir().join(format!("zori-trk-steer-{}", now_secs()));
        let (tr, em) = tracked_fixture(&dir).await;
        tr.run_all(&em).await;

        tr.steer(&em, "a", "actually use the word ZEBRA", None).await.unwrap();

        let s = em.snapshot();
        // a gained [human, agent] turns and its output is the steer reply.
        assert_eq!(s.nodes["a"].turns.len(), 3);
        assert_eq!(s.nodes["a"].turns[1].role, Role::Human);
        assert_eq!(s.nodes["a"].turns[2].text, "actually use the word ZEBRA");
        assert_eq!(s.nodes["a"].output, "actually use the word ZEBRA");
        assert_eq!(s.nodes["a"].status, TaskStatus::Done);
        // b depended on a's output → now Stale, awaiting re-run.
        assert_eq!(s.nodes["b"].status, TaskStatus::Stale);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rerun_stale_refreshes_downstream_from_new_output() {
        let dir = std::env::temp_dir().join(format!("zori-trk-rerun-{}", now_secs()));
        let (tr, em) = tracked_fixture(&dir).await;
        tr.run_all(&em).await;
        tr.steer(&em, "a", "REVISED", None).await.unwrap();
        assert_eq!(em.snapshot().nodes["b"].status, TaskStatus::Stale);

        tr.rerun_stale(&em).await;

        let s = em.snapshot();
        assert_eq!(s.nodes["b"].status, TaskStatus::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rerun_picks_up_operator_override() {
        // The console's fix-and-rerun loop: a task registered via
        // override_task must be what the next rerun executes — not the
        // boot-time spec. This is the "typo'd model, node failed, fixed it in
        // Config" path; modelled here on the prompt, which echo echoes back.
        let dir = std::env::temp_dir().join(format!("zori-trk-ovr-{}", now_secs()));
        let (tr, em) = tracked_fixture(&dir).await;
        tr.run_all(&em).await;
        assert_eq!(em.snapshot().nodes["a"].output, "say a");

        // Mark the node Failed, as a dead attempt would be.
        em.mutate(|st| {
            let n = st.nodes.get_mut("a").unwrap();
            n.status = TaskStatus::Failed;
            n.output.clear();
        });

        // The operator's fix: same id/deps, corrected prompt.
        let mut fixed = tr.task("a").unwrap();
        fixed.in_field = "CORRECTED PROMPT".into();
        tr.override_task(fixed);
        tr.rerun(&em, "a").await.unwrap();

        let s = em.snapshot();
        assert_eq!(s.nodes["a"].status, TaskStatus::Done);
        assert_eq!(s.nodes["a"].output, "CORRECTED PROMPT");
        // The effective flow (what gets persisted) carries the fix too.
        let eff = tr.effective_flow();
        let a = eff.tasks.iter().find(|t| t.id == "a").unwrap();
        assert_eq!(a.in_field, "CORRECTED PROMPT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rerun_driver_error_marks_node_failed_not_running() {
        // Regression: a rerun whose exec driver-errors (bad command) used to
        // leave the node zombie-Running — the claim had flipped it and the Err
        // path never recorded anything, so the node could never be re-run.
        let dir = std::env::temp_dir().join(format!("zori-trk-rerr-{}", now_secs()));
        let flow: Flow = serde_yaml::from_str(
            "name: zomb\ndefault_timeout_secs: 10\nharnesses:\n  dead:\n    command: [/nonexistent/broken-cmd]\ntasks:\n  - id: a\n    harness: dead\n    in: hi\n",
        )
        .unwrap();
        let plan = plan(&flow).unwrap();
        let tr = TrackedRun::new(flow, plan.clone(), &[], dir.join("run"), dir.join("sessions")).unwrap();
        let state = RunState::new(
            "zomb-1".into(),
            "zomb".into(),
            "z.yml".into(),
            Some(dir.join("run")),
            plan.levels.clone(),
            &[("a".into(), "dead".into(), None, false, vec![])],
        );
        let em = Emitter::new(state);
        tr.run_all(&em).await;
        assert_eq!(em.snapshot().nodes["a"].status, TaskStatus::Failed);

        // Now the rerun: must fail again (command still broken) but the node
        // must land Failed — not Running.
        let _ = tr.rerun(&em, "a").await;
        let n = &em.snapshot().nodes["a"];
        assert_eq!(n.status, TaskStatus::Failed, "zombie Running is the bug");
        assert!(n.stderr.contains("driver error"), "stderr: {}", n.stderr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rerun_refuses_unfinished_deps_before_wiping_the_node() {
        // Review M4: `rerun` is reachable without the console's claim gate
        // (rerun_stale, future callers), so its own dependency check must run
        // BEFORE reset_node — the old order destroyed the session and the
        // captured output first and only then died on the dep error.
        let dir = std::env::temp_dir().join(format!("zori-trk-rdep-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "rdep".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![echo_task("a", &[]), echo_task("b", &["a"])],
        };
        let plan = plan(&flow).unwrap();
        let tr = TrackedRun::new(
            flow.clone(),
            plan.clone(),
            &[],
            dir.join("run"),
            dir.join("sessions"),
        )
        .unwrap();
        let state = RunState::new(
            "rdep-1".into(),
            "rdep".into(),
            "r.yml".into(),
            Some(dir.join("run")),
            plan.levels.clone(),
            &[
                ("a".into(), "echo".into(), None, false, vec![]),
                ("b".into(), "echo".into(), None, false, vec!["a".into()]),
            ],
        );
        let em = Emitter::new(state);
        // a died, b was skipped under it — the exact shape the bug accepted.
        em.mutate(|s| {
            s.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
            let b = s.nodes.get_mut("b").unwrap();
            b.status = TaskStatus::Skipped;
            b.output = "precious earlier transcript".into();
        });
        let e = tr.rerun(&em, "b").await.unwrap_err();
        assert!(e.to_string().contains("unfinished dependencies: a"), "{e:#}");
        let b = &em.snapshot().nodes["b"];
        assert_eq!(b.status, TaskStatus::Skipped, "refused rerun must not touch the node");
        assert_eq!(b.output, "precious earlier transcript", "output must survive the refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rerun_stale_holds_back_nodes_with_unfinished_deps() {
        // Review M4: the stale sweep must skip a Stale node whose producer is
        // not Done *now* (e.g. the producer's own rerun in an earlier level
        // failed again) — and say so on the node, instead of silently running
        // it with a broken prompt or silently doing nothing.
        let dir = std::env::temp_dir().join(format!("zori-trk-sdep-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "sdep".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![echo_task("a", &[]), echo_task("b", &["a"])],
        };
        let plan = plan(&flow).unwrap();
        let tr = TrackedRun::new(
            flow.clone(),
            plan.clone(),
            &[],
            dir.join("run"),
            dir.join("sessions"),
        )
        .unwrap();
        let state = RunState::new(
            "sdep-1".into(),
            "sdep".into(),
            "s.yml".into(),
            Some(dir.join("run")),
            plan.levels.clone(),
            &[
                ("a".into(), "echo".into(), None, false, vec![]),
                ("b".into(), "echo".into(), None, false, vec!["a".into()]),
            ],
        );
        let em = Emitter::new(state);
        em.mutate(|s| {
            s.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
            s.nodes.get_mut("b").unwrap().status = TaskStatus::Stale;
        });
        tr.rerun_stale(&em).await;
        let s = em.snapshot();
        assert_eq!(s.nodes["b"].status, TaskStatus::Stale, "held back, not run");
        assert!(
            s.nodes["b"].stderr.contains("rerun-stale held back"),
            "the skip must be explained: {}",
            s.nodes["b"].stderr
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn optional_failed_dep_does_not_block_join() {
        // a(optional) fails via a bad harness? echo never fails, so model it by
        // pre-marking a as Failed and checking b still runs.
        let dir = std::env::temp_dir().join(format!("zori-trk-opt-{}", now_secs()));
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "opt".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![
                {
                    let mut t = echo_task("a", &[]);
                    t.optional = true;
                    t
                },
                echo_task("b", &["a"]),
            ],
        };
        let plan = plan(&flow).unwrap();
        let tr = TrackedRun::new(
            flow.clone(),
            plan.clone(),
            &[],
            dir.join("run"),
            dir.join("sessions"),
        )
        .unwrap();
        let state = RunState::new(
            "opt-1".into(), "opt".into(), "o.yml".into(), Some(dir.join("run")),
            plan.levels.clone(),
            &flow.tasks.iter().map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone())).collect::<Vec<_>>(),
        );
        let em = Emitter::new(state);
        // Pretend the optional branch died before b's level.
        em.set_status("a", TaskStatus::Failed);
        // b must not be blocked (optional dep), and its prompt resolves the gap.
        assert!(!tr.blocked(&em, &flow.tasks[1]));
        // Review (minor): the untested half was what the JOIN actually SEES of
        // the dead branch. `blocked` only says "not blocking"; the real risk
        // was the join rendering either the branch's stale output or a hard
        // "unknown reference" error. `outputs_snapshot` is the production
        // function that substitutes an honest gap — pin it: a Failed optional
        // maps to the `missing_output` placeholder, not its output and not a
        // missing key.
        let outs = tr.outputs_snapshot(&em);
        assert_eq!(
            outs.get("a"),
            Some(&missing_output("a")),
            "the join receives an honest gap for the failed optional branch"
        );
        assert!(
            outs.get("a").is_some_and(|t| t.contains("did not complete")),
            "the placeholder is explicit that the branch produced nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- proposal gate: the propose→validate→(fix) loop for chat nodes -----
    //
    // `proposal_gate` runs on a chat node's FINAL turn: a ```zori-flow block
    // becomes a server-checked FlowProposal. A valid graph is parked for the
    // operator's Run click; an invalid one earns a bounded repair turn. These
    // drive the gate directly with a controlled node so the verdict is asserted
    // without depending on an echo round-trip (the echo wiring is exercised by
    // the existing supervision tests).

    /// A chat-shaped single-node TrackedRun: node "chat" is resumable (Echo) and
    /// `is_chat` is on, so `proposal_gate` engages.
    async fn chat_gate_fixture(dir: &std::path::Path) -> (TrackedRun, Emitter, Task) {
        let task = Task { id: "chat".into(), ..echo_task("chat", &[]) };
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "chat".into(),
            harnesses: BTreeMap::new(),
            inputs: BTreeMap::new(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: Some(1),
            tasks: vec![task.clone()],
        };
        let plan = plan(&flow).unwrap();
        let tr = TrackedRun::new(
            flow.clone(),
            plan.clone(),
            &[],
            dir.join("run"),
            dir.join("sessions"),
        )
        .unwrap();
        let state = RunState::new(
            "chat-1".into(),
            flow.name.clone(),
            "chat.yml".into(),
            Some(dir.join("run")),
            plan.levels.clone(),
            &[("chat".into(), "echo".into(), None, false, vec![])],
        );
        let em = Emitter::new(state);
        em.mutate(|st| st.is_chat = true);
        (tr, em, task)
    }

    /// Commit a fake agent reply (and a resumable session) onto the chat node.
    fn set_chat_reply(em: &Emitter, output: &str) {
        em.mutate(|st| {
            let n = st.nodes.get_mut("chat").expect("chat node");
            n.output = output.to_string();
            n.session = Some(SessionRef::Echo);
        });
    }

    const VALID_GRAPH: &str =
        "here is the agreed graph:\n```zori-flow\nname: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n```";
    const VALID_GRAPH_V2: &str =
        "```zori-flow\nname: ok\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: yo\n    depends_on: [a]\n```";
    const BAD_GRAPH: &str =
        "```zori-flow\nname: bad\ntasks:\n  - id: a\n    harness: nosuch-harness\n    in: hi\n```";

    #[tokio::test]
    async fn gate_first_proposal_base_is_the_board_shipped_with_the_turn() {
        let dir = std::env::temp_dir().join(format!("zori-gate-base1-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        // The operator shipped a board with the turn (a steer carries `graph`);
        // the runner parks it in `canvas` for exactly this moment.
        let shipped = serde_json::json!({
            "name": "shipped", "tasks": [
                { "id": "pre", "harness": "echo", "in": "op-edited" }
            ]
        });
        *tr.canvas.lock().unwrap() = Some(shipped.clone());
        set_chat_reply(&em, VALID_GRAPH);
        tr.proposal_gate(&em, &task).await;
        let p = em.snapshot().nodes["chat"].proposal.clone().expect("proposal stored");
        assert_eq!(
            p.base.as_ref(),
            Some(&shipped),
            "a first proposal merges against the board shipped with the turn"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gate_later_proposal_base_is_the_prior_accepted_graph() {
        let dir = std::env::temp_dir().join(format!("zori-gate-base2-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        // Turn 1: a valid proposal is accepted (its spec becomes the agent's view).
        set_chat_reply(&em, VALID_GRAPH);
        tr.proposal_gate(&em, &task).await;
        let first = em.snapshot().nodes["chat"].proposal.clone().expect("first proposal");
        assert!(first.base.is_none(), "no canvas shipped with this turn");

        // Turn 2 ships a NEWER board (the operator edited before sending), but
        // the merge base must stay the agent's last accepted graph: an edit
        // made before sending is operator-owned and a reply echoing the
        // pre-edit value must not be able to revert it.
        let shipped = serde_json::json!({
            "name": "shipped", "tasks": [
                { "id": "a", "harness": "echo", "in": "OPERATOR-EDIT" }
            ]
        });
        *tr.canvas.lock().unwrap() = Some(shipped);
        set_chat_reply(&em, VALID_GRAPH_V2);
        tr.proposal_gate(&em, &task).await;
        let second = em.snapshot().nodes["chat"].proposal.clone().expect("second proposal");
        assert_eq!(
            second.base.as_ref(),
            first.spec.as_ref(),
            "the second proposal merges against the first proposal's spec, not the shipped board"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gate_stores_valid_proposal_and_parks_it() {
        let dir = std::env::temp_dir().join(format!("zori-gate-ok-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        set_chat_reply(&em, VALID_GRAPH);
        let fix = tr.proposal_gate(&em, &task).await;
        assert!(fix.is_none(), "a valid graph parks, it earns no repair turn");
        let s = em.snapshot();
        let p = s.nodes["chat"].proposal.clone().expect("proposal stored");
        assert!(p.valid);
        assert_eq!(p.status, ProposalStatus::Proposed);
        assert!(p.errors.is_empty());
        assert!(p.levels.iter().flatten().any(|id| id == "a"), "levels sketched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gate_asks_to_repair_an_invalid_proposal_once() {
        let dir = std::env::temp_dir().join(format!("zori-gate-fix-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        set_chat_reply(&em, BAD_GRAPH);
        let fix = tr.proposal_gate(&em, &task).await;
        assert!(fix.is_some(), "an invalid graph earns a repair turn");
        let Some(ExecMode::Resume(sess, text)) = fix else {
            panic!("the repair turn must resume the same session");
        };
        assert!(matches!(sess, SessionRef::Echo), "same session continues");
        assert!(text.contains(crate::proposal::FENCE_TAG), "repair text names the fence");
        let s = em.snapshot();
        let p = s.nodes["chat"].proposal.clone().unwrap();
        assert!(!p.valid);
        assert_eq!(p.fix_rounds, 1, "the first repair is counted");
        assert!(!p.errors.is_empty(), "the failures are reported");
        assert!(
            s.nodes["chat"]
                .turns
                .iter()
                .any(|t| t.role == Role::System && t.text.contains("failed validation")),
            "a system turn announces the repair"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gate_stops_repairing_once_the_budget_is_spent() {
        let dir = std::env::temp_dir().join(format!("zori-gate-cap-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        set_chat_reply(&em, BAD_GRAPH);
        // Pretend the bounded budget is already spent.
        em.mutate(|st| {
            st.nodes.get_mut("chat").unwrap().proposal = Some(FlowProposal {
                raw: String::new(),
                hash: String::new(),
                valid: false,
                errors: vec!["x".into()],
                warnings: vec![],
                levels: vec![],
                spec: None,
                base: None,
                fix_rounds: crate::proposal::MAX_FIX_ROUNDS,
                status: ProposalStatus::Proposed,
                run_id: None,
                ts: 0,
            });
        });
        let fix = tr.proposal_gate(&em, &task).await;
        assert!(fix.is_none(), "budget exhausted → no further repair turn");
        let s = em.snapshot();
        assert_eq!(
            s.nodes["chat"].proposal.as_ref().unwrap().fix_rounds,
            crate::proposal::MAX_FIX_ROUNDS,
            "the count does not run past the budget"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gate_ignores_a_reply_without_a_block() {
        let dir = std::env::temp_dir().join(format!("zori-gate-none-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        set_chat_reply(&em, "let's discuss the plan first: step 1, then step 2");
        let fix = tr.proposal_gate(&em, &task).await;
        assert!(fix.is_none(), "a plain conversational turn is not a proposal");
        let s = em.snapshot();
        assert!(s.nodes["chat"].proposal.is_none(), "no block leaves no proposal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gate_is_a_noop_for_non_chat_runs() {
        let dir = std::env::temp_dir().join(format!("zori-gate-nonchat-{}", now_secs()));
        let (tr, em, task) = chat_gate_fixture(&dir).await;
        em.mutate(|st| st.is_chat = false);
        set_chat_reply(&em, VALID_GRAPH);
        let fix = tr.proposal_gate(&em, &task).await;
        assert!(fix.is_none());
        let s = em.snapshot();
        assert!(s.nodes["chat"].proposal.is_none(), "an ordinary flow is never gated");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::{Flow, HarnessOverride, PromptVia, Task};
    use std::collections::BTreeMap;

    /// An isolated runs root per test call: runner tests must never litter
    /// `zori-runs/` into the repo cwd (they visibly did before artifacts
    /// gained an injectable root).
    fn test_runs_root() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "zori-runner-test-{}-{}",
            std::process::id(),
            artifacts::new_run_id("t")
        ));
        std::fs::create_dir_all(&d).expect("test runs root");
        d
    }

    /// A minimal Task shell for unit tests; fields get filled per test.
    fn plain_task(id: &str) -> Task {
        Task {
           skills: vec![],
            watchdog: None,
            id: id.to_string(),
            harness: "echo".into(),
            model: None,
            in_field: String::new(),
            out: None,
            depends_on: vec![],
            cwd: None,
            env: BTreeMap::new(),
            args: vec![],
            timeout_secs: None,
            optional: false,
            pos: None,
            workspace: None,
            chat: None,
        }
    }

    /// A flow of one `sh` task running `script`, prompt fed via stdin.
    fn sh_flow(name: &str, task_id: &str, script: &str) -> Flow {
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "shh".to_string(),
            HarnessOverride {
                command: Some(vec!["sh".to_string(), "-c".to_string(), script.to_string()]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        Flow {
           skills: vec![],
           skills_dirs: vec![],
            watchdog: None,
            name: name.to_string(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![Task {
                skills: vec![],
                watchdog: None,
                id: task_id.to_string(),
                harness: "shh".to_string(),
                model: None,
                in_field: "ignored-by-sh".to_string(),
                chat: None,
                out: None,
                depends_on: vec![],
                optional: false,
                pos: None,
                workspace: None,
                cwd: None,
                timeout_secs: None,
                args: vec![],
                env: BTreeMap::new(),
            }],
        }
    }

    /// The per-task artifact layout: every attempt lands under
    /// `tasks/<id>/<nonce>/` with prompt/out/err/meta, and the manifest
    /// indexes it — the substrate fork-resume and the console build on.
    #[tokio::test]
    async fn artifacts_land_in_per_task_attempt_dirs() {
        let root = test_runs_root();
        let flow = sh_flow("layout-test", "only", "echo hi");
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &root).await.expect("run ok");
        assert_eq!(report.succeeded(), 1);

        let run_dir = report.out_dir.expect("run dir recorded");
        let attempt = run_dir.join("tasks").join("only").join("r0");
        for f in ["prompt.txt", "out.txt", "err.txt", "meta.json"] {
            assert!(attempt.join(f).is_file(), "missing {}", attempt.join(f).display());
        }
        assert!(run_dir.join("tasks").join("only").join("work").is_dir(), "work dir created");
        let out = std::fs::read_to_string(attempt.join("out.txt")).unwrap();
        assert!(out.contains("hi"), "captured stdout persisted: {out:?}");
        let prompt = std::fs::read_to_string(attempt.join("prompt.txt")).unwrap();
        assert!(prompt.contains("ignored-by-sh"), "rendered prompt kept: {prompt:?}");

        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let task = &manifest.tasks["only"];
        assert_eq!(task.harness, "shh");
        assert_eq!(task.attempts["r0"].status, "done");
        let recorded = task.attempts["r0"].out.as_ref().unwrap();
        assert_eq!(recorded.bytes, out.len());
        assert_eq!(
            recorded.sha256,
            artifacts::sha256_hex(out.as_bytes()),
            "manifest checksum matches the file"
        );
    }

    /// `workspace: task` (the default): the child actually runs inside its
    /// isolated work dir — the fix for agents writing into the repo root.
    #[tokio::test]
    async fn isolated_workdir_is_the_child_cwd() {
        let root = test_runs_root();
        let flow = sh_flow("workdir-test", "where", "pwd");
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &root).await.expect("run ok");
        let out = &report.results["where"].output;
        let expected = report.out_dir.unwrap().join("tasks/where/work").canonicalize().unwrap();
        assert_eq!(std::path::Path::new(out.trim()).canonicalize().unwrap(), expected);
    }

    /// The harness cwd flag must be ABSOLUTE: the child already runs inside
    /// that directory, and CLIs resolve their cwd flag relative to their own
    /// process cwd — a relative `-C`/`--cwd` doubles the path and codex dies
    /// instantly with ENOENT (the live zori-review failure of 2026-11-17).
    #[test]
    fn effective_cwd_is_absolute_so_the_flag_does_not_double() {
        let t = plain_task("t");
        // workspace: task (the default) via a relative work dir, as the
        // tracked driver builds it when the run root is relative.
        let (proc_cwd, flag) = effective_cwd(&None, &t, Some(Path::new("zori-runs/r/tasks/t/work")))
            .expect("cwd");
        assert!(proc_cwd.is_absolute(), "process cwd must be absolute: {}", proc_cwd.display());
        let flag = flag.expect("workspace: task passes the cwd flag");
        assert!(flag.is_absolute(), "flag cwd must be absolute: {}", flag.display());
        // An explicit relative task.cwd gets the same treatment.
        let t2 = Task { cwd: Some("rel/dir".into()), ..t };
        let (proc_cwd, flag) = effective_cwd(&None, &t2, None).expect("cwd");
        assert!(proc_cwd.is_absolute() && flag.expect("flag").is_absolute());
    }

    /// The postmortem bug: `cwd: ~/x` in the flow, consumed through
    /// `workspace: flow`, used to spawn agents in the literal
    /// `<zori>/~/x` — the tilde branch never went through `absolute()`.
    #[test]
    fn flow_cwd_tilde_expands_on_the_workspace_flow_branch() {
        let home = std::env::var("HOME").expect("HOME in the test env");
        let t = Task { workspace: Some(Workspace::Flow), ..plain_task("t") };
        let (proc_cwd, _flag) =
            effective_cwd(&Some(PathBuf::from("~/repo/x")), &t, None).expect("cwd");
        assert!(proc_cwd.is_absolute(), "absolute: {}", proc_cwd.display());
        assert!(!proc_cwd.starts_with("~"), "no literal tilde: {}", proc_cwd.display());
        assert!(proc_cwd.starts_with(&home), "anchored at home: {}", proc_cwd.display());
        // A bare `~` is the home itself, same value `absolute()` produces.
        let (bare, _) = effective_cwd(&Some(PathBuf::from("~")), &t, None).expect("cwd");
        assert_eq!(bare, artifacts::absolute(Path::new("~")));
    }

    /// The tracked executor hands `sessions_dir` to the agent CLI (coddy
    /// `-sessions-dir`), which resolves it against ITS OWN process cwd — and
    /// the runs root is often relative (`zori-runs`). A relative stored path
    /// grew a nested sessions tree under a task's work dir, and a
    /// `workspace: flow` chat in a project folder would drop the sessions
    /// INTO the operator's project. `TrackedRun::new` must store absolute.
    #[test]
    fn tracked_sessions_dir_is_absolute_even_from_a_relative_root() {
        let root = test_runs_root();
        // A genuinely relative path to the same directory — no chdir (tests
        // run in parallel; the process cwd is global).
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in cwd.ancestors().skip(1) {
            rel.push("..");
        }
        rel.extend(root.strip_prefix(std::path::Path::new("/")).unwrap().components());
        assert!(!rel.is_absolute(), "fixture must be relative: {}", rel.display());
        let flow = sh_flow("sessions-abs", "only", "echo hi");
        let p = plan(&flow).expect("plan");
        let tr = TrackedRun::new(flow, p, &[], root.join("run"), rel).expect("tracked run");
        assert!(tr.sessions_dir.is_absolute(), "stored absolute: {}", tr.sessions_dir.display());
        assert_eq!(tr.sessions_dir, root.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Dependency edges carry data: a dep the `in` text does not reference
    /// lands in the prompt as a `# <id>` section; a referenced one is not
    /// duplicated; an empty `in` becomes just the fed sections.
    #[test]
    fn dep_edges_feed_outputs_into_the_prompt() {
        let mut outputs = HashMap::new();
        outputs.insert("a".to_string(), "A-OUT".to_string());
        outputs.insert("b".to_string(), "B-OUT".to_string());
        let ctx = PromptCtx::default();

        let t = Task {
            skills: vec![],
            in_field: "Do a review".into(),
            depends_on: vec!["a".into(), "b".into()],
            ..plain_task("join")
        };
        let p = render_prompt(&t, &HashMap::new(), &outputs, &ctx, None, None).unwrap();
        assert!(p.contains("Do a review\n\n# a\nA-OUT\n\n# b\nB-OUT"), "sections appended in dep order: {p:?}");

        // Referenced deps keep their hand-placed position, no duplicate.
        let t = Task {
            skills: vec![],
            in_field: "x ${steps.a.output}".into(),
            depends_on: vec!["a".into(), "b".into()],
            ..plain_task("join")
        };
        let p = render_prompt(&t, &HashMap::new(), &outputs, &ctx, None, None).unwrap();
        assert_eq!(p.matches("A-OUT").count(), 1, "referenced dep not re-fed: {p:?}");
        assert!(p.contains("# b\nB-OUT"), "unreferenced dep still fed: {p:?}");

        // Empty `in`: the fed section IS the prompt (the prompt-block case).
        let t = Task { depends_on: vec!["a".into()], ..plain_task("join") };
        let p = render_prompt(&t, &HashMap::new(), &outputs, &ctx, None, None).unwrap();
        assert!(p.starts_with("# a\nA-OUT"), "no leading blanks: {p:?}");
    }

    /// The watchdog's protocol section rides at the very END of the prompt,
    /// after `# Output` — the deliverable spec stays the closing instruction
    /// for the agent's work, the protocol is meta.
    #[test]
    fn preamble_section_rides_after_output() {
        let t = Task {
            skills: vec![],
            in_field: "do a review".into(),
            out: Some("a short report".into()),
            ..plain_task("join")
        };
        let pre = "PROTOCOL RULESET";
        let p = render_prompt(&t, &HashMap::new(), &HashMap::new(), &PromptCtx::default(), Some(pre), None).unwrap();
        let out_at = p.find("# Output").expect("output section present");
        let pre_at = p.find(pre).expect("preamble present");
        assert!(pre_at > out_at, "protocol after the output spec: {p:?}");
        assert!(p.ends_with("PROTOCOL RULESET\n"), "trimmed and terminated: {p:?}");

        // None → no section at all (byte-exact prompts stay byte-exact).
        let bare = render_prompt(&t, &HashMap::new(), &HashMap::new(), &PromptCtx::default(), None, None).unwrap();
        assert!(!bare.contains("PROTOCOL RULESET"));
    }

    /// A skill block rides AFTER the dependency sections (it is context about
    /// the work, as much as a dep's output is) but BEFORE `# Output` and the
    /// preamble, so the deliverable spec and the protocol stay the closing
    /// instructions the agent reads last.
    #[test]
    fn skill_section_rides_between_deps_and_output() {
        let mut outputs = HashMap::new();
        outputs.insert("a".to_string(), "A-OUT".to_string());
        let t = Task {
            skills: vec![],
            in_field: "do it".into(),
            depends_on: vec!["a".into()],
            out: Some("report".into()),
            ..plain_task("join")
        };
        let sk = "# Skill: review\nLook at the boundaries";
        let pre = "PROTOCOL";
        let p = render_prompt(&t, &HashMap::new(), &outputs, &PromptCtx::default(), Some(pre), Some(sk)).unwrap();
        let dep_at = p.find("# a\nA-OUT").expect("dep section");
        let skill_at = p.find("# Skill: review").expect("skill section");
        let out_at = p.find("# Output").expect("output section");
        let pre_at = p.find(pre).expect("preamble");
        assert!(dep_at < skill_at, "skills after the dep results: {p:?}");
        assert!(skill_at < out_at, "skills before the deliverable spec: {p:?}");
        assert!(out_at < pre_at, "spec before the protocol: {p:?}");

        // No skill → the byte-exact prompt is untouched (echo test outputs safe).
        let bare = render_prompt(&t, &HashMap::new(), &outputs, &PromptCtx::default(), None, None).unwrap();
        assert!(!bare.contains("# Skill"), "no block when none passed: {bare:?}");
    }

    /// The `prompt` harness in the classic driver: no process runs, the node's
    /// output is its rendered text, and a dependent receives it as a
    /// `# prompt` section — the starting-block pattern end to end.
    #[tokio::test]
    async fn prompt_block_feeds_dependents_classic() {
        let root = test_runs_root();
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "catting".to_string(),
            HarnessOverride {
                command: Some(vec!["sh".to_string(), "-c".to_string(), "cat".to_string()]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "prompt-block".into(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![
                Task { harness: "prompt".into(), in_field: "The project needs a review".into(), ..plain_task("prompt") },
                Task { harness: "catting".into(), in_field: "You are a reviewer.".into(), depends_on: vec!["prompt".into()], ..plain_task("worker") },
            ],
        };
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &root).await.expect("run ok");
        assert_eq!(report.succeeded(), 2, "both nodes done");
        assert_eq!(report.results["prompt"].output, "The project needs a review");
        let fed = &report.results["worker"].output;
        assert!(fed.contains("You are a reviewer.\n\n# prompt\nThe project needs a review"), "worker saw the fed section: {fed:?}");
        // The prompt node recorded an attempt like any harness.
        let run_dir = report.out_dir.unwrap();
        let out = std::fs::read_to_string(run_dir.join("tasks/prompt/r0/out.txt")).unwrap();
        assert_eq!(out, "The project needs a review");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A join can read what its branches wrote on disk via `${steps.<id>.dir}`
    /// — the reviewer-writes-a-file, joiner-reads-it pattern from the first
    /// real issue-planning run, now without any repo pollution.
    #[tokio::test]
    async fn join_reads_upstream_work_files_via_steps_dir() {
        let root = test_runs_root();
        let mut harnesses = BTreeMap::new();
        for (id, script) in [
            ("writer", "echo secret-note > note.txt; echo wrote"),
            ("reader", "cat \"$1\"", ),
        ] {
            harnesses.insert(
                id.to_string(),
                HarnessOverride {
                    command: Some(vec!["sh".to_string(), "-c".to_string(), script.to_string()]),
                    prompt_via: Some(PromptVia::Arg),
                    ..Default::default()
                },
            );
        }
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "steps-dir-test".to_string(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![
                Task {
                   skills: vec![],
                    watchdog: None,
                    id: "writer".to_string(),
                    harness: "writer".to_string(),
                    model: None,
                    in_field: "write the note".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec![],
                    optional: false,
                    pos: None,
                    workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
                Task {
                   skills: vec![],
                    watchdog: None,
                    id: "reader".to_string(),
                    harness: "reader".to_string(),
                    model: None,
                    in_field: "${steps.writer.dir}/work/note.txt".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec!["writer".to_string()],
                    optional: false,
                    pos: None,
                    workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
            ],
        };
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &root).await.expect("run ok");
        assert_eq!(report.succeeded(), 2, "both tasks done");
        let out = &report.results["reader"].output;
        assert!(
            out.contains("secret-note"),
            "reader got the upstream work file via ${{steps.id.dir}}, got {out:?}"
        );
    }

    /// `workspace: flow` keeps the pre-isolation behaviour: the child runs in
    /// the flow's cwd, for tasks that must operate in the project directly.
    #[tokio::test]
    async fn workspace_flow_keeps_legacy_cwd() {
        let root = test_runs_root();
        let project = test_runs_root(); // stand-in "project" dir
        let mut flow = sh_flow("legacy-cwd-test", "where", "pwd");
        flow.cwd = Some(project.clone());
        flow.tasks[0].workspace = Some(crate::config::Workspace::Flow);
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &root).await.expect("run ok");
        let out = &report.results["where"].output;
        assert!(
            out.contains(&project.file_name().unwrap().to_string_lossy().to_string()),
            "pwd should be the flow cwd {project:?}, got {out:?}"
        );
    }

    /// Run ids use milliseconds and never collide for back-to-back runs (the
    /// whole-second stamp produced duplicate run dirs).
    #[tokio::test]
    async fn run_ids_do_not_collide_within_a_second() {
        let root = test_runs_root();
        let flow = sh_flow("id-test", "only", "echo x");
        let p = plan(&flow).expect("plan");
        let a = run(&flow, &p, &[], &root).await.expect("run 1");
        let b = run(&flow, &p, &[], &root).await.expect("run 2");
        assert_ne!(
            a.out_dir, b.out_dir,
            "two runs in the same second must get distinct dirs"
        );
    }

    /// One flow whose single task hangs forever in a custom `bash` harness.
    /// The distinctive `exec -a` name is the marker a test greps for to prove
    /// the child was actually killed (an exec-replaced `sleep` keeps argv0).
    fn hanging_flow(timeout_secs: u64) -> Flow {
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "hanger".to_string(),
            HarnessOverride {
                command: Some(vec![
                    "bash".to_string(),
                    "-c".to_string(),
                    "exec -a zori-probe-hanger sleep 300".to_string(),
                ]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        Flow {
           skills: vec![],
           skills_dirs: vec![],
            watchdog: None,
            name: "timeout-test".to_string(),
            cwd: None,
            default_timeout_secs: Some(timeout_secs),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![Task {
                skills: vec![],
                watchdog: None,
                id: "slow".to_string(),
                harness: "hanger".to_string(),
                model: None,
                in_field: "prompt that a sleeping process ignores".to_string(),
                chat: None,
                out: None,
                depends_on: vec![],
                optional: false,
                pos: None, workspace: None,
                cwd: None,
                timeout_secs: Some(timeout_secs),
                args: vec![],
                env: BTreeMap::new(),
            }],
        }
    }

    /// Regression for the first real run hanging forever: the timeout branch
    /// used to await the stdout/stderr readers *before* killing the child, so
    /// on a hung child the readers never returned and the timeout never fired.
    /// The whole `run()` is wrapped in an outer timeout: if the bug is back the
    /// inner deadline never trips, `run()` blocks, and the outer guard fails
    /// the test instead of hanging the suite.
    #[tokio::test]
    async fn timeout_kills_a_hung_task_instead_of_hanging() {
        let flow = hanging_flow(2);
        let p = plan(&flow).expect("plan");

        let run = tokio::time::timeout(
            Duration::from_secs(20),
            run(&flow, &p, &[], &test_runs_root()),
        )
        .await;

        let report = run
            .expect("run() must return within its own budget; hung = the timeout-deadlock bug is back")
            .expect("run() itself returned Err");
        let r = report.results.get("slow").expect("slow task result present");
        assert_eq!(r.status, Status::Failed, "a timed-out task is Failed");
        assert_eq!(r.exit_code, None, "a killed task has no exit code");
        assert!(
            r.stderr.starts_with("TIMED OUT"),
            "stderr should record the timeout, got: {:?}",
            r.stderr
        );

        // The kill must have actually reached the child, not abandoned it.
        // SIGKILL is asynchronous, so poll for the marked process to disappear
        // instead of a single fixed sleep — under a loaded suite (or a box
        // running agents) a fixed 500ms window produced false orphans.
        // `pgrep -f` exit 0 would mean it is still alive.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while std::time::Instant::now() < deadline {
            match std::process::Command::new("pgrep")
                .args(["-f", "zori-probe-hanger"])
                .status()
            {
                // pgrep missing (not on this box): the run() returning on time
                // already proves the deadlock fix, so don't fail on the probe.
                Ok(st) => gone = !st.success(),
                Err(_) => {
                    gone = true;
                    break;
                }
            }
            if gone {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        assert!(
            gone,
            "timed-out child survived the kill (orphan): pgrep still finds zori-probe-hanger after 5s"
        );
    }

    /// Postmortem bug 1 (classic driver): the live zori-review run showed
    /// `failed` nodes with `error: null` in the manifest — nothing recorded
    /// WHY an attempt failed. A non-zero exit now records the code, a timeout
    /// records the reason, in the manifest and in `err.txt`.
    #[tokio::test]
    async fn failed_and_timed_out_attempts_record_the_reason_in_the_manifest() {
        // Non-zero exit.
        let flow = sh_flow("manifest-fail", "boom", "echo noisy >&2; exit 3");
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &test_runs_root()).await.expect("run ok");
        let r = &report.results["boom"];
        assert_eq!(r.status, Status::Failed);
        assert_eq!(r.exit_code, Some(3));
        let run_dir = report.out_dir.clone().expect("classic run has an out dir");
        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let att = &manifest.tasks["boom"].attempts["r0"];
        assert_eq!(att.error.as_deref(), Some("exited with code 3"), "manifest names the exit code");
        let err = std::fs::read_to_string(run_dir.join("tasks/boom/r0/err.txt")).unwrap();
        assert!(err.contains("noisy"), "stderr text still lands in err.txt: {err:?}");
        let _ = std::fs::remove_dir_all(&run_dir);

        // Timeout. A distinct process marker: the suite runs in parallel and
        // the kill-probe test above pgreps for its own marker only.
        let mut flow = hanging_flow(2);
        flow.name = "manifest-timeout".into();
        flow.harnesses.get_mut("hanger").expect("hanger").command = Some(vec![
            "bash".into(),
            "-c".into(),
            "exec -a zori-probe-manifest-hanger sleep 300".into(),
        ]);
        let p = plan(&flow).expect("plan");
        let report = tokio::time::timeout(Duration::from_secs(20), run(&flow, &p, &[], &test_runs_root()))
            .await
            .expect("returns within budget")
            .expect("run ok");
        assert_eq!(report.results["slow"].status, Status::Failed);
        let run_dir = report.out_dir.clone().expect("out dir");
        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let att = &manifest.tasks["slow"].attempts["r0"];
        assert_eq!(
            att.error.as_deref(),
            Some("TIMED OUT after 2s (killed with its process group)"),
            "manifest names the timeout"
        );
        let err = std::fs::read_to_string(run_dir.join("tasks/slow/r0/err.txt")).unwrap();
        assert!(err.starts_with("TIMED OUT after 2s"), "err.txt carries the timeout prefix: {err:?}");
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    /// Contract (review minor): a task's own `timeout_secs` must OVERRIDE the
    /// flow default, and that must be the value actually enforced. The existing
    /// `hanging_flow(n)` sets task == flow == n, so a driver that silently used
    /// the flow default and ignored `timeout_secs` would still pass. Here they
    /// are DISTINCT: flow default 300s, task 2s. If the task value were
    /// ignored, the child (sleep 300) would run to the outer 20s guard and
    /// fail this test instead of being killed at ~2s. The manifest reason names
    /// "2s", proving the *task* timeout is the one that fired.
    #[tokio::test]
    async fn task_timeout_overrides_the_flow_default() {
        let mut flow = hanging_flow(300); // flow default 300s
        flow.name = "timeout-precedence".into();
        // A distinct process marker: the suite runs in parallel and the
        // kill-probe test pgreps for `zori-probe-hanger`.
        flow.harnesses.get_mut("hanger").expect("hanger").command = Some(vec![
            "bash".into(),
            "-c".into(),
            "exec -a zori-probe-precedence-hanger sleep 300".into(),
        ]);
        flow.tasks[0].timeout_secs = Some(2); // task wins: 2s, not 300s
        let p = plan(&flow).expect("plan");
        let report = tokio::time::timeout(Duration::from_secs(20), run(&flow, &p, &[], &test_runs_root()))
            .await
            .expect("killed near 2s, not held to the 300s flow default")
            .expect("run ok");
        assert_eq!(report.results["slow"].status, Status::Failed);
        let run_dir = report.out_dir.clone().expect("out dir");
        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest.tasks["slow"].attempts["r0"].error.as_deref(),
            Some("TIMED OUT after 2s (killed with its process group)"),
            "the TASK timeout (2s), not the flow default (300s), fired"
        );
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    /// Contract (review minor): every real attempt carries wall-clock
    /// bookkeeping. `artifacts.rs` only ever asserted `started_at` via hand-
    /// written literals; this proves the actual classic driver RECORDS them on
    /// a done attempt — non-zero, and `ended_at` never before `started_at`.
    #[tokio::test]
    async fn completed_attempt_records_start_and_end_time() {
        let flow = sh_flow("timing", "tick", "echo hi");
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &test_runs_root()).await.expect("run ok");
        assert_eq!(report.results["tick"].status, Status::Done);
        let run_dir = report.out_dir.clone().expect("classic run has an out dir");
        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let att = &manifest.tasks["tick"].attempts["r0"];
        let started = att.started_at.expect("a done attempt records when it started");
        let ended = att.ended_at.expect("a done attempt records when it ended");
        assert!(started > 0, "started_at is a real wall-clock second, not 0/None: {started}");
        assert!(ended >= started, "ended_at is not before started_at: {started}..{ended}");
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    /// The pure timeout-resolution helpers, whose precedence the drivers now
    /// share. `task_timeout_overrides_the_flow_default` proves the task value
    /// is enforced end-to-end; these pin the *fallbacks* that test cannot reach
    /// (task None => flow default; flow None => DEFAULT_TIMEOUT_SECS).
    #[test]
    fn timeout_resolution_precedence_and_fallbacks() {
        let mut t = plain_task("x");
        t.timeout_secs = Some(5);
        assert_eq!(timeout_for(&t, 999).as_secs(), 5, "task value wins over flow default");
        t.timeout_secs = None;
        assert_eq!(timeout_for(&t, 999).as_secs(), 999, "no task value => flow default");

        let mut flow = sh_flow("to", "t", "echo x");
        flow.default_timeout_secs = Some(42);
        assert_eq!(flow_default_timeout(&flow), 42, "flow value used");
        flow.default_timeout_secs = None;
        assert_eq!(
            flow_default_timeout(&flow),
            DEFAULT_TIMEOUT_SECS,
            "neither set => the crate-wide default"
        );
    }

    /// The task's captured stdout still reaches a downstream task on the happy
    /// path (guards that the SharedBuf reader refactor kept piping output).
    #[tokio::test]
    async fn happy_path_captures_output_for_downstream() {
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            "echoer".to_string(),
            HarnessOverride {
                command: Some(vec!["echo".to_string()]),
                prompt_via: Some(PromptVia::Arg),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "echo-test".to_string(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![
                Task {
                   skills: vec![],
                    watchdog: None,
                    id: "a".to_string(),
                    harness: "echoer".to_string(),
                    model: None,
                    in_field: "hello-from-a".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec![],
                    optional: false,
                    pos: None, workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
                Task {
                   skills: vec![],
                    watchdog: None,
                    id: "b".to_string(),
                    harness: "echoer".to_string(),
                    model: None,
                    in_field: "${steps.a.output}".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec!["a".to_string()],
                    optional: false,
                    pos: None, workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
            ],
        };
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &test_runs_root()).await.expect("run ok");
        assert_eq!(report.succeeded(), 2, "both echo tasks succeed");
        let b = report.results.get("b").unwrap();
        assert!(
            b.output.contains("hello-from-a"),
            "b echoes a's captured output, got {:?}",
            b.output
        );
    }

    /// Regression for the first real `issue-planning` run: a best-effort branch
    /// (the zcode reviewer, which fails in print mode) used to skip the whole
    /// join via strict cascade. With `optional: true` on the branch, the join
    /// must still run and see an honest "did not complete" gap instead of its
    /// output or a hard `${steps.a.output}` error. A hard failure is NOT
    /// optional, so `failed()` stays 0 and only `degraded()` counts the branch.
    #[tokio::test]
    async fn optional_branch_failure_does_not_skip_the_join() {
        let mut harnesses = BTreeMap::new();
        // A harness that always fails (exit 3), ignoring the prompt on stdin.
        harnesses.insert(
            "boom".to_string(),
            HarnessOverride {
                command: Some(vec!["sh".to_string(), "-c".to_string(), "exit 3".to_string()]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        // A harmless echo harness for the join.
        harnesses.insert(
            "echoer".to_string(),
            HarnessOverride {
                command: Some(vec!["echo".to_string()]),
                prompt_via: Some(PromptVia::Arg),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "optional-test".to_string(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![
                Task {
                   skills: vec![],
                    watchdog: None,
                    id: "a".to_string(),
                    harness: "boom".to_string(),
                    model: None,
                    in_field: "ignored".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec![],
                    optional: true,
                    pos: None, workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
                Task {
                   skills: vec![],
                    watchdog: None,
                    id: "join".to_string(),
                    harness: "echoer".to_string(),
                    model: None,
                    in_field: "see ${steps.a.output}".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec!["a".to_string()],
                    optional: false,
                    pos: None, workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
            ],
        };
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &test_runs_root()).await.expect("run ok");

        let a = report.results.get("a").unwrap();
        assert_eq!(a.status, Status::Failed, "the boom branch fails");
        assert!(a.optional, "the branch is marked optional");

        let join = report.results.get("join").expect("join ran");
        assert_eq!(
            join.status,
            Status::Done,
            "the join must NOT be skipped when only an optional dep failed"
        );
        assert!(
            join.output.contains("did not complete"),
            "the join sees an honest gap for the failed optional branch, got {:?}",
            join.output
        );

        assert_eq!(report.failed(), 0, "an optional failure is not a hard failure");
        assert_eq!(report.degraded(), 1, "the optional failure counts as degraded");
    }

    /// Review (minor): a cascaded task (skipped because a required dependency
    /// failed) used to be invisible in the run index — `AttemptRecord.error`
    /// documented "which dependency blocked a skip" but skips were never
    /// written. Now the classic driver records a `skipped` attempt naming the
    /// blockers, so a reloaded manifest shows the task existed and why it ran
    /// no agent.
    #[test]
    fn render_argv_never_leaks_live_shell_syntax() {
        // Review (minor): the old renderer left a no-whitespace token like
        // `$(rm -rf)` unquoted and did not escape `$`/backtick inside its
        // double quotes, so the "copy-pasteable" line could run something
        // other than the argv. Safe tokens stay bare; anything else is
        // single-quoted so metacharacters can never look live.
        assert_eq!(render_argv(&[]), "");
        // Bare when wholly shell-safe (incl. `-` and `.`).
        assert_eq!(
            render_argv(&["coddy".into(), "--model".into(), "gpt-4.1-mini".into(), "-C".into()]),
            "coddy --model gpt-4.1-mini -C"
        );
        // Whitespace alone still quotes.
        assert_eq!(render_argv(&["say hi".into()]), "'say hi'");
        // The adversarial cases: none may emerge bare.
        let line = render_argv(&["$(rm -rf)".into()]);
        assert_eq!(line, "'$(rm -rf)'", "command substitution is single-quoted");
        let line = render_argv(&["a`b".into()]);
        assert_eq!(line, "'a`b'", "backtick is quoted so it cannot execute");
        // An embedded single quote uses the POSIX '\'' close/escape idiom.
        assert_eq!(render_argv(&["it's".into()]), r"'it'\''s'");
        // Newlines and tabs become escapes so one token stays one line.
        assert_eq!(render_argv(&["two\nlines".into()]), r"'two\nlines'");
        // Empty token quotes rather than vanishing into a bare nothing.
        assert_eq!(render_argv(&["".into()]), "''");
    }

    #[tokio::test]
    async fn skipped_dependent_is_recorded_in_the_manifest_with_its_blocker() {
        let mut harnesses = BTreeMap::new();
        // A producer that always fails (exit 3).
        harnesses.insert(
            "boom".to_string(),
            HarnessOverride {
                command: Some(vec!["sh".to_string(), "-c".to_string(), "exit 3".to_string()]),
                prompt_via: Some(PromptVia::Stdin),
                ..Default::default()
            },
        );
        harnesses.insert(
            "echoer".to_string(),
            HarnessOverride {
                command: Some(vec!["echo".to_string()]),
                prompt_via: Some(PromptVia::Arg),
                ..Default::default()
            },
        );
        let flow = Flow {
            skills: vec![],
            skills_dirs: vec![],
            watchdog: None,
            name: "skip-record-test".to_string(),
            cwd: None,
            default_timeout_secs: Some(20),
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses,
            tasks: vec![
                Task {
                    skills: vec![],
                    watchdog: None,
                    id: "a".to_string(),
                    harness: "boom".to_string(),
                    model: None,
                    in_field: "ignored".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec![],
                    optional: false,
                    pos: None, workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
                Task {
                    skills: vec![],
                    watchdog: None,
                    id: "b".to_string(),
                    harness: "echoer".to_string(),
                    model: None,
                    in_field: "never runs".to_string(),
                    chat: None,
                    out: None,
                    depends_on: vec!["a".to_string()],
                    optional: false,
                    pos: None, workspace: None,
                    cwd: None,
                    timeout_secs: None,
                    args: vec![],
                    env: BTreeMap::new(),
                },
            ],
        };
        let p = plan(&flow).expect("plan");
        let report = run(&flow, &p, &[], &test_runs_root()).await.expect("run ok");
        assert_eq!(report.results["a"].status, Status::Failed);
        assert_eq!(report.results["b"].status, Status::Skipped, "b cascades");

        let run_dir = report.out_dir.clone().expect("classic run has an out dir");
        let manifest: artifacts::Manifest = serde_json::from_str(
            &std::fs::read_to_string(run_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let b = manifest.tasks.get("b").expect("the skipped task is in the index");
        let att = b.attempts.get("skipped").expect("a skip attempt is recorded");
        assert_eq!(att.status, "skipped");
        let err = att.error.as_deref().unwrap_or_default();
        assert!(
            err.contains("unfinished dependencies") && err.contains("a"),
            "the skip names its blocker 'a', got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&run_dir);
    }
}
