//! The on-disk shape of a run: the single owner of every path zori writes,
//! for terminal and console runs.
//!
//! Layout under the runs root (default `zori-runs/`, override: `--runs-root`
//! for `zori ui`, `ZORI_RUNS_ROOT` for `zori run`):
//!
//! ```text
//! <runs-root>/<run-id>/            run id: <flow>-<unix-millis>
//!   manifest.json                  provenance index (this module; atomic rewrite)
//!   state.json                     the ui server's live state (NOT ours)
//!   sessions/                      coddy sessions root
//!   tasks/<task>/work/             the task's isolated cwd (workspace: task)
//!   tasks/<task>/<attempt>/        one attempt: r0 = first run, t<secs> = steer
//!     prompt.txt                   the fully rendered prompt, kept forever
//!     out.txt / err.txt            captured stdout / stderr (err always)
//!     meta.json                    argv, env keys, exit, timings, checksums
//! ```
//!
//! Why attempts are directories: a steered node accumulates executions over
//! its life (`r0`, then `t<secs>` per steer/rerun). Keeping each attempt's
//! prompt/argv/output together makes the run an append-only record — the
//! substrate `fork` needs later (fork = new run seeded from ancestor
//! attempts; `Manifest::forked_from` reserves the pointer).
//!
//! `manifest.json` is deliberately lean (status, timings, paths, checksums):
//! the full argv/env detail lives in each attempt's `meta.json`, so the index
//! never drifts from the detail it points at.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Env var that overrides the runs root for `zori run` (tests, CI, servers).
pub const RUNS_ROOT_ENV: &str = "ZORI_RUNS_ROOT";

/// The runs root: `ZORI_RUNS_ROOT` when set, else `zori-runs` in the cwd.
pub fn runs_root() -> PathBuf {
    match std::env::var_os(RUNS_ROOT_ENV) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from("zori-runs"),
    }
}

/// Monotonic millisecond clock for run ids: two runs of the same flow started
/// in the same millisecond must not share a directory (the old whole-second
/// stamp visibly collided — `timeout-test-1794681390` twice).
static LAST_MILLIS: AtomicU64 = AtomicU64::new(0);

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fresh run id: `<sanitized-flow>-<unix-millis>`, monotonic within the
/// process (same-millis boots bump by one rather than collide).
pub fn new_run_id(flow_name: &str) -> String {
    format!("{}-{}", sanitize_component(flow_name), next_millis())
}

pub(crate) fn next_millis() -> u64 {
    let mut ms = now_millis();
    // compare_exchange loop: never return a millis <= one we already handed out.
    loop {
        let prev = LAST_MILLIS.load(Ordering::Relaxed);
        if ms <= prev {
            ms = prev + 1;
        }
        if LAST_MILLIS
            .compare_exchange(prev, ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    ms
}

/// Keep a path component id-safe, injectively: alphanumerics and `_` pass
/// through, every other byte of the UTF-8 encoding becomes `-XX` (lowercase
/// hex). The plain `-` mapping used to fold `a.b` and `a-b` into the same
/// directory — one task silently wrote into the other's artifacts (review
/// F03). Escaping `-` itself is what makes the map one-to-one: an output
/// `-XX` decodes to exactly one source byte.
pub fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push_str(&format!("-{b:02x}"));
        }
    }
    out
}

/// Make a path absolute for handing to child processes and prompts: canonical
/// when it exists (resolves symlinks), else just absolutised.
/// Expand a leading `~` / `~/…` to the user's home. Without this a flow with
/// `cwd: ~/repos/x` canonicalized to `<runs_root>/~/repos/x` — a path that
/// never exists, which the per-task workdir then masked (review F05).
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.as_os_str().to_string_lossy();
    if s != "~" && !s.starts_with("~/") && !s.starts_with("~\\") {
        return path.to_path_buf();
    }
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) => {
            let rest = s.trim_start_matches('~').trim_start_matches(['/', '\\']);
            let home = PathBuf::from(home);
            if rest.is_empty() { home } else { home.join(rest) }
        }
        None => path.to_path_buf(),
    }
}

pub fn absolute(path: &Path) -> PathBuf {
    let path = &expand_tilde(path);
    if path.is_absolute() {
        return std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    }
    let abs = std::env::current_dir()
        .unwrap_or_default()
        .join(path);
    std::fs::canonicalize(&abs).unwrap_or(abs)
}

/// SHA-256 of the captured output, hex-encoded — lets a fork verify a reused
/// ancestor attempt is byte-identical, and the UI cache by content.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Atomically claim a fresh run directory under `runs_root`, returning its
/// path and the id it actually landed on.
///
/// Review M9: run ids are only *process*-monotonic — two zori processes
/// booting the same flow in the same millisecond (or a crash-recovery boot
/// racing a still-running one) generated the same id, and `create_dir_all`
/// happily adopted the existing directory, so both processes wrote into one
/// run's artifacts, manifest included. `create_dir` is the claim: exactly
/// one caller gets the base name; a loser retries as `<id>-2`, `<id>-3`, …
/// (same shape as [`RunDirs::claim_attempt`]).
pub fn reserve_run_dir(runs_root: &Path, base_id: &str) -> Result<(PathBuf, String)> {
    std::fs::create_dir_all(runs_root)
        .with_context(|| format!("creating runs root {}", runs_root.display()))?;
    let mut candidate = runs_root.join(base_id);
    let mut attempt: u32 = 1;
    loop {
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                std::fs::create_dir_all(candidate.join("tasks"))
                    .with_context(|| format!("creating task dirs {}", candidate.display()))?;
                let id = candidate
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                return Ok((candidate, id));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt += 1;
                if attempt > 10_000 {
                    return Err(anyhow::anyhow!(
                        "cannot reserve a run dir for '{base_id}': {attempt} name collisions under {}",
                        runs_root.display()
                    ));
                }
                candidate = runs_root.join(format!("{base_id}-{attempt}"));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reserving run dir {}", candidate.display())))
            }
        }
    }
}

/// Paths for one run. Cheap to construct; creation is explicit so `--dry-run`
/// never touches the disk.
#[derive(Debug, Clone)]
pub struct RunDirs {
    root: PathBuf,
}

impl RunDirs {
    /// Wrap an existing run directory (e.g. reloaded by the ui server).
    pub fn open(root: PathBuf) -> Self {
        RunDirs { root }
    }

    /// The directory name this run lives under — after any M9 suffix, so a
    /// manifest stamped from this never disagrees with the folder.
    pub fn run_id(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// A task's subtree, keyed by the sanitized task id.
    pub fn task_dir(&self, task_id: &str) -> PathBuf {
        self.root.join("tasks").join(sanitize_component(task_id))
    }

    /// The task's isolated working directory (`workspace: task`).
    pub fn work_dir(&self, task_id: &str) -> PathBuf {
        self.task_dir(task_id).join("work")
    }

    /// One attempt's directory (`r0`, `t<secs>`, …).
    pub fn attempt_dir(&self, task_id: &str, nonce: &str) -> PathBuf {
        self.task_dir(task_id).join(nonce)
    }

    /// Create the task subtree and its work dir; returns the task dir.
    pub fn ensure_task(&self, task_id: &str) -> Result<PathBuf> {
        let dir = self.task_dir(task_id);
        std::fs::create_dir_all(dir.join("work"))
            .with_context(|| format!("creating task dir {}", dir.display()))?;
        Ok(dir)
    }

    /// Reserve a *new* attempt directory for `task_id`, returning the nonce it
    /// landed on. `create_dir` (not `create_dir_all`) is the claim: it fails
    /// with AlreadyExists when the nonce is taken, so two steers/reruns inside
    /// the same second — or a restart that lands on the same second — get
    /// `t<secs>`, `t<secs>-2`, … instead of the second wiping the first's
    /// prompt/out/meta and its manifest record (review F03).
    pub fn claim_attempt(&self, task_id: &str, base: &str) -> Result<(PathBuf, String)> {
        let task = self.task_dir(task_id);
        std::fs::create_dir_all(&task)
            .with_context(|| format!("creating task dir {}", task.display()))?;
        for i in 0u32.. {
            let nonce = if i == 0 { base.to_string() } else { format!("{base}-{}", i + 1) };
            match std::fs::create_dir(task.join(&nonce)) {
                Ok(()) => return Ok((task.join(&nonce), nonce)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("claiming attempt dir for task {task_id}")))
                }
            }
        }
        unreachable!("the loop only returns Err or Ok")
    }

    /// Store an artifact path relative to the run root when possible, so the
    /// manifest survives the runs root being moved.
    pub fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root).unwrap_or(path).to_path_buf()
    }
}

/// A captured output file's bookkeeping entry in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutFile {
    /// Relative to the run root.
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: usize,
}

/// One execution of one task (a first run or a steer/resume attempt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// Terminal status label: done / failed / skipped.
    pub status: String,
    /// Why the attempt never produced a normal result: the spawn error text,
    /// or which dependency blocked a skip. `None` on a normal attempt. Old
    /// manifests (no such key) load fine as `None` (review F03).
    #[serde(default)]
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    /// The process cwd this attempt actually ran in (absolute).
    pub cwd: Option<PathBuf>,
    /// Session handle, when the harness left a resumable one (tracked path).
    pub session: Option<crate::state::SessionRef>,
    pub out: Option<OutFile>,
}

/// Per-task manifest entry: identity plus its attempts, keyed by nonce.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskEntry {
    pub harness: String,
    pub model: Option<String>,
    pub optional: bool,
    pub attempts: BTreeMap<String, AttemptRecord>,
}

/// The run's provenance index. Lean on purpose: everything needed to trust,
/// list or fork a run; nothing that duplicates `meta.json` detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub run_id: String,
    pub flow_name: String,
    pub created_at: u64,
    /// Absolute path of the project the flow runs against (`flow.cwd` or the
    /// zori cwd) — what `${project.dir}` / `ZORI_PROJECT_DIR` point agents at.
    pub project_dir: Option<PathBuf>,
    /// Reserved for fork-resume: the run this one branched from.
    pub forked_from: Option<String>,
    pub inputs: BTreeMap<String, String>,
    pub levels: Vec<Vec<String>>,
    pub tasks: BTreeMap<String, TaskEntry>,
}

impl Manifest {
    /// A fresh manifest for a new run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: &str,
        flow_name: &str,
        project_dir: Option<PathBuf>,
        inputs: &BTreeMap<String, String>,
        levels: &[Vec<String>],
    ) -> Self {
        Manifest {
            schema: 1,
            run_id: run_id.to_string(),
            flow_name: flow_name.to_string(),
            created_at: now_millis() / 1000,
            project_dir,
            forked_from: None,
            inputs: inputs.clone(),
            levels: levels.to_vec(),
            tasks: BTreeMap::new(),
        }
    }
}

/// Shared, lock-guarded manifest handle: both drivers may finish attempts
/// concurrently within a level, so the read-modify-write of the whole file is
/// serialised here and every write is atomic (tmp + rename).
#[derive(Clone, Debug)]
pub struct ManifestStore {
    inner: Arc<Mutex<Manifest>>,
    path: PathBuf,
}

impl ManifestStore {
    /// Create the manifest file for a fresh run (replaces any leftover).
    pub fn init(dirs: &RunDirs, manifest: Manifest) -> Result<Self> {
        let store = ManifestStore {
            inner: Arc::new(Mutex::new(manifest)),
            path: dirs.manifest_path(),
        };
        store.flush()?;
        Ok(store)
    }

    /// Adopt an existing manifest (a reloaded run), or initialise one when the
    /// run predates manifests — old runs keep their flat artifacts untouched,
    /// new attempts just start recording.
    ///
    /// Only a *missing* file means "no manifest yet". Any other failure (a
    /// read error, unparseable JSON) is returned as an error naming the path:
    /// silently initialising here would overwrite the run's attempt history
    /// with an empty manifest (review M7).
    pub fn load_or_init(dirs: &RunDirs, manifest: Manifest) -> Result<Self> {
        let path = dirs.manifest_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let existing: Manifest = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("parsing {}: {e} — refusing to overwrite the attempt history", path.display()))?;
                Ok(ManifestStore {
                    inner: Arc::new(Mutex::new(existing)),
                    path,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::init(dirs, manifest),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }

    /// Record one attempt under its task, creating the task entry on first
    /// sight. Best-effort at the call sites: a manifest failure must never
    /// fail a run that already produced output.
    pub fn record_attempt(
        &self,
        task_id: &str,
        harness: &str,
        model: Option<String>,
        optional: bool,
        nonce: &str,
        record: AttemptRecord,
    ) {
        let mut guard = self.inner.lock().unwrap();
        let entry = guard.tasks.entry(task_id.to_string()).or_default();
        // Review (minor): refresh the summary on EVERY attempt, not just the
        // first. A rerun/steer that changed harness or model used to leave the
        // summary stamped with the original configuration forever; the entry
        // is meant to describe the task's CURRENT identity — per-attempt
        // argv/env truth lives in each attempt's own `meta.json`.
        entry.harness = harness.to_string();
        entry.model = model;
        entry.optional = optional;
        entry.attempts.insert(nonce.to_string(), record);
        if let Err(e) = self.flush_locked(&guard) {
            eprintln!("  [warning] manifest write failed: {e:#}");
        }
    }

    /// The current manifest (cloned out of the lock).
    pub fn snapshot(&self) -> Manifest {
        self.inner.lock().unwrap().clone()
    }

    fn flush(&self) -> Result<()> {
        let guard = self.inner.lock().unwrap();
        self.flush_locked(&guard)
    }

    /// Write atomically: a crash mid-write leaves the previous file intact.
    fn flush_locked(&self, manifest: &Manifest) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(manifest)
            .with_context(|| "serialising manifest")?;
        std::fs::write(&tmp, text)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming into {}", self.path.display()))?;
        Ok(())
    }
}

/// Per-attempt detail written next to the outputs: the exact argv the harness
/// saw (post `${...}` resolution, prompt-file path substituted), env keys,
/// timings and checksums. This is what a `fork` replays and a debug session
/// reads; the manifest only indexes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptMeta {
    pub harness: String,
    pub model: Option<String>,
    /// The final argv, exactly as spawned.
    pub argv: Vec<String>,
    /// Env var NAMES set for the child (connector + task + ZORI_*); values are
    /// never recorded — prompts and secrets must not leak into artifacts.
    pub env_keys: Vec<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    /// The process cwd (absolute).
    pub cwd: Option<PathBuf>,
    pub prompt_sha256: String,
}

/// Write one attempt's `meta.json` into its directory.
pub fn write_meta(attempt_dir: &Path, meta: &AttemptMeta) -> Result<()> {
    let path = attempt_dir.join("meta.json");
    let text = serde_json::to_string_pretty(meta).with_context(|| "serialising meta")?;
    // Review (minor): write atomically (tmp + rename), like the manifest.
    // A crash mid-write used to leave a truncated meta.json behind — the one
    // file holding this attempt's argv/env/exit provenance.
    let tmp = attempt_dir.join("meta.json.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// The `ZORI_*` context every child process receives, so an agent can find
/// its own scratch dir and the project without prompt plumbing. Absolutised:
/// a relative path would mean something else inside the child's cwd.
pub fn task_env(
    run_dir: &Path,
    task_dir: &Path,
    work_dir: &Path,
    project_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut v = vec![
        ("ZORI_RUN_DIR".to_string(), absolute(run_dir).display().to_string()),
        ("ZORI_TASK_DIR".to_string(), absolute(task_dir).display().to_string()),
        ("ZORI_TASK_WORK".to_string(), absolute(work_dir).display().to_string()),
    ];
    if let Some(p) = project_dir {
        v.push(("ZORI_PROJECT_DIR".to_string(), absolute(p).display().to_string()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "zori-artifacts-test-{}-{}",
            std::process::id(),
            next_millis()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn run_ids_are_distinct_within_the_same_millisecond() {
        let a = new_run_id("issue planning");
        let b = new_run_id("issue planning");
        assert_ne!(a, b, "same-millis boots must not collide");
        // injective sanitize (F03): the space hex-escapes as `-20`
        assert!(a.starts_with("issue-20planning-"), "flow name sanitized: {a}");
    }

    // Review M9: ids are only *process*-monotonic, so a second zori process
    // could mint the same id and the two would share one run dir through
    // create_dir_all. reserve_run_dir claims the name with create_dir; the
    // loser lands on a suffixed folder instead of adopting the first's.
    #[test]
    fn reserve_run_dir_claims_names_across_processes() {
        let root = temp_root();
        let (d1, id1) = reserve_run_dir(&root, "flow-1").unwrap();
        assert_eq!(id1, "flow-1");
        assert_eq!(d1, root.join("flow-1"));
        assert!(d1.join("tasks").is_dir(), "tree created with the claim");
        // Same id again (the second process): a distinct folder, suffixed.
        let (d2, id2) = reserve_run_dir(&root, "flow-1").unwrap();
        assert_eq!(id2, "flow-1-2");
        assert_ne!(d1, d2, "the second claim must not adopt the first dir");
        assert!(!d2.join("manifest.json").exists());
        assert_eq!(RunDirs::open(d2).run_id(), id2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rundirs_layout_is_per_task_with_work_and_attempts() {
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-1").unwrap().0);
        // injective sanitize (F03): ' ' → `-20`, '#' → `-23`
        let task = dirs.ensure_task("review #2").unwrap();
        assert_eq!(task, root.join("demo-1").join("tasks").join("review-20-232"));
        assert!(dirs.work_dir("review #2").is_dir(), "work dir created with the task");
        let (att, _) = dirs.claim_attempt("review #2", "r0").unwrap();
        assert_eq!(att, task.join("r0"));
        assert_eq!(
            dirs.relative(&att.join("out.txt")),
            PathBuf::from("tasks/review-20-232/r0/out.txt"),
            "manifest paths are run-root relative"
        );
    }

    #[test]
    fn manifest_records_attempts_and_survives_reload() {
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-2").unwrap().0);
        let inputs = BTreeMap::from([("prompt".to_string(), "hi".to_string())]);
        let manifest = Manifest::new("demo-2", "demo", None, &inputs, &[vec!["a".into()]]);
        let store = ManifestStore::init(&dirs, manifest).unwrap();

        store.record_attempt(
            "a",
            "coddy",
            Some("m1".into()),
            false,
            "r0",
            AttemptRecord {
                error: None,
                status: "done".into(),
                exit_code: Some(0),
                started_at: Some(1),
                ended_at: Some(2),
                cwd: None,
                session: None,
                out: Some(OutFile {
                    path: "tasks/a/r0/out.txt".into(),
                    sha256: sha256_hex(b"hello"),
                    bytes: 5,
                }),
            },
        );
        // A steer attempt must not clobber the first-run record.
        store.record_attempt(
            "a",
            "coddy",
            Some("m1".into()),
            false,
            "t42",
            AttemptRecord {
                error: None,
                status: "done".into(),
                exit_code: Some(0),
                started_at: Some(43),
                ended_at: Some(44),
                cwd: None,
                session: None,
                out: None,
            },
        );

        let reloaded = ManifestStore::load_or_init(
            &dirs,
            Manifest::new("demo-2", "demo", None, &inputs, &[]),
        )
        .unwrap();
        let snap = reloaded.snapshot();
        assert_eq!(snap.tasks["a"].attempts.len(), 2);
        assert!(snap.tasks["a"].attempts.contains_key("r0"));
        assert!(snap.tasks["a"].attempts.contains_key("t42"));
        assert_eq!(snap.tasks["a"].attempts["r0"].out.as_ref().unwrap().bytes, 5);
        assert_eq!(snap.inputs["prompt"], "hi");
    }

    #[test]
    fn record_attempt_refreshes_the_task_summary() {
        // Review (minor): the per-task summary (harness/model/optional) must
        // follow the LATEST attempt, not stay frozen on the first. A rerun
        // that switched harness or model used to leave the manifest claiming
        // the original configuration forever.
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-sum").unwrap().0);
        let inputs = BTreeMap::new();
        let manifest = Manifest::new("demo-sum", "demo", None, &inputs, &[vec!["a".into()]]);
        let store = ManifestStore::init(&dirs, manifest).unwrap();

        let rec = |status: &str| AttemptRecord {
            error: None,
            status: status.into(),
            exit_code: Some(0),
            started_at: None,
            ended_at: None,
            cwd: None,
            session: None,
            out: None,
        };
        store.record_attempt("a", "coddy", Some("m1".into()), false, "r0", rec("done"));
        // A rerun under a different harness/model/optional flips the summary.
        store.record_attempt("a", "claude", Some("opus".into()), true, "r1", rec("done"));

        let snap = store.snapshot();
        assert_eq!(snap.tasks["a"].harness, "claude", "summary tracks the newest attempt");
        assert_eq!(snap.tasks["a"].model.as_deref(), Some("opus"));
        assert!(snap.tasks["a"].optional, "optional follows the newest attempt");
        // Both attempts survive: the summary moves, the history does not.
        assert_eq!(snap.tasks["a"].attempts.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_meta_is_atomic_and_leaves_no_temp() {
        // Review (minor): meta.json used to be written with a plain fs::write;
        // a crash mid-write left a truncated provenance file. tmp + rename
        // guarantees the file is either the old bytes or the new, never torn,
        // and the temp never survives a success.
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-meta").unwrap().0);
        let (att, _) = dirs.claim_attempt("a", "r0").unwrap();
        let meta = AttemptMeta {
            harness: "coddy".into(),
            model: Some("m1".into()),
            argv: vec!["coddy".into(), "run".into()],
            env_keys: vec!["ZORI_RUN_ID".into()],
            status: "done".into(),
            exit_code: Some(0),
            started_at: Some(1),
            ended_at: Some(2),
            cwd: Some("/tmp".into()),
            prompt_sha256: sha256_hex(b"hi"),
        };
        write_meta(&att, &meta).unwrap();

        let path = att.join("meta.json");
        assert!(path.exists(), "meta.json is present");
        assert!(
            !att.join("meta.json.tmp").exists(),
            "the temp file must not survive a successful write"
        );
        // Round-trips: the bytes on disk are valid JSON matching the schema.
        let text = std::fs::read_to_string(&path).unwrap();
        let back: AttemptMeta = serde_json::from_str(&text).expect("meta.json parses");
        assert_eq!(back.harness, "coddy");
        assert_eq!(back.argv, vec!["coddy".to_string(), "run".to_string()]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn load_or_init_refuses_to_clobber_a_corrupt_manifest() {
        // Review M7: a read/parse failure must NOT fall through to init —
        // init rewrites the file with an empty `tasks`, destroying the
        // attempt history of the run being reloaded.
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-4").unwrap().0);
        std::fs::write(dirs.manifest_path(), b"{ not json").unwrap();
        let fresh = Manifest::new("demo-4", "d", None, &BTreeMap::new(), &[]);
        let err = ManifestStore::load_or_init(&dirs, fresh).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("manifest.json"), "error names the path: {msg}");
        // The original (corrupt) file is left exactly as it was.
        assert_eq!(std::fs::read_to_string(dirs.manifest_path()).unwrap(), "{ not json");
    }

    #[test]
    fn load_or_init_initialises_only_when_missing() {
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-5").unwrap().0);
        assert!(!dirs.manifest_path().exists());
        let inputs = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let store =
            ManifestStore::load_or_init(&dirs, Manifest::new("demo-5", "d", None, &inputs, &[]))
                .unwrap();
        assert_eq!(store.snapshot().inputs["k"], "v", "missing → fresh init");
    }

    #[test]
    fn manifest_write_is_atomic_no_tmp_left_behind() {
        let root = temp_root();
        let dirs = RunDirs::open(reserve_run_dir(&root, "demo-3").unwrap().0);
        let _store = ManifestStore::init(&dirs, Manifest::new("demo-3", "d", None, &BTreeMap::new(), &[])).unwrap();
        assert!(dirs.manifest_path().is_file());
        assert!(
            !dirs.manifest_path().with_extension("json.tmp").exists(),
            "tmp file renamed away"
        );
        // The flushed file parses back to the same shape.
        let text = std::fs::read_to_string(dirs.manifest_path()).unwrap();
        let m: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(m.run_id, "demo-3");
        assert_eq!(m.schema, 1);
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn task_env_points_at_absolute_paths() {
        let root = temp_root();
        let run = root.join("r");
        let task = run.join("tasks").join("a");
        std::fs::create_dir_all(task.join("work")).unwrap();
        let env = task_env(&run, &task, &task.join("work"), Some(&root));
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).unwrap().1.clone();
        assert!(get("ZORI_RUN_DIR").starts_with('/'), "absolute: {}", get("ZORI_RUN_DIR"));
        assert!(get("ZORI_TASK_WORK").ends_with("/work"));
        assert_eq!(get("ZORI_PROJECT_DIR"), root.canonicalize().unwrap().display().to_string());
    }
}
