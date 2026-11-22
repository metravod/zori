//! Harness connectors: how a task becomes an argv + stdin for a CLI agent.
//!
//! Built-ins cover the CLIs the owner drives — coddy, codex, zcode, opencode —
//! plus an `echo` harness used by smoke tests and `zori run --dry-run` so the
//! DAG can be exercised without spending any model tokens. Every connector is
//! overridable from config (see `HarnessOverride`) and via env vars, because
//! entrypoints in particular live inside .app bundles and may move.
//!
//! Capability note: model selection is NOT uniform. `coddy --model`,
//! `codex exec -m` and `opencode run --model` take a per-run model; the
//! bundled zcode CLI has no such flag (its model is a per-session `/model`
//! setting). `supports_model` makes that explicit rather than silently
//! dropping the user's `model:` value.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

use crate::config::{HarnessOverride, PromptVia, Task};

/// A fully-resolved connector: built-in merged with any user override.
#[derive(Debug, Clone)]
pub struct Connector {
    pub id: String,
    /// Base argv before per-task flags (e.g. `["coddy"]`, or the zcode entry).
    pub command: Vec<String>,
    /// Flag carrying the model, or `None` when the harness cannot pick per run.
    pub model_flag: Option<String>,
    /// Flag carrying the working directory.
    pub cwd_flag: Option<String>,
    /// How the prompt is delivered.
    pub prompt_via: PromptVia,
    /// When `prompt_via == Arg`: the flag that precedes the prompt
    /// (e.g. `-p`). `None` means the prompt is a bare positional argument.
    pub prompt_arg: Option<String>,
    /// When `prompt_via == File`: the flag that takes the temp-file path.
    pub prompt_file_flag: Option<String>,
    /// Whether the harness honours a per-run model at all.
    pub supports_model: bool,
    /// Built-in trailing args (e.g. codex's `-s workspace-write`), before the
    /// task's own `args`.
    pub extra_args: Vec<String>,
    /// Flags in `extra_args` that a task's own `args` may *replace* rather than
    /// sit next to. When a task supplies any of these flags, the connector
    /// drops its built-in occurrence (flag plus its value) so the CLI sees a
    /// single value — matching single-value flag semantics (e.g. codex's
    /// `-s <mode>`, where emitting `-s workspace-write -s read-only` is wrong
    /// and only works by clap last-wins luck). Aliases of the same flag (e.g.
    /// `-s` and `--sandbox`) belong to one group: supplying any one replaces
    /// the built-in occurrences of all of them. Only flags whose value never
    /// begins with `-` may be listed, so value-stripping stays safe.
    pub replaceable_flags: Vec<String>,
    /// Built-in env for the child process.
    pub env: BTreeMap<String, String>,
    /// Whether this harness is a real agent (false for `echo`). Drives warnings.
    pub is_agent: bool,
}

/// Environment var that overrides each built-in command's argv[0]/entrypoint.
const ENV_BIN: &[(&str, &str)] = &[
    ("coddy", "ZORI_CODDY_BIN"),
    ("codex", "ZORI_CODEX_BIN"),
    ("zcode", "ZORI_ZCODE_BIN"),
    ("opencode", "ZORI_OPENCODE_BIN"),
];

/// The zcode CLI lives inside the application bundle; it is not on PATH today.
/// Verified on this machine via `node .../zcode.cjs --help` → `zcode 0.17.1`.
const ZCODE_ENTRY: &str =
    "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs";

/// Locate the bundled ZCode provider config and hand it to the CLI via env.
///
/// The bundled `zcode.cjs` resolves its builtin provider config relative to
/// itself (`<bundle>/provider/zcode-builtin.json`), which does not exist: the
/// real file lives under `~/.zcode/v2/runtime/provider/<os>/<arch>/<ver>/<hash>/zcode-builtin.json`
/// and the ZCode *app* injects `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` when it
/// spawns the CLI. Without it, a bare CLI run dies instantly with
/// `无法定位 CLI ZCode Built-in Provider Config`. So we do what the app does:
/// discover the newest matching file and set the var. `ZORI_ZCODE_BUILTIN_CONFIG`
/// overrides the discovery (e.g. after a ZCode upgrade changes the layout).
fn zcode_provider_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let override_path = std::env::var("ZORI_ZCODE_BUILTIN_CONFIG").ok().filter(|p| !p.is_empty());
    let found = match override_path {
        Some(p) => Some(std::path::PathBuf::from(p)),
        None => discover_zcode_builtin_config(),
    };
    if let Some(path) = found {
        env.insert(
            "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".to_string(),
            path.display().to_string(),
        );
    }
    env
}

fn discover_zcode_builtin_config() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let root = std::path::Path::new(&home).join(".zcode/v2/runtime/provider");
    discover_under(&root)
}

/// Recursively collect every directory *directly* containing a
/// `zcode-builtin.json` under `root`, returning the newest by mtime.
///
/// The provider tree's depth is NOT stable across ZCode versions, so a
/// fixed-depth walk is wrong. Two layouts are real on this machine's family:
///
/// - `<os>/<arch>/<version>/<hash>/` (four levels under `provider/`), and
/// - `<os>-<arch>/<version>/<hash>/` — os and arch *merged* into one dir, so
///   the file sits one level shallower (three levels under `provider/`).
///
/// This machine ships the merged form:
/// `~/.zcode/v2/runtime/provider/darwin-aarch64/3.18.2/endpoint-…/zcode-builtin.json`.
/// The previous fixed 4-level walk therefore never landed on a dir holding the
/// file, discovery returned `None`, `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` was
/// never set, and every zcode task died in <1s with
/// `无法定位 CLI ZCode Built-in Provider Config` — including my own print-mode
/// and app-server legs. Matching on the file's presence to a bounded depth
/// covers both layouts without assuming which one the app uses. Newest-by-mtime
/// keeps the stale version dirs (e.g. `0.0.0-dev`, an old `3.17.2`) from
/// shadowing the one ZCode is actually running.
fn discover_under(root: &std::path::Path) -> Option<std::path::PathBuf> {
    // Bounded so a symlink cycle can never spin: the real tree is at most a
    // handful of levels deep, and depth 8 is far past it.
    const MAX_DEPTH: u32 = 8;
    fn walk(dir: &std::path::Path, depth: u32, best: &mut Option<(std::time::SystemTime, std::path::PathBuf)>) {
        let file = dir.join("zcode-builtin.json");
        if let Ok(meta) = std::fs::metadata(&file) {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                *best = Some((mtime, file));
            }
        }
        if depth == 0 {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, depth - 1, best);
                }
            }
        }
    }
    let mut best = None;
    walk(root, MAX_DEPTH, &mut best);
    best.map(|(_, p)| p)
}

/// Built-in connector ids in stable display order. `zori harnesses` and the
/// console's `/api/harnesses` iterate this instead of re-listing ids (the UI
/// briefly shipped a second, already-stale copy — the opencode connector was
/// missing from `/api/harnesses` while present everywhere else).
pub fn builtin_ids() -> &'static [&'static str] {
    &["prompt", "coddy", "codex", "zcode", "opencode", "echo"]
}

/// The operator's personal harness store: `~/.zori/harnesses.yml`, a map of
/// the same shape as a flow's `harnesses:` block (D11 — machine-personal
/// state that follows the operator, not any project). A personal entry on a
/// built-in id overrides that built-in everywhere (flows may still refine it
/// per flow); a new id becomes a first-class harness visible to the console
/// picker, the Fleet dialog, the planner brief and `zori harnesses`. The
/// next CLI "just slots in": drop a block in this file, no code change.
pub fn personal_harnesses() -> BTreeMap<String, HarnessOverride> {
    personal_harnesses_from(&crate::skills::personal_dir().join("harnesses.yml"))
}

/// The pure half of [`personal_harnesses`], on an explicit path (test seam —
/// a unit test must not read the operator's real `~/.zori`). Absent file →
/// empty; malformed file → empty with a loud warning, the same non-fatal
/// contract as the fleet and skills stores. The `prompt`/`echo` ids are
/// refused: they are zori's data plumbing, not agents, and a personal file
/// must not redefine them out from under every flow.
pub fn personal_harnesses_from(path: &Path) -> BTreeMap<String, HarnessOverride> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return BTreeMap::new(),
    };
    let map: BTreeMap<String, HarnessOverride> = match serde_yaml::from_str(&text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "  [warning] personal harness file {} unparseable ({e}); ignoring it",
                path.display()
            );
            return BTreeMap::new();
        }
    };
    let mut out = BTreeMap::new();
    for (id, ov) in map {
        if matches!(id.as_str(), "prompt" | "echo") {
            eprintln!(
                "  [warning] personal harness file {} tries to redefine '{id}' — skipped (zori's plumbing ids)",
                path.display()
            );
            continue;
        }
        out.insert(id, ov);
    }
    out
}

/// Every harness id resolvable on this machine without a flow: the built-ins
/// plus the personal store's ids. What `/api/harnesses`, the console picker,
/// the planner brief, `zori harnesses` and `zori models` enumerate — one
/// list, so a personally-added CLI shows up everywhere at once.
pub fn all_ids() -> Vec<String> {
    let mut v: Vec<String> = builtin_ids().iter().map(|s| s.to_string()).collect();
    for id in personal_harnesses().keys() {
        if !v.contains(id) {
            v.push(id.clone());
        }
    }
    v
}

/// The built-in connector table. User overrides are merged on top in
/// [`resolve`].
fn builtin(id: &str) -> Option<Connector> {
    let c = match id {
        // The flow's starting block: no CLI, no process — its rendered `in`
        // text becomes the node's output, so tasks that depend on it receive
        // the brief as a `# <id>` section (dependency edges carry data, see
        // `render_prompt`). Gives the canvas a first block to draw edges from.
        "prompt" => Connector {
            id: id.into(),
            command: vec![],
            model_flag: None,
            cwd_flag: None,
            prompt_via: PromptVia::Arg,
            prompt_arg: None,
            prompt_file_flag: None,
            supports_model: false,
            extra_args: vec![],
            replaceable_flags: vec![],
            env: BTreeMap::new(),
            is_agent: false,
        },
        "coddy" => Connector {
            id: id.into(),
            command: vec!["coddy".into()],
            model_flag: Some("--model".into()),
            cwd_flag: Some("--cwd".into()),
            prompt_via: PromptVia::Arg,
            prompt_arg: Some("-p".into()),
            prompt_file_flag: Some("-prompt-file".into()),
            supports_model: true,
            // coddy one-shot reads stdin unless told not to; we pass the prompt
            // by flag, so never attach stray stdin.
            extra_args: vec!["-no-stdin".into()],
            replaceable_flags: vec![],
            env: BTreeMap::new(),
            is_agent: true,
        },
        "codex" => Connector {
            id: id.into(),
            command: vec!["codex".into(), "exec".into()],
            model_flag: Some("-m".into()),
            cwd_flag: Some("-C".into()),
            prompt_via: PromptVia::Arg,
            // `codex exec [PROMPT]` takes the prompt as a positional argument.
            prompt_arg: None,
            prompt_file_flag: Some("-".into()),
            supports_model: true,
            // Non-interactive exec needs a sandbox policy to be predictable.
            extra_args: vec!["-s".into(), "workspace-write".into()],
            // A task that passes its own sandbox policy replaces the built-in
            // `-s workspace-write` rather than emitting a second `-s`.
            replaceable_flags: vec!["-s".into(), "--sandbox".into()],
            env: BTreeMap::new(),
            is_agent: true,
        },
        "zcode" => Connector {
            id: id.into(),
            command: vec!["node".into(), ZCODE_ENTRY.into()],
            model_flag: None,
            cwd_flag: Some("--cwd".into()),
            prompt_via: PromptVia::Arg,
            prompt_arg: Some("-p".into()),
            prompt_file_flag: Some("-p".into()),
            // Bundled zcode CLI has no per-run --model; model is per-session.
            supports_model: false,
            // --mode default for --prompt is `yolo`; we leave that to the
            // operator's task `args` rather than silently escalating rights.
            extra_args: vec![],
            replaceable_flags: vec![],
            // The bundled CLI can't find its own provider config (see
            // zcode_provider_env); without this every zcode task dies in
            // under a second with 无法定位 CLI ZCode Built-in Provider Config.
            env: zcode_provider_env(),
            is_agent: true,
        },
        // OpenCode: `opencode run` is a one-shot non-interactive driver. The
        // prompt is a trailing positional; stdout carries exactly the agent's
        // reply in the default format (spinner/tool noise goes to stderr), so
        // `${steps.<id>.output}` stays clean without any parsing. Models are
        // per-run as `provider/model[#variant]` (`opencode models` lists them);
        // an unknown id fails loudly (`Model unavailable`, exit 1) rather than
        // silently falling back. No cwd flag exists — the runner sets the
        // child's working directory, which OpenCode resolves as the project.
        // Verified live on this box: v2.3.0, `run --help` + a PONG round-trip
        // + session resume by `-s <id>` (see the opencode connector thread).
        "opencode" => Connector {
            id: id.into(),
            command: vec!["opencode".into(), "run".into()],
            model_flag: Some("--model".into()),
            cwd_flag: None,
            prompt_via: PromptVia::Arg,
            // `opencode run [<message...>]` takes the prompt positionally.
            prompt_arg: None,
            prompt_file_flag: None,
            supports_model: true,
            // Nothing baked in: non-interactive runs already auto-approve
            // permissions (proven by a headless file-write probe), and the
            // operator's opencode.jsonc governs the rest. `--auto` and friends
            // stay available through the task's own `args`.
            extra_args: vec![],
            replaceable_flags: vec![],
            env: BTreeMap::new(),
            is_agent: true,
        },
        "echo" => Connector {
            id: id.into(),
            command: vec!["echo".into()],
            model_flag: None,
            cwd_flag: None,
            prompt_via: PromptVia::Arg,
            // echo takes the whole prompt as one positional string.
            prompt_arg: None,
            prompt_file_flag: None,
            supports_model: false,
            extra_args: vec![],
            replaceable_flags: vec![],
            env: BTreeMap::new(),
            is_agent: false,
        },
        _ => return None,
    };
    Some(c)
}

/// Replace a built-in's executable while keeping the CLI contract baked into
/// its base argv: codex needs `exec`, opencode needs `run` (review F02 —
/// the old code collapsed the whole command to `[bin]` and silently broke
/// both). zcode is the special case: its CLI is a node script, so an
/// override pointing at a `.cjs`/`.js` keeps the `node` launcher, while a
/// native binary replaces the whole argv (the old documented contract).
fn apply_env_bin(conn: &mut Connector, id: &str, bin: String) {
    if id == "zcode" {
        conn.command = if bin.ends_with(".cjs") || bin.ends_with(".js") {
            vec!["node".into(), bin]
        } else {
            vec![bin]
        };
        return;
    }
    let tail: Vec<String> = conn.command.iter().skip(1).cloned().collect();
    let mut cmd = vec![bin];
    cmd.extend(tail);
    conn.command = cmd;
}

/// Trim and validate an env-provided binary path. An empty value used to
/// launch nothing (or the wrong thing) with a confusing OS error; refuse
/// early with the variable name in the message.
fn normalize_env_bin(key: &str, id: &str, raw: &str) -> Result<String> {
    let bin = raw.trim();
    if bin.is_empty() {
        bail!("{key} is set but empty — refusing to launch harness '{id}' with an empty executable");
    }
    Ok(bin.to_string())
}

/// Resolve a harness id to a connector. Three layers, tightest last:
/// built-in → the personal store (`~/.zori/harnesses.yml`) → this flow's
/// `harnesses:` block. A personal entry on a built-in id refines it; a new
/// id is defined by the personal store outright (the flow may still refine
/// it). Unknown ids with neither are an error.
pub fn resolve(id: &str, flow: Option<&HarnessOverride>) -> Result<Connector> {
    resolve_with(id, flow, &personal_harnesses())
}

/// The pure half of [`resolve`], on an explicit personal store (test seam —
/// a unit test must not depend on the machine's real `~/.zori/harnesses.yml`).
pub fn resolve_with(
    id: &str,
    flow: Option<&HarnessOverride>,
    personal: &BTreeMap<String, HarnessOverride>,
) -> Result<Connector> {
    let personal = personal.get(id).cloned();
    let mut c = builtin(id);

    // Allow env override of the binary for the known agent harnesses even
    // when a built-in exists (e.g. point coddy at a local build). The
    // override must not destroy the CLI contract baked into the built-in
    // command (review F02): codex needs `exec`, opencode needs `run`.
    if let Some(conn) = c.as_mut() {
        if let Some((_, key)) = ENV_BIN.iter().find(|(name, _)| *name == id) {
            if let Ok(raw) = std::env::var(key) {
                let bin = normalize_env_bin(key, id, &raw)?;
                apply_env_bin(conn, id, bin);
            }
        }
    }

    match c {
        Some(mut c) => {
            if let Some(p) = &personal {
                apply_override(&mut c, p);
            }
            if let Some(o) = flow {
                apply_override(&mut c, o);
            }
            Ok(c)
        }
        None => {
            // A user-defined harness must declare at least a command.
            let o = match (&personal, flow) {
                (Some(p), Some(f)) => compose_over(f, p),
                (Some(p), None) => p.clone(),
                (None, Some(f)) => f.clone(),
                (None, None) => bail!(
                    "unknown harness '{id}' (no built-in, nothing in ~/.zori/harnesses.yml, no command override)"
                ),
            };
            user_defined(id, &o)
        }
    }
}

/// Construct a connector for an id with no built-in behind it: the personal
/// store or the flow owns the whole CLI contract.
fn user_defined(id: &str, o: &HarnessOverride) -> Result<Connector> {
    let command = match o.command.as_ref().filter(|v| !v.is_empty()) {
        Some(cmd) => cmd.clone(),
        None => bail!("harness '{id}' needs a command (no built-in and no command override)"),
    };
    Ok(Connector {
        id: id.into(),
        command,
        model_flag: o.model_flag.clone(),
        cwd_flag: o.cwd_flag.clone(),
        prompt_via: o.prompt_via.unwrap_or(PromptVia::Arg),
        // `-p` is the historical custom-harness default; an explicit
        // `prompt_arg:` wins (e.g. a CLI whose print flag is `--prompt`).
        prompt_arg: o.prompt_arg.clone().or_else(|| Some("-p".into())),
        prompt_file_flag: o.prompt_file_flag.clone(),
        supports_model: o.model_flag.is_some(),
        extra_args: o.extra_args.clone(),
        replaceable_flags: o.replaceable_flags.clone(),
        env: o.env.clone(),
        is_agent: true,
    })
}

/// Apply an override on top of any connector (built-in or user-defined):
/// a `Some(..)` field replaces, `extra_args` append, `env` merges per key.
fn apply_override(c: &mut Connector, o: &HarnessOverride) {
    if let Some(cmd) = &o.command {
        if !cmd.is_empty() {
            c.command = cmd.clone();
        }
    }
    if o.model_flag.is_some() {
        c.model_flag = o.model_flag.clone();
        c.supports_model = c.model_flag.is_some();
    }
    if o.cwd_flag.is_some() {
        c.cwd_flag = o.cwd_flag.clone();
    }
    if let Some(pv) = o.prompt_via {
        c.prompt_via = pv;
    }
    if o.prompt_arg.is_some() {
        c.prompt_arg = o.prompt_arg.clone();
    }
    if o.prompt_file_flag.is_some() {
        c.prompt_file_flag = o.prompt_file_flag.clone();
    }
    c.extra_args.extend(o.extra_args.iter().cloned());
    if !o.replaceable_flags.is_empty() {
        c.replaceable_flags = o.replaceable_flags.clone();
    }
    for (k, v) in &o.env {
        c.env.insert(k.clone(), v.clone());
    }
}

/// Flow refinement over a personal definition, for an id with no built-in
/// behind it: the flow wins per field, `extra_args` concatenate
/// personal-then-flow, `env` merges with the flow winning per key.
fn compose_over(flow: &HarnessOverride, personal: &HarnessOverride) -> HarnessOverride {
    let mut env = personal.env.clone();
    env.extend(flow.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    HarnessOverride {
        description: flow.description.clone().or_else(|| personal.description.clone()),
        model_notes: if flow.model_notes.is_empty() { personal.model_notes.clone() } else { flow.model_notes.clone() },
        command: flow.command.clone().or_else(|| personal.command.clone()),
        model_flag: flow.model_flag.clone().or_else(|| personal.model_flag.clone()),
        cwd_flag: flow.cwd_flag.clone().or_else(|| personal.cwd_flag.clone()),
        prompt_via: flow.prompt_via.or(personal.prompt_via),
        prompt_arg: flow.prompt_arg.clone().or_else(|| personal.prompt_arg.clone()),
        extra_args: personal.extra_args.iter().cloned().chain(flow.extra_args.iter().cloned()).collect(),
        prompt_file_flag: flow.prompt_file_flag.clone().or_else(|| personal.prompt_file_flag.clone()),
        models_cmd: if flow.models_cmd.is_empty() { personal.models_cmd.clone() } else { flow.models_cmd.clone() },
        replaceable_flags: if flow.replaceable_flags.is_empty() { personal.replaceable_flags.clone() } else { flow.replaceable_flags.clone() },
        env,
    }
}

/// The command we would run for a task, split so the runner can decide how to
/// feed stdin / write a temp prompt file.
#[derive(Debug)]
pub struct SpawnPlan {
    pub program: String,
    pub argv: Vec<String>,
    /// Prompt text to write to the child's stdin (when `prompt_via == Stdin`).
    pub stdin_prompt: Option<String>,
    /// Prompt text to write to a temp file passed via `prompt_file_flag`.
    pub file_prompt: Option<String>,
}

/// Assemble the argv for a task under a connector. `prompt` is the final
/// rendered prompt (after `${...}` interpolation and `out` append).
/// `cwd_flag` is the working directory to hand the harness via its cwd flag
/// (the caller decides it — see the runner's workspace matrix); `None` omits
/// the flag entirely.
///
/// Returns Err if the task sets `model:` on a harness that cannot honour it —
/// surfacing the mismatch loudly beats silently ignoring the user's intent.
pub fn plan(conn: &Connector, task: &Task, prompt: &str, cwd_flag: Option<&Path>) -> Result<SpawnPlan> {
    if task.model.is_some() && !conn.supports_model {
        bail!(
            "task '{}': harness '{}' does not support per-run model selection \
             (got model: {:?}). Set the model out-of-band (e.g. zcode /model), \
             add a model_flag via a harness override, or drop `model:`.",
            task.id,
            conn.id,
            task.model.as_deref().unwrap_or("")
        );
    }

    let mut argv: Vec<String> = Vec::new();

    if let (Some(flag), Some(model)) = (&conn.model_flag, &task.model) {
        argv.push(flag.clone());
        argv.push(model.clone());
    }
    if let (Some(flag), Some(cwd)) = (&conn.cwd_flag, cwd_flag) {
        argv.push(flag.clone());
        argv.push(cwd.display().to_string());
    }

    // Prompt delivery.
    let mut stdin_prompt = None;
    let mut file_prompt = None;
    // A positional prompt that would parse as a flag gets sent last, after a
    // `--` terminator — with task args *before* the terminator so they stay
    // flags rather than becoming part of the prompt text (review F02).
    let mut trailing_prompt: Option<String> = None;
    match conn.prompt_via {
        PromptVia::Arg => {
            if let Some(flag) = &conn.prompt_arg {
                argv.push(flag.clone());
                argv.push(prompt.to_string());
            } else if prompt.starts_with('-') {
                trailing_prompt = Some(prompt.to_string());
            } else {
                argv.push(prompt.to_string());
            }
        }
        PromptVia::Stdin => stdin_prompt = Some(prompt.to_string()),
        PromptVia::File => {
            // Without a flag there is no way to tell the CLI where the prompt
            // file is; silently writing one and never passing it produced
            // exit-0 runs that never saw the prompt (review F02).
            let Some(flag) = &conn.prompt_file_flag else {
                bail!(
                    "task '{}': harness '{}' declares prompt_via: file but has no \
                     prompt_file_flag — the prompt could never reach the CLI. Set \
                     prompt_file_flag in the harness override, or use prompt_via: arg/stdin.",
                    task.id,
                    conn.id
                );
            };
            file_prompt = Some(prompt.to_string());
            argv.push(flag.clone());
            argv.push("__ZORI_PROMPT_FILE__".into());
        }
    }

    // Built-in trailing args, unless the task re-specifies one of the
    // connector's single-value flags — then the built-in occurrence (flag plus
    // its value) is dropped so the CLI sees a single value (e.g. codex `-s`),
    // not two. Without this, a read-only reviewer got both `-s workspace-write`
    // and `-s read-only`, working only by clap last-wins luck.
    let extra: Vec<String> = if !conn.replaceable_flags.is_empty()
        && task.args.iter().any(|a| flag_in_group(a, &conn.replaceable_flags))
    {
        strip_flag_group(&conn.extra_args, &conn.replaceable_flags)
    } else {
        conn.extra_args.clone()
    };
    argv.extend(extra);
    argv.extend(task.args.iter().cloned());

    // A prompt that starts with '-' went out of the normal position above;
    // send it last after `--` so no CLI parses it as a flag. Task args are
    // already in argv, so they cannot be swallowed into the prompt text.
    if let Some(p) = trailing_prompt {
        argv.push("--".into());
        argv.push(p);
    }

    let program = conn
        .command
        .first()
        .cloned()
        .unwrap_or_else(|| conn.id.clone());
    let mut full = conn.command.clone();
    full.extend(argv);

    Ok(SpawnPlan {
        program,
        argv: full,
        stdin_prompt,
        file_prompt,
    })
}

/// True when `tok` is a group flag: it equals a member (e.g. `-s`) or is its
/// `=`-form (e.g. `--sandbox=read-only`). Members are aliases of one
/// single-value flag, so matching any one treats it as that flag.
fn flag_in_group(tok: &str, group: &[String]) -> bool {
    group.iter().any(|f| tok == f || tok.starts_with(&format!("{f}=")))
}

/// Drop every occurrence of a single-value flag group from `base`, along with
/// the value that follows it, so a task's own occurrence is the only one the
/// CLI sees. A bare match (`-s`) consumes the next token as its value; an
/// `=`-form (`--sandbox=read-only`) carries its own value.
fn strip_flag_group(base: &[String], group: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(base.len());
    let mut it = base.iter().peekable();
    while let Some(tok) = it.next() {
        if flag_in_group(tok, group) {
            if !tok.contains('=') {
                it.next(); // skip the value token that follows the bare flag
            }
            continue;
        }
        out.push(tok.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Tracked / resumable planning (the `zori ui` steering path).
//
// The classic `plan()` above is print-and-forget and stays untouched — every
// runner test depends on it. For the interactive console we additionally need
// to (a) turn a *first* run into a resumable session, and (b) resume that
// session with a fresh human turn. These helpers do that without disturbing
// `plan()`; the tracked executor calls `plan()` then appends `track_args()`.
//
// Proven live on this box (see the web-ui thread): coddy reopens a print-run by
// `-session-id` under an isolated `-sessions-dir` (deterministic id, no `-c`
// race, zero cross-talk between two tasks sharing a cwd); codex `exec --json`
// emits `thread.started` and `codex exec resume --json <id>` continues it with
// context (resume rejects the `-s` sandbox flag, so it is omitted here).
// ---------------------------------------------------------------------------

/// Extra argv that makes a tracked *first* run resumable, per harness family.
/// Appended after the normal `plan()` argv. The fixed `session_id` and isolated
/// `sessions_dir` are supplied by the caller (the run manager) so each node's
/// session is unambiguously reopenable.
pub fn track_args(conn: &Connector, session_id: &str, sessions_dir: &std::path::Path) -> Vec<String> {
    match conn.id.as_str() {
        // Coddy stores the run as a print-run we can reopen by id; isolate it so
        // a sibling task in the same cwd can never be the "most recent".
        "coddy" => vec![
            "-session-id".into(),
            session_id.to_string(),
            "-sessions-dir".into(),
            sessions_dir.display().to_string(),
        ],
        // Codex: `--json` so the runner can read `thread.started` (the resume
        // key) and `item.completed{agent_message}` (the reply) from a stream;
        // run outside a git repo is allowed so an arbitrary flow cwd works.
        "codex" => vec!["--json".into(), "--skip-git-repo-check".into()],
        // OpenCode: `--format json` streams JSONL whose every line carries
        // `sessionID` (the `-s` resume key) and whose `type:"text"` parts carry
        // the reply. The default human format would put the session id nowhere
        // and decorate stdout with spinner lines.
        "opencode" => vec!["--format".into(), "json".into()],
        // Echo needs nothing (the exec layer scripts it); other harnesses are
        // tracked opaquely and not steerable.
        _ => vec![],
    }
}

/// Merge [`track_args`] into a planned argv *on the flags side* of the `--`
/// terminator. Review M5: when the prompt starts with `-`, `plan()` sends it
/// last behind a `--` terminator (so the CLI cannot read the prompt as a
/// flag — F02). A blind `argv.extend(track_args)` then lands `--json` /
/// `--format json` behind that terminator too, where the CLI sees them as
/// positional text: codex stops emitting `thread.started`, opencode stops
/// emitting `sessionID`, and tracked capture silently dies on a dash-first
/// prompt. Inserting at the terminator keeps the flags real for any prompt.
pub fn insert_track_args(argv: &mut Vec<String>, track: Vec<String>) {
    if track.is_empty() {
        return;
    }
    match argv.iter().position(|t| t == "--") {
        Some(pos) => {
            argv.splice(pos..pos, track);
        }
        None => argv.extend(track),
    }
}

/// Build the argv that resumes a node's agent session with a new human prompt.
/// Supports the two steerable families; anything else is an honest error so the
/// UI disables the input box rather than spawning a command that silently
/// starts a *fresh* session (which would look like steering but forget).
pub fn resume_plan(
    conn: &Connector,
    session: &crate::state::SessionRef,
    prompt: &str,
    cwd: Option<&std::path::Path>,
) -> Result<SpawnPlan> {
    match session {
        crate::state::SessionRef::Coddy { sessions_dir, session_id } => {
            // Reopen-or-create by the fixed id under the isolated dir. `-p`
            // carries the new turn; `-no-stdin` so stray stdin never attaches.
            let mut full = conn.command.clone(); // ["coddy"] or env override
            if let Some(cwd) = cwd {
                if let Some(f) = &conn.cwd_flag {
                    full.push(f.clone());
                    full.push(cwd.display().to_string());
                }
            }
            if let Some(f) = &conn.prompt_arg {
                full.push(f.clone()); // -p
            }
            full.push(prompt.to_string());
            full.push("-session-id".into());
            full.push(session_id.clone());
            full.push("-sessions-dir".into());
            full.push(sessions_dir.display().to_string());
            full.push("-no-stdin".into());
            Ok(SpawnPlan {
                program: conn.command.first().cloned().unwrap_or_else(|| "coddy".into()),
                argv: full,
                stdin_prompt: None,
                file_prompt: None,
            })
        }
        crate::state::SessionRef::Codex { thread_id } => {
            // Review (minor): a blank resume key is never a real thread id.
            // The parser now filters `"thread_id":""` out, but a session stored
            // before that fix (or a hand-built SessionRef) must not spawn
            // `codex exec resume ""` — which silently starts a FRESH session and
            // reads like steering while forgetting everything. Fail loudly so
            // the caller records a refusal rather than a false resume.
            if thread_id.trim().is_empty() {
                bail!("codex thread id is blank; this session cannot be resumed");
            }
            // `codex exec resume --json [--skip-git-repo-check] <id> <prompt>`.
            // No `-s`/`-m` (the session already has its policy and model, and
            // `resume` rejects `-s`). Derive from conn.command = ["codex","exec"].
            let mut full = conn.command.clone();
            // conn.command may already be ["codex","exec"]; ensure "resume".
            if !full.iter().any(|a| a == "resume") {
                full.push("resume".into());
            }
            full.push("--json".into());
            full.push("--skip-git-repo-check".into());
            full.push(thread_id.clone());
            // Resume takes the prompt as a trailing positional argument.
            full.push(prompt.to_string());
            Ok(SpawnPlan {
                program: conn.command.first().cloned().unwrap_or_else(|| "codex".into()),
                argv: full,
                stdin_prompt: None,
                file_prompt: None,
            })
        }
        crate::state::SessionRef::Opencode { session_id } => {
            // Review (minor): same blank-key guard as codex. `opencode run -s ""`
            // would open a fresh session — steering that silently forgets.
            if session_id.trim().is_empty() {
                bail!("opencode session id is blank; this session cannot be resumed");
            }
            // `opencode run --format json -s <id> <prompt>` continues the
            // session with full context (proven live: a resumed session
            // recalled a turn-1 secret word). No `--model` on resume — the
            // session keeps its model, matching codex's resume semantics.
            // cwd rides on the child's working directory (no flag exists).
            let mut full = conn.command.clone(); // ["opencode","run"] or env override
            full.push("--format".into());
            full.push("json".into());
            full.push("-s".into());
            full.push(session_id.clone());
            // The prompt is a trailing positional argument.
            full.push(prompt.to_string());
            Ok(SpawnPlan {
                program: conn.command.first().cloned().unwrap_or_else(|| "opencode".into()),
                argv: full,
                stdin_prompt: None,
                file_prompt: None,
            })
        }
        crate::state::SessionRef::Echo => {
            // Handled by the exec layer (a canned reply, no process). Reaching
            // here means a caller forgot to special-case echo.
            bail!("echo sessions are steered without spawning; not a connector error")
        }
        crate::state::SessionRef::Opaque { detail } => {
            bail!("this session cannot be steered ({detail})")
        }
    }
}

/// A parsed view of `codex exec --json` output: the resume key plus every agent
/// message, in order. Lines we don't understand are ignored (forward-compat).
#[derive(Debug, Default, Clone)]
pub struct CodexEvent {
    pub thread_id: Option<String>,
    pub agent_messages: Vec<String>,
    pub turn_completed: bool,
    pub error: Option<String>,
}

/// Parse the JSONL that `codex exec --json` prints on stdout. Robust to
/// interleaved non-JSON lines (codex also prints a human summary on stderr and
/// occasionally on stdout) by skipping any line that is not an object.
pub fn parse_codex_jsonl(stdout: &str) -> CodexEvent {
    let mut ev = CodexEvent::default();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match val.get("type").and_then(|t| t.as_str()) {
            Some("turn.failed") | Some("error") => {
                let message = val.pointer("/error/message").or_else(|| val.get("message"));
                ev.error = Some(message.and_then(|v| v.as_str()).unwrap_or("Codex stream error").to_owned());
            }
            // Codex may report a disconnect and then reconnect successfully.
            Some("turn.started") => {
                ev.turn_completed = false;
                ev.error = None;
            }
            Some("turn.completed") => {
                ev.turn_completed = true;
                ev.error = None;
            }
            Some("thread.started") => {
                // Review (minor): a blank `thread_id` is not a resume key. A
                // codex `thread.started` carrying `""` used to record Some(""),
                // which resume_plan then handed to `codex exec resume ""`. Only
                // a non-blank id counts; otherwise keep looking.
                if let Some(id) = val.get("thread_id").and_then(|t| t.as_str()).filter(|s| !s.trim().is_empty()) {
                    ev.thread_id = Some(id.to_string());
                }
            }
            Some("item.completed") => {
                let item = val.get("item");
                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str()) == Some("agent_message") {
                    if let Some(text) = item.and_then(|i| i.get("text")).and_then(|t| t.as_str()) {
                        ev.agent_messages.push(text.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    ev
}

/// True when a harness family can be steered through the console. Drives the
/// UI (input box enabled) and gates `resume_plan`.
pub fn steerable(conn: &Connector) -> bool {
    matches!(conn.id.as_str(), "coddy" | "codex" | "opencode" | "echo")
}

/// A parsed view of `opencode run --format json` output: the resume key plus
/// every agent text part, in order. Unknown lines are ignored (forward-compat).
#[derive(Debug, Default, Clone)]
pub struct OpencodeEvent {
    pub session_id: Option<String>,
    pub agent_texts: Vec<String>,
    /// Offset of the final model step within the full transcript.
    pub final_step_start: usize,
    pub step_open: bool,
    /// Final step closed normally before OpenCode reported a cancelled input request.
    pub stopped: bool,
    pub late_input_error: Option<String>,
    pub error: Option<String>,
}

/// Parse the JSONL that `opencode run --format json` prints on stdout: every
/// line carries `sessionID` (`ses_…`, the `-s` resume key); the reply lives in
/// `type:"text"` events as `part.text`. Same skip-non-object discipline as the
/// codex parser so interleaved noise can never break it.
pub fn parse_opencode_jsonl(stdout: &str) -> OpencodeEvent {
    let mut ev = OpencodeEvent::default();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match val.get("type").and_then(|v| v.as_str()) {
            Some("step_start") => {
                if let Some(previous) = ev.late_input_error.take() {
                    ev.error.get_or_insert(previous);
                }
                ev.stopped = false;
                ev.final_step_start = ev.agent_texts.len();
                ev.step_open = true;
            }
            Some("text" | "tool_use") => ev.stopped = false,
            Some("step_finish") => {
                ev.stopped = ev.step_open && val.pointer("/part/reason").and_then(|v| v.as_str()) == Some("stop");
                ev.step_open = false;
            }
            _ => {}
        }
        if val.get("type").and_then(|v| v.as_str()) == Some("error") {
            let message = val.pointer("/error/data/message").or_else(|| val.pointer("/error/message"))
                .or_else(|| val.pointer("/error/name"));
            let message = message.and_then(|v| v.as_str()).unwrap_or("OpenCode stream error");
            if ev.stopped && message.starts_with("Cancelled a request for input this run cannot answer: ") {
                ev.late_input_error = Some(message.to_owned());
            } else {
                ev.error = Some(message.to_owned());
            }
        }
        if ev.session_id.is_none() {
            // Review (minor): an OpenCode event may carry a blank `sessionID`
            // before the real one arrives. Accepting "" locked the slot
            // (is_none() guard) so the genuine ses_… on a later line was never
            // picked up, and resume_plan would have run `-s ""`. Skip blanks
            // and keep looking.
            if let Some(id) = val.get("sessionID").and_then(|t| t.as_str()).filter(|s| !s.trim().is_empty()) {
                ev.session_id = Some(id.to_string());
            }
        }
        if val.get("type").and_then(|t| t.as_str()) == Some("text") {
            // Only the assistant's own text parts; tool/reasoning parts have
            // other `part.type`s and never reach the reply.
            let part = val.get("part");
            if part.and_then(|p| p.get("type")).and_then(|t| t.as_str()) == Some("text") {
                if let Some(text) = part.and_then(|p| p.get("text")).and_then(|t| t.as_str()) {
                    ev.agent_texts.push(text.to_string());
                }
            }
        }
    }
    ev
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use crate::state::SessionRef;

    fn argv_str(p: &SpawnPlan) -> String {
        p.argv.join(" ")
    }

    /// Minimal task builder for the opencode plan test (the richer `task`
    /// helper lives in the other test module).
    fn plain_task(id: &str, harness: &str) -> crate::config::Task {
        crate::config::Task {
            skills: vec![],
            watchdog: None,
            id: id.into(),
            harness: harness.into(),
            model: None,
            in_field: "hi".into(),
            out: None,
            depends_on: vec![],
            optional: false,
            pos: None,
            workspace: None,
            cwd: None,
            timeout_secs: None,
            args: vec![],
            env: BTreeMap::new(),
            chat: None,
        }
    }

    #[test]
    fn track_args_per_harness() {
        let sd = std::path::Path::new("/tmp/run1/sessions");
        let coddy = builtin("coddy").unwrap();
        let ta = track_args(&coddy, "sess_x", sd);
        assert!(ta.contains(&"-session-id".to_string()));
        assert!(ta.contains(&"sess_x".to_string()));
        assert!(ta.contains(&"-sessions-dir".to_string()));
        assert!(ta.contains(&sd.display().to_string().to_string()));

        let codex = builtin("codex").unwrap();
        let tc = track_args(&codex, "ignored", sd);
        assert!(tc.contains(&"--json".to_string()));
        assert!(tc.contains(&"--skip-git-repo-check".to_string()));

        let echo = builtin("echo").unwrap();
        assert!(track_args(&echo, "x", sd).is_empty());

        let oc = builtin("opencode").unwrap();
        let to = track_args(&oc, "ignored", sd);
        assert!(to.contains(&"--format".to_string()));
        assert!(to.contains(&"json".to_string()));
    }

    #[test]
    fn coddy_resume_reopens_by_id() {
        let conn = builtin("coddy").unwrap();
        let sess = SessionRef::Coddy {
            sessions_dir: std::path::PathBuf::from("/tmp/run1/sessions"),
            session_id: "sess_alpha".into(),
        };
        let p = resume_plan(&conn, &sess, "and again but shorter", None).unwrap();
        let a = argv_str(&p);
        assert!(a.contains("-session-id sess_alpha"), "{a}");
        assert!(a.contains("-sessions-dir /tmp/run1/sessions"), "{a}");
        assert!(a.contains("-p and again but shorter"), "{a}");
        assert!(a.contains("-no-stdin"), "{a}");
        assert_eq!(p.program, "coddy");
    }

    #[test]
    fn coddy_resume_carries_cwd_when_the_harness_has_a_cwd_flag() {
        let conn = builtin("coddy").unwrap();
        let sess = SessionRef::Coddy {
            sessions_dir: "/tmp/r".into(),
            session_id: "s".into(),
        };
        let p = resume_plan(&conn, &sess, "hi", Some(std::path::Path::new("/work"))).unwrap();
        let a = argv_str(&p);
        assert!(a.contains("--cwd /work"), "{a}");
    }

    #[test]
    fn codex_resume_is_by_id_without_sandbox_or_model() {
        let conn = builtin("codex").unwrap();
        let sess = SessionRef::Codex { thread_id: "tid-123".into() };
        let p = resume_plan(&conn, &sess, "clarify please", None).unwrap();
        let a = argv_str(&p);
        assert!(a.contains("exec resume"), "{a}");
        assert!(a.contains("--json"), "{a}");
        assert!(a.contains("--skip-git-repo-check"), "{a}");
        assert!(a.contains("tid-123"), "{a}");
        assert!(a.ends_with("clarify please"), "{a}");
        // The two footguns proven by the spike: resume must NOT carry -s or -m.
        assert!(!a.contains(" -s "), "resume must not carry -s: {a}");
        assert!(!a.contains("workspace-write"), "no default sandbox on resume: {a}");
        assert!(!a.contains(" -m "), "no model flag on resume: {a}");
        assert_eq!(p.program, "codex");
    }

    #[test]
    fn opaque_and_echo_are_not_spliced_by_resume_plan() {
        let conn = builtin("coddy").unwrap();
        let o = SessionRef::Opaque { detail: "zcode".into() };
        assert!(resume_plan(&conn, &o, "x", None).is_err());
        let e = SessionRef::Echo;
        assert!(resume_plan(&conn, &e, "x", None).is_err());
    }

    // Review (minor): a blank resume key must be refused, not spliced. The CLI
    // would treat `codex exec resume ""` / `opencode run -s ""` as a request
    // for a FRESH session — steering that silently forgets the whole context,
    // which is worse than an error. A session persisted before the parser
    // started filtering blanks (or a hand-built SessionRef) hits this guard.
    #[test]
    fn resume_plan_refuses_a_blank_thread_or_session_id() {
        let conn = builtin("codex").unwrap();
        let empty = SessionRef::Codex { thread_id: "".into() };
        let blank = SessionRef::Codex { thread_id: "   ".into() };
        for s in [empty, blank] {
            let e = resume_plan(&conn, &s, "x", None).unwrap_err().to_string();
            assert!(e.contains("blank"), "codex blank id must be refused loudly: {e}");
        }
        let conn = builtin("opencode").unwrap();
        let e = resume_plan(&conn, &SessionRef::Opencode { session_id: "".into() }, "x", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("blank"), "opencode blank id must be refused loudly: {e}");
    }

    #[test]
    fn steerable_only_for_known_families() {
        assert!(steerable(&builtin("coddy").unwrap()));
        assert!(steerable(&builtin("codex").unwrap()));
        assert!(steerable(&builtin("opencode").unwrap()));
        assert!(steerable(&builtin("echo").unwrap()));
        assert!(!steerable(&builtin("zcode").unwrap()));
    }

    #[test]
    fn opencode_plan_puts_model_flag_before_positional_prompt() {
        let c = builtin("opencode").unwrap();
        let mut t = plain_task("oc", "opencode");
        t.model = Some("zai-coding-plan/glm-5.3".into());
        let p = plan(&c, &t, "say PONG", None).unwrap();
        assert_eq!(p.program, "opencode");
        // command base + model + positional prompt, in that order.
        assert_eq!(
            p.argv,
            vec![
                "opencode".to_string(),
                "run".to_string(),
                "--model".to_string(),
                "zai-coding-plan/glm-5.3".to_string(),
                "say PONG".to_string(),
            ]
        );
        assert!(p.stdin_prompt.is_none() && p.file_prompt.is_none());
    }

    #[test]
    fn opencode_resume_is_by_session_id_with_json() {
        let conn = builtin("opencode").unwrap();
        let sess = SessionRef::Opencode { session_id: "ses_abc123".into() };
        let p = resume_plan(&conn, &sess, "and now shorter", None).unwrap();
        let a = argv_str(&p);
        assert!(a.contains("run"), "keeps the run subcommand: {a}");
        assert!(a.contains("--format json"), "{a}");
        assert!(a.contains("-s ses_abc123"), "{a}");
        assert!(a.ends_with("and now shorter"), "prompt is the positional: {a}");
        // The session carries its model; resume must not re-specify --model.
        assert!(!a.contains("--model"), "no model flag on resume: {a}");
        assert_eq!(p.program, "opencode");
    }

    #[test]
    fn opencode_jsonl_parses_session_and_texts_in_order() {
        // Shape captured live from `opencode run --format json` (v2.3.0):
        // every line carries sessionID; replies are type:"text" with
        // part.type:"text". Tool/reasoning lines must not leak into texts.
        let sample = "\
{\"type\":\"step_start\",\"sessionID\":\"ses_e20a\",\"part\":{\"type\":\"step-start\"}}
{\"type\":\"text\",\"sessionID\":\"ses_e20a\",\"part\":{\"type\":\"text\",\"text\":\"PONG\"}}
{\"type\":\"step_finish\",\"sessionID\":\"ses_e20a\",\"part\":{\"type\":\"step-finish\"}}
{\"type\":\"text\",\"sessionID\":\"ses_e20a\",\"part\":{\"type\":\"tool\",\"text\":\"ignored\"}}
{\"type\":\"text\",\"sessionID\":\"ses_e20a\",\"part\":{\"type\":\"text\",\"text\":\"done\"}}
not json at all
";
        let ev = parse_opencode_jsonl(sample);
        assert_eq!(ev.session_id.as_deref(), Some("ses_e20a"));
        assert_eq!(ev.agent_texts, vec!["PONG".to_string(), "done".to_string()]);
    }

    #[test]
    fn opencode_jsonl_tolerates_garbage_and_keeps_first_session_id() {
        let ev = parse_opencode_jsonl("hello\n42\n{broken\n");
        assert_eq!(ev.session_id, None);
        assert!(ev.agent_texts.is_empty());
        // A second sessionID (should not happen in one run) never overwrites
        // the first — the handle stays the session we started steering.
        let ev = parse_opencode_jsonl(
            "{\"sessionID\":\"ses_one\"}\n{\"sessionID\":\"ses_two\"}\n",
        );
        assert_eq!(ev.session_id.as_deref(), Some("ses_one"));
    }

    #[test]
    fn codex_jsonl_parses_thread_and_agent_messages_in_order() {
        let sample = "\
Reading extra prompt from stdin.
{\"type\":\"thread.started\",\"thread_id\":\"01a1-uuid\"}
{\"type\":\"turn.started\"}
{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"OK\"}}
{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"command_execution\",\"command\":\"ls\"}}
{\"type\":\"item.completed\",\"item\":{\"id\":\"item_2\",\"type\":\"agent_message\",\"text\":\"done\"}}
{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10}}
not json at all
";
        let ev = parse_codex_jsonl(sample);
        assert_eq!(ev.thread_id.as_deref(), Some("01a1-uuid"));
        assert_eq!(ev.agent_messages, vec!["OK".to_string(), "done".to_string()]);
    }

    #[test]
    fn recovered_codex_stream_keeps_the_successful_turn() {
        let ev = parse_codex_jsonl(r#"{"type":"error","message":"connection lost"}
{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}
{"type":"turn.completed"}"#);
        assert!(ev.turn_completed && ev.error.is_none());
        assert_eq!(ev.agent_messages, ["Done"]);
    }

    #[test]
    fn codex_jsonl_tolerates_garbage() {
        let ev = parse_codex_jsonl("hello\n42\n{broken\n");
        assert_eq!(ev.thread_id, None);
        assert!(ev.agent_messages.is_empty());
    }

    // Review (minor): a blank thread id is not a resume key. A lone blank must
    // leave the handle unset (resume_plan would run `codex exec resume ""`),
    // and a real id on a later line must still win the slot.
    #[test]
    fn codex_jsonl_ignores_a_blank_thread_id() {
        let only_blank = parse_codex_jsonl("{\"type\":\"thread.started\",\"thread_id\":\"\"}");
        assert_eq!(only_blank.thread_id, None, "a blank thread_id must not settle");
        let blank_then_real = parse_codex_jsonl(
            "{\"type\":\"thread.started\",\"thread_id\":\"\"}\n\
             {\"type\":\"thread.started\",\"thread_id\":\"01a1-real\"}",
        );
        assert_eq!(blank_then_real.thread_id.as_deref(), Some("01a1-real"));
    }

    // Review (minor): the is_none() guard meant the FIRST sessionID locked the
    // handle, so a leading blank permanently hid the real ses_… that follows.
    // The parser now skips blanks and keeps looking.
    #[test]
    fn opencode_jsonl_skips_a_blank_session_id_and_finds_the_real_one() {
        let only_blank = parse_opencode_jsonl("{\"type\":\"text\",\"sessionID\":\"\",\"part\":{\"type\":\"text\",\"text\":\"x\"}}");
        assert_eq!(only_blank.session_id, None, "a blank sessionID must not lock the slot");
        let blank_then_real = parse_opencode_jsonl(
            "{\"sessionID\":\"\"}\n{\"sessionID\":\"ses_real\"}\n",
        );
        assert_eq!(blank_then_real.session_id.as_deref(), Some("ses_real"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Task;
    use std::collections::BTreeMap;

    fn task(id: &str, harness: &str, args: &[&str]) -> Task {
        Task {
           skills: vec![],
            watchdog: None,
            id: id.into(),
            harness: harness.into(),
            model: None,
            in_field: "hi".into(),
            out: None,
            depends_on: vec![],
            optional: false,
            pos: None,
            workspace: None,
            cwd: None,
            timeout_secs: None,
            args: args.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            chat: None,
        }
    }

    fn g(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn count_flag(argv: &[String], group: &[&str]) -> usize {
        let grp = g(group);
        argv.iter().filter(|t| flag_in_group(t, &grp)).count()
    }

    // The bug the new read-only reviewer step exposed: `codex exec ...
    // -s workspace-write -s read-only`. Re-specifying a single-value flag must
    // REPLACE the built-in occurrence, not sit next to it.
    #[test]
    fn codex_sandbox_is_replaced_not_duplicated() {
        let c = builtin("codex").unwrap();
        let p = plan(&c, &task("review", "codex", &["-s", "read-only"]), "hi", None).unwrap();
        assert_eq!(count_flag(&p.argv, &["-s", "--sandbox"]), 1, "one -s: {:?}", p.argv);
        assert!(p.argv.iter().any(|t| t == "read-only"), "{:?}", p.argv);
        assert!(
            !p.argv.iter().any(|t| t == "workspace-write"),
            "built-in -s value must be dropped: {:?}",
            p.argv
        );
    }

    // No task override => the built-in policy is still there, exactly once.
    #[test]
    fn codex_default_sandbox_kept_once() {
        let p = plan(&builtin("codex").unwrap(), &task("r", "codex", &[]), "hi", None).unwrap();
        assert_eq!(count_flag(&p.argv, &["-s", "--sandbox"]), 1, "{:?}", p.argv);
    }

    // Contract (review minor): resume is pinned by `codex_resume_is_by_id_…`
    // but the FIRST-run argv was never asserted. This is the whole shape a
    // first codex turn must have: `exec`, model before cwd, the prompt as a
    // POSITIONAL (after -C <cwd>, before the built-in sandbox), and exactly the
    // one `-s workspace-write`. Exact equality — a reorder (e.g. the prompt
    // landing before `-C /work`, where codex would read it as the directory) is
    // a live regression the resume test cannot catch.
    #[test]
    fn codex_first_run_argv_shape() {
        let mut t = task("r", "codex", &[]);
        t.model = Some("gpt-5-codex".into());
        let p = plan(
            &builtin("codex").unwrap(),
            &t,
            "do the thing",
            Some(std::path::Path::new("/work")),
        )
        .unwrap();
        assert_eq!(
            p.argv,
            g(&["codex", "exec", "-m", "gpt-5-codex", "-C", "/work", "do the thing", "-s", "workspace-write"]),
            "{:?}",
            p.argv
        );
        assert_eq!(p.program, "codex");
        // The prompt went positionally, not via stdin or a file flag.
        assert!(p.stdin_prompt.is_none() && p.file_prompt.is_none(), "{p:?}");
    }

    // Review F02: `ZORI_*_BIN` replaced the ENTIRE argv, so codex lost `exec`
    // and opencode lost `run` — the override opened an interactive TUI instead
    // of a one-shot run (which then hung until its timeout and reported Done).
    // The env override swaps argv[0] only; the subcommand contract survives.
    #[test]
    fn env_bin_keeps_the_cli_subcommand_contract() {
        let mut c = builtin("codex").unwrap();
        apply_env_bin(&mut c, "codex", "/opt/dev/codex".into());
        assert_eq!(c.command, g(&["/opt/dev/codex", "exec"]), "{:?}", c.command);

        let mut c = builtin("opencode").unwrap();
        apply_env_bin(&mut c, "opencode", "/opt/dev/opencode".into());
        assert_eq!(c.command, g(&["/opt/dev/opencode", "run"]), "{:?}", c.command);

        // coddy has no subcommand: argv[0] alone is the whole contract.
        let mut c = builtin("coddy").unwrap();
        apply_env_bin(&mut c, "coddy", "/opt/dev/coddy".into());
        assert_eq!(c.command, g(&["/opt/dev/coddy"]), "{:?}", c.command);

        // zcode is the documented special case: the bundled CLI is a node
        // script (keep the launcher), a native binary replaces argv wholesale.
        let mut c = builtin("zcode").unwrap();
        apply_env_bin(&mut c, "zcode", "/opt/dev/zcode.cjs".into());
        assert_eq!(c.command, g(&["node", "/opt/dev/zcode.cjs"]), "{:?}", c.command);
        let mut c = builtin("zcode").unwrap();
        apply_env_bin(&mut c, "zcode", "/opt/dev/zcode".into());
        assert_eq!(c.command, g(&["/opt/dev/zcode"]), "{:?}", c.command);
    }

    // Review F02: `ZORI_CODEX_BIN=""` used to launch an empty executable and
    // fail with an opaque OS error. Refuse early, naming the variable.
    #[test]
    fn empty_env_bin_is_refused_with_the_variable_name() {
        let err = normalize_env_bin("ZORI_CODEX_BIN", "codex", "   ").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ZORI_CODEX_BIN"), "must name the variable: {msg}");
        assert!(msg.contains("empty"), "must say why: {msg}");
        // A real path survives trimming.
        assert_eq!(normalize_env_bin("ZORI_CODEX_BIN", "codex", " /x/codex ").unwrap(), "/x/codex");
    }

    // Review F02: `prompt_via: file` without a flag silently wrote a prompt
    // file nobody was told about — the child ran, saw no prompt, exited 0, and
    // the node reported Done. That combination must be a validation error.
    #[test]
    fn file_prompt_without_a_flag_is_rejected() {
        let mut c = builtin("opencode").unwrap(); // its prompt_file_flag is None
        c.prompt_via = PromptVia::File;
        let err = plan(&c, &task("t", "opencode", &[]), "hi", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("prompt_file_flag"), "must say what to set: {msg}");
        assert!(msg.contains("prompt_via"), "must name the offending mode: {msg}");
    }

    // …and once the flag is declared, the prompt goes to a file and the path
    // is handed to the CLI via that flag.
    #[test]
    fn file_prompt_is_passed_via_the_declared_flag() {
        let mut c = builtin("opencode").unwrap();
        c.prompt_via = PromptVia::File;
        c.prompt_file_flag = Some("--prompt-file".into());
        let p = plan(&c, &task("t", "opencode", &[]), "hello", None).unwrap();
        assert_eq!(p.file_prompt.as_deref(), Some("hello"));
        assert!(p.argv.iter().any(|t| t == "--prompt-file"), "{:?}", p.argv);
        assert!(p.argv.iter().any(|t| t == "__ZORI_PROMPT_FILE__"), "{:?}", p.argv);
        assert!(p.stdin_prompt.is_none(), "file mode must not also feed stdin");
    }

    // Review F02: a positional prompt beginning with '-' was passed straight
    // through, so a prompt like `-r` became a CLI flag (prompt injection into
    // argv). It must travel last, after a `--` terminator, with the task's own
    // args BEFORE the terminator so they cannot be swallowed into the prompt.
    #[test]
    fn dash_prompt_cannot_masquerade_as_a_flag() {
        let c = builtin("codex").unwrap(); // prompt_arg None => positional
        let p = plan(&c, &task("t", "coddy", &["--json"]), "-rm", None).unwrap();
        let dash = p.argv.iter().rposition(|t| t == "--").expect("terminator present");
        assert_eq!(p.argv.len(), dash + 2, "prompt is the last arg after --: {:?}", p.argv);
        assert_eq!(p.argv[dash + 1], "-rm");
        let json = p.argv.iter().position(|t| t == "--json").expect("task arg kept");
        assert!(json < dash, "task args stay before --: {:?}", p.argv);
        // A normal prompt keeps the old, terminator-free shape: positional,
        // followed by the built-in flags exactly where they always were.
        let plain = plan(&c, &task("t", "codex", &[]), "hello", None).unwrap();
        assert!(!plain.argv.iter().any(|t| t == "--"), "{:?}", plain.argv);
        assert_eq!(plain.argv, g(&["codex", "exec", "hello", "-s", "workspace-write"]), "{:?}", plain.argv);
    }

    // Review M5: the tracked executor appended track_args blindly, so on a
    // dash-first prompt (`--json` etc.) landed *behind* the F02 terminator and
    // the CLI read it as positional text — tracked capture silently died.
    // insert_track_args must splice them on the flags side of `--`.
    #[test]
    fn track_args_merge_before_the_terminator() {
        let c = builtin("codex").unwrap();
        let mut p = plan(&c, &task("t", "codex", &[]), "-rm", None).unwrap();
        let dash_before = p.argv.iter().rposition(|t| t == "--").expect("terminator present");
        insert_track_args(&mut p.argv, vec!["--json".into()]);
        let dash = p.argv.iter().rposition(|t| t == "--").expect("terminator survives");
        assert_eq!(dash, dash_before + 1, "terminator shifts right by the inserted flags");
        let json = p.argv.iter().position(|t| t == "--json").expect("flag added");
        assert!(json < dash, "track flags stay on the flags side: {:?}", p.argv);
        assert_eq!(p.argv.len(), dash_before + 3); // +flag, prompt still last
        // No terminator: plain append, same shape as before the fix.
        let mut q = plan(&c, &task("t", "codex", &[]), "hello", None).unwrap();
        insert_track_args(&mut q.argv, vec!["--json".into()]);
        assert_eq!(q.argv.last().map(String::as_str), Some("--json"));
        // Empty track list (untracked harness) changes nothing.
        let mut r = plan(&c, &task("t", "codex", &[]), "-rm", None).unwrap();
        let before = r.argv.clone();
        insert_track_args(&mut r.argv, vec![]);
        assert_eq!(r.argv, before);
    }

    // The `--sandbox=value` form counts as re-specifying the flag too.
    #[test]
    fn codex_sandbox_eq_form_replaces() {
        let p = plan(&builtin("codex").unwrap(), &task("r", "codex", &["--sandbox=read-only"]), "hi", None).unwrap();
        assert_eq!(count_flag(&p.argv, &["-s", "--sandbox"]), 1, "{:?}", p.argv);
        assert!(
            !p.argv.iter().any(|t| t == "workspace-write"),
            "{:?}",
            p.argv
        );
    }

    // A non-replaceable built-in flag is never stripped by an unrelated arg.
    #[test]
    fn coddy_keeps_nonreplaceable_extra() {
        let p = plan(&builtin("coddy").unwrap(), &task("r", "coddy", &["--whatever", "x"]), "hi", None).unwrap();
        assert!(p.argv.iter().any(|t| t == "-no-stdin"), "{:?}", p.argv);
    }

    #[test]
    fn flag_helpers() {
        let grp = g(&["-s", "--sandbox"]);
        assert!(flag_in_group("-s", &grp));
        assert!(flag_in_group("--sandbox=read-only", &grp));
        assert!(!flag_in_group("--foo", &grp));
        assert_eq!(strip_flag_group(&g(&["-m", "x", "-s", "workspace-write"]), &grp), g(&["-m", "x"]));
        assert_eq!(strip_flag_group(&g(&["--sandbox=r"]), &grp), Vec::<String>::new());
    }

    // --- provider-config discovery: the bug that made every zcode leg die ---

    use std::sync::atomic::{AtomicUsize, Ordering};
    static FIXTURE_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Build a throwaway provider tree under a fresh temp dir and return it.
    fn fixture_root(tag: &str) -> std::path::PathBuf {
        let n = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("zori-disc-{}-{}-{}", std::process::id(), tag, n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Write a `zcode-builtin.json` at `root/rel`, stamping an explicit mtime
    /// so newest-by-mtime is deterministic (no sleep-based races).
    fn plant(root: &std::path::Path, rel: &str, mtime_secs: u64) -> std::path::PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("zcode-builtin.json");
        std::fs::write(&file, b"{}").unwrap();
        let mtime = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(mtime_secs);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        file
    }

    // The layout this machine actually ships: os and arch MERGED into one dir,
    // so the file sits three levels under provider/. The old fixed 4-deep walk
    // missed this and returned None -> instant crash. Regression guard.
    #[test]
    fn discovery_finds_merged_os_arch_layout() {
        let root = fixture_root("merged");
        let want = plant(
            &root,
            "darwin-aarch64/3.18.2/endpoint-78d7c3bef4024722642626fe3669a799",
            1_000,
        );
        assert_eq!(discover_under(&root).as_deref(), Some(want.as_path()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // The other documented layout (os and arch separate) must still work —
    // the fix is layout-agnostic, not a patch tuned to one machine.
    #[test]
    fn discovery_finds_split_os_arch_layout() {
        let root = fixture_root("split");
        let want = plant(&root, "darwin/aarch64/3.18.2/hash-abc", 1_000);
        assert_eq!(discover_under(&root).as_deref(), Some(want.as_path()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // Newest by mtime wins, so a stale `0.0.0-dev` / old-version dir cannot
    // shadow the provider config ZCode is actually running.
    #[test]
    fn discovery_picks_newest_by_mtime() {
        let root = fixture_root("newest");
        plant(&root, "darwin-aarch64/0.0.0-dev/endpoint-old", 100); // stale
        let current = plant(&root, "darwin-aarch64/3.18.2/endpoint-new", 900); // newest
        plant(&root, "darwin-aarch64/3.17.2/endpoint-mid", 500); // older release
        assert_eq!(discover_under(&root).as_deref(), Some(current.as_path()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // No provider dir at all => None (so zcode_provider_env simply omits the
    // env var and the failure surfaces at the CLI, not as a bogus path).
    #[test]
    fn discovery_returns_none_when_absent() {
        let root = fixture_root("empty");
        assert_eq!(discover_under(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ---- the personal harness store (~/.zori/harnesses.yml) ----------------

    fn devin_like() -> HarnessOverride {
        HarnessOverride {
            description: Some("Devin CLI".into()),
            model_notes: Default::default(),
            command: Some(vec!["devin".into()]),
            model_flag: Some("--model".into()),
            cwd_flag: None,
            prompt_via: Some(PromptVia::Arg),
            prompt_arg: Some("-p".into()),
            extra_args: vec!["--permission-mode".into(), "accept-edits".into()],
            prompt_file_flag: Some("--prompt-file".into()),
            models_cmd: vec!["sh".into(), "-c".into(), "echo swe-2-high".into()],
            replaceable_flags: vec!["--permission-mode".into()],
            env: Default::default(),
        }
    }

    #[test]
    fn personal_file_parses_and_refuses_plumbing_ids() {
        let dir = std::env::temp_dir().join(format!("zori-harness-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("harnesses.yml");
        std::fs::write(
            &path,
            "devin:\n  command: [devin]\n  model_flag: \"--model\"\nprompt:\n  command: [evil]\n",
        )
        .unwrap();
        let map = personal_harnesses_from(&path);
        assert!(map.contains_key("devin"), "the real entry parses");
        assert!(!map.contains_key("prompt"), "zori's plumbing ids are refused");
        std::fs::write(&path, "{ not yaml").unwrap();
        assert!(personal_harnesses_from(&path).is_empty(), "malformed file is ignored, not fatal");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn personal_store_defines_a_new_harness_end_to_end() {
        let personal = BTreeMap::from([("devin".to_string(), devin_like())]);
        let c = resolve_with("devin", None, &personal).unwrap();
        assert_eq!(c.command, vec!["devin"]);
        assert!(c.supports_model && c.model_flag.as_deref() == Some("--model"));
        assert_eq!(c.prompt_arg.as_deref(), Some("-p"));
        assert_eq!(c.extra_args, vec!["--permission-mode", "accept-edits"]);
        assert!(c.is_agent);
    }

    #[test]
    fn flow_refines_personal_and_concatenates_extra_args() {
        let personal = BTreeMap::from([("devin".to_string(), devin_like())]);
        let flow = HarnessOverride {
            extra_args: vec!["--cloud".into()],
            model_flag: Some("--model".into()), // same value, wins per field
            ..Default::default()
        };
        let c = resolve_with("devin", Some(&flow), &personal).unwrap();
        // personal first, flow appended — a flow narrows, never loses the base
        assert_eq!(c.extra_args, vec!["--permission-mode", "accept-edits", "--cloud"]);
        // a flow-only id with no personal entry keeps the old contract
        let flow_only = HarnessOverride {
            command: Some(vec!["mycli".into()]),
            ..Default::default()
        };
        let c = resolve_with("mycli", Some(&flow_only), &personal).unwrap();
        assert_eq!(c.prompt_arg.as_deref(), Some("-p"), "historical -p default preserved");
        assert!(resolve_with("nope", None, &personal).is_err(), "unknown id still errors");
    }

    #[test]
    fn personal_entry_refines_a_builtin() {
        let mut ov = devin_like();
        ov.command = Some(vec!["coddy-local".into()]);
        let personal = BTreeMap::from([("coddy".to_string(), ov)]);
        let c = resolve_with("coddy", None, &personal).unwrap();
        assert_eq!(c.command, vec!["coddy-local"], "personal wins over the built-in");
        assert_eq!(c.cwd_flag.as_deref(), Some("--cwd"), "unset fields keep the built-in's");
        // and the flow still wins on top of the personal layer
        let flow = HarnessOverride { command: Some(vec!["coddy-flow".into()]), ..Default::default() };
        let c = resolve_with("coddy", Some(&flow), &personal).unwrap();
        assert_eq!(c.command, vec!["coddy-flow"]);
    }
}
