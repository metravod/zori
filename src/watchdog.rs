//! The supervisor: inspects every finished attempt and decides what zori does
//! next — nudge the same session, re-run from scratch, park a question for the
//! operator (or answer it canned in unattended mode), or accept the result.
//!
//! Born of the first real overnight `zori-review` (2026-11-17): one node
//! looped «Продолжаю…» until its context died and ended in a
//! `<response clipped>` marker with exit 0, two others died on a timeout kill
//! and a crash — and the only recourse was a human in the morning. The
//! watchdog turns each of those into an automatic next step, bounded by
//! budgets so it can never burn the night in a loop of its own.
//!
//! The pieces:
//!   * [`Settings::resolve`] — flow block ⊕ task block ⊕ defaults.
//!   * [`preamble`] — the protocol section mixed into agent prompts (the
//!     `ASK:` convention, or its "nobody will answer" unattended variant).
//!   * [`judge`] — a pure verdict over one attempt's facts.
//!   * The action loop itself lives in `runner::TrackedRun::exec` (it needs
//!     sessions, mailboxes and artifacts); everything decidable without them
//!     is here so it is unit-testable.

use crate::config::{Flow, Task, Watchdog};

/// Marker a harness prints when it cut the reply short (budget/context
/// exhaustion). The incident output literally contained `<response clipped>`.
const CLIPPED_MARKER: &str = "<response clipped";

/// Line-start marker of a question to the operator, per the mixed protocol.
const ASK_PREFIX: &str = "ASK:";

/// Fully-resolved supervision settings: every `Option` folded to its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Overnight mode: "don't ask" preamble + canned auto-answer.
    pub unattended: bool,
    /// Teach the agent the `ASK:` convention (and honour it).
    pub ask: bool,
    /// Auto-continue budget for unfinished replies (clipped/empty).
    pub nudges: u32,
    /// From-scratch retry budget for failed attempts.
    pub retry: u32,
    /// How long a parked question waits for the operator (0 = forever).
    pub ask_timeout_secs: u64,
}

impl Settings {
    /// What applies when no `watchdog:` block exists anywhere: the
    /// supervisor is ON with bounded budgets (the owner's call — the
    /// automation should work without ceremony; `nudges: 0, retry: 0,
    /// ask: false` switches it off).
    pub fn defaults() -> Self {
        Settings {
            unattended: false,
            ask: true,
            nudges: 1,
            retry: 1,
            ask_timeout_secs: 0,
        }
    }

    /// Flow-level block first, then task-level overrides on top (each `Some`
    /// wins; the last writer is the task). Absent blocks → defaults.
    pub fn resolve(flow: Option<&Watchdog>, task: Option<&Watchdog>) -> Self {
        let mut s = Settings::defaults();
        let mut apply = |w: &Watchdog| {
            if let Some(v) = w.unattended {
                s.unattended = v;
            }
            if let Some(v) = w.ask {
                s.ask = v;
            }
            if let Some(v) = w.nudges {
                s.nudges = v;
            }
            if let Some(v) = w.retry {
                s.retry = v;
            }
            if let Some(v) = w.ask_timeout_secs {
                s.ask_timeout_secs = v;
            }
        };
        if let Some(w) = flow {
            apply(w);
        }
        if let Some(w) = task {
            apply(w);
        }
        s
    }

    /// Settings for a task inside a flow (the common call site).
    pub fn for_task(flow: &Flow, task: &Task) -> Self {
        Self::resolve(flow.watchdog.as_ref(), task.watchdog.as_ref())
    }
}

/// Why a reply deserves a nudge: the same session continues with
/// [`nudge_text`] as the next message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeKind {
    /// The reply carries a clip marker — the model never got to finish.
    Clipped,
    /// Exit 0 but the reply is empty — nothing flowed downstream.
    Empty,
    /// The entire reply consists of a progress placeholder.
    Progress,
}

/// What the supervisor concludes about one finished attempt. Pure facts →
/// nature of the problem; budgets and session capabilities are applied by the
/// loop, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing suspicious — accept the attempt.
    Ok,
    /// The reply looks unfinished; continue the SAME session.
    Nudge(NudgeKind),
    /// The agent asked the operator something (the payload after `ASK:`).
    Ask(String),
    /// The attempt failed in a way worth one more from-scratch try
    /// (timeout, non-zero exit). Deterministic driver errors never reach
    /// `judge` — they error out before an attempt exists.
    Retry(String),
}

/// Facts of one finished attempt, as the driver knows them.
#[derive(Debug, Clone)]
pub struct Attempt<'a> {
    /// The reply that would flow downstream (parsed agent text or stdout).
    pub reply: &'a str,
    /// Child exited 0.
    pub success: bool,
    /// The deadline killed the child (exit code is a signal, or none).
    pub timed_out: bool,
    pub exit_code: Option<i32>,
}

/// Judge one attempt. Failure outranks everything (a question in the output
/// of a crashed process is not a question); the ask protocol outranks
/// finish-suspicion (a clipped reply asking something still gets its answer
/// first — the nudge may come after, on the reply to the answer).
pub fn judge(a: &Attempt, ask_protocol: bool) -> Verdict {
    if !a.success {
        let reason = if a.timed_out {
            "timed out".to_string()
        } else {
            match a.exit_code {
                Some(c) => format!("exit {c}"),
                None => "no exit status".to_string(),
            }
        };
        return Verdict::Retry(reason);
    }
    if ask_protocol {
        if let Some(q) = ask_of(a.reply) {
            return Verdict::Ask(q);
        }
    }
    // The harness can append a preview-file reference after its marker.
    if a.reply.lines().any(|line| line.trim_start().starts_with(CLIPPED_MARKER))
        || a.reply.trim_end().ends_with("<response clipped>")
    {
        return Verdict::Nudge(NudgeKind::Clipped);
    }
    if a.reply.trim().is_empty() {
        return Verdict::Nudge(NudgeKind::Empty);
    }
    if a.reply.lines().filter(|l| !l.trim().is_empty()).all(|l| {
        matches!(l.trim().trim_end_matches(['.', '…']).to_lowercase().as_str(),
                 "продолжаю" | "continuing" | "working on it")
    }) {
        return Verdict::Nudge(NudgeKind::Progress);
    }
    // Only standalone diagnostics. Reports discussing an earlier outage
    // or quoting an error alongside a deliverable must pass through.
    let diagnostic = a.reply.trim().trim_end_matches('.').to_ascii_lowercase();
    if matches!(diagnostic.as_str(), "connection lost" | "connection reset by peer"
        | "stream disconnected before completion" | "rate limit exceeded"
        | "usage limit exceeded" | "insufficient_quota" | "429 too many requests") {
        return Verdict::Retry(a.reply.trim().to_string());
    }
    Verdict::Ok
}

/// The first standalone blocking or inline question outside a fenced example.
pub fn ask_of(reply: &str) -> Option<String> {
    let mut fence = None;
    reply.lines().find_map(|line| protocol_line(line, &mut fence).map(str::to_owned))
}

/// A marker inside an example code block is part of the answer.
pub fn protocol_line<'a>(line: &'a str, fence: &mut Option<char>) -> Option<&'a str> {
    let line = line.trim();
    if line.starts_with("```") || line.starts_with("~~~") {
        let marker = line.chars().next();
        if fence.is_none() { *fence = marker; } else if *fence == marker { *fence = None; }
        return None;
    }
    if fence.is_some() { return None; }
    let payload = line.strip_prefix(ASK_PREFIX).or_else(|| line.strip_prefix("ASK_INLINE:"))?;
    (!payload.trim().is_empty()).then_some(line)
}

/// One protocol line. Plain ASK questions from older flows remain valid.
pub fn question_line(line: &str) -> Option<crate::state::OperatorQuestion> {
    #[derive(serde::Deserialize)]
    struct Payload {
        question: String,
        #[serde(default)]
        options: Vec<String>,
    }
    let line = line.trim();
    let (payload, inline) = if let Some(q) = line.strip_prefix("ASK_INLINE:") {
        (q.trim(), true)
    } else {
        (line.strip_prefix(ASK_PREFIX)?.trim(), false)
    };
    let (text, options) = match serde_json::from_str::<Payload>(payload) {
        Ok(p) => (p.question, p.options),
        Err(_) => (payload.to_owned(), Vec::new()),
    };
    if text.trim().is_empty() { return None; }
    Some(crate::state::OperatorQuestion { id: 0, text, options, inline, answer: None, closed: false })
}

/// The message a nudge continues the session with. Deliberately orders the
/// model to STOP working and produce the final answer: the incident pattern
/// was a model "Продолжаю"-ing itself into context death — a naive "continue"
/// nudge would feed exactly that loop.
pub fn nudge_text(kind: NudgeKind) -> &'static str {
    match kind {
        NudgeKind::Clipped => {
            "⚠ Твой предыдущий ответ оборвался (лимит вывода/контекста). \
             НЕ перечитывай файлы и не продолжай работу с того же места: немедленно выдай финальный ответ \
             одним сообщением — компактное резюме сделанного и результат. Если что-то осталось несделанным, \
             перечисли недоделки коротким списком в конце."
        }
        NudgeKind::Progress => "Ответ содержит только сообщение о ходе работы. Заверши задачу и выдай результат.",
        NudgeKind::Empty => {
            "⚠ Твой предыдущий ответ пуст. Выдай ответ по задаче одним сообщением — полностью и без вопросов."
        }
    }
}

/// The canned answer an unattended run gives a question that slipped through
/// despite the "don't ask" preamble.
pub const AUTO_ANSWER: &str = "Запуск идёт без оператора, отвечать никто не будет. \
     Действуй по собственному усмотрению: прими разумные допущения и закончи задачу. \
     Все принятые допущения перечисли в финальном ответе.";

/// How many canned answers an unattended run gives before the question is
/// left in the output as-is (bounding an ask-looping model).
pub const AUTO_ANSWERS_MAX: u32 = 2;

/// The section mixed into an agent task's prompt, as the last block. `None`
/// when the protocol is fully off (`ask: false` and not `unattended`) — e.g.
/// flows whose prompts must stay byte-exact.
pub fn preamble(s: &Settings) -> Option<String> {
    if s.unattended {
        Some(
            "# Протокол связи с оператором\n\n\
             Этот запуск идёт без оператора (пакетный/ночной): не задавай вопросов и не жди ответа. \
             Если информации не хватает — прими разумное допущение и продолжай; все допущения перечисли в финальном ответе."
                .into(),
        )
    } else if s.ask {
        Some(
            "# Протокол связи с оператором\n\n\
             Если без решения оператора дальше идти нельзя, напиши отдельной строкой `ASK: вопрос` и заверши ход. \
             Для уточнения по ходу работы напиши отдельную строку с префиксом `ASK_INLINE: ` и продолжай независимую работу. \
             После любого префикса можно передать JSON вида {\"question\":\"Какой формат?\",\"options\":[\"JSON\",\"CSV\"]}. \
             Пиши вопросы обычным сообщением, без блока кода и без вызова интерактивного инструмента. \
             В конце хода дождись ответов через zori: заверши сообщение, ответы придут следующим ходом."
                .into(),
        )
    } else {
        None
    }
}

/// The preamble the CLASSIC driver mixes: it has no chat, so the ask
/// protocol would teach the model to park questions nobody can ever answer.
/// An `ask: false` flow still gets no preamble at all; everything else runs
/// as if unattended.
pub fn preamble_for_classic(flow: &Flow, task: &Task) -> Option<String> {
    let mut s = Settings::for_task(flow, task);
    if s.ask {
        s.unattended = true;
    }
    preamble(&s)
}

/// Which harnesses get the protocol preamble mixed into their prompt: the
/// built-in agent CLIs zori knows how to steer. Custom `harnesses:` overrides
/// are deliberately excluded — their "prompt" is often an argv path or a
/// one-shot script (e.g. `sh -c 'cat "$1"'`), where an appended section
/// corrupts the value; and zori cannot resume them (Opaque session), so the
/// ASK convention would be a dead end anyway. A new built-in agent joins
/// this list together with `connectors::track_args`.
pub fn preamble_applies(conn: &crate::connectors::Connector) -> bool {
    conn.is_agent && matches!(conn.id.as_str(), "coddy" | "codex" | "opencode")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(reply: &str) -> Attempt<'_> {
        Attempt { reply, success: true, timed_out: false, exit_code: Some(0) }
    }

    #[test]
    fn defaults_switch_the_supervisor_on() {
        let d = Settings::defaults();
        assert_eq!(d, Settings::resolve(None, None));
        assert_eq!(d, Settings { unattended: false, ask: true, nudges: 1, retry: 1, ask_timeout_secs: 0 });
    }

    #[test]
    fn task_overrides_flow_overrides_defaults() {
        let flow = Watchdog {
            unattended: Some(true),
            ask: Some(true),
            nudges: Some(2),
            retry: Some(0),
            ask_timeout_secs: Some(600),
        };
        let s = Settings::resolve(Some(&flow), None);
        assert!(s.unattended && s.ask && s.nudges == 2 && s.retry == 0 && s.ask_timeout_secs == 600);

        // The task restates one knob; the rest inherit from the flow.
        let task = Watchdog { unattended: Some(false), ..Default::default() };
        let s = Settings::resolve(Some(&flow), Some(&task));
        assert!(!s.unattended, "task wins over flow");
        assert_eq!(s.nudges, 2, "unset fields inherit the flow block");
        assert_eq!(s.retry, 0);

        // Task-only: everything else stays default.
        let task = Watchdog { nudges: Some(3), ..Default::default() };
        let s = Settings::resolve(None, Some(&task));
        assert_eq!((s.nudges, s.retry, s.ask), (3, 1, true));
    }

    #[test]
    fn judge_spots_every_pathology() {
        // The incident verbatim: exit 0, clipped marker in the reply.
        let v = judge(&attempt("Продолжаю…\n<response clipped>"), true);
        assert_eq!(v, Verdict::Nudge(NudgeKind::Clipped));

        // Empty reply on success.
        assert_eq!(judge(&attempt("  \n"), true), Verdict::Nudge(NudgeKind::Empty));

        // A question, per the protocol.
        let v = judge(&attempt("Готов готовить план.\nASK: какой формат ответа?"), true);
        assert_eq!(v, Verdict::Ask("ASK: какой формат ответа?".into()));

        // With the protocol off the same text is just… text.
        let v = judge(&attempt("ASK: какой формат ответа?"), false);
        assert_eq!(v, Verdict::Ok);

        // Failures outrank questions and clips.
        let a = Attempt { reply: "ASK: что?\n<response clipped>", success: false, timed_out: false, exit_code: Some(1) };
        assert_eq!(judge(&a, true), Verdict::Retry("exit 1".into()));
        let a = Attempt { reply: "", success: false, timed_out: true, exit_code: None };
        assert_eq!(judge(&a, true), Verdict::Retry("timed out".into()));

        assert_eq!(judge(&attempt("Продолжаю…\nПродолжаю…"), true), Verdict::Nudge(NudgeKind::Progress));
        assert_eq!(judge(&attempt("connection lost"), true), Verdict::Retry("connection lost".into()));
        assert_eq!(judge(&attempt("Investigated connection lost. Reconnect test passes."), true), Verdict::Ok);
        assert_eq!(judge(&attempt("Продолжаю…\nГотово."), true), Verdict::Ok);

        assert_eq!(judge(&attempt("Fixed handling of `<response clipped>`. Tests pass."), true), Verdict::Ok);
        assert_eq!(judge(&attempt("Partial\n<response clipped><entry>Preview omitted"), true), Verdict::Nudge(NudgeKind::Clipped));

        // Healthy reply.
        assert_eq!(judge(&attempt("Готово: 3 правки, тесты зелёные."), true), Verdict::Ok);
    }

    #[test]
    fn ask_of_takes_the_first_ask_line_only() {
        let r = "шапка\n  ASK: первый вопрос?\nхвост\nASK: второй?";
        assert_eq!(ask_of(r).as_deref(), Some("ASK: первый вопрос?"));
        assert_eq!(ask_of("нет вопросов тут"), None);
        // `TASK:` and friends are not asks; the marker is case-sensitive.
        assert_eq!(ask_of("ask: lowercase?"), None);
        assert_eq!(ask_of("TASK: not a question"), None);
        assert_eq!(ask_of("```text\nASK_INLINE: example\n```"), None);
        assert_eq!(ask_of("ASK_INLINE:   "), None);
        assert_eq!(ask_of("ASK_INLINE: формат?").as_deref(), Some("ASK_INLINE: формат?"));
    }

    #[test]
    fn preamble_variants() {
        let ask = preamble(&Settings::defaults()).expect("ask protocol is on by default");
        assert!(ask.contains("ASK: "), "the convention must be taught: {ask}");

        let night = preamble(&Settings { unattended: true, ..Settings::defaults() }).unwrap();
        assert!(night.contains("без оператора"), "{night}");
        assert!(!night.contains("ASK: "), "unattended must not invite questions: {night}");

        assert!(preamble(&Settings { ask: false, ..Settings::defaults() }).is_none());
    }

    #[test]
    fn nudge_texts_are_not_self_triggered() {
        // A nudge must never look like the pathologies it detects — the echo
        // probe replies with the nudge text itself, and a real model might
        // quote it; either way the loop must come to rest.
        for k in [NudgeKind::Clipped, NudgeKind::Empty] {
            let t = nudge_text(k);
            assert!(!t.contains(CLIPPED_MARKER));
            assert!(ask_of(t).is_none());
            assert!(!t.trim().is_empty());
        }
        assert!(ask_of(AUTO_ANSWER).is_none());
    }
}
