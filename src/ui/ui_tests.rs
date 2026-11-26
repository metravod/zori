use super::*;

// Review (minor): a handler panicking under the registry lock used to
// poison the shared mutex so every later `lock().unwrap()` in every
// handler died — one run's panic shut the whole console. The registry
// only holds Arc handles, so lock_recovering must enter a poisoned lock
// loudly and hand back the still-valid data.
#[test]
fn lock_recovering_enters_a_poisoned_mutex() {
    let m: Mutex<Vec<u32>> = Mutex::new(vec![1, 2]);
    // Poison it: panic while the guard is held.
    std::thread::scope(|s| {
        s.spawn(|| {
            let _g = m.lock().unwrap();
            panic!("handler exploded");
        })
        .join()
        .unwrap_err(); // the spawn thread panicked; scope unwinds here
    });
    assert!(m.is_poisoned());
    // Recovery: the guard comes back and the old contents are intact.
    {
        let g = lock_recovering(&m, "test registry");
        assert_eq!(*g, vec![1, 2]);
    }
}

// Review (minor): the Host/Origin boundary (F10) had no direct coverage —
// the rebinding/CSRF guard was reachable only through a live server. Pin
// the pure allow-set logic: the address the server bound, IPv6 bracket
// forms browsers actually send, `null` origins, and foreign hosts.
#[test]
fn allowed_hosts_accepts_the_bind_and_rejects_foreign() {
    let loopback = AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap());
    // Loopback names are always in the set, even for a LAN bind.
    assert!(loopback.allows_host("127.0.0.1:8787"), "the bound host+port");
    assert!(loopback.allows_host("localhost:8787"), "localhost always answers");
    assert!(loopback.allows_host("LOCALHOST"), "host is matched case-insensitively");
    assert!(loopback.allows_host("[::1]:9000"), "bracketed IPv6 loopback");
    assert!(!loopback.allows_host("evil.example"), "a foreign host is rejected");
    assert!(!loopback.allows_host(""), "empty host is rejected");

    // A LAN bind adds exactly its own address (any port), nothing more.
    let lan = AllowedHosts::for_bind("192.168.1.5:8787".parse().unwrap());
    assert!(lan.allows_host("192.168.1.5:1234"), "the bind answers any port");
    assert!(lan.allows_host("localhost"), "loopback still served on the same box");
    assert!(!lan.allows_host("192.168.1.6"), "a sibling host is foreign");
}

#[test]
fn allowed_origin_accepts_matching_and_rejects_foreign_and_null() {
    let hosts = AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap());
    assert!(hosts.allows_origin("http://127.0.0.1:8787"), "same-origin POST");
    assert!(hosts.allows_origin("https://localhost:8787"), "https + any scheme");
    assert!(!hosts.allows_origin("http://evil.example"), "a foreign origin is refused");
    // Sandboxed/file contexts send the literal `null` origin: never local.
    assert!(!hosts.allows_origin("null"), "null origin is foreign");
    assert!(!hosts.allows_origin(""), "empty origin is foreign");
    assert!(!hosts.allows_origin("127.0.0.1:8787"), "a bare host is not an origin");
}

#[test]
fn normalize_host_strips_port_and_brackets_and_lowercases() {
    assert_eq!(normalize_host("127.0.0.1:8787"), "127.0.0.1");
    assert_eq!(normalize_host("[::1]:9000"), "::1");
    assert_eq!(normalize_host("LOCALHOST"), "localhost");
    assert_eq!(normalize_host("  Example.COM:80  "), "example.com");
    // A bare (unbracketed) IPv6 literal has no port to strip.
    assert_eq!(normalize_host("::1"), "::1");
    // A trailing colon with no digits is not a port; leave the token intact.
    assert_eq!(normalize_host("host:"), "host:");
}

#[test]
fn manifest_run_inputs_reads_effective_inputs() {
    // Review M2: after a restart the run's inputs must come from the
    // manifest (the effective map), not from YAML defaults alone.
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("zori-ui-inputs-{}-{}", std::process::id(), uniq));
    std::fs::create_dir_all(&dir).unwrap();
    let m = crate::artifacts::Manifest::new(
        "r1",
        "f",
        None,
        &std::collections::BTreeMap::from([
            ("k".to_string(), "override".to_string()),
            ("only_boot".to_string(), "v".to_string()),
        ]),
        &[],
    );
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string(&m).unwrap(),
    )
    .unwrap();
    let mut got = manifest_run_inputs(&dir, "r1");
    got.sort();
    assert_eq!(
        got,
        vec![
            ("k".to_string(), "override".to_string()),
            ("only_boot".to_string(), "v".to_string())
        ]
    );
    // missing manifest → empty (old behaviour: YAML defaults)
    assert!(manifest_run_inputs(&dir.join("nowhere"), "r1").is_empty());
    // corrupt manifest → empty with a warning, never a panic
    std::fs::write(dir.join("manifest.json"), b"{ broken").unwrap();
    assert!(manifest_run_inputs(&dir, "r1").is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

// Review M8: the old save path checked `exists()` then wrote with a plain
// write — two concurrent saves clobbered each other. claim_flow_file must
// hand every writer a distinct file, including same-second saves.
#[tokio::test]
async fn claim_flow_file_never_clobbers() {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("zori-ui-flows-{}-{}", std::process::id(), uniq));
    // Two writers racing for the same name both succeed, on distinct paths.
    let (a, b) = tokio::join!(
        claim_flow_file(&dir, "review", "name: a\n"),
        claim_flow_file(&dir, "review", "name: b\n")
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a, b, "colliding saves must land on distinct files");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "name: a\n");
    assert_eq!(std::fs::read_to_string(&b).unwrap(), "name: b\n");
    // A third save of the same name gets yet another file, with suffix.
    let c = claim_flow_file(&dir, "review", "name: c\n").await.unwrap();
    assert_ne!(c, a);
    assert_ne!(c, b);
    assert_eq!(std::fs::read_to_string(&c).unwrap(), "name: c\n");
    assert!(c.file_name().unwrap().to_string_lossy().starts_with("review-"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn slug_sanitizes_flow_name() {
    assert_eq!(slug("Review Pipeline"), "review-pipeline");
    assert_eq!(slug("a/b c"), "a-b-c");
    assert_eq!(slug(""), "run");
}

#[test]
fn rerunnable_covers_failed_and_skipped_but_not_inflight() {
    assert!(rerunnable(TaskStatus::Done));
    assert!(rerunnable(TaskStatus::Stale));
    // The owner's case: a node died on a typo'd model — rerun must work.
    assert!(rerunnable(TaskStatus::Failed));
    assert!(rerunnable(TaskStatus::Skipped));
    assert!(!rerunnable(TaskStatus::Running));
    assert!(!rerunnable(TaskStatus::Pending));
}

#[test]
fn task_edit_accepts_leaf_fixes_but_not_topology() {
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n    depends_on: [b]\n  - id: b\n    harness: echo\n    in: seed\n");
    let existing = f.tasks.iter().find(|t| t.id == "a").unwrap();

    // Same id + deps, leaf fields corrected (the typo'd-model case): ok.
    let mut fix = existing.clone();
    fix.in_field = "hi, corrected".into();
    fix.out = Some("shorter answer".into());
    fix.model = None;
    assert!(validate_task_edit("a", existing, &fix, &f).is_ok());

    // Dropping an explicit dep = topology change: rejected.
    let mut topo = existing.clone();
    topo.depends_on = vec![];
    assert!(validate_task_edit("a", existing, &topo, &f).is_err());

    // Adding a ${steps.x.output} ref wires a new edge: rejected.
    let mut refed = existing.clone();
    refed.in_field = "take ${steps.c.output}".into();
    assert!(validate_task_edit("a", existing, &refed, &f).is_err());

    // Id must match the node being re-run.
    let mut wrong = existing.clone();
    wrong.id = "zz".into();
    assert!(validate_task_edit("a", existing, &wrong, &f).is_err());

    // An emptied prompt is fine when a dependency feeds it (edges carry
    // data: dep b lands as a `# b` section), but a node with nothing
    // feeding it must keep its text — that would be a guaranteed
    // driver failure.
    let mut empty = existing.clone();
    empty.in_field = "   ".into();
    assert!(validate_task_edit("a", existing, &empty, &f).is_ok());
    let b = f.tasks.iter().find(|t| t.id == "b").unwrap();
    let mut b_empty = b.clone();
    b_empty.in_field = "".into();
    assert!(validate_task_edit("b", b, &b_empty, &f).is_err());

    // Model on a harness that cannot set one: rejected with the reason.
    let mut modelled = existing.clone();
    modelled.model = Some("gpt-9".into());
    let e = validate_task_edit("a", existing, &modelled, &f).unwrap_err();
    assert!(e.contains("model"), "{e}");
}

#[test]
fn task_edit_resolves_custom_harnesses_from_the_flow() {
    // A flow-local harness fix (point the task at a working command) is a
    // legal correction — resolve must consult the run's own table.
    let f = spec(
        "name: x\nharnesses:\n  fine:\n    command: [/bin/echo]\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n",
    );
    let existing = f.tasks.iter().find(|t| t.id == "a").unwrap();
    let mut fix = existing.clone();
    fix.harness = "fine".into();
    assert!(validate_task_edit("a", existing, &fix, &f).is_ok(),);

    // …but an id neither builtin nor in the flow is still unknown.
    let mut ghost = existing.clone();
    ghost.harness = "nosuch".into();
    assert!(validate_task_edit("a", existing, &ghost, &f).is_err());
}

#[test]
fn run_status_summary_is_lowercase_debug() {
    assert_eq!(format!("{:?}", crate::state::RunStatus::Completed).to_lowercase(), "completed");
}

fn spec(yaml: &str) -> Flow {
    serde_yaml::from_str(yaml).expect("test spec must parse")
}

#[test]
fn validate_accepts_a_valid_flow_and_returns_levels() {
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hello\n");
    let plan = validate_spec(&f).expect("valid");
    assert_eq!(plan.levels, vec![vec!["a".to_string()]]);
}

#[test]
fn validate_rejects_unknown_harness() {
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: nosuch-cli\n    in: hi\n");
    let e = validate_spec(&f).unwrap_err();
    assert!(e.contains("nosuch-cli"), "{e}");
}

#[test]
fn validate_rejects_model_on_a_harness_without_model_support() {
    // echo never sets a model per run — the builder must say so, not boot a
    // run that dies at the first task.
    assert!(!crate::connectors::resolve("echo", None).unwrap().supports_model);
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    model: gpt-9\n    in: hi\n");
    let e = validate_spec(&f).unwrap_err();
    assert!(e.contains("model"), "{e}");
}

#[test]
fn validate_rejects_a_cycle() {
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n    depends_on: [b]\n  - id: b\n    harness: echo\n    in: hi\n    depends_on: [a]\n");
    assert!(validate_spec(&f).is_err());
}

#[test]
fn step_refs_extracts_referenced_ids() {
    assert_eq!(step_refs("see ${steps.a.output} then ${steps.b.output}"), vec!["a", "b"]);
    // dedup, and non-.output keys are ignored
    assert_eq!(step_refs("${steps.a.output}${steps.a.output}${inputs.x}"), vec!["a"]);
    assert!(step_refs("no refs here").is_empty());
}

#[test]
fn merge_ref_deps_folds_prompt_references_into_edges() {
    let mut f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: \"${steps.a.output}\"\n");
    merge_ref_deps(&mut f);
    let b = f.tasks.iter().find(|t| t.id == "b").unwrap();
    assert_eq!(b.depends_on, vec!["a".to_string()]);
}

#[test]
fn input_refs_extracts_keys() {
    assert_eq!(input_refs("${inputs.a} and ${inputs.topic}"), vec!["a", "topic"]);
    // dedup; ${steps.*} is not an input
    assert_eq!(input_refs("${inputs.a}${inputs.a}${steps.b.output}"), vec!["a"]);
    assert!(input_refs("no refs").is_empty());
}

#[test]
fn missing_inputs_names_unsupplied_keys() {
    // prompt referenced, never supplied by the flow → the lost-inputs bug.
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.prompt}\"\n");
    assert_eq!(missing_inputs(&f, &[]), vec!["prompt".to_string()]);
    // supplied by the flow's inputs block → fine
    let f = spec("name: x\ninputs:\n  prompt: topic\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.prompt}\"\n");
    assert!(missing_inputs(&f, &[]).is_empty());
    // supplied only at boot (--input) → fine
    assert!(missing_inputs(
        &spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: \"${inputs.prompt}\"\n"),
        &["prompt".to_string()]
    ).is_empty());
    // referenced in `out` too, not just `in`
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n    out: \"about ${inputs.topic}\"\n");
    assert_eq!(missing_inputs(&f, &[]), vec!["topic".to_string()]);
}

#[test]
fn upstream_closure_is_the_node_plus_all_ancestors() {
    //  a ← b ← c  and an unrelated  d . Running c pulls in b and a, never d.
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: hi\n    depends_on: [a]\n  - id: c\n    harness: echo\n    in: hi\n    depends_on: [b]\n  - id: d\n    harness: echo\n    in: hi\n");
    let c = upstream_closure(&f, "c").unwrap();
    let mut want = std::collections::HashSet::new();
    for id in ["a", "b", "c"] { want.insert(id.to_string()); }
    assert_eq!(c, want);
    // a leaf with no deps closes to itself alone
    assert_eq!(upstream_closure(&f, "a").unwrap().len(), 1);
    // a name the flow does not have is the canvas asking to run a block
    // that isn't there — a caller error, not an empty closure.
    assert!(upstream_closure(&f, "zz").is_err());
}

#[test]
fn missing_inputs_filtered_ignores_nodes_outside_the_closure() {
    // b (outside) references ${inputs.topic}; a (the focused node) is clean.
    // A focused run of `a` must boot despite b's unsupplied input — only
    // the closure it executes needs its inputs satisfied.
    let yaml = "name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: \"${inputs.topic}\"\n    depends_on: [a]\n";
    let f = spec(yaml);
    // unfiltered, the whole flow is blocked by topic…
    assert_eq!(missing_inputs(&f, &[]), vec!["topic".to_string()]);
    // …but a focused run of a only inspects a → nothing missing.
    let only = upstream_closure(&f, "a").unwrap();
    assert!(missing_inputs_filtered(&f, &[], Some(&only)).is_empty());
    // focusing b (which pulls in itself) still needs topic.
    let only_b = upstream_closure(&f, "b").unwrap();
    assert_eq!(missing_inputs_filtered(&f, &[], Some(&only_b)), vec!["topic".to_string()]);
}

#[test]
fn saved_yaml_roundtrips_and_drops_empty_fields() {
    let f = spec("name: x\ntasks:\n  - id: a\n    harness: echo\n    in: hi\n  - id: b\n    harness: echo\n    in: \"${steps.a.output}\"\n");
    let yaml = spec_to_yaml(&f).unwrap();
    // The ref-implied edge is on disk, so the file matches the validated plan.
    assert!(yaml.contains("depends_on"), "{yaml}");
    // skip_serializing_if keeps unset optionals out of the file.
    assert!(!yaml.contains("model:"), "unset model should not appear:\n{yaml}");
    // …and it parses back to the same DAG.
    let reparsed = spec(&yaml);
    assert_eq!(reparsed.tasks.len(), 2);
    assert_eq!(reparsed.tasks[1].depends_on, vec!["a".to_string()]);
}

// ---- review M3/M4: the atomic claim gate ------------------------------

/// An emitter over a chain `a → b → c` (b depends on a, c on b) with the
/// given statuses; the closure receives them in chain order.
fn gate_emitter(statuses: [TaskStatus; 3]) -> Emitter {
    let tasks: Vec<crate::state::TaskMeta> = vec![
        ("a".into(), "echo".into(), None, false, vec![]),
        ("b".into(), "echo".into(), None, false, vec!["a".into()]),
        ("c".into(), "echo".into(), None, false, vec!["b".into()]),
    ];
    let mut st = RunState::new(
        "gate-1".into(),
        "gate".into(),
        "gate.yml".into(),
        None,
        vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]],
        &tasks,
    );
    for (node, s) in st.nodes.values_mut().zip(statuses) {
        node.status = s;
    }
    Emitter::new(st)
}

#[tokio::test]
async fn corrected_model_rejoins_the_active_level_before_the_join() {
    let dir = std::env::temp_dir().join(crate::artifacts::new_run_id("retry-in-level"));
    let flows = dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    let state = Arc::new(AppState {
        runs: Mutex::default(),
        runs_root: dir.join("runs"),
        flows_root: flows.clone(),
        models_cache: Mutex::default(),
        allowed_hosts: AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap()),
    });
    let mut flow = spec(r#"
name: retry
default_timeout_secs: 8
watchdog: {retry: 0, nudges: 0, ask: false}
harnesses:
  probe:
    command: [sh, -c, 'test "$2" != bad || exit 1; while test ! -f "$2"; do sleep 0.02; done; echo result-"$2"', probe]
    model_flag: --model
    prompt_via: stdin
tasks:
  - {id: producer, harness: probe, model: bad, in: produce, optional: true}
  - {id: sibling, harness: probe, model: other, in: wait}
  - {id: join, harness: echo, in: summarize, depends_on: [producer, sibling]}
"#);
    for task in &mut flow.tasks {
        task.cwd = Some(dir.clone());
    }
    let yaml = spec_to_yaml(&flow).unwrap();
    let path = flows.join("retry.yml");
    std::fs::write(&path, &yaml).unwrap();
    let run = boot_flow(state.clone(), &yaml, Some(path.clone()), vec![], None).await.unwrap();
    let m = lookup(&state, &run).unwrap();
    let status = |id: &str| m.emitter.snapshot().nodes[id].status;
    let wait_for = |id: &'static str, wanted| async move {
        tokio::time::timeout(std::time::Duration::from_secs(6), async {
            while status(id) != wanted {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("node did not reach expected status");
    };
    wait_for("producer", TaskStatus::Failed).await;
    wait_for("sibling", TaskStatus::Running).await;
    // The failed attempt may still be writing its manifest.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while m.tracked.reserve_level_retry("producer").is_err() {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    flow.tasks[0].model = Some("fixed".into());
    let request = || Json(RerunReq {
        node: "producer".into(),
        task: Some(serde_json::to_value(&flow.tasks[0]).unwrap()),
    });
    let _ack = rerun(State(state.clone()), AxPath(run.clone()), request()).await.unwrap();
    let duplicate = rerun(State(state.clone()), AxPath(run.clone()), request()).await.unwrap_err();
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    std::fs::write(dir.join("other"), "").unwrap();
    wait_for("sibling", TaskStatus::Done).await;
    assert_eq!(status("join"), TaskStatus::Pending);
    std::fs::write(dir.join("fixed"), "").unwrap();
    wait_for("join", TaskStatus::Done).await;
    let snap = m.emitter.snapshot();
    assert!(snap.nodes["join"].output.contains("result-fixed"));
    assert!(snap.nodes["join"].output.contains("result-other"));
    assert_eq!(snap.nodes["producer"].model.as_deref(), Some("fixed"));
    assert_eq!(spec(&std::fs::read_to_string(path).unwrap()).tasks[0].model.as_deref(), Some("fixed"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn reloaded_pending_descendants_allow_retry_of_failed_root() {
    let dir = std::env::temp_dir().join(crate::artifacts::new_run_id("reload-retry"));
    let run_dir = dir.join("gate-1");
    std::fs::create_dir_all(&run_dir).unwrap();
    let flow_path = dir.join("flow.yml");
    std::fs::write(&flow_path, r#"
name: recovered
tasks:
  - {id: a, harness: echo, in: root}
  - {id: b, harness: echo, in: middle, depends_on: [a]}
  - {id: c, harness: echo, in: end, depends_on: [b]}
"#).unwrap();
    let mut snapshot = gate_emitter([TaskStatus::Failed, TaskStatus::Pending, TaskStatus::Pending]).snapshot();
    snapshot.flow_path = flow_path;
    std::fs::write(run_dir.join("state.json"), serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let state = Arc::new(AppState {
        runs: Mutex::default(),
        runs_root: dir.clone(),
        flows_root: dir.clone(),
        models_cache: Mutex::default(),
        allowed_hosts: AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap()),
    });
    let id = load_run(state.clone(), &run_dir, &dir).await.unwrap();
    let em = lookup(&state, &id).unwrap().emitter;
    assert_eq!(em.snapshot().nodes["b"].status, TaskStatus::Stale);
    assert_eq!(em.snapshot().nodes["c"].status, TaskStatus::Stale);
    claim_node(&em, "a", &[TaskStatus::Failed], NodeAction::Rerun).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn claim_gate_refuses_while_a_descendant_is_still_owed_work() {
    // M3: the owner's scenario — the first pass is mid-flight: `a`
    // finished, `b` is running, `c` is pending. Steering or re-running
    // `a` now must be refused, naming the in-flight descendant.
    let em = gate_emitter([TaskStatus::Done, TaskStatus::Running, TaskStatus::Pending]);
    let e = claim_node(&em, "a", &[TaskStatus::Done], NodeAction::Steer).unwrap_err();
    assert!(e.contains("b") && e.contains("c"), "{e}");
    assert!(e.contains("downstream"), "{e}");
    // The refusal must leave the node exactly as it was.
    assert_eq!(em.snapshot().nodes["a"].status, TaskStatus::Done);
}

#[test]
fn claim_gate_blocks_transitively_not_only_children() {
    // `b` is terminal but `c` — two levels down — is still Pending:
    // the walk is transitive, so `a` is still owed.
    let em = gate_emitter([TaskStatus::Done, TaskStatus::Done, TaskStatus::Pending]);
    let e = claim_node(&em, "a", &[TaskStatus::Done], NodeAction::Rerun).unwrap_err();
    assert!(e.contains("c"), "{e}");
}

#[test]
fn claim_gate_allows_a_settled_run_and_flips_the_claim() {
    // A fully settled run (all Done): free to steer/rerun, and the claim
    // flips the node to Running atomically.
    let em = gate_emitter([TaskStatus::Done, TaskStatus::Done, TaskStatus::Done]);
    claim_node(&em, "a", &[TaskStatus::Done], NodeAction::Rerun).unwrap();
    assert_eq!(em.snapshot().nodes["a"].status, TaskStatus::Running);
    // The second click loses the race — status is no longer in `from`.
    let e = claim_node(&em, "a", &[TaskStatus::Done], NodeAction::Rerun).unwrap_err();
    assert!(e.contains("busy"), "{e}");
}

#[test]
fn claim_gate_terminal_descendants_are_not_owed_work() {
    // Failed/Skipped descendants are terminal: a settled-but-failed run
    // must stay re-runnable (the F03 owner's case), even deep.
    let em = gate_emitter([TaskStatus::Done, TaskStatus::Skipped, TaskStatus::Failed]);
    claim_node(&em, "a", &[TaskStatus::Done], NodeAction::Steer).unwrap();
}

#[test]
fn claim_gate_rerun_requires_done_deps_first() {
    // M4: `b` is Skipped under a Failed `a`. A rerun re-renders its
    // prompt from upstream, so the claim must be refused naming `a` —
    // BEFORE any flip, so nothing gets wiped.
    let mut tasks: Vec<crate::state::TaskMeta> = vec![
        ("a".into(), "echo".into(), None, false, vec![]),
        ("b".into(), "echo".into(), None, false, vec!["a".into()]),
        ("c".into(), "echo".into(), None, false, vec!["b".into()]),
    ];
    tasks[1].4 = vec!["a".into()];
    let st = RunState::new(
        "gate-2".into(),
        "gate".into(),
        "gate.yml".into(),
        None,
        vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]],
        &tasks,
    );
    let em = Emitter::new(st);
    em.mutate(|s| {
        s.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
        s.nodes.get_mut("b").unwrap().status = TaskStatus::Skipped;
    });
    let e = claim_node(&em, "b", &[TaskStatus::Done, TaskStatus::Stale, TaskStatus::Failed, TaskStatus::Skipped], NodeAction::Rerun).unwrap_err();
    assert!(e.contains("unfinished dependencies: a"), "{e}");
    // Refused before the flip: still Skipped, nothing claimed.
    assert_eq!(em.snapshot().nodes["b"].status, TaskStatus::Skipped);
}

#[test]
fn claim_gate_optional_failed_dep_does_not_block_rerun() {
    // An optional (`best-effort`) parent that failed never gates —
    // same rule as the scheduler's `blocked()`.
    let tasks: Vec<crate::state::TaskMeta> = vec![
        ("a".into(), "echo".into(), None, true, vec![]),
        ("b".into(), "echo".into(), None, false, vec!["a".into()]),
        ("c".into(), "echo".into(), None, false, vec!["b".into()]),
    ];
    let st = RunState::new(
        "gate-3".into(),
        "gate".into(),
        "gate.yml".into(),
        None,
        vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]],
        &tasks,
    );
    let em = Emitter::new(st);
    em.mutate(|s| {
        s.nodes.get_mut("a").unwrap().status = TaskStatus::Failed;
        s.nodes.get_mut("b").unwrap().status = TaskStatus::Done;
        s.nodes.get_mut("c").unwrap().status = TaskStatus::Done;
    });
    claim_node(&em, "b", &[TaskStatus::Done], NodeAction::Rerun).unwrap();
}

#[test]
fn claim_gate_steer_ignores_upstream_but_not_downstream() {
    // Steer (require_deps=false): an ancestor turned Stale by an earlier
    // steer must not lock a finished node's transcript — but the M3
    // descendant rule still applies. Here `b` (ancestor of c, pending)
    // is Stale and `c` is Done: steering `c` is fine.
    let em = gate_emitter([TaskStatus::Stale, TaskStatus::Stale, TaskStatus::Done]);
    claim_node(&em, "c", &[TaskStatus::Done], NodeAction::Steer).unwrap();
    // …while steering `b` is refused for the same reason it always was:
    // `c` is not what's owed — nothing downstream is — but `b`'s own
    // status is Stale, outside the steer's from-set.
    let e = claim_node(&em, "b", &[TaskStatus::Done], NodeAction::Steer).unwrap_err();
    assert!(e.contains("busy"), "{e}");
}

// planner-cwd: the project folder a chat is started with must reach the
// agent process as its cwd. These pin the two pure pieces `start_chat`
// delegates to; the end-to-end cwd (meta.json of both turns) is exercised
// on a live console below.
fn chat_req(cwd: Option<&str>) -> ChatRequest {
    ChatRequest {
        graph: None, attached_run: None,
        harness: "coddy".into(),
        model: "m".into(),
        prompt: "hi".into(),
        cwd: cwd.map(str::to_string),
        skills: Some(vec![]),
    }
}

#[test]
fn resolve_project_dir_names_the_path_on_every_failure() {
    let tmp = std::env::temp_dir().join(format!(
        "zori-ui-proj-{}-{}",
        std::process::id(),
        epoch_secs()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    // A reachable folder resolves to its canonical absolute self.
    let got = resolve_project_dir(tmp.to_str().unwrap()).expect("dir resolves");
    assert!(got.is_absolute(), "{}", got.display());
    assert_eq!(got, tmp.canonicalize().unwrap());
    // A missing path names both the typed value and the resolved one.
    let e = resolve_project_dir(tmp.join("nope").to_str().unwrap()).unwrap_err();
    assert!(e.contains("unavailable") && e.contains("nope"), "{e}");
    // A regular file is not a project folder.
    let f = tmp.join("file.txt");
    std::fs::write(&f, b"x").unwrap();
    let e = resolve_project_dir(f.to_str().unwrap()).unwrap_err();
    assert!(e.contains("not a directory") && e.contains("file.txt"), "{e}");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn chat_flow_without_a_folder_keeps_the_classic_shape() {
    let y = chat_flow_yaml(&chat_req(None), &[], "OUT", None, "").unwrap();
    let f: Flow = serde_yaml::from_str(&y).unwrap();
    assert!(f.cwd.is_none(), "no cwd without a folder: {y}");
    assert!(f.tasks[0].workspace.is_none(), "task keeps its isolated work dir: {y}");
}

#[test]
fn chat_flow_with_a_folder_sets_cwd_and_workspace_flow() {
    let dir = std::env::temp_dir().join(format!(
        "zori-ui-proj2-{}-{}",
        std::process::id(),
        epoch_secs()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let y = chat_flow_yaml(&chat_req(Some(dir.to_str().unwrap())), &[], "OUT", Some(&dir), "").unwrap();
    let f: Flow = serde_yaml::from_str(&y).unwrap();
    assert_eq!(f.cwd.as_deref(), Some(dir.as_path()));
    assert_eq!(f.tasks[0].workspace, Some(crate::config::Workspace::Flow));
    // And that combination is exactly what puts the process in the folder:
    let (proc_cwd, _flag) = crate::runner::effective_cwd(&f.cwd, &f.tasks[0], None).expect("cwd");
    assert_eq!(proc_cwd, dir.canonicalize().unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

// D12 as amended: a live run's graph is editable like opening sessions by
// hand. A node added mid-run launches by itself, a pending node wired to it
// waits for it (instead of being skipped), cycles and duplicates are refused,
// removing an input turns a finished consumer Stale, and the edited graph is
// persisted beside the run, never into the saved pipeline.
#[tokio::test]
async fn live_patch_adds_wires_and_removes_nodes() {
    let dir = std::env::temp_dir().join(crate::artifacts::new_run_id("live-patch"));
    let flows = dir.join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    let state = Arc::new(AppState {
        runs: Mutex::default(),
        runs_root: dir.join("runs"),
        flows_root: flows.clone(),
        models_cache: Mutex::default(),
        allowed_hosts: AllowedHosts::for_bind("127.0.0.1:8787".parse().unwrap()),
    });
    let mut flow = spec(r#"
name: live
default_timeout_secs: 8
watchdog: {retry: 0, nudges: 0, ask: false}
harnesses:
  probe:
    command: [sh, -c, 'while test ! -f "$2"; do sleep 0.02; done; echo result-"$2"', probe]
    model_flag: --model
    prompt_via: stdin
tasks:
  - {id: producer, harness: probe, model: gate, in: produce}
  - {id: join, harness: echo, in: summarize, depends_on: [producer]}
"#);
    for task in &mut flow.tasks {
        task.cwd = Some(dir.clone());
    }
    let yaml = spec_to_yaml(&flow).unwrap();
    let path = flows.join("live.yml");
    std::fs::write(&path, &yaml).unwrap();
    let run = boot_flow(state.clone(), &yaml, Some(path.clone()), vec![], None).await.unwrap();
    let m = lookup(&state, &run).unwrap();
    let status = |id: &str| m.emitter.snapshot().nodes.get(id).map(|n| n.status);
    let wait_for = |id: &'static str, wanted| async move {
        tokio::time::timeout(std::time::Duration::from_secs(6), async {
            while status(id) != Some(wanted) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.unwrap_or_else(|_| panic!("{id} did not reach {wanted:?}"));
    };
    let patch = |v: serde_json::Value| {
        let state = state.clone();
        let run = run.clone();
        async move { patch_graph(State(state), AxPath(run), Json(serde_json::from_value(v).unwrap())).await }
    };
    wait_for("producer", TaskStatus::Running).await;

    let _ack = patch(json!({
        "add": [{"id": "extra", "harness": "echo", "in": "extra-brief", "pos": {"x": 300.0, "y": 0.0}}],
        "wire": [["extra", "join"]],
    })).await.unwrap();
    wait_for("extra", TaskStatus::Done).await;
    assert_eq!(status("join"), Some(TaskStatus::Pending), "join waits for its live-wired input");
    assert!(m.emitter.snapshot().levels.concat().contains(&"extra".to_string()));

    let dup = patch(json!({"add": [{"id": "extra", "harness": "echo", "in": "x"}]})).await.unwrap_err();
    assert_eq!(dup.0, StatusCode::CONFLICT);
    let bad = patch(json!({"add": [{"id": "ghost", "harness": "no-such-cli", "in": "x"}]})).await.unwrap_err();
    assert_eq!(bad.0, StatusCode::CONFLICT);
    let busy = patch(json!({"remove": ["producer"]})).await.unwrap_err();
    assert_eq!(busy.0, StatusCode::CONFLICT, "a working node cannot be removed");

    std::fs::write(dir.join("gate"), "").unwrap();
    wait_for("join", TaskStatus::Done).await;
    let out = m.emitter.snapshot().nodes["join"].output.clone();
    assert!(out.contains("result-gate") && out.contains("extra-brief"), "{out}");

    let cycle = patch(json!({"wire": [["join", "extra"]]})).await.unwrap_err();
    assert_eq!(cycle.0, StatusCode::CONFLICT);
    let _ack = patch(json!({"remove": ["extra"]})).await.unwrap();
    assert_eq!(status("extra"), None);
    assert_eq!(status("join"), Some(TaskStatus::Stale), "a finished consumer of a removed input is stale");
    assert_eq!(m.emitter.snapshot().nodes["join"].depends_on, vec!["producer".to_string()]);

    let snap = m.emitter.snapshot();
    assert!(snap.flow_path.starts_with(snap.out_dir.as_ref().unwrap()), "persisted beside the run");
    let saved = spec(&std::fs::read_to_string(&snap.flow_path).unwrap());
    assert_eq!(saved.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["producer", "join"]);
    assert_eq!(spec(&std::fs::read_to_string(&path).unwrap()).tasks.len(), 2, "the saved pipeline is untouched");
    std::fs::remove_dir_all(dir).unwrap();
}
