use super::*;

/// Default per-task timeout when neither the flow nor the task sets one.
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// The flow-level timeout, falling back to [`DEFAULT_TIMEOUT_SECS`].
pub(super) fn flow_default_timeout(flow: &Flow) -> u64 {
    flow.default_timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// The timeout that actually applies to a task: its own `timeout_secs` when
/// set, else the already-resolved flow default. Task wins over flow — this
/// precedence is the whole contract, so it lives in exactly one place.
pub(super) fn timeout_for(task: &Task, flow_default_secs: u64) -> Duration {
    Duration::from_secs(task.timeout_secs.unwrap_or(flow_default_secs))
}

/// Seconds between heartbeat lines for a still-running task. A run that looks
/// dead for minutes (the first real run hung on a deadlock and looked the same
/// as one that was merely slow) is impossible to tell apart without this.
pub(super) const HEARTBEAT_SECS: u64 = 30;

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
    unattended: bool,
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
            let preamble = crate::watchdog::preamble_applies(&conn)
                .then(|| crate::watchdog::preamble(&supervision_settings(flow, task, unattended)))
                .flatten();
            let skills = skill_block(flow, task)?;
            let prompt = render_prompt(task, &inputs, &outputs, &ctx, preamble.as_deref(), skills.as_deref())?;
            if matches!(conn.id.as_str(), "echo" | "prompt") {
                println!("  [{id}] {}  (harness={}, no process)", render_argv(std::slice::from_ref(&prompt)), conn.id);
                outputs.insert(id.clone(), format!("<output of {id}>"));
                continue;
            }
            let mut sp = connectors::plan(&conn, task, &prompt, flag_cwd.as_deref())?;
            if !conn.transcript_chat || conn.id == "coddy" {
                let sessions = ctx.run_dir.as_ref().expect("preview run dir").join("sessions");
                let track = connectors::track_args(&conn, "<session-id>", &sessions);
                connectors::insert_track_args(&mut sp.argv, track);
            }
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

pub(super) fn supervision_settings(flow: &Flow, task: &Task, unattended: bool) -> Settings {
    let mut settings = Settings::for_task(flow, task);
    settings.unattended |= unattended && settings.ask;
    settings
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
pub(super) fn render_argv(argv: &[String]) -> String {
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
