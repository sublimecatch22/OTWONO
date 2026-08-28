//! Project and task lifecycles.
//!
//! Both are explicit state machines. Illegal transitions are refused here so
//! that no caller — HTTP handler, orchestrator or recovery routine — can put a
//! row into a state the rest of the system does not expect.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectState {
    Draft,
    Planned,
    AwaitingApproval,
    Running,
    Blocked,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Ready,
    Running,
    AwaitingApproval,
    Blocked,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

macro_rules! state_str {
    ($ty:ty, $( $variant:ident => $text:literal ),+ $(,)?) => {
        impl $ty {
            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $text ),+ }
            }
            pub fn parse(value: &str) -> DomainResult<Self> {
                match value { $( $text => Ok(Self::$variant), )+
                    other => Err(DomainError::validation("state", format!("unknown state {other:?}"))),
                }
            }
        }
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

state_str!(ProjectState,
    Draft => "draft",
    Planned => "planned",
    AwaitingApproval => "awaiting_approval",
    Running => "running",
    Blocked => "blocked",
    Verifying => "verifying",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Archived => "archived",
);

state_str!(TaskState,
    Queued => "queued",
    Ready => "ready",
    Running => "running",
    AwaitingApproval => "awaiting_approval",
    Blocked => "blocked",
    Verifying => "verifying",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
);

impl ProjectState {
    /// A terminal project can only be archived (or, for `Archived`, nothing).
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Archived
        )
    }

    pub fn allows(self, to: Self) -> bool {
        use ProjectState::*;
        match (self, to) {
            (a, b) if a == b => false,
            (Draft, Planned | Cancelled | Archived) => true,
            (Planned, AwaitingApproval | Running | Draft | Cancelled | Archived) => true,
            (AwaitingApproval, Running | Planned | Cancelled | Archived) => true,
            (Running, Blocked | Verifying | AwaitingApproval | Failed | Cancelled) => true,
            (Blocked, Running | AwaitingApproval | Failed | Cancelled) => true,
            (Verifying, Completed | Running | Failed | Cancelled) => true,
            (Completed | Failed | Cancelled, Archived) => true,
            _ => false,
        }
    }

    pub fn transition(self, to: Self) -> DomainResult<Self> {
        if self.allows(to) {
            Ok(to)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "project",
                from: self.to_string(),
                to: to.to_string(),
            })
        }
    }
}

impl TaskState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// A task that was mid-flight when the process died. Recovery returns these
    /// to a state the scheduler can pick up again.
    pub const fn is_interrupted_on_restart(self) -> bool {
        matches!(self, Self::Running | Self::Verifying)
    }

    pub fn allows(self, to: Self) -> bool {
        use TaskState::*;
        match (self, to) {
            (a, b) if a == b => false,
            (Queued, Ready | Blocked | Cancelled) => true,
            (Ready, Running | AwaitingApproval | Blocked | Cancelled) => true,
            (Running, Verifying | AwaitingApproval | Blocked | Completed | Failed | Cancelled) => {
                true
            }
            (AwaitingApproval, Running | Ready | Blocked | Failed | Cancelled) => true,
            (Blocked, Ready | Running | Failed | Cancelled) => true,
            (Verifying, Completed | Ready | Failed | Cancelled) => true,
            // Rework: a failed task may be requeued while retries remain.
            (Failed, Ready) => true,
            _ => false,
        }
    }

    pub fn transition(self, to: Self) -> DomainResult<Self> {
        if self.allows(to) {
            Ok(to)
        } else {
            Err(DomainError::InvalidTransition {
                entity: "task",
                from: self.to_string(),
                to: to.to_string(),
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub title: String,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub state: ProjectState,
    pub workspace_id: Option<String>,
    pub orchestrator_agent_id: Option<String>,
    pub verifier_agent_id: Option<String>,
    pub max_steps: u32,
    pub max_task_retries: u32,
    pub budget_id: Option<String>,
    /// Explicit opt-in before any project metadata reaches the relay API.
    pub sync_enabled: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub ordinal: u32,
    pub title: String,
    pub instructions: String,
    pub acceptance_criteria: Vec<String>,
    pub state: TaskState,
    pub assigned_agent_id: Option<String>,
    /// Task ids that must reach `completed` before this one becomes `ready`.
    pub depends_on: Vec<String>,
    pub requires_approval: bool,
    pub attempt: u32,
    pub max_attempts: u32,
    pub output: Option<String>,
    pub failure_reason: Option<String>,
    pub verification_notes: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Result of asking whether a task's dependencies are satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyStatus {
    Satisfied,
    Waiting,
    Unsatisfiable,
}

/// Given the states of a task's dependencies, decide whether it can start.
pub fn dependency_status(dependency_states: &[TaskState]) -> DependencyStatus {
    if dependency_states
        .iter()
        .any(|s| matches!(s, TaskState::Failed | TaskState::Cancelled))
    {
        return DependencyStatus::Unsatisfiable;
    }
    if dependency_states.iter().all(|s| *s == TaskState::Completed) {
        DependencyStatus::Satisfied
    } else {
        DependencyStatus::Waiting
    }
}

/// Detect a cycle in a task dependency graph. Returns the ids taking part in a
/// cycle, empty when the graph is a DAG.
pub fn detect_cycle(nodes: &[(String, Vec<String>)]) -> Vec<String> {
    use std::collections::{HashMap, HashSet};
    let edges: HashMap<&str, &Vec<String>> =
        nodes.iter().map(|(id, deps)| (id.as_str(), deps)).collect();
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut on_stack: HashSet<&str> = HashSet::new();

    fn walk<'a>(
        node: &'a str,
        edges: &HashMap<&'a str, &'a Vec<String>>,
        visited: &mut HashSet<&'a str>,
        stack: &mut Vec<&'a str>,
        on_stack: &mut HashSet<&'a str>,
    ) -> Option<Vec<String>> {
        if on_stack.contains(node) {
            let start = stack.iter().position(|n| *n == node).unwrap_or(0);
            return Some(stack[start..].iter().map(|s| s.to_string()).collect());
        }
        if !visited.insert(node) {
            return None;
        }
        stack.push(node);
        on_stack.insert(node);
        if let Some(deps) = edges.get(node) {
            for dep in deps.iter() {
                if let Some(cycle) = walk(dep.as_str(), edges, visited, stack, on_stack) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        on_stack.remove(node);
        None
    }

    for (id, _) in nodes {
        if let Some(cycle) = walk(id.as_str(), &edges, &mut visited, &mut stack, &mut on_stack) {
            return cycle;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_happy_path_is_allowed() {
        let path = [
            ProjectState::Draft,
            ProjectState::Planned,
            ProjectState::AwaitingApproval,
            ProjectState::Running,
            ProjectState::Verifying,
            ProjectState::Completed,
            ProjectState::Archived,
        ];
        for pair in path.windows(2) {
            pair[0].transition(pair[1]).expect("legal transition");
        }
    }

    #[test]
    fn project_cannot_skip_from_draft_to_running() {
        let err = ProjectState::Draft
            .transition(ProjectState::Running)
            .unwrap_err();
        assert_eq!(
            err,
            DomainError::InvalidTransition {
                entity: "project",
                from: "draft".into(),
                to: "running".into()
            }
        );
    }

    #[test]
    fn archived_projects_are_final() {
        for target in [
            ProjectState::Draft,
            ProjectState::Running,
            ProjectState::Completed,
        ] {
            assert!(ProjectState::Archived.transition(target).is_err());
        }
    }

    #[test]
    fn completed_task_cannot_be_restarted() {
        assert!(TaskState::Completed.transition(TaskState::Ready).is_err());
        assert!(TaskState::Completed.transition(TaskState::Running).is_err());
    }

    #[test]
    fn failed_task_may_be_requeued_for_rework() {
        assert_eq!(
            TaskState::Failed.transition(TaskState::Ready).unwrap(),
            TaskState::Ready
        );
    }

    #[test]
    fn interrupted_states_are_identified_for_restart_recovery() {
        assert!(TaskState::Running.is_interrupted_on_restart());
        assert!(TaskState::Verifying.is_interrupted_on_restart());
        assert!(!TaskState::AwaitingApproval.is_interrupted_on_restart());
        assert!(!TaskState::Completed.is_interrupted_on_restart());
    }

    #[test]
    fn state_names_round_trip() {
        for s in [ProjectState::AwaitingApproval, ProjectState::Archived] {
            assert_eq!(ProjectState::parse(s.as_str()).unwrap(), s);
        }
        for s in [TaskState::AwaitingApproval, TaskState::Verifying] {
            assert_eq!(TaskState::parse(s.as_str()).unwrap(), s);
        }
        assert!(TaskState::parse("nonsense").is_err());
    }

    #[test]
    fn dependencies_gate_readiness() {
        assert_eq!(dependency_status(&[]), DependencyStatus::Satisfied);
        assert_eq!(
            dependency_status(&[TaskState::Completed, TaskState::Completed]),
            DependencyStatus::Satisfied
        );
        assert_eq!(
            dependency_status(&[TaskState::Completed, TaskState::Running]),
            DependencyStatus::Waiting
        );
        assert_eq!(
            dependency_status(&[TaskState::Failed]),
            DependencyStatus::Unsatisfiable
        );
    }

    #[test]
    fn cycles_are_detected() {
        let dag = vec![
            ("a".to_string(), vec![]),
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["a".to_string(), "b".to_string()]),
        ];
        assert!(detect_cycle(&dag).is_empty());

        let cyclic = vec![
            ("a".to_string(), vec!["c".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
            ("c".to_string(), vec!["b".to_string()]),
        ];
        let cycle = detect_cycle(&cyclic);
        assert_eq!(cycle.len(), 3, "expected a three-node cycle, got {cycle:?}");
    }
}
