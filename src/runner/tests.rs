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

async fn run_fixture(flow: &Flow, root: &Path) -> crate::state::RunState {
    let plan = plan(flow).expect("plan");
    let wanted = artifacts::new_run_id(&flow.name);
    let (dir, id) = artifacts::reserve_run_dir(root, &wanted).expect("reserve run dir");
    let tasks = flow.tasks.iter()
        .map(|t| (t.id.clone(), t.harness.clone(), t.model.clone(), t.optional, t.depends_on.clone()))
        .collect::<Vec<_>>();
    let emitter = Emitter::new(crate::state::RunState::new(
        id, flow.name.clone(), "test.yml".into(), Some(dir.clone()), plan.levels.clone(), &tasks,
    ));
    let tracked = TrackedRun::new(flow.clone(), plan, &[], dir.clone(), dir.join("sessions"))
        .expect("tracked run")
        .with_unattended(true);
    tracked.run_all(&emitter).await;
    emitter.snapshot()
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
    let report = run_fixture(&flow, &root).await;
    assert_eq!(report.nodes["only"].status, TaskStatus::Done);

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
    let report = run_fixture(&flow, &root).await;
    let out = &report.nodes["where"].output;
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

/// The `prompt` harness runs in-process, and the node's
/// output is its rendered text, and a dependent receives it as a
/// `# prompt` section — the starting-block pattern end to end.
#[tokio::test]
async fn prompt_block_feeds_a_child_process() {
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
    let report = run_fixture(&flow, &root).await;
    assert!(report.nodes.values().all(|n| n.status == TaskStatus::Done), "both nodes done");
    assert_eq!(report.nodes["prompt"].output, "The project needs a review");
    let fed = &report.nodes["worker"].output;
    assert!(fed.contains("You are a reviewer.\n\n# prompt\nThe project needs a review"), "worker saw the fed section: {fed:?}");
    // The prompt node recorded an attempt like any harness.
    let run_dir = report.out_dir.unwrap();
    let out = std::fs::read_to_string(run_dir.join("tasks/prompt/r0/out.txt")).unwrap();
    assert_eq!(out, "The project needs a review");
    assert_eq!(std::fs::read_to_string(run_dir.join("tasks/prompt/r0/err.txt")).unwrap(), "");
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
    let report = run_fixture(&flow, &root).await;
    assert!(report.nodes.values().all(|n| n.status == TaskStatus::Done), "both tasks done");
    let out = &report.nodes["reader"].output;
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
    let report = run_fixture(&flow, &root).await;
    let out = &report.nodes["where"].output;
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
    let a = run_fixture(&flow, &root).await;
    let b = run_fixture(&flow, &root).await;
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
        watchdog: Some(crate::config::Watchdog { retry: Some(0), ..Default::default() }),
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

    let run = tokio::time::timeout(
        Duration::from_secs(20),
        run_fixture(&flow, &test_runs_root()),
    )
    .await;

    let report = run
        .expect("run must return within its own budget");
    let r = report.nodes.get("slow").expect("slow task result present");
    assert_eq!(r.status, TaskStatus::Failed, "a timed-out task is Failed");
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

/// Postmortem bug 1: the live zori-review run showed
/// `failed` nodes with `error: null` in the manifest — nothing recorded
/// WHY an attempt failed. A non-zero exit now records the code, a timeout
/// records the reason, in the manifest and in `err.txt`.
#[tokio::test]
async fn failed_and_timed_out_attempts_record_the_reason_in_the_manifest() {
    // Non-zero exit.
    let flow = sh_flow("manifest-fail", "boom", "echo noisy >&2; exit 3");
    let report = run_fixture(&flow, &test_runs_root()).await;
    let r = &report.nodes["boom"];
    assert_eq!(r.status, TaskStatus::Failed);
    assert_eq!(r.exit_code, Some(3));
    let run_dir = report.out_dir.clone().expect("run has an out dir");
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
    let report = tokio::time::timeout(Duration::from_secs(20), run_fixture(&flow, &test_runs_root()))
        .await
        .expect("returns within budget");
    assert_eq!(report.nodes["slow"].status, TaskStatus::Failed);
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
    let report = tokio::time::timeout(Duration::from_secs(20), run_fixture(&flow, &test_runs_root()))
        .await
        .expect("killed near 2s, not held to the 300s flow default");
    assert_eq!(report.nodes["slow"].status, TaskStatus::Failed);
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

/// A completed attempt records ordered wall-clock timestamps.
#[tokio::test]
async fn completed_attempt_records_start_and_end_time() {
    let flow = sh_flow("timing", "tick", "echo hi");
    let report = run_fixture(&flow, &test_runs_root()).await;
    assert_eq!(report.nodes["tick"].status, TaskStatus::Done);
    let run_dir = report.out_dir.clone().expect("run has an out dir");
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
    let report = run_fixture(&flow, &test_runs_root()).await;
    assert!(report.nodes.values().all(|n| n.status == TaskStatus::Done), "both echo tasks succeed");
    let b = report.nodes.get("b").unwrap();
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
/// output or a hard `${steps.a.output}` error.
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
    let report = run_fixture(&flow, &test_runs_root()).await;

    let a = report.nodes.get("a").unwrap();
    assert_eq!(a.status, TaskStatus::Failed, "the boom branch fails");
    assert!(a.optional, "the branch is marked optional");

    let join = report.nodes.get("join").expect("join ran");
    assert_eq!(
        join.status,
        TaskStatus::Done,
        "the join must NOT be skipped when only an optional dep failed"
    );
    assert!(
        join.output.contains("did not complete"),
        "the join sees an honest gap for the failed optional branch, got {:?}",
        join.output
    );

    assert_eq!(report.status, crate::state::RunStatus::Completed);
}

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

/// A cascaded skip records the required dependency that blocked it.
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
    let report = run_fixture(&flow, &test_runs_root()).await;
    assert_eq!(report.nodes["a"].status, TaskStatus::Failed);
    assert_eq!(report.nodes["b"].status, TaskStatus::Skipped, "b cascades");

    let run_dir = report.out_dir.clone().expect("run has an out dir");
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
