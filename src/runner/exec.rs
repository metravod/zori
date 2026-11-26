use super::*;

/// How to drive one node: a first run (build from scratch) or a resume (steer).
pub(super) enum ExecMode {
    /// Build from scratch. Carries the upstream outputs map frozen *before
    /// this node's level started* so same-level siblings running
    /// concurrently can never observe each other's output: a
    /// `${steps.x.output}` without a declared dep fails deterministically
    /// instead of resolving by scheduler luck.
    FirstRun { outputs: HashMap<String, String> },
    /// Resume an existing session with this human/system message (the
    /// watchdog's nudges and canned answers take the same path as a steer).
    Resume(SessionRef, String),
    /// One report-only turn after Coddy exhausted its step budget.
    CoddySummary { sessions_dir: PathBuf, session_id: String },
}

/// With supervision disabled only process failures count. A non-resumable
/// agent gets a fresh attempt when a nudge would otherwise be needed.
pub(super) fn attempt_verdict(facts: &Attempt, settings: Option<&Settings>, can_resume: bool) -> Verdict {
    if facts.success && settings.is_none_or(|s| !s.ask && s.nudges == 0 && s.retry == 0) {
        return Verdict::Ok;
    }
    let verdict = crate::watchdog::judge(facts, settings.is_some_and(|s| s.ask));
    match verdict {
        Verdict::Nudge(kind) if !can_resume => Verdict::Retry(format!("unfinished reply ({kind:?}), no session to resume")),
        other => other,
    }
}

pub(super) fn verdict_failure(verdict: &Verdict) -> Option<String> {
    match verdict {
        Verdict::CoddyStepLimit(n) => Some(format!("Coddy reached agent.max_turns ({n} steps)")),
        Verdict::Incomplete => Some("Coddy summary reports unfinished work".into()),
        Verdict::Nudge(kind) => Some(format!("watchdog: unfinished reply ({kind:?})")),
        Verdict::Retry(reason) => Some(reason.clone()),
        _ => None,
    }
}

/// A nonce distinguishing artifact files per attempt: a first run is `r0`,
/// its watchdog retries `r1`, `r2`, …, and every resume (steer, nudge, ask
/// answer) is the current second — so artifacts never overwrite each other.
pub(super) fn mode_nonce(mode: &ExecMode, attempt: u32) -> String {
    match mode {
        ExecMode::FirstRun { .. } => format!("r{attempt}"),
        ExecMode::Resume(..) | ExecMode::CoddySummary { .. } => format!("t{}", now_secs()),
    }
}

/// True when the session handle can carry a resume (the watchdog's nudges,
/// answers and parks all continue the same session; an `Opaque`/absent
/// session can only be re-run from scratch or left alone).
pub(super) fn resumable(session: &Option<SessionRef>) -> bool {
    matches!(
        session,
        Some(
            SessionRef::Coddy { .. }
                | SessionRef::Codex { .. }
                | SessionRef::Opencode { .. }
                | SessionRef::Echo
                | SessionRef::Transcript { .. }
        )
    )
}

/// Result of one tracked child execution.
pub(crate) struct TrackedOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub timeout: Duration,
    pub success: bool,
}
