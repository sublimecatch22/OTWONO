//! The orchestration engine.
//!
//! Bounded, inspectable, and restartable. It never loops without a budget, it
//! never runs past an approval gate, and every step it takes is written to the
//! activity log before the next one begins.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use otwono_providers::ChatTurn;
use otwono_store::repo::activity::{ActivityRepo, NewActivity, Outcome};
use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::projects::{NewTask, ProjectRepo};
use otwono_store::repo::workspaces::WorkspaceRepo;
use otwono_store::Db;
use otwono_types::agent::Agent;
use otwono_types::project::{Project, ProjectState, Task, TaskState};

use crate::executor::{AgentExecutor, AgentTurn};
use crate::prompt;
use crate::verify::{Verdict, Verification};

/// One task as proposed by the planner, before it becomes a row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedTask {
    pub title: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Positions (1-based) of earlier tasks in this same plan.
    #[serde(default)]
    pub depends_on: Vec<usize>,
    /// The role the planner thinks should do this, matched against the agents
    /// actually available. A role that does not exist is ignored, not invented.
    #[serde(default)]
    pub suggested_role: Option<String>,
    #[serde(default)]
    pub requires_approval: bool,
}

/// Ceiling on how many tasks one plan may contain, whatever the model returns.
pub const MAX_PLANNED_TASKS: usize = 40;

/// The instruction given to a planning agent.
pub fn planning_prompt(project: &Project, available_roles: &[String]) -> String {
    let criteria = if project.acceptance_criteria.is_empty() {
        "(none stated)".to_string()
    } else {
        project
            .acceptance_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let roles = if available_roles.is_empty() {
        "(no specialised agents are available; leave suggested_role null)".to_string()
    } else {
        available_roles.join(", ")
    };

    format!(
        "Objective: {}\n\nWhat the user said: {}\n\nAcceptance criteria for the whole project:\n{criteria}\n\n\
         Roles available to assign work to: {roles}\n\n\
         Produce a plan as a JSON array and nothing else. Each element:\n\
         {{\"title\": string, \"instructions\": string, \"acceptance_criteria\": [string], \
         \"depends_on\": [integer], \"suggested_role\": string or null, \"requires_approval\": boolean}}\n\n\
         Rules:\n\
         - `depends_on` holds 1-based positions of earlier tasks in this array. Leave it empty \
           when a task can start immediately.\n\
         - Set `requires_approval` to true only when the task would send something outside this \
           machine or commit the user to something.\n\
         - At most {MAX_PLANNED_TASKS} tasks. Prefer fewer, clearer tasks.\n\
         - Every task must be finishable by one agent in one sitting.",
        project.title, project.objective
    )
}

/// Pull a JSON array out of a model's answer, tolerating fences and preamble.
pub fn extract_json_array(answer: &str) -> Option<String> {
    // A fenced block is the most common shape.
    if let Some(start) = answer.find("```") {
        let after = &answer[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        if let Some(end) = after.find("```") {
            let inner = after[..end].trim();
            if inner.starts_with('[') {
                return Some(inner.to_string());
            }
        }
    }

    // Otherwise take the first balanced `[ … ]`, ignoring brackets inside
    // strings so a title containing one cannot truncate the plan.
    let bytes = answer.as_bytes();
    let start = answer.find('[')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for index in start..bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(answer[start..=index].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Parse and sanitise a plan. Refuses nonsense rather than storing it.
pub fn parse_plan(answer: &str) -> Result<Vec<PlannedTask>> {
    let json = extract_json_array(answer)
        .ok_or_else(|| anyhow::anyhow!("the planner did not return a JSON array of tasks"))?;
    let mut tasks: Vec<PlannedTask> = serde_json::from_str(&json)
        .map_err(|e| anyhow::anyhow!("the planner's JSON could not be read: {e}"))?;

    tasks.retain(|task| !task.title.trim().is_empty());
    if tasks.is_empty() {
        bail!("the planner returned no tasks");
    }
    tasks.truncate(MAX_PLANNED_TASKS);

    let count = tasks.len();
    for (position, task) in tasks.iter_mut().enumerate() {
        task.title = task.title.trim().chars().take(200).collect();
        // A dependency may only point backwards, and only at a task that
        // exists. Anything else is dropped rather than trusted.
        task.depends_on.retain(|dependency| {
            *dependency >= 1 && *dependency <= position && *dependency <= count
        });
        task.depends_on.sort_unstable();
        task.depends_on.dedup();
        task.acceptance_criteria.retain(|c| !c.trim().is_empty());
        if let Some(role) = &task.suggested_role {
            if role.trim().is_empty() {
                task.suggested_role = None;
            }
        }
    }
    Ok(tasks)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub project_id: String,
    pub steps_used: u32,
    pub tasks_completed: u32,
    pub tasks_failed: u32,
    pub tasks_reworked: u32,
    pub awaiting_approval: Vec<String>,
    pub final_state: String,
    /// Why the run stopped, in words for the run drawer.
    pub stopped_because: String,
}

pub struct Orchestrator<'a> {
    db: &'a Db,
    executor: &'a dyn AgentExecutor,
}

impl<'a> Orchestrator<'a> {
    pub fn new(db: &'a Db, executor: &'a dyn AgentExecutor) -> Self {
        Self { db, executor }
    }

    fn log(&self, project_id: &str, action: &str, outcome: Outcome, detail: serde_json::Value) {
        let entry = NewActivity::system(action)
            .with_project(project_id)
            .with_outcome(outcome)
            .with_detail(detail);
        if let Err(error) = ActivityRepo::new(self.db).record(entry) {
            tracing::error!(%error, "could not write to the activity log");
        }
    }

    fn agent_turn(&self, agent: &Agent, messages: Vec<ChatTurn>) -> Result<AgentTurn> {
        let model = agent
            .model
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{} has no model selected", agent.name))?;
        Ok(AgentTurn {
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            model,
            messages,
            temperature: agent.parameters.temperature,
            max_output_tokens: agent.parameters.max_output_tokens,
            timeout_seconds: agent.timeout_seconds,
        })
    }

    /// Turn a draft project's objective into a task plan.
    pub async fn plan(&self, project_id: &str) -> Result<Vec<Task>> {
        let projects = ProjectRepo::new(self.db);
        let project = projects
            .get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("project {project_id} does not exist"))?;
        if project.state != ProjectState::Draft {
            bail!(
                "this project is {} and cannot be re-planned; move it back to draft first",
                project.state
            );
        }
        if !projects.tasks(project_id)?.is_empty() {
            bail!("this project already has tasks");
        }

        let agents = AgentRepo::new(self.db);
        let orchestrator = project
            .orchestrator_agent_id
            .as_deref()
            .and_then(|id| agents.get(id).ok().flatten())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "this project has no orchestrator agent. Choose one in the project's settings."
                )
            })?;

        let candidates = self.assignable_agents(&project)?;
        let roles: Vec<String> = candidates
            .iter()
            .map(|agent| format!("{} ({})", agent.role, agent.name))
            .collect();

        let workspace_instructions = self.workspace_instructions(&project)?;
        let mut parts = prompt::for_agent(&orchestrator, workspace_instructions);
        parts.user_message = planning_prompt(&project, &roles);
        let messages = prompt::build(&parts);

        let outcome = self
            .executor
            .run(self.agent_turn(&orchestrator, messages)?)
            .await?;
        let planned = parse_plan(&outcome.text)?;

        // Write the plan, resolving positional dependencies to task ids.
        let mut created: Vec<Task> = Vec::with_capacity(planned.len());
        for task in &planned {
            let depends_on: Vec<String> = task
                .depends_on
                .iter()
                .filter_map(|position| created.get(position - 1).map(|t| t.id.clone()))
                .collect();
            let assigned = task
                .suggested_role
                .as_deref()
                .and_then(|role| match_agent(role, &candidates))
                .map(|agent| agent.id.clone());

            created.push(projects.add_task(
                project_id,
                NewTask {
                    title: task.title.clone(),
                    instructions: task.instructions.clone(),
                    acceptance_criteria: task.acceptance_criteria.clone(),
                    assigned_agent_id: assigned,
                    depends_on,
                    requires_approval: task.requires_approval,
                    max_attempts: Some(project.max_task_retries + 1),
                },
            )?);
        }

        projects.transition(project_id, ProjectState::Planned)?;
        projects.refresh_readiness(project_id)?;
        self.log(
            project_id,
            "project.plan",
            Outcome::Ok,
            serde_json::json!({ "tasks": created.len(), "planner": orchestrator.name }),
        );
        projects.tasks(project_id)
    }

    fn workspace_instructions(&self, project: &Project) -> Result<Option<String>> {
        let Some(workspace_id) = &project.workspace_id else {
            return Ok(None);
        };
        Ok(WorkspaceRepo::new(self.db)
            .get(workspace_id)?
            .map(|workspace| workspace.shared_instructions))
    }

    /// Agents that could be assigned work on this project: the workspace's
    /// members if it has any, otherwise every agent.
    fn assignable_agents(&self, project: &Project) -> Result<Vec<Agent>> {
        let agents = AgentRepo::new(self.db);
        if let Some(workspace_id) = &project.workspace_id {
            let members = WorkspaceRepo::new(self.db).members(workspace_id)?;
            if !members.is_empty() {
                return Ok(members
                    .iter()
                    .filter_map(|member| agents.get(&member.agent_id).ok().flatten())
                    .collect());
            }
        }
        agents.list(None, false)
    }

    /// Execute ready tasks until the project finishes, blocks, or runs out of
    /// budget. Safe to call repeatedly: it picks up where it left off.
    pub async fn run(&self, project_id: &str) -> Result<RunReport> {
        let projects = ProjectRepo::new(self.db);
        let project = projects
            .get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("project {project_id} does not exist"))?;

        if matches!(
            project.state,
            ProjectState::Planned | ProjectState::AwaitingApproval | ProjectState::Blocked
        ) {
            projects.transition(project_id, ProjectState::Running)?;
        } else if project.state != ProjectState::Running {
            bail!(
                "a project in state {} cannot be run; it must be planned, approved, blocked or \
                 already running",
                project.state
            );
        }

        let mut report = RunReport {
            project_id: project_id.to_string(),
            ..Default::default()
        };
        let budget = project.max_steps.max(1);

        loop {
            if report.steps_used >= budget {
                report.stopped_because = format!(
                    "This run reached its limit of {budget} steps. Increase the project's step \
                     budget or run it again to continue."
                );
                projects.transition(project_id, ProjectState::Blocked).ok();
                break;
            }

            projects.refresh_readiness(project_id)?;
            let tasks = projects.tasks(project_id)?;

            // A task waiting for a person stops the run: nothing after it may
            // proceed on its own.
            let waiting: Vec<String> = tasks
                .iter()
                .filter(|task| task.state == TaskState::AwaitingApproval)
                .map(|task| task.id.clone())
                .collect();
            if !waiting.is_empty() {
                report.awaiting_approval = waiting;
                report.stopped_because =
                    "Waiting for your approval before going further.".to_string();
                projects
                    .transition(project_id, ProjectState::AwaitingApproval)
                    .ok();
                break;
            }

            let Some(next) = tasks
                .iter()
                .find(|task| task.state == TaskState::Ready)
                .cloned()
            else {
                let unfinished: Vec<&Task> = tasks
                    .iter()
                    .filter(|task| !task.state.is_terminal())
                    .collect();
                if unfinished.is_empty() {
                    let any_failed = tasks.iter().any(|task| task.state == TaskState::Failed);
                    let final_state = if any_failed {
                        ProjectState::Failed
                    } else {
                        ProjectState::Verifying
                    };
                    projects.transition(project_id, final_state)?;
                    if final_state == ProjectState::Verifying {
                        projects.transition(project_id, ProjectState::Completed)?;
                        report.stopped_because =
                            "Every task finished and was verified.".to_string();
                    } else {
                        report.stopped_because =
                            "The project stopped because a task could not be completed."
                                .to_string();
                    }
                } else {
                    projects.transition(project_id, ProjectState::Blocked).ok();
                    report.stopped_because = format!(
                        "{} task(s) cannot start because a task they depend on did not complete.",
                        unfinished.len()
                    );
                }
                break;
            };

            report.steps_used += 1;
            match self.run_task(&project, &next).await {
                Ok(TaskProgress::Completed) => report.tasks_completed += 1,
                Ok(TaskProgress::Reworked) => report.tasks_reworked += 1,
                Ok(TaskProgress::Failed) => report.tasks_failed += 1,
                Ok(TaskProgress::AwaitingApproval) => continue,
                Err(error) => {
                    // An unexpected error fails the task rather than the whole
                    // engine, so the rest of the plan can still be reported on.
                    report.tasks_failed += 1;
                    let mut failed = next.clone();
                    failed.failure_reason = Some(error.to_string());
                    projects.update_task(&failed)?;
                    projects.transition_task(&next.id, TaskState::Failed).ok();
                    self.log(
                        project_id,
                        "task.error",
                        Outcome::Failed,
                        serde_json::json!({ "task": next.title, "error": error.to_string() }),
                    );
                }
            }
        }

        let final_project = projects.get(project_id)?;
        report.final_state = final_project
            .map(|p| p.state.as_str().to_string())
            .unwrap_or_else(|| "unknown".into());
        self.log(
            project_id,
            "project.run",
            Outcome::Ok,
            serde_json::to_value(&report).unwrap_or_default(),
        );
        Ok(report)
    }

    /// Run one task: approval gate, execution, verification, rework.
    async fn run_task(&self, project: &Project, task: &Task) -> Result<TaskProgress> {
        let projects = ProjectRepo::new(self.db);
        let agents = AgentRepo::new(self.db);

        if task.requires_approval {
            projects.transition_task(&task.id, TaskState::AwaitingApproval)?;
            self.log(
                &project.id,
                "task.awaiting_approval",
                Outcome::Pending,
                serde_json::json!({ "task": task.title }),
            );
            return Ok(TaskProgress::AwaitingApproval);
        }

        let agent = task
            .assigned_agent_id
            .as_deref()
            .and_then(|id| agents.get(id).ok().flatten())
            .or_else(|| {
                project
                    .orchestrator_agent_id
                    .as_deref()
                    .and_then(|id| agents.get(id).ok().flatten())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no agent is assigned to \"{}\" and no orchestrator is set",
                    task.title
                )
            })?;

        projects.transition_task(&task.id, TaskState::Running)?;
        let mut running = projects
            .get_task(&task.id)?
            .ok_or_else(|| anyhow::anyhow!("task vanished"))?;
        running.attempt += 1;
        projects.update_task(&running)?;

        let workspace_instructions = self.workspace_instructions(project)?;
        let mut parts = prompt::for_agent(&agent, workspace_instructions);
        parts.user_message = task_prompt(project, &running);
        let messages = prompt::build(&parts);

        let outcome = self
            .executor
            .run(self.agent_turn(&agent, messages)?)
            .await?;
        running.output = Some(outcome.text.clone());
        projects.update_task(&running)?;
        self.log(
            &project.id,
            "task.executed",
            Outcome::Ok,
            serde_json::json!({
                "task": running.title,
                "agent": agent.name,
                "attempt": running.attempt,
                "output_bytes": outcome.text.len(),
                "truncated": outcome.was_truncated(),
            }),
        );

        // Verification.
        projects.transition_task(&task.id, TaskState::Verifying)?;
        let verification = self.verify(project, &running, &outcome.text).await?;
        running.verification_notes = Some(verification.notes.clone());

        if verification.verdict.allows_completion() {
            projects.update_task(&running)?;
            projects.transition_task(&task.id, TaskState::Completed)?;
            self.log(
                &project.id,
                "task.verified",
                Outcome::Ok,
                serde_json::json!({ "task": running.title, "verdict": "pass" }),
            );
            return Ok(TaskProgress::Completed);
        }

        running.failure_reason = verification.required_changes.clone();
        projects.update_task(&running)?;
        projects.transition_task(&task.id, TaskState::Failed)?;
        self.log(
            &project.id,
            "task.verification_failed",
            Outcome::Failed,
            serde_json::json!({
                "task": running.title,
                "verdict": verification.verdict.as_str(),
                "attempt": running.attempt,
            }),
        );

        if running.attempt < running.max_attempts {
            projects.transition_task(&task.id, TaskState::Ready)?;
            self.log(
                &project.id,
                "task.rework",
                Outcome::Pending,
                serde_json::json!({
                    "task": running.title,
                    "attempt": running.attempt,
                    "of": running.max_attempts,
                }),
            );
            return Ok(TaskProgress::Reworked);
        }

        Ok(TaskProgress::Failed)
    }

    async fn verify(&self, project: &Project, task: &Task, output: &str) -> Result<Verification> {
        let agents = AgentRepo::new(self.db);
        let Some(verifier) = project
            .verifier_agent_id
            .as_deref()
            .and_then(|id| agents.get(id).ok().flatten())
        else {
            // No verifier configured. Say so rather than passing by default:
            // unchecked work is inconclusive, not verified.
            return Ok(Verification {
                verdict: Verdict::Inconclusive,
                notes: "No verification agent is set for this project, so the work was not \
                        checked. Choose a verifier in the project's settings."
                    .into(),
                required_changes: Some(
                    "Set a verification agent for this project so its output can be checked."
                        .into(),
                ),
            });
        };

        let mut parts = prompt::for_agent(&verifier, None);
        parts.user_message = Verification::prompt(&task.title, &task.acceptance_criteria, output);
        let messages = prompt::build(&parts);
        let outcome = self
            .executor
            .run(self.agent_turn(&verifier, messages)?)
            .await?;
        Ok(Verification::parse(&outcome.text))
    }

    /// Approve a task that was waiting, so the next run can execute it.
    pub fn approve_task(&self, task_id: &str) -> Result<Task> {
        let projects = ProjectRepo::new(self.db);
        let task = projects
            .get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task {task_id} does not exist"))?;
        if task.state != TaskState::AwaitingApproval {
            bail!("that task is not waiting for approval");
        }
        // Clear the gate so the run does not stop at it again.
        let mut approved = task.clone();
        approved.requires_approval = false;
        projects.update_task(&approved)?;
        let ready = projects.transition_task(task_id, TaskState::Ready)?;
        self.log(
            &task.project_id,
            "task.approved",
            Outcome::Ok,
            serde_json::json!({ "task": task.title }),
        );
        Ok(ready)
    }

    /// Refuse a task that was waiting.
    pub fn decline_task(&self, task_id: &str, reason: &str) -> Result<Task> {
        let projects = ProjectRepo::new(self.db);
        let task = projects
            .get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task {task_id} does not exist"))?;
        if task.state != TaskState::AwaitingApproval {
            bail!("that task is not waiting for approval");
        }
        let mut declined = task.clone();
        declined.failure_reason = Some(format!("Declined by the user: {reason}"));
        projects.update_task(&declined)?;
        let cancelled = projects.transition_task(task_id, TaskState::Cancelled)?;
        self.log(
            &task.project_id,
            "task.declined",
            Outcome::Denied,
            serde_json::json!({ "task": task.title, "reason": reason }),
        );
        Ok(cancelled)
    }

    /// The completion report shown when a project finishes.
    pub fn completion_report(&self, project_id: &str) -> Result<String> {
        let projects = ProjectRepo::new(self.db);
        let project = projects
            .get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("project {project_id} does not exist"))?;
        let tasks = projects.tasks(project_id)?;
        let artifacts = projects.artifacts(project_id)?;

        let mut report = format!(
            "# {}\n\n**State:** {}\n\n## Objective\n\n{}\n\n",
            project.title, project.state, project.objective
        );

        if !project.acceptance_criteria.is_empty() {
            report.push_str("## Acceptance criteria\n\n");
            for criterion in &project.acceptance_criteria {
                report.push_str(&format!("- {criterion}\n"));
            }
            report.push('\n');
        }

        report.push_str("## Tasks\n\n");
        for task in &tasks {
            report.push_str(&format!(
                "### {}. {} — {}\n\n",
                task.ordinal + 1,
                task.title,
                task.state
            ));
            if let Some(reason) = &task.failure_reason {
                report.push_str(&format!("**Not completed because:** {reason}\n\n"));
            }
            if let Some(output) = &task.output {
                report.push_str(&format!("{output}\n\n"));
            }
            if let Some(notes) = &task.verification_notes {
                report.push_str(&format!("*Verification:* {notes}\n\n"));
            }
        }

        if !artifacts.is_empty() {
            report.push_str("## Deliverables\n\n");
            for artifact in &artifacts {
                report.push_str(&format!(
                    "- {} ({} bytes) — {}\n",
                    artifact.name, artifact.byte_size, artifact.path
                ));
            }
        }

        Ok(report)
    }
}

enum TaskProgress {
    Completed,
    Reworked,
    Failed,
    AwaitingApproval,
}

/// The message given to an agent doing one task.
pub fn task_prompt(project: &Project, task: &Task) -> String {
    let mut prompt = format!(
        "Project: {}\nProject objective: {}\n\nYour task: {}\n\n{}\n",
        project.title, project.objective, task.title, task.instructions
    );

    if !task.acceptance_criteria.is_empty() {
        prompt.push_str("\nThis task is finished when:\n");
        for criterion in &task.acceptance_criteria {
            prompt.push_str(&format!("- {criterion}\n"));
        }
    }

    if task.attempt > 1 {
        if let Some(reason) = &task.failure_reason {
            prompt.push_str(&format!(
                "\nThis is attempt {} of {}. The previous attempt was rejected. What must change:\n{reason}\n",
                task.attempt, task.max_attempts
            ));
        }
    }

    prompt.push_str("\nProduce the work itself, not a description of how you would do it.");
    prompt
}

/// Match a planner's suggested role to an agent that actually exists.
fn match_agent<'agents>(role: &str, candidates: &'agents [Agent]) -> Option<&'agents Agent> {
    let needle = role.to_ascii_lowercase();
    candidates
        .iter()
        .find(|agent| {
            agent.role.eq_ignore_ascii_case(role) || agent.name.eq_ignore_ascii_case(role)
        })
        .or_else(|| {
            candidates.iter().find(|agent| {
                needle.contains(&agent.role.to_ascii_lowercase())
                    || needle.contains(&agent.name.to_ascii_lowercase())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_json_plan_parses() {
        let plan = parse_plan(
            r#"[{"title":"Gather figures","instructions":"Collect Q3 revenue",
                "acceptance_criteria":["All three months present"],"depends_on":[],
                "suggested_role":"Research","requires_approval":false}]"#,
        )
        .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].title, "Gather figures");
        assert_eq!(plan[0].acceptance_criteria.len(), 1);
    }

    #[test]
    fn a_fenced_plan_with_preamble_parses() {
        let answer = "Here is the plan you asked for:\n\n```json\n\
            [{\"title\":\"One\"},{\"title\":\"Two\",\"depends_on\":[1]}]\n```\n\nLet me know.";
        let plan = parse_plan(answer).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[1].depends_on, vec![1]);
    }

    #[test]
    fn a_bracket_inside_a_title_does_not_truncate_the_plan() {
        let answer = r#"[{"title":"Fix the [broken] table"},{"title":"Second"}]"#;
        let plan = parse_plan(answer).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].title, "Fix the [broken] table");
    }

    #[test]
    fn forward_and_self_dependencies_are_dropped() {
        let plan = parse_plan(
            r#"[{"title":"One","depends_on":[1,2,5]},{"title":"Two","depends_on":[1,1,99]}]"#,
        )
        .unwrap();
        assert!(
            plan[0].depends_on.is_empty(),
            "a first task cannot depend on anything"
        );
        assert_eq!(
            plan[1].depends_on,
            vec![1],
            "duplicates and out-of-range are dropped"
        );
    }

    #[test]
    fn a_plan_is_capped_however_long_the_model_makes_it() {
        let items: Vec<String> = (1..=200)
            .map(|i| format!("{{\"title\":\"Task {i}\"}}"))
            .collect();
        let plan = parse_plan(&format!("[{}]", items.join(","))).unwrap();
        assert_eq!(plan.len(), MAX_PLANNED_TASKS);
    }

    #[test]
    fn empty_and_unparseable_plans_are_refused() {
        assert!(parse_plan("I would suggest starting with research.").is_err());
        assert!(parse_plan("[]").is_err());
        assert!(parse_plan(r#"[{"title":"   "}]"#).is_err());
        assert!(parse_plan("[{not json}]").is_err());
    }

    #[test]
    fn extracting_an_array_ignores_prose_and_objects() {
        assert!(extract_json_array("no array here").is_none());
        assert_eq!(
            extract_json_array(r#"prose {"a":1} then [1,2] more"#).as_deref(),
            Some("[1,2]")
        );
    }

    #[test]
    fn the_planning_prompt_states_the_shape_and_the_limits() {
        let project = Project {
            id: "prj_1".into(),
            title: "Quarterly report".into(),
            objective: "Produce the Q3 report".into(),
            acceptance_criteria: vec!["Includes revenue".into()],
            state: ProjectState::Draft,
            workspace_id: None,
            orchestrator_agent_id: None,
            verifier_agent_id: None,
            max_steps: 40,
            max_task_retries: 2,
            budget_id: None,
            sync_enabled: false,
            created_at: otwono_types::now(),
            updated_at: otwono_types::now(),
        };
        let prompt = planning_prompt(&project, &["Research (Researcher)".into()]);
        assert!(prompt.contains("Produce the Q3 report"));
        assert!(prompt.contains("Includes revenue"));
        assert!(prompt.contains("Research (Researcher)"));
        assert!(prompt.contains("1-based positions"));
        assert!(prompt.contains(&MAX_PLANNED_TASKS.to_string()));
    }

    #[test]
    fn a_reworked_task_is_told_what_was_wrong() {
        let project = Project {
            id: "prj_1".into(),
            title: "Report".into(),
            objective: "Write it".into(),
            acceptance_criteria: vec![],
            state: ProjectState::Running,
            workspace_id: None,
            orchestrator_agent_id: None,
            verifier_agent_id: None,
            max_steps: 40,
            max_task_retries: 2,
            budget_id: None,
            sync_enabled: false,
            created_at: otwono_types::now(),
            updated_at: otwono_types::now(),
        };
        let task = Task {
            id: "tsk_1".into(),
            project_id: "prj_1".into(),
            ordinal: 0,
            title: "Draft the summary".into(),
            instructions: "Write 300 words.".into(),
            acceptance_criteria: vec!["Mentions revenue".into()],
            state: TaskState::Running,
            assigned_agent_id: None,
            depends_on: vec![],
            requires_approval: false,
            attempt: 2,
            max_attempts: 3,
            output: None,
            failure_reason: Some("Add the revenue figures.".into()),
            verification_notes: None,
            created_at: otwono_types::now(),
            updated_at: otwono_types::now(),
        };

        let prompt = task_prompt(&project, &task);
        assert!(prompt.contains("attempt 2 of 3"));
        assert!(prompt.contains("Add the revenue figures."));
        assert!(prompt.contains("Mentions revenue"));
        assert!(prompt.contains("Produce the work itself"));
    }
}
