use super::*;

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
    /// Adopt an existing run directory (the tracked engine, incl. reloads):
    /// the manifest is loaded when present, initialised when the run predates
    /// manifests — old flat artifacts stay untouched on disk.
    pub(super) fn adopt(
        out_dir: &Path,
        flow: &Flow,
        plan: &ExecutionPlan,
        inputs: &HashMap<String, String>,
    ) -> Result<Self> {
        let dirs = RunDirs::open(out_dir.to_path_buf());
        let run_id = dirs.run_id();
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
pub(super) fn project_dir_of(flow: &Flow) -> Option<PathBuf> {
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
pub(super) fn dry_preview(flow: &Flow, task: &Task) -> (PromptCtx, Option<PathBuf>) {
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
