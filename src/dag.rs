//! Level-based DAG plan — ported from zymi-core `src/config/dag.rs` (MIT).
//!
//! Why a port and not a dependency: zymi's scheduler is 300 lines of pure
//! Kahn's-algorithm BFS over `PipelineConfig`, but its *executor* is welded to
//! zymi's `Runtime` (mandatory event store, ESAA orchestrator) and a closed
//! `#[non_exhaustive] EventKind` enum, and zymi is in quiet maintenance
//! (D55) so a new step kind cannot be upstreamed. The scheduler semantics —
//! group independent steps into levels, run a level's steps concurrently,
//! await the whole level before the next, fail loudly on cycles — are exactly
//! what we want and are cheap to own. See .omut/decisions.md D1.

use std::collections::{HashMap, HashSet, VecDeque};

/// A level-based execution plan. Steps within a level are mutually
/// independent; levels run in order.
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    /// Each inner `Vec` is one execution level, sorted lexicographically so
    /// plans are deterministic.
    pub levels: Vec<Vec<String>>,
}

impl ExecutionPlan {
    pub fn step_count(&self) -> usize {
        self.levels.iter().map(|l| l.len()).sum()
    }

    pub fn is_sequential(&self) -> bool {
        self.levels.iter().all(|l| l.len() == 1)
    }

    pub fn level_of(&self, step_id: &str) -> Option<usize> {
        self.levels.iter().position(|l| l.iter().any(|s| s == step_id))
    }
}

/// A DAG problem found while building the plan.
#[derive(Debug, Clone)]
pub enum DagError {
    /// A dependency names a task that does not exist.
    UnknownDependency { task: String, depends_on: String },
    /// Duplicate task ids.
    DuplicateId(String),
    /// A task whose id is empty or whitespace-only (1-based position). Such
    /// an id cannot name a task directory — sanitizing maps it onto `tasks/`
    /// itself, so the task would overwrite its own container — and no
    /// `${steps.<id>.output}` can reference it. Enforced here, in the shared
    /// planner, so *every* driver (UI, CLI, proposals) gets the guarantee,
    /// not only the UI validator that happens to check it earlier (review,
    /// minor). Substituting a placeholder instead would collide with the
    /// legitimate id `_`.
    EmptyId(usize),
    /// The graph contains a cycle; the list names the tasks left in it.
    CyclicDependency { cycle: Vec<String> },
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::UnknownDependency { task, depends_on } => {
                write!(f, "task '{task}' depends on unknown task '{depends_on}'")
            }
            DagError::DuplicateId(id) => write!(f, "duplicate task id '{id}'"),
            DagError::EmptyId(pos) => {
                write!(f, "task #{pos} has an empty id — every task needs a name")
            }
            DagError::CyclicDependency { cycle } => {
                write!(f, "cyclic dependency among tasks: {}", cycle.join(", "))
            }
        }
    }
}

impl std::error::Error for DagError {}

/// Build a plan from `(id, depends_on)` pairs. Unlike zymi, we reject unknown
/// dependencies here rather than skipping them: a typo in `depends_on` must not
/// silently change the shape of the graph.
pub fn build_plan(tasks: &[(String, Vec<String>)]) -> Result<ExecutionPlan, DagError> {
    if tasks.is_empty() {
        return Ok(ExecutionPlan { levels: vec![] });
    }

    // Review (minor): empty ids are rejected here, in the shared planner, not
    // only by the UI validator. A blank id sanitizes to "", which names the
    // `tasks/` directory itself — the task would write its artifacts over its
    // own container — and no `${steps..output}` can ever refer to it.
    for (pos, (id, _)) in tasks.iter().enumerate() {
        if id.trim().is_empty() {
            return Err(DagError::EmptyId(pos + 1));
        }
    }

    let ids: HashSet<&str> = tasks.iter().map(|(id, _)| id.as_str()).collect();
    if ids.len() != tasks.len() {
        let mut seen: HashSet<&str> = HashSet::new();
        for (id, _) in tasks {
            if !seen.insert(id.as_str()) {
                return Err(DagError::DuplicateId(id.clone()));
            }
        }
    }

    for (id, deps) in tasks {
        for dep in deps {
            if !ids.contains(dep.as_str()) {
                return Err(DagError::UnknownDependency {
                    task: id.clone(),
                    depends_on: dep.clone(),
                });
            }
        }
    }

    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();

    for (id, deps) in tasks {
        let id = id.as_str();
        in_degree.entry(id).or_insert(0);
        dependents.entry(id).or_default();
        for dep in deps {
            // self-edge: a task depending on itself is a cycle, keep the count
            dependents.entry(dep.as_str()).or_default().push(id);
            *in_degree.entry(id).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut visited = 0usize;

    while !queue.is_empty() {
        let mut next_queue = VecDeque::new();
        let mut level = Vec::with_capacity(queue.len());

        for node in queue.drain(..) {
            visited += 1;
            level.push(node.to_string());
            for &dependent in dependents.get(node).into_iter().flatten() {
                let deg = in_degree.get_mut(dependent).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    next_queue.push_back(dependent);
                }
            }
        }

        level.sort();
        levels.push(level);
        queue = next_queue;
    }

    if visited != ids.len() {
        let cycle: Vec<String> = in_degree
            .iter()
            .filter(|(_, &deg)| deg > 0)
            .map(|(&id, _)| id.to_string())
            .collect();
        return Err(DagError::CyclicDependency { cycle });
    }

    Ok(ExecutionPlan { levels })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, deps: &[&str]) -> (String, Vec<String>) {
        (id.into(), deps.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn empty() {
        assert!(build_plan(&[]).unwrap().levels.is_empty());
    }

    #[test]
    fn linear_chain() {
        let p = build_plan(&[t("s1", &[]), t("s2", &["s1"]), t("s3", &["s2"])]).unwrap();
        assert_eq!(p.levels, vec![vec!["s1"], vec!["s2"], vec!["s3"]]);
        assert!(p.is_sequential());
    }

    #[test]
    fn wide_parallel() {
        let p = build_plan(&[t("s1", &[]), t("s2", &[]), t("s3", &[])]).unwrap();
        assert_eq!(p.levels.len(), 1);
        assert_eq!(p.levels[0], vec!["s1", "s2", "s3"]);
        assert!(!p.is_sequential());
    }

    #[test]
    fn diamond() {
        let p = build_plan(&[
            t("s1", &[]),
            t("s2", &["s1"]),
            t("s3", &["s1"]),
            t("s4", &["s2", "s3"]),
        ])
        .unwrap();
        assert_eq!(p.levels.len(), 3);
        assert_eq!(p.levels[1], vec!["s2", "s3"]);
        assert_eq!(p.level_of("s4"), Some(2));
    }

    #[test]
    fn cycle_detected() {
        let err = build_plan(&[t("s1", &["s3"]), t("s2", &["s1"]), t("s3", &["s2"])]).unwrap_err();
        assert!(matches!(err, DagError::CyclicDependency { .. }));
    }

    #[test]
    fn self_cycle() {
        let err = build_plan(&[t("s1", &["s1"])]).unwrap_err();
        assert!(matches!(err, DagError::CyclicDependency { .. }));
    }

    #[test]
    fn unknown_dependency_rejected() {
        let err = build_plan(&[t("s1", &["ghost"])]).unwrap_err();
        assert!(matches!(err, DagError::UnknownDependency { .. }));
    }

    #[test]
    fn duplicate_id_rejected() {
        let err = build_plan(&[t("s1", &[]), t("s1", &[])]).unwrap_err();
        assert!(matches!(err, DagError::DuplicateId(_)));
    }

    // Review (minor): a blank id reached the shared planner before (only the
    // UI validator caught it). It must be rejected here so no driver ever
    // writes a task's artifacts over the `tasks/` directory itself.
    #[test]
    fn empty_id_rejected() {
        let err = build_plan(&[t("s1", &[]), t("  ", &["s1"])]).unwrap_err();
        assert!(matches!(err, DagError::EmptyId(2)), "got {err:?}");
        assert!(err.to_string().contains("#2"));
    }

    #[test]
    fn level_order_is_lexicographic() {
        let p = build_plan(&[t("zebra", &[]), t("alpha", &[]), t("mike", &[])]).unwrap();
        assert_eq!(p.levels[0], vec!["alpha", "mike", "zebra"]);
    }
}
