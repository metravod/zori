//! Config schema for a zori flow.
//!
//! A flow is a small DAG of tasks. Each task hands one prompt to one CLI agent
//! (a "harness") running one model, and writes its result somewhere so that
//! downstream tasks can consume it. Everything is driven by config, per the
//! owner's brief: `{ in, harness, model, out }` plus `depends_on` to wire the DAG.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A whole flow: a named set of tasks wired into a DAG.
///
/// `Serialize` exists so the `zori ui` flow builder can round-trip a flow:
/// the browser edits a JSON spec, the server parses it as `Flow` (validation),
/// then re-serialises it to the YAML written to disk. The `skip_serializing_if`
/// attributes keep that YAML free of empty/`null` fields so a saved file reads
/// like a hand-written one.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Flow {
    pub name: String,
    /// Default working directory for tasks that do not set their own `cwd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Default per-task timeout in seconds. Default 1800 (agents can be slow).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_timeout_secs: Option<u64>,
    /// How many tasks may run at once within a level. Default = level width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<usize>,
    /// Named values usable as `${inputs.<name>}` inside task `in`/`out`.
    /// Filled by `--input k=v` at the command line, merged over this block.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    /// Optional harness overrides / additions, keyed by harness id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub harnesses: BTreeMap<String, HarnessOverride>,
    /// Supervision defaults for every task (see [`Watchdog`]). A task-level
    /// `watchdog:` overrides the fields it sets; absent everywhere the
    /// defaults apply — see [`crate::watchdog::Settings::defaults`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<Watchdog>,
    /// Skills attached to EVERY task by default (see [`crate::skills`]): named
    /// markdown fragments mixed into each task's starting prompt as a
    /// `# Skill:` section. A task's own `skills:` are appended to these
    /// (de-duplicated). Resolved at run time from `skills_dirs`, `zori-skills/`
    /// and the global `~/.zori/skills`; an unresolvable name is a plan error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Extra directories to search for skill files, in priority order
    /// (earlier wins), searched before the per-project `zori-skills/` and the
    /// global `~/.zori/skills`. Tilde and relative paths are expanded relative
    /// to zori's cwd. Empty just means "only the project and global stores".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills_dirs: Vec<PathBuf>,
    pub tasks: Vec<Task>,
}

/// One node in the DAG: one prompt to one harness+model.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Task {
    /// Unique id; referenced as `${steps.<id>.output}` and in `depends_on`.
    pub id: String,
    /// Which CLI agent runs this task: a harness id (built-in or user-defined).
    pub harness: String,
    /// Model id passed to the harness. Meaning is harness-specific; some
    /// harnesses cannot set it per run (see `Connector::supports_model_flag`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The prompt. Either literal text, or a reference to another task's
    /// output (`${steps.<id>.output}`) or an input (`${inputs.<name>}`).
    /// The owner's brief calls this "in": the user prompt or a task's output.
    /// Rust forbids `in` as a field name, so it is renamed for serde.
    #[serde(rename = "in")]
    pub in_field: String,
    /// Plain-language description of what the agent should produce. Appended
    /// to the prompt as an "# Output" section — "out, in plain words, part of
    /// the starting prompt" per the brief.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out: Option<String>,
    /// Ids of tasks this one waits for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// If true, a downstream task is NOT skipped when this one fails: it runs
    /// and this task's `${steps.<id>.output}` resolves to an explicit
    /// "did not complete" placeholder instead of aborting the join. Use it for
    /// best-effort fan-out branches (e.g. a reviewer on a harness that may be
    /// unavailable) where the other branches must still be merged. Strict
    /// cascade (skip the join) stays the default. See D3.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Per-task working directory; overrides the flow `cwd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Where this task's file side-effects land. `task` (default): an isolated
    /// `<run>/tasks/<id>/work/` dir created per task, so parallel agents never
    /// clobber each other or the repo, and a fork can attribute state. `flow`:
    /// the pre-isolation behaviour — the process cwd is the flow `cwd` (or
    /// zori's), for tasks that must operate directly in the project. An
    /// explicit `cwd:` still wins over both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Workspace>,
    /// Per-task timeout in seconds; overrides `default_timeout_secs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Extra CLI args appended to the harness invocation — the escape hatch
    /// for flags zori does not model natively (e.g. `["--mode", "edit"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables set for this task's process only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Where this task's block sits on the builder's canvas. Purely a UI
    /// convenience — the runner never reads it — stored so a flow clicked
    /// together in `zori ui` (Build) reopens with its layout intact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pos: Option<TaskPos>,
    /// Id of the supermodel chat run bound to this block — the dialogue the
    /// operator started from this node (`POST /api/chats`). Purely a UI
    /// convenience like `pos` (the runner never reads it), stored so the chat
    /// stays *with the block*: navigate anywhere in zori, come back, and the
    /// same dialogue reopens. `None` = no chat started from this block yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    /// Per-task supervision overrides (see [`Watchdog`]); fields left unset
    /// inherit the flow-level block (or the defaults).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog: Option<Watchdog>,
    /// Skills for THIS task, appended after the flow's `skills` (de-duplicated;
    /// see [`crate::skills`]). Each resolves to a markdown fragment mixed into
    /// the starting prompt as a `# Skill:` section — the operator's way to hand
    /// any node reusable context from the shared store without editing its
    /// `in` by hand. An unresolvable name fails the run at plan time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

/// Canvas coordinates of a task's block in the visual builder.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct TaskPos {
    pub x: f64,
    pub y: f64,
}

/// Supervision settings — what zori does *after* an attempt ends (the
/// "watchdog"). Partial by design: every field is optional, an unset field
/// inherits the flow-level block (or the built-in default), so a task can
/// tighten one knob (`watchdog: {nudges: 3}`) without restating the rest.
///
/// What the knobs mean once resolved (see `watchdog::Settings`):
///   * `unattended` — the prompt gets a "nobody will answer" preamble instead
///     of the ask-protocol one, and an `ASK:` question is auto-answered with
///     a canned "act at your discretion" follow-up (the overnight mode).
///   * `ask` — teach the agent the `ASK:` convention so it can park a
///     question for the operator (answered from the chat, the session
///     resumes). Default on.
///   * `nudges` — how many times a suspiciously unfinished reply (clipped,
///     empty) is auto-continued in the same session. Default 1.
///   * `retry` — how many times a failed attempt (timeout, non-zero exit) is
///     re-run from scratch. Default 1.
///   * `ask_timeout_secs` — how long a parked question waits for the
///     operator before the node fails (0 = forever). Default 0.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Watchdog {
    /// Overnight mode: the prompt says "don't ask", and a question that
    /// slips through anyway is auto-answered, not parked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattended: Option<bool>,
    /// Mix the `ASK:` protocol into agent prompts. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<bool>,
    /// Auto-continue budget for unfinished replies. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudges: Option<u32>,
    /// From-scratch retry budget for failed attempts. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u32>,
    /// Parked-question timeout in seconds; 0 waits forever. Default 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_timeout_secs: Option<u64>,
}

/// Where a task's file side-effects land. See `Task::workspace`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Workspace {
    /// An isolated per-task work dir under the run (`<run>/tasks/<id>/work`).
    /// The default: agents write their scratch files there, never into the
    /// repo, and `${task.work}` / `ZORI_TASK_WORK` point at it.
    Task,
    /// The flow's cwd (or zori's own cwd) — the pre-isolation behaviour,
    /// for tasks that must edit the project in place.
    Flow,
}

/// serde helper: omit a bool that is just the default `false`.
fn is_false(b: &bool) -> bool {
    !*b
}

impl Task {
    /// The `in` field, named to avoid the Rust keyword.
    pub fn in_text(&self) -> &str {
        &self.in_field
    }
}

/// User overrides merged on top of a built-in harness definition. A field set
/// to `Some(..)` replaces the built-in; `extra_args` is appended, not replaced.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HarnessOverride {
    /// Short routing advice for the flow planner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Operator notes, keyed by the exact ID in this harness's model list.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_notes: BTreeMap<String, String>,
    /// Replace the base command (argv[0..]). Use this to point zcode at its
    /// real entrypoint, or to wrap coddy/codex in a sandbox/`stdbuf`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_via: Option<PromptVia>,
    /// Args appended after the harness's own flags, before the task's `args`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// When `prompt_via: file`: the flag that takes the prompt-file path.
    /// Without it, a custom harness declaring `prompt_via: file` cannot
    /// receive the prompt and is rejected at plan time (review F02).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file_flag: Option<String>,
    /// An argv whose stdout is one model id per line — how the console lists
    /// selectable models for a *custom* harness (built-ins have their own
    /// sources; see `crate::models`). Empty means "no listing available".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_cmd: Vec<String>,
    /// Flags a task's `args` may replace (rather than sit next to). See
    /// [`Connector::replaceable_flags`]; use this to make a custom harness's
    /// single-value flags overridable the way codex's `-s` is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaceable_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// How the prompt reaches the CLI: as a positional/flag argument, on stdin,
/// or via a prompt file (the `--prompt-file` style some CLIs offer).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PromptVia {
    /// Passed as its own argv token (`-p <prompt>`).
    Arg,
    /// Written to the child's stdin.
    Stdin,
    /// Written to a temp file, passed through `prompt_file_flag`.
    File,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builder stores canvas coordinates on each task; the runner ignores
    /// them, but they must survive the JSON↔Flow↔YAML round-trip so a flow
    /// clicked together in `zori ui` reopens with its layout intact — and a
    /// task without a position must not gain an empty `pos:` line on disk.
    #[test]
    fn task_pos_round_trips_and_omits_when_absent() {
        let yaml = "name: f\ntasks:\n\
                    - id: a\n  harness: echo\n  in: hi\n  pos: {x: 120, y: 40}\n\
                    - id: b\n  harness: echo\n  in: yo\n";
        let flow: Flow = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(flow.tasks[0].pos, Some(TaskPos { x: 120.0, y: 40.0 }));
        assert_eq!(flow.tasks[1].pos, None);

        let out = serde_yaml::to_string(&flow).unwrap();
        let reparsed: Flow = serde_yaml::from_str(&out).unwrap();
        assert_eq!(reparsed.tasks[0].pos, Some(TaskPos { x: 120.0, y: 40.0 }));
        assert_eq!(reparsed.tasks[1].pos, None);
        assert_eq!(out.matches("pos:").count(), 1, "pos omitted for b: {out}");
    }

    /// The skill fields must round-trip through YAML (a flow clicked together
    /// in `zori ui` reopens with its skills intact) yet stay absent from the
    /// serialised file when empty, so a saved flow still reads hand-written.
    #[test]
    fn skills_round_trip_and_omit_when_empty() {
        let yaml = "name: f\nskills: [zori-config]\nskills_dirs: [~/x]\ntasks:\n\
                    - id: a\n  harness: echo\n  in: hi\n  skills: [extra]\n\
                    - id: b\n  harness: echo\n  in: yo\n";
        let flow: Flow = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(flow.skills, vec!["zori-config"]);
        assert_eq!(flow.skills_dirs, vec![PathBuf::from("~/x")]);
        assert_eq!(flow.tasks[0].skills, vec!["extra"]);
        assert_eq!(flow.tasks[1].skills, Vec::<String>::new());

        let out = serde_yaml::to_string(&flow).unwrap();
        let reparsed: Flow = serde_yaml::from_str(&out).unwrap();
        assert_eq!(reparsed.skills, vec!["zori-config"]);
        assert_eq!(reparsed.tasks[0].skills, vec!["extra"]);

        // A flow with no skills writes no `skills:`/`skills_dirs:` line at all.
        let bare = Flow {
            name: "g".into(),
            cwd: None,
            default_timeout_secs: None,
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses: BTreeMap::new(),
            watchdog: None,
            skills: vec![],
            skills_dirs: vec![],
            tasks: vec![flow.tasks[1].clone()],
        };
        let out = serde_yaml::to_string(&bare).unwrap();
        assert!(!out.contains("skills"), "empty skills omitted: {out}");
    }
}
