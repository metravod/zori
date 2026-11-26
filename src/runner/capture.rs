use super::*;

/// Append-only line buffer shared between a reader and the capture loop, so a
/// killed child still leaves its partial output for the artifact files.
#[derive(Clone, Default)]
struct SharedBuf {
    text: Arc<StdMutex<String>>,
    nbytes: Arc<AtomicUsize>,
    nlines: Arc<AtomicUsize>,
}

impl SharedBuf {
    fn push_line(&self, line: &str) {
        self.nbytes.fetch_add(line.len() + 1, Ordering::Relaxed);
        self.nlines.fetch_add(1, Ordering::Relaxed);
        let mut g = self.text.lock().unwrap();
        g.push_str(line);
        g.push('\n');
    }
    fn bytes(&self) -> usize {
        self.nbytes.load(Ordering::Relaxed)
    }
    fn lines(&self) -> usize {
        self.nlines.load(Ordering::Relaxed)
    }
    fn snapshot(&self) -> String {
        self.text.lock().unwrap().clone()
    }
}

/// Read a child stream line by line, appending each line to the shared buffer.
/// Output is not echoed line-by-line (parallel tasks interleave); the per-task
/// status line summarises and the run dir keeps the full text.
async fn read_stream<S>(stream: S, buf: SharedBuf)
where
    S: tokio::io::AsyncRead + Unpin,
{
    read_stream_with(stream, buf, |_| {}).await;
}

async fn read_stream_with<S: tokio::io::AsyncRead + Unpin>(
    stream: S, buf: SharedBuf, mut on_line: impl FnMut(&str),
) {
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        buf.push_line(&line);
        on_line(&line);
    }
}

pub(super) fn first_line(s: &str) -> String {
    let line = s.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "(empty)".to_string()
    } else {
        // take() over chars: byte-slicing can split a multibyte UTF-8 char.
        let mut short: String = line.chars().take(80).collect();
        if line.chars().count() > 80 {
            short.push('…');
        }
        short
    }
}

/// Start reading output before feeding stdin. On timeout, kill the process
/// group before draining with a bounded grace period so partial output is
/// available even after a kill.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn_captured(
    program: &str,
    argv: &[String],
    prompt_via: PromptVia,
    stdin_prompt: Option<&str>,
    cwd: Option<&Path>,
    env: &[(String, String)],
    timeout: Duration,
    node_id: &str,
    emitter: &Emitter,
    mut activity: Option<Activity>,
    question_harness: Option<String>,
) -> Result<TrackedOutcome> {
    let mut cmd = Command::new(program);
    cmd.args(&argv[1..]); // argv[0] is the program
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning '{program}': {}", render_argv(argv)))?;
    let group = GroupRegistration::register(child.id());

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout handle"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr handle"))?;
    let out_buf = SharedBuf::default();
    let err_buf = SharedBuf::default();
    let observer = question_harness.map(|harness| (emitter.clone(), harness));
    let stdout = ObservedOutput(stdout, activity.as_ref().map(|a| a.stdout.clone()));
    let question_node = node_id.to_owned();
    let mut plain_text_fence = None;
    let mut stdout_task = tokio::spawn(read_stream_with(stdout, out_buf.clone(), move |line| {
        let Some((emitter, harness)) = &observer else { return };
        match harness.as_str() {
            "codex" => {
                for text in connectors::parse_codex_jsonl(line).agent_messages {
                    publish_questions(emitter, &question_node, &text, &mut None);
                }
            }
            "opencode" => {
                for text in connectors::parse_opencode_jsonl(line).agent_texts {
                    publish_questions(emitter, &question_node, &text, &mut None);
                }
            }
            _ => publish_questions(emitter, &question_node, line, &mut plain_text_fence),
        }
    }));
    let mut stderr_task = tokio::spawn(read_stream(stderr, err_buf.clone()));

    // A child that never drains a big prompt is still subject to the deadline.
    let start = Instant::now();
    let timer_start = tokio::time::Instant::now();
    let mut deadline = timer_start + timeout;
    let mut timed_out = false;

    {
        let mut si = child.stdin.take().ok_or_else(|| anyhow!("no stdin handle"))?;
        if matches!(prompt_via, PromptVia::Stdin) {
            let text = stdin_prompt.unwrap_or("");
            tokio::select! {
                biased;
                r = si.write_all(text.as_bytes()) => {
                    if let Err(e) = r {
                        if e.kind() != std::io::ErrorKind::BrokenPipe {
                            emitter.emit(RunEvent::Notice(node_id, &format!("stdin write error: {e}")));
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    emitter.emit(RunEvent::Notice(node_id, &format!("TIMED OUT feeding stdin ({}s)", timeout.as_secs())));
                    timed_out = true;
                }
            }
        }
    }

    let wait = child.wait();
    tokio::pin!(wait);
    let mut ticker = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECS));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    let push_progress = |bytes: usize, lines: usize, secs: u64| {
        emitter.mutate(|st| {
            if let Some(n) = st.nodes.get_mut(node_id) {
                n.progress.bytes = bytes;
                n.progress.lines = lines;
                n.progress.elapsed_secs = secs;
            }
        });
    };

    let mut exit_res: Option<std::io::Result<std::process::ExitStatus>> = None;
    while !timed_out {
        tokio::select! {
            biased;
            r = &mut wait => { exit_res = Some(r); break; }
            _ = tokio::time::sleep_until(deadline) => {
                let evidence = activity.as_mut().and_then(|a| a.take_recent(Instant::now(), std::time::SystemTime::now()));
                if let Some(reason) = evidence {
                    deadline = tokio::time::Instant::now() + EXTENSION;
                    let note = format!("[watchdog] {reason}. Deadline extended by 10 minutes.");
                    emitter.emit(RunEvent::Notice(node_id, &note));
                    emitter.append_turn(node_id, Role::System, note);
                } else {
                    timed_out = true;
                }
            }
            _ = ticker.tick() => {
                let secs = start.elapsed().as_secs();
                emitter.emit(RunEvent::Progress(node_id, secs, out_buf.bytes()));
                push_progress(out_buf.bytes(), out_buf.lines(), secs);
            }
        }
    }

    let timeout = deadline.duration_since(timer_start);
    if timed_out {
        emitter.emit(RunEvent::Notice(node_id, &format!("TIMED OUT after {}s — killing group", timeout.as_secs())));
        group.kill_group().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut wait).await;
    }

    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = (&mut stdout_task).await;
        let _ = (&mut stderr_task).await;
    })
    .await;
    stdout_task.abort();
    stderr_task.abort();

    push_progress(out_buf.bytes(), out_buf.lines(), start.elapsed().as_secs());

    let stdout_text = out_buf.snapshot();
    let stderr_text = err_buf.snapshot();

    let (success, exit_code) = if timed_out {
        (false, None)
    } else {
        let st = exit_res.unwrap().with_context(|| format!("waiting on '{node_id}'"))?;
        (st.success(), st.code())
    };

    Ok(TrackedOutcome {
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code,
        timed_out,
        timeout,
        success,
    })
}
