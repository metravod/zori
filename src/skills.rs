//! The skill store: reusable prompt fragments an operator hands to a node.
//!
//! A *skill* is a small markdown file whose body is mixed into a task's
//! starting prompt as a `# Skill: <name>` section — the same injection point
//! the watchdog uses to teach an agent the `ASK:` protocol (see
//! [`crate::watchdog::preamble`]). Where the watchdog preamble is protocol
//! ("how to behave"), a skill is domain context ("how to do *this kind* of
//! task", e.g. the zori flow schema the flow planner reads).
//!
//! Skills live in three layers, searched in order so an operator's file always
//! wins over a built-in one:
//!
//! 1. **Flow-local** — every directory in `flow.skills_dirs`, plus a
//!    `zori-skills/` directory in the current working directory (the project
//!    store that travels with the checkout).
//! 2. **Global** — `~/.zori/skills/` (the shared store from which any node on
//!    any machine can pull a skill by name).
//! 3. **Built-in** — markdown compiled into the binary via `include_str!`.
//!    Ships the `zori-config` cheat-sheet; a file of the same name in a store
//!    above overrides it.
//!
//! A skill's *name* is its file stem (or the built-in id). An optional YAML
//! frontmatter block carries a one-line `description` for the picker UIs; the
//! body — the text that reaches the agent — is the file minus that block.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::config::Flow;

/// The operator's personal zori home: `~/.zori`. Machine-level state that
/// follows the operator, not any project checkout (D11): the global skill
/// store lives in `~/.zori/skills`, the model fleet in `~/.zori/fleet.json`.
pub fn personal_dir() -> PathBuf {
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) => PathBuf::from(home).join(".zori"),
        None => PathBuf::from(".zori"),
    }
}

/// The shared, machine-wide skill store: `~/.zori/skills`.
pub fn global_dir() -> PathBuf {
    personal_dir().join("skills")
}
/// The per-project skill store, looked up in the current working directory.
pub const PROJECT_SUBDIR: &str = "zori-skills";

/// The flow-planner cheat-sheet compiled into the binary. It tells the model
/// the flow schema so a chat can emit a YAML graph the server validates.
pub const BUILTIN_CONFIG: &str = include_str!("../skills/zori-config.md");

/// The planner's *judgement* skill: how to run the propose→confirm→run phases,
/// when to fan out, how to pick a harness, and the hard «never save, never
/// run» boundary. Where `zori-config` is the syntax, this is the discipline —
/// it is what turns a free-agent model that self-boots a 25-minute pipeline
/// into a planner that proposes and waits. Attached to a chat node by default
/// alongside `zori-config` (see `start_chat`).
pub const BUILTIN_PLANNER: &str = include_str!("../skills/zori-planner.md");

/// Names of the built-in skills, in the order they are listed.
pub const BUILTINS: &[&str] = &["zori-config", "zori-planner"];

/// Where a skill came from — shown so an operator can tell a store file from
/// a built-in one at a glance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillSource {
    /// Loaded from a directory (flow `skills_dirs`, `zori-skills/`, or global).
    File(PathBuf),
    /// Compiled into the binary.
    BuiltIn,
}

/// One resolved skill, ready to be rendered into a prompt or listed.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    /// One-line summary from frontmatter, for the picker UIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The markdown that reaches the agent (file minus frontmatter).
    pub body: String,
    pub source: SkillSource,
}

impl Skill {
    /// The `# Skill: <name>` block for a prompt, body trimmed of surrounding
    /// whitespace. Empty body yields a header-only block (never nothing), so a
    /// task always shows which skills were attached.
    fn section(&self) -> String {
        format!("# Skill: {}\n{}", self.name, self.body.trim())
    }
}

/// The effective skill list for a task: the flow default plus the task's own,
/// de-duplicated with order preserved (flow first). Shared by the runner (what
/// to render) and the UI validation (what must resolve), so both agree on the
/// names a boot is judged by.
pub fn effective(flow: &Flow, task: &crate::config::Task) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in flow.skills.iter().chain(task.skills.iter()) {
        if !out.iter().any(|n| n == name) {
            out.push(name.clone());
        }
    }
    out
}

/// Reads skills from the layers a given flow can see. Cheap to build (it only
/// records the directories); each `resolve` hits the filesystem, so a skill
/// edited between runs is picked up, and within a run the handful of small
/// reads are negligible.
pub struct Resolver {
    dirs: Vec<PathBuf>,
}

impl Resolver {
    /// The resolver for a flow: its `skills_dirs` (tilde/relative expanded,
    /// absolute), then the project store in the cwd, then the global store.
    /// Built-ins are consulted last by [`Resolver::resolve`], not listed here.
    pub fn for_flow(flow: &Flow) -> Resolver {
        let mut dirs: Vec<PathBuf> = flow
            .skills_dirs
            .iter()
            .map(|p| crate::artifacts::absolute(p))
            .collect();
        let project = crate::artifacts::absolute(&PathBuf::from(PROJECT_SUBDIR));
        if !dirs.contains(&project) {
            dirs.push(project);
        }
        let global = global_dir();
        if !dirs.contains(&global) {
            dirs.push(global);
        }
        Resolver { dirs }
    }

    /// A resolver over explicit directories — the test seam (a unit test must
    /// not depend on the machine's real `~/.zori/skills`).
    pub fn with_dirs(dirs: Vec<PathBuf>) -> Resolver {
        Resolver { dirs }
    }

    /// The operator's own stores — the per-project `zori-skills/` and the
    /// global `~/.zori/skills` — with no flow in hand. What `zori skills` and
    /// the console list so a name can be picked before any flow exists.
    pub fn standard() -> Resolver {
        let project = crate::artifacts::absolute(&PathBuf::from(PROJECT_SUBDIR));
        let global = global_dir();
        let mut dirs = vec![project.clone()];
        if global != project {
            dirs.push(global);
        }
        Resolver { dirs }
    }

    /// The directories searched, in priority order (exposed for `zori skills`).
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// Find a skill by name: the first directory that holds `<name>.md` wins
    /// (so a store file overrides a built-in of the same name), else a built-in
    /// of that name. `Ok(None)` when nothing matches.
    ///
    /// Review M11: an existing but *unreadable* `<name>.md` used to fall
    /// through to the next layer — up to and including a built-in of the same
    /// name, so the agent quietly received different instructions than the
    /// operator's file. Now only a missing file (`NotFound`, including the
    /// race where it disappears between lookup and read) continues the
    /// search; any other read error is returned with the path and the OS
    /// cause, the same contract as an unresolvable name in [`Self::sections`].
    pub fn resolve(&self, name: &str) -> anyhow::Result<Option<Skill>> {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            // A name is a file stem, never a path — refuse traversal outright.
            return Ok(None);
        }
        for dir in &self.dirs {
            let path = dir.join(format!("{name}.md"));
            match std::fs::read_to_string(&path) {
                Ok(text) => return Ok(Some(from_markdown(name.to_string(), &text, SkillSource::File(path)))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("reading skill '{name}' from {}", path.display())))
                }
            }
        }
        Ok(builtin_skill(name))
    }

    /// Render the `# Skill:` block for an ordered list of names. Returns `Err`
    /// naming the first name no layer can resolve — an unresolvable skill is a
    /// config error, surfaced like an unknown `${...}` reference rather than a
    /// node that dies mid-run with a mystery.
    pub fn sections(&self, names: &[String]) -> anyhow::Result<Option<String>> {
        let mut blocks: Vec<String> = Vec::new();
        for name in names {
            match self.resolve(name)? {
                Some(s) => blocks.push(s.section()),
                None => anyhow::bail!("unknown skill '{name}' (not in skills_dirs, {PROJECT_SUBDIR}/, {} or built-ins)", global_dir().display()),
            }
        }
        if blocks.is_empty() {
            Ok(None)
        } else {
            Ok(Some(blocks.join("\n\n")))
        }
    }

    /// Every skill visible to this resolver, de-duplicated by name (the
    /// highest-priority source wins), including the built-ins, sorted by name.
    /// Feeds `zori skills` and `GET /api/skills`.
    pub fn list(&self) -> Vec<Skill> {
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
        // Built-ins first (lowest priority), then layers override them.
        for name in BUILTINS {
            if let Some(s) = builtin_skill(name) {
                by_name.insert(s.name.clone(), s);
            }
        }
        // Walk dirs highest-priority-last so a higher layer overwrites: iterate
        // in reverse so the first-listed (highest-priority) dir lands last.
        for dir in self.dirs.iter().rev() {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) if !s.is_empty() && !s.starts_with('.') => s.to_string(),
                    _ => continue,
                };
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let skill = from_markdown(stem.clone(), &text, SkillSource::File(path));
                    by_name.insert(stem, skill);
                }
            }
        }
        by_name.into_values().collect()
    }
}

/// A built-in skill by name, or `None`. Kept in one match so the list and the
/// resolver agree on the set; add a `include_str!` arm here for each new one.
fn builtin_skill(name: &str) -> Option<Skill> {
    match name {
        "zori-config" => Some(from_markdown(
            "zori-config".to_string(),
            BUILTIN_CONFIG,
            SkillSource::BuiltIn,
        )),
        "zori-planner" => Some(from_markdown(
            "zori-planner".to_string(),
            BUILTIN_PLANNER,
            SkillSource::BuiltIn,
        )),
        _ => None,
    }
}

/// Split optional YAML frontmatter from a skill's markdown body. A leading
/// `---\n…\n---` block may carry `description:`; malformed frontmatter is not
/// fatal (we serve the file with no description) — a skill should never fail a
/// run because a comment in its header is a typo.
fn from_markdown(name: String, text: &str, source: SkillSource) -> Skill {
    let mut description: Option<String> = None;
    let mut body = text;
    if let Some(rest) = strip_frontmatter(text) {
        let (fm, after) = rest;
        if let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(fm) {
            if let Some(d) = map.get(serde_yaml::Value::String("description".into())) {
                if let Some(s) = d.as_str() {
                    let s = s.trim();
                    if !s.is_empty() {
                        description = Some(s.to_string());
                    }
                }
            }
        }
        body = after;
    }
    Skill { name, description, body: body.trim().to_string(), source }
}

/// If `text` opens with a `---` fence, return `(frontmatter, body_after)`.
/// Returns `None` when there is no opening line, no closing line, or the
/// opening is not the very first line (so a document that merely starts with a
/// horizontal rule is not mistaken for frontmatter).
fn strip_frontmatter(text: &str) -> Option<(&str, &str)> {
    // Normalise a leading UTF-8 BOM so a saved-with-BOM file still parses.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?.strip_suffix('\n').unwrap_or(text.lines().next()?);
    if first.trim_end() != "---" {
        return None;
    }
    let mut consumed = first.len() + 1; // bytes of the opening line incl. \n
    for line in lines {
        if line.strip_suffix('\n').unwrap_or(line).trim_end() == "---" {
            let fm_end = consumed;
            let body_start = consumed + line.len();
            return Some((&text[first.len()..fm_end], &text[body_start.min(text.len())..]));
        }
        consumed += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Task;

    fn task_with(skills: &[&str]) -> Task {
        Task {
            id: "t".into(),
            harness: "echo".into(),
            model: None,
            in_field: "hi".into(),
            out: None,
            depends_on: vec![],
            optional: false,
            cwd: None,
            workspace: None,
            timeout_secs: None,
            args: vec![],
            env: BTreeMap::new(),
            pos: None,
            watchdog: None,
            chat: None,
            skills: skills.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn flow_with(skills: &[&str]) -> Flow {
        Flow {
            name: "f".into(),
            cwd: None,
            default_timeout_secs: None,
            max_parallel: None,
            inputs: BTreeMap::new(),
            harnesses: BTreeMap::new(),
            watchdog: None,
            tasks: vec![task_with(&[])],
            skills: skills.iter().map(|s| s.to_string()).collect(),
            skills_dirs: vec![],
        }
    }

    #[test]
    fn effective_merges_flow_and_task_and_dedups() {
        let flow = flow_with(&["a", "shared"]);
        let t = task_with(&["shared", "b"]);
        assert_eq!(effective(&flow, &t), vec!["a", "shared", "b"]);
    }

    #[test]
    fn frontmatter_description_parsed_and_body_excludes_it() {
        let md = "---\ndescription: does a thing\n---\n# Body\ncontent\n";
        let s = from_markdown("x".into(), md, SkillSource::BuiltIn);
        assert_eq!(s.description.as_deref(), Some("does a thing"));
        assert_eq!(s.body, "# Body\ncontent");
    }

    #[test]
    fn no_frontmatter_keeps_whole_body() {
        let s = from_markdown("x".into(), "# just markdown\n", SkillSource::BuiltIn);
        assert_eq!(s.description, None);
        assert_eq!(s.body, "# just markdown");
    }

    #[test]
    fn malformed_frontmatter_is_not_fatal() {
        let md = "---\n: : not yaml [[\n---\nbody\n";
        let s = from_markdown("x".into(), md, SkillSource::BuiltIn);
        assert_eq!(s.description, None);
        assert_eq!(s.body, "body");
    }

    #[test]
    fn builtin_config_resolves_with_known_content() {
        let r = Resolver::with_dirs(vec![]);
        let s = r.resolve("zori-config").unwrap().expect("built-in resolves");
        assert!(matches!(s.source, SkillSource::BuiltIn));
        // A token from the cheat-sheet, so the include_str! points at the file.
        assert!(s.body.contains("depends_on"), "built-in carries the schema");
    }

    #[test]
    fn store_file_overrides_builtin_of_same_name() {
        let dir = tempdir("override");
        std::fs::write(dir.join("zori-config.md"), "MINE\n").unwrap();
        let r = Resolver::with_dirs(vec![dir]);
        let s = r.resolve("zori-config").unwrap().unwrap();
        assert!(matches!(s.source, SkillSource::File(_)));
        assert_eq!(s.body, "MINE");
    }

    #[test]
    fn higher_dir_wins_over_lower() {
        let hi = tempdir("hi");
        let lo = tempdir("lo");
        std::fs::write(hi.join("s.md"), "HIGH\n").unwrap();
        std::fs::write(lo.join("s.md"), "LOW\n").unwrap();
        let r = Resolver::with_dirs(vec![hi.clone(), lo.clone()]);
        assert_eq!(r.resolve("s").unwrap().unwrap().body, "HIGH");
    }

    // Review M11: an existing-but-unreadable `<name>.md` used to fall through
    // every layer up to a built-in of the same name, so the agent silently got
    // different instructions than the operator's file. The read failure must
    // surface with the path instead of being swallowed. A directory stands in
    // for "exists but unreadable" — root cannot read it as text either, so the
    // test is not permission-sensitive.
    #[test]
    fn unreadable_skill_is_an_error_not_a_builtin_fallback() {
        let dir = tempdir("unreadable");
        // A *directory* named zori-config.md: read_to_string fails EISDIR,
        // and the built-in of that name is right behind it in the search.
        std::fs::create_dir(dir.join("zori-config.md")).unwrap();
        let r = Resolver::with_dirs(vec![dir.clone()]);
        let e = r.resolve("zori-config").unwrap_err().to_string();
        assert!(e.contains("reading skill 'zori-config'"), "{e}");
        assert!(e.contains("zori-config.md"), "error names the path: {e}");
        // sections() propagates it too — the node must not run on the built-in.
        let e2 = r.sections(&["zori-config".into()]).unwrap_err().to_string();
        assert!(e2.contains("reading skill 'zori-config'"), "{e2}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_skill_names_in_the_error() {
        let r = Resolver::with_dirs(vec![]);
        let e = r.sections(&["nope".into()]).unwrap_err().to_string();
        assert!(e.contains("unknown skill 'nope'"), "{e}");
    }

    #[test]
    fn sections_render_headers_in_order() {
        let dir = tempdir("order");
        std::fs::write(dir.join("a.md"), "AA\n").unwrap();
        std::fs::write(dir.join("b.md"), "BB\n").unwrap();
        let r = Resolver::with_dirs(vec![dir]);
        let s = r.sections(&["a".into(), "b".into()]).unwrap().unwrap();
        let a = s.find("# Skill: a").unwrap();
        let b = s.find("# Skill: b").unwrap();
        assert!(a < b, "a before b");
        assert!(s.contains("AA") && s.contains("BB"));
    }

    #[test]
    fn empty_names_yield_no_section() {
        let r = Resolver::with_dirs(vec![]);
        assert!(r.sections(&[]).unwrap().is_none());
    }

    #[test]
    fn path_traversal_names_are_refused() {
        let r = Resolver::with_dirs(vec![]);
        assert!(r.resolve("../escape").unwrap().is_none());
        assert!(r.resolve("a/b").unwrap().is_none());
        assert!(r.resolve("").unwrap().is_none());
    }

    #[test]
    fn list_merges_dirs_and_builtins_by_name() {
        let dir = tempdir("listing");
        std::fs::write(dir.join("custom.md"), "C\n").unwrap();
        std::fs::write(dir.join("notmd.txt"), "x\n").unwrap();
        std::fs::write(dir.join(".hidden.md"), "x\n").unwrap();
        let r = Resolver::with_dirs(vec![dir]);
        let names: Vec<String> = r.list().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"custom".to_string()));
        assert!(names.contains(&"zori-config".to_string()), "built-in listed");
        assert!(!names.iter().any(|n| n == "notmd" || n == "hidden"));
        // Sorted by name.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    /// A fresh, uniquely-named temp dir under the system tmp (no deps crate).
    fn tempdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("zori-skills-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
