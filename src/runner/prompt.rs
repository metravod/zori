use super::*;

/// Path context available to `${...}` interpolation. A dry run has no run
/// tree, so its ctx carries no paths and the path refs render placeholders.
#[derive(Clone, Default)]
pub(super) struct PromptCtx {
    pub(super) run_dir: Option<PathBuf>,
    pub(super) task_dir: Option<PathBuf>,
    pub(super) work_dir: Option<PathBuf>,
    pub(super) project_dir: Option<PathBuf>,
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
pub(super) fn render_prompt(
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
pub(super) fn skill_block(flow: &Flow, task: &Task) -> Result<Option<String>> {
    let names = crate::skills::effective(flow, task);
    if names.is_empty() {
        return Ok(None);
    }
    crate::skills::Resolver::for_flow(flow).sections(&names)
}

/// Replace `${inputs.k}`, `${steps.id.output}`, `${steps.id.dir}`, `${env.VAR}`
/// and the path refs (`${run.dir}`, `${project.dir}`, `${task.dir}`,
/// `${task.work}`). Unknown references are an error (no silent empty strings).
pub(super) fn interpolate(
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
pub(super) fn missing_output(id: &str) -> String {
    format!(
        "<step '{id}' did not complete: this best-effort branch produced no output; proceed without it>"
    )
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
