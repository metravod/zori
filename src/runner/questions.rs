use super::*;

pub(super) fn publish_questions(emitter: &Emitter, id: &str, text: &str, fence: &mut Option<char>) {
    for mut question in text.lines().filter_map(|l| crate::watchdog::protocol_line(l, fence))
        .filter_map(crate::watchdog::question_line) {
        emitter.mutate(|st| {
            let Some(n) = st.nodes.get_mut(id) else { return };
            if n.questions.iter().any(|q| !q.closed && q.text == question.text && q.options == question.options) {
                return;
            }
            question.id = n.questions.len() + 1;
            n.questions.push(question);
        });
    }
}

pub(super) fn question_verdict(emitter: &Emitter, id: &str, verdict: Verdict, can_resume: bool, unattended: bool) -> Verdict {
    if matches!(verdict, Verdict::Retry(_) | Verdict::Incomplete) { return verdict; }
    if let Some(q) = emitter.snapshot().nodes[id].questions.iter().find(|q| !q.closed) {
        if can_resume { return Verdict::Ask(q.text.clone()); }
        return Verdict::Retry("operator question without a resumable session".into());
    }
    if !unattended && matches!(verdict, Verdict::Ask(_)) { Verdict::Ok } else { verdict }
}

pub(super) fn question_status(node: &mut crate::state::TaskNode, status: TaskStatus, verdict: &Verdict) -> TaskStatus {
    if matches!(verdict, Verdict::Ask(_)) && node.questions.iter().any(|q| !q.closed) {
        return TaskStatus::Running;
    }
    if status == TaskStatus::Failed {
        for q in &mut node.questions { q.closed = true; }
    }
    status
}
