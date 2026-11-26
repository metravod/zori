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
    dry_run(&flow, &plan, &[], true).expect("dry_run survives path refs");
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
async fn text_cli_waits_for_operator_then_releases_review_and_recovers_old_turn() {
    let dir = std::env::temp_dir().join(artifacts::new_run_id("transcript-review"));
    let flow: Flow = serde_yaml::from_str(r#"
name: transcript-review
harnesses:
  devin:
    transcript_chat: true
    command:
      - sh
      - -c
      - |
        # Like the real Devin CLI: without `-p` it starts an interactive
        # session and never prints a reply, so a follow-up must keep `-p`.
        test "$1" = -p || exit 6
        shift
        case "$1" in
          --prompt-file)
            grep -q 'original brief' "$2" && grep -q 'How would you like me to proceed?' "$2" &&
              grep -q 'use review.diff' "$2" || exit 8
            test "$3" = '--permission-mode' && test "$4" = auto || exit 9
            if grep -q 'second answer' "$2"; then
              echo 'review complete'
            elif grep -q 'respond after reload' "$2"; then
              exit 10
            else
              echo 'ASK: Which module?'
            fi
            ;;
          *)
            case "$1" in *'ASK_INLINE:'*) ;; *) exit 7 ;; esac
            echo 'warning: rejected a tool call that requires confirmation.' >&2
            echo 'How would you like me to proceed?'
            echo '- Provide a local diff.'
            ;;
        esac
      - devin
    prompt_arg: -p
    prompt_file_flag: --prompt-file
    prompt_file_args: [-p]
tasks:
  - {id: swe, harness: devin, in: original brief, args: [--permission-mode, auto]}
  - {id: join, harness: echo, in: compare, depends_on: [swe]}
"#).unwrap();
    let (tr, em) = supervised_fixture(&dir, flow).await;
    let worker = tokio::spawn({
        let tr = tr.clone();
        let em = em.clone();
        async move { tr.run_all(&em).await }
    });
    for (i, answer) in ["use review.diff", "second answer"].iter().enumerate() {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let s = em.snapshot();
                if s.nodes["swe"].status == TaskStatus::Waiting && s.nodes["swe"].questions.len() == i + 1 {
                    assert_eq!(s.nodes["join"].status, TaskStatus::Pending);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        tr.answer_question(&em, "swe", Some(i + 1), answer, None).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), worker).await.unwrap().unwrap();
    let mut state = em.snapshot();
    assert!(state.nodes["join"].output.contains("review complete"));
    assert!(state.nodes["swe"].questions.iter().all(|q| q.closed && q.answer.is_some()));
    // Older saved runs have the same artifacts, but no session handle.
    state.nodes.get_mut("swe").unwrap().session = None;
    tr.restore_transcript_chats(&mut state);
    let reloaded = Emitter::new(state);
    tr.steer(&reloaded, "swe", "respond after reload", None).await.unwrap();
    assert_eq!(reloaded.snapshot().nodes["swe"].status, TaskStatus::Done);
    assert_eq!(reloaded.snapshot().nodes["join"].status, TaskStatus::Stale);
    std::fs::remove_dir_all(dir).unwrap();
}

/// `prompt_via: file` substitutes the prompt-file path into argv, but the
/// runs root is often relative (`zori-runs`) while the child's cwd is the
/// task work dir — a relative path makes the CLI exit before it reads the
/// prompt (a Devin node's watchdog restart died on `os error 2`). The
/// substituted path must be absolute.
#[tokio::test]
async fn file_prompt_path_is_absolute_under_a_relative_run_dir() {
    let dir = std::env::temp_dir().join(artifacts::new_run_id("file-prompt-rel"));
    std::fs::create_dir_all(&dir).unwrap();
    // A relative path to the same dir, resolved against the process cwd —
    // mirrors `zori-runs` without chdir (tests share the process cwd).
    let cwd = std::env::current_dir().unwrap();
    let mut rel = PathBuf::new();
    for _ in cwd.ancestors().skip(1) {
        rel.push("..");
    }
    rel.extend(dir.strip_prefix(Path::new("/")).unwrap().components());
    assert!(!rel.is_absolute(), "fixture must be relative: {}", rel.display());
    let flow: Flow = serde_yaml::from_str(r#"
name: file-prompt-rel
harnesses:
  filecli:
    prompt_via: file
    prompt_file_flag: --prompt-file
    command: [sh, -c, 'cat "$1"']
tasks:
  - {id: t, harness: filecli, in: hello-prompt-marker}
"#).unwrap();
    let plan = plan(&flow).unwrap();
    let tr = TrackedRun::new(flow, plan, &[], rel.join("run"), dir.join("sessions")).unwrap();
    let state = RunState::new(
        "sup-1".into(),
        "file-prompt-rel".into(),
        "sup.yml".into(),
        Some(dir.join("run")),
        vec![vec!["t".to_string()]],
        &[("t".to_string(), "filecli".to_string(), None, false, vec![])],
    );
    let em = Emitter::new(state);
    tr.run_all(&em).await;
    let state = em.snapshot();
    assert_eq!(state.nodes["t"].status, TaskStatus::Done, "{:?}", state.nodes["t"].stderr);
    assert!(state.nodes["t"].output.contains("hello-prompt-marker"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn watchdog_extends_quiet_coddy_without_restarting_its_review() {
    let dir = std::env::temp_dir().join(artifacts::new_run_id("coddy-extension"));
    let mut flow: Flow = serde_yaml::from_str(r#"
name: extension
default_timeout_secs: 1
watchdog: {retry: 0}
tasks:
  - {id: review, harness: coddy, in: review}
  - {id: join, harness: echo, in: summary, depends_on: [review]}
"#).unwrap();
    // Real Coddy print sessions update this header after tool turns.
    // Keep stdout entirely silent until after the configured deadline.
    let script = r#"
while [ "$#" -gt 0 ]; do
  case "$1" in
    -session-id) sid=$2; shift ;;
    -sessions-dir) root=$2; shift ;;
  esac
  shift
done
mkdir -p "$root/$sid"
printf '{"messageCount":3,"messagesModTime":%s000000000}' "$(date +%s)" > "$root/$sid/session.json"
sleep 2
echo 'final review'
"#;
    flow.harnesses.insert("coddy".into(), crate::config::HarnessOverride {
        command: Some(vec!["sh".into(), "-c".into(), script.into(), "coddy".into()]),
        ..Default::default()
    });
    let (tr, em) = supervised_fixture(&dir, flow).await;
    tr.run_all(&em).await;
    let state = em.snapshot();
    assert_eq!(state.nodes["review"].status, TaskStatus::Done);
    assert!(state.nodes["join"].output.contains("final review"));
    assert!(state.nodes["review"].turns.iter().any(|t|
        t.role == Role::System && t.text.contains("Coddy session advanced")));
    let manifest: artifacts::Manifest = serde_json::from_slice(
        &std::fs::read(dir.join("run/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest.tasks["review"].attempts.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn watchdog_extends_partial_stdout_but_keeps_idle_and_strict_deadlines() {
    let dir = std::env::temp_dir().join(artifacts::new_run_id("output-extension"));
    let flow: Flow = serde_yaml::from_str(r#"
name: output-extension
default_timeout_secs: 1
watchdog: {retry: 0}
harnesses:
  writer:
    command: [sh, -c, 'printf partial; sleep 2; echo " result"']
  stalled:
    command: [sh, -c, 'echo diagnostic >&2; sleep 30']
tasks:
  - {id: streaming, harness: writer, in: go}
  - {id: strict, harness: writer, in: go, watchdog: {extend_on_progress: false}}
  - {id: idle, harness: stalled, in: go}
"#).unwrap();
    let (tr, em) = supervised_fixture(&dir, flow).await;
    tokio::time::timeout(Duration::from_secs(12), tr.run_all(&em)).await.unwrap();
    let state = em.snapshot();
    assert_eq!(state.nodes["streaming"].output.trim(), "partial result");
    assert_eq!(state.nodes["streaming"].status, TaskStatus::Done);
    for id in ["strict", "idle"] {
        assert_eq!(state.nodes[id].status, TaskStatus::Failed);
        assert!(state.nodes[id].stderr.starts_with("TIMED OUT after 1s"));
    }
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

/// A timeout leaves a `TIMED OUT` line on the node and in `err.txt`,
/// with the reason in the manifest's `error` field.
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
