//! Projects, tasks, dependencies and artefacts.
//!
//! Every state change goes through `otwono_types`' state machines, so an
//! illegal transition is refused here rather than being written and discovered
//! later.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::project::{
    dependency_status, detect_cycle, DependencyStatus, Project, ProjectState, Task, TaskState,
};

use crate::Db;

const PROJECT_COLUMNS: &str = "id, title, objective, acceptance_criteria, state, workspace_id, \
    orchestrator_agent_id, verifier_agent_id, max_steps, max_task_retries, budget_id, \
    sync_enabled, created_at, updated_at";

const TASK_COLUMNS: &str = "id, project_id, ordinal, title, instructions, acceptance_criteria, \
    state, assigned_agent_id, requires_approval, attempt, max_attempts, output, failure_reason, \
    verification_notes, created_at, updated_at";

fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        title: row.get(1)?,
        objective: row.get(2)?,
        acceptance_criteria: crate::json_column(row.get(3)?),
        state: ProjectState::parse(&row.get::<_, String>(4)?).unwrap_or(ProjectState::Draft),
        workspace_id: row.get(5)?,
        orchestrator_agent_id: row.get(6)?,
        verifier_agent_id: row.get(7)?,
        max_steps: row.get::<_, i64>(8)? as u32,
        max_task_retries: row.get::<_, i64>(9)? as u32,
        budget_id: row.get(10)?,
        sync_enabled: row.get::<_, i64>(11)? != 0,
        created_at: crate::parse_ts(&row.get::<_, String>(12)?),
        updated_at: crate::parse_ts(&row.get::<_, String>(13)?),
    })
}

fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        project_id: row.get(1)?,
        ordinal: row.get::<_, i64>(2)? as u32,
        title: row.get(3)?,
        instructions: row.get(4)?,
        acceptance_criteria: crate::json_column(row.get(5)?),
        state: TaskState::parse(&row.get::<_, String>(6)?).unwrap_or(TaskState::Queued),
        assigned_agent_id: row.get(7)?,
        depends_on: Vec::new(),
        requires_approval: row.get::<_, i64>(8)? != 0,
        attempt: row.get::<_, i64>(9)? as u32,
        max_attempts: row.get::<_, i64>(10)? as u32,
        output: row.get(11)?,
        failure_reason: row.get(12)?,
        verification_notes: row.get(13)?,
        created_at: crate::parse_ts(&row.get::<_, String>(14)?),
        updated_at: crate::parse_ts(&row.get::<_, String>(15)?),
    })
}

#[derive(Debug, Clone, Default)]
pub struct NewProject {
    pub title: String,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub workspace_id: Option<String>,
    pub orchestrator_agent_id: Option<String>,
    pub verifier_agent_id: Option<String>,
    pub max_steps: Option<u32>,
    pub max_task_retries: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub title: String,
    pub instructions: String,
    pub acceptance_criteria: Vec<String>,
    pub assigned_agent_id: Option<String>,
    /// Ids of tasks already created within the same project.
    pub depends_on: Vec<String>,
    pub requires_approval: bool,
    pub max_attempts: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Artifact {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub name: String,
    pub media_type: String,
    pub path: String,
    pub byte_size: u64,
    pub created_at: String,
}

pub struct ProjectRepo<'a> {
    db: &'a Db,
}

impl<'a> ProjectRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    // ---- projects

    pub fn create(&self, new: NewProject) -> Result<Project> {
        if new.title.trim().is_empty() {
            bail!("a project needs a title");
        }
        let id = otwono_types::new_id("prj");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO projects
               (id, title, objective, acceptance_criteria, state, workspace_id,
                orchestrator_agent_id, verifier_agent_id, max_steps, max_task_retries,
                sync_enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'draft', ?5, ?6, ?7, ?8, ?9, 0, ?10, ?10)",
            params![
                id,
                new.title.trim(),
                new.objective,
                crate::to_json(&new.acceptance_criteria),
                new.workspace_id,
                new.orchestrator_agent_id,
                new.verifier_agent_id,
                new.max_steps.unwrap_or(40).clamp(1, 500) as i64,
                new.max_task_retries.unwrap_or(2).clamp(0, 10) as i64,
                now
            ],
        )?;
        self.get(&id)?
            .ok_or_else(|| anyhow::anyhow!("project not found after creation"))
    }

    pub fn get(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?1"),
                [id],
                map_project,
            )
            .optional()?)
    }

    pub fn list(&self, workspace_id: Option<&str>) -> Result<Vec<Project>> {
        let conn = self.db.conn()?;
        let (sql, binds): (String, Vec<String>) = match workspace_id {
            Some(w) => (
                format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE workspace_id = ? ORDER BY updated_at DESC"),
                vec![w.to_string()],
            ),
            None => (
                format!("SELECT {PROJECT_COLUMNS} FROM projects ORDER BY updated_at DESC"),
                Vec::new(),
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), map_project)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update(&self, project: &Project) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE projects SET title = ?2, objective = ?3, acceptance_criteria = ?4,
                    workspace_id = ?5, orchestrator_agent_id = ?6, verifier_agent_id = ?7,
                    max_steps = ?8, max_task_retries = ?9, budget_id = ?10, sync_enabled = ?11,
                    updated_at = ?12
              WHERE id = ?1",
            params![
                project.id,
                project.title,
                project.objective,
                crate::to_json(&project.acceptance_criteria),
                project.workspace_id,
                project.orchestrator_agent_id,
                project.verifier_agent_id,
                project.max_steps as i64,
                project.max_task_retries as i64,
                project.budget_id,
                project.sync_enabled as i64,
                crate::now_str()
            ],
        )?;
        Ok(())
    }

    /// Move a project to a new state, refusing illegal transitions.
    pub fn transition(&self, project_id: &str, to: ProjectState) -> Result<Project> {
        let project = self
            .get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("project {project_id} does not exist"))?;
        let next = project.state.transition(to)?;
        self.db.conn()?.execute(
            "UPDATE projects SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![project_id, next.as_str(), crate::now_str()],
        )?;
        self.get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("project vanished during transition"))
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM projects WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- tasks

    pub fn add_task(&self, project_id: &str, new: NewTask) -> Result<Task> {
        if new.title.trim().is_empty() {
            bail!("a task needs a title");
        }
        let project = self
            .get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("project {project_id} does not exist"))?;
        let id = otwono_types::new_id("tsk");
        let now = crate::now_str();
        let max_attempts = new
            .max_attempts
            .unwrap_or(project.max_task_retries + 1)
            .clamp(1, 20);

        self.db.transaction(|tx| {
            let ordinal: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM tasks WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO tasks
                   (id, project_id, ordinal, title, instructions, acceptance_criteria, state,
                    assigned_agent_id, requires_approval, attempt, max_attempts, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?8, 0, ?9, ?10, ?10)",
                params![
                    id, project_id, ordinal, new.title.trim(), new.instructions,
                    crate::to_json(&new.acceptance_criteria), new.assigned_agent_id,
                    new.requires_approval as i64, max_attempts as i64, now
                ],
            )?;
            for dependency in &new.depends_on {
                let same_project: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM tasks WHERE id = ?1 AND project_id = ?2",
                    params![dependency, project_id],
                    |r| r.get(0),
                )?;
                if same_project == 0 {
                    anyhow::bail!("dependency {dependency} is not a task in this project");
                }
                tx.execute(
                    "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on_task_id)
                     VALUES (?1, ?2)",
                    params![id, dependency],
                )?;
            }
            Ok(())
        })?;

        // A plan with a cycle would never become ready; refuse it at write time.
        let graph = self.dependency_graph(project_id)?;
        let cycle = detect_cycle(&graph);
        if !cycle.is_empty() {
            self.delete_task(&id)?;
            bail!(
                "this dependency would create a cycle: {}",
                cycle.join(" -> ")
            );
        }

        self.get_task(&id)?
            .ok_or_else(|| anyhow::anyhow!("task not found after creation"))
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        let conn = self.db.conn()?;
        let task = conn
            .query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                [id],
                map_task,
            )
            .optional()?;
        drop(conn);
        match task {
            Some(mut task) => {
                task.depends_on = self.dependencies_of(&task.id)?;
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    pub fn tasks(&self, project_id: &str) -> Result<Vec<Task>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE project_id = ?1 ORDER BY ordinal"
        ))?;
        let mut tasks = stmt
            .query_map([project_id], map_task)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        for task in &mut tasks {
            task.depends_on = self.dependencies_of(&task.id)?;
        }
        Ok(tasks)
    }

    fn dependencies_of(&self, task_id: &str) -> Result<Vec<String>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT depends_on_task_id FROM task_dependencies WHERE task_id = ?1 ORDER BY depends_on_task_id",
        )?;
        let rows = stmt.query_map([task_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn dependency_graph(&self, project_id: &str) -> Result<Vec<(String, Vec<String>)>> {
        let tasks = self.tasks(project_id)?;
        Ok(tasks.into_iter().map(|t| (t.id, t.depends_on)).collect())
    }

    pub fn update_task(&self, task: &Task) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE tasks SET title = ?2, instructions = ?3, acceptance_criteria = ?4,
                    assigned_agent_id = ?5, requires_approval = ?6, attempt = ?7,
                    max_attempts = ?8, output = ?9, failure_reason = ?10,
                    verification_notes = ?11, updated_at = ?12
              WHERE id = ?1",
            params![
                task.id,
                task.title,
                task.instructions,
                crate::to_json(&task.acceptance_criteria),
                task.assigned_agent_id,
                task.requires_approval as i64,
                task.attempt as i64,
                task.max_attempts as i64,
                task.output,
                task.failure_reason,
                task.verification_notes,
                crate::now_str()
            ],
        )?;
        Ok(())
    }

    pub fn transition_task(&self, task_id: &str, to: TaskState) -> Result<Task> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task {task_id} does not exist"))?;
        let next = task.state.transition(to)?;
        self.db.conn()?.execute(
            "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![task_id, next.as_str(), crate::now_str()],
        )?;
        self.get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task vanished during transition"))
    }

    pub fn delete_task(&self, id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Promote every queued task whose dependencies are satisfied to `ready`,
    /// and block those whose dependencies can never be satisfied.
    pub fn refresh_readiness(&self, project_id: &str) -> Result<Vec<Task>> {
        let tasks = self.tasks(project_id)?;
        let states: std::collections::HashMap<&str, TaskState> =
            tasks.iter().map(|t| (t.id.as_str(), t.state)).collect();

        let mut changed = Vec::new();
        for task in &tasks {
            if task.state != TaskState::Queued && task.state != TaskState::Blocked {
                continue;
            }
            let dependency_states: Vec<TaskState> = task
                .depends_on
                .iter()
                .filter_map(|id| states.get(id.as_str()).copied())
                .collect();
            let target = match dependency_status(&dependency_states) {
                DependencyStatus::Satisfied => TaskState::Ready,
                DependencyStatus::Waiting => continue,
                DependencyStatus::Unsatisfiable => TaskState::Blocked,
            };
            if task.state == target {
                continue;
            }
            changed.push(self.transition_task(&task.id, target)?);
        }
        Ok(changed)
    }

    /// Bring tasks that were mid-flight when the process stopped back to a
    /// state the scheduler can pick up. Called once at start-up.
    pub fn recover_interrupted(&self) -> Result<Vec<Task>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE state IN ('running', 'verifying')"
        ))?;
        let interrupted = stmt
            .query_map([], map_task)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);

        let mut recovered = Vec::new();
        for task in interrupted {
            debug_assert!(task.state.is_interrupted_on_restart());
            // `running` has no direct route back to `ready` — a task cannot
            // un-start itself. The restart *is* a blocker, so recovery takes
            // the legal path through `blocked`, and both hops are recorded.
            if !task.state.allows(TaskState::Ready) {
                self.transition_task(&task.id, TaskState::Blocked)?;
            }
            let mut restored = self.transition_task(&task.id, TaskState::Ready)?;
            restored.failure_reason =
                Some("Recovered after the application restarted mid-run.".into());
            self.update_task(&restored)?;
            recovered.push(restored);
        }

        // A project that was running becomes blocked until the user resumes it,
        // rather than silently continuing work they did not watch start.
        let conn = self.db.conn()?;
        let mut stmt =
            conn.prepare("SELECT id FROM projects WHERE state IN ('running', 'verifying')")?;
        let project_ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        for id in project_ids {
            self.transition(&id, ProjectState::Blocked).ok();
        }

        Ok(recovered)
    }

    // ---- artefacts

    pub fn add_artifact(
        &self,
        project_id: &str,
        task_id: Option<&str>,
        name: &str,
        media_type: &str,
        path: &str,
        byte_size: u64,
    ) -> Result<Artifact> {
        let id = otwono_types::new_id("art");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO artifacts (id, project_id, task_id, name, media_type, path, byte_size, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, project_id, task_id, name, media_type, path, byte_size as i64, now],
        )?;
        Ok(Artifact {
            id,
            project_id: project_id.into(),
            task_id: task_id.map(Into::into),
            name: name.into(),
            media_type: media_type.into(),
            path: path.into(),
            byte_size,
            created_at: now,
        })
    }

    pub fn artifacts(&self, project_id: &str) -> Result<Vec<Artifact>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, task_id, name, media_type, path, byte_size, created_at
               FROM artifacts WHERE project_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([project_id], |row| {
            Ok(Artifact {
                id: row.get(0)?,
                project_id: row.get(1)?,
                task_id: row.get(2)?,
                name: row.get(3)?,
                media_type: row.get(4)?,
                path: row.get(5)?,
                byte_size: row.get::<_, i64>(6)? as u64,
                created_at: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(db: &Db) -> Project {
        ProjectRepo::new(db)
            .create(NewProject {
                title: "Quarterly report".into(),
                objective: "Produce the Q3 report".into(),
                acceptance_criteria: vec!["Includes revenue".into()],
                ..Default::default()
            })
            .unwrap()
    }

    fn task(repo: &ProjectRepo<'_>, project_id: &str, title: &str, deps: Vec<String>) -> Task {
        repo.add_task(
            project_id,
            NewTask {
                title: title.into(),
                depends_on: deps,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn a_project_starts_as_a_draft_and_keeps_its_criteria() {
        let db = Db::open_in_memory().unwrap();
        let project = project(&db);
        assert_eq!(project.state, ProjectState::Draft);
        assert_eq!(
            project.acceptance_criteria,
            vec!["Includes revenue".to_string()]
        );
        assert!(
            !project.sync_enabled,
            "synchronisation is off until the user opts in"
        );
    }

    #[test]
    fn illegal_project_transitions_are_refused_and_nothing_is_written() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        assert!(repo.transition(&project.id, ProjectState::Running).is_err());
        assert_eq!(
            repo.get(&project.id).unwrap().unwrap().state,
            ProjectState::Draft
        );

        repo.transition(&project.id, ProjectState::Planned).unwrap();
        repo.transition(&project.id, ProjectState::AwaitingApproval)
            .unwrap();
        let running = repo.transition(&project.id, ProjectState::Running).unwrap();
        assert_eq!(running.state, ProjectState::Running);
    }

    #[test]
    fn illegal_task_transitions_are_refused() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        let task = task(&repo, &project.id, "Gather figures", vec![]);
        assert!(
            repo.transition_task(&task.id, TaskState::Running).is_err(),
            "a queued task must become ready first"
        );
        repo.transition_task(&task.id, TaskState::Ready).unwrap();
        repo.transition_task(&task.id, TaskState::Running).unwrap();
        repo.transition_task(&task.id, TaskState::Completed)
            .unwrap();
        assert!(
            repo.transition_task(&task.id, TaskState::Running).is_err(),
            "a completed task cannot restart"
        );
    }

    #[test]
    fn tasks_become_ready_only_when_their_dependencies_complete() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        let first = task(&repo, &project.id, "Gather", vec![]);
        let second = task(&repo, &project.id, "Write", vec![first.id.clone()]);

        repo.refresh_readiness(&project.id).unwrap();
        assert_eq!(
            repo.get_task(&first.id).unwrap().unwrap().state,
            TaskState::Ready
        );
        assert_eq!(
            repo.get_task(&second.id).unwrap().unwrap().state,
            TaskState::Queued,
            "a dependent task waits"
        );

        repo.transition_task(&first.id, TaskState::Running).unwrap();
        repo.transition_task(&first.id, TaskState::Completed)
            .unwrap();
        repo.refresh_readiness(&project.id).unwrap();
        assert_eq!(
            repo.get_task(&second.id).unwrap().unwrap().state,
            TaskState::Ready
        );
    }

    #[test]
    fn a_task_whose_dependency_failed_is_blocked_not_left_queued() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        let first = task(&repo, &project.id, "Gather", vec![]);
        let second = task(&repo, &project.id, "Write", vec![first.id.clone()]);

        repo.refresh_readiness(&project.id).unwrap();
        repo.transition_task(&first.id, TaskState::Running).unwrap();
        repo.transition_task(&first.id, TaskState::Failed).unwrap();
        repo.refresh_readiness(&project.id).unwrap();

        assert_eq!(
            repo.get_task(&second.id).unwrap().unwrap().state,
            TaskState::Blocked
        );
    }

    #[test]
    fn a_cyclic_plan_is_refused_at_write_time() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        let a = task(&repo, &project.id, "A", vec![]);
        let b = task(&repo, &project.id, "B", vec![a.id.clone()]);

        // Now try to make A depend on B.
        let err = repo
            .add_task(
                &project.id,
                NewTask {
                    title: "C".into(),
                    depends_on: vec![b.id.clone()],
                    ..Default::default()
                },
            )
            .inspect(|c| {
                // C -> B is fine; force the cycle through a direct insert.
                db.conn()
                    .unwrap()
                    .execute(
                        "INSERT INTO task_dependencies (task_id, depends_on_task_id) VALUES (?1, ?2)",
                        params![a.id, c.id],
                    )
                    .unwrap();
            })
            .unwrap();
        let _ = err;

        // Adding any further task now surfaces the cycle.
        let result = repo.add_task(
            &project.id,
            NewTask {
                title: "D".into(),
                depends_on: vec![],
                ..Default::default()
            },
        );
        let message = result.unwrap_err().to_string();
        assert!(message.contains("cycle"), "{message}");
    }

    #[test]
    fn a_dependency_from_another_project_is_refused() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let one = project(&db);
        let two = repo
            .create(NewProject {
                title: "Other".into(),
                ..Default::default()
            })
            .unwrap();
        let foreign = task(&repo, &two.id, "Foreign", vec![]);

        let err = repo
            .add_task(
                &one.id,
                NewTask {
                    title: "Depends".into(),
                    depends_on: vec![foreign.id],
                    ..Default::default()
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a task in this project"), "{err}");
        assert!(repo.tasks(&one.id).unwrap().is_empty());
    }

    #[test]
    fn interrupted_work_recovers_after_a_restart() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        repo.transition(&project.id, ProjectState::Planned).unwrap();
        repo.transition(&project.id, ProjectState::Running).unwrap();

        let running = task(&repo, &project.id, "Running", vec![]);
        let verifying = task(&repo, &project.id, "Verifying", vec![]);
        let waiting = task(&repo, &project.id, "Waiting for me", vec![]);
        repo.refresh_readiness(&project.id).unwrap();
        repo.transition_task(&running.id, TaskState::Running)
            .unwrap();
        repo.transition_task(&verifying.id, TaskState::Running)
            .unwrap();
        repo.transition_task(&verifying.id, TaskState::Verifying)
            .unwrap();
        repo.transition_task(&waiting.id, TaskState::AwaitingApproval)
            .unwrap();

        let recovered = repo.recover_interrupted().unwrap();
        assert_eq!(recovered.len(), 2);
        assert_eq!(
            repo.get_task(&running.id).unwrap().unwrap().state,
            TaskState::Ready
        );
        assert_eq!(
            repo.get_task(&verifying.id).unwrap().unwrap().state,
            TaskState::Ready
        );
        assert_eq!(
            repo.get_task(&running.id)
                .unwrap()
                .unwrap()
                .failure_reason
                .as_deref(),
            Some("Recovered after the application restarted mid-run."),
            "the run drawer should say why the task restarted"
        );
        assert_eq!(
            repo.get_task(&waiting.id).unwrap().unwrap().state,
            TaskState::AwaitingApproval,
            "a task waiting for a person must keep waiting, not restart"
        );
        assert_eq!(
            repo.get(&project.id).unwrap().unwrap().state,
            ProjectState::Blocked,
            "a project must not resume on its own after a crash"
        );
    }

    #[test]
    fn a_failed_task_can_be_requeued_for_rework() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        let task = task(&repo, &project.id, "Draft", vec![]);
        repo.transition_task(&task.id, TaskState::Ready).unwrap();
        repo.transition_task(&task.id, TaskState::Running).unwrap();
        let mut failed = repo.transition_task(&task.id, TaskState::Failed).unwrap();
        failed.attempt = 1;
        failed.failure_reason = Some("verification rejected the draft".into());
        repo.update_task(&failed).unwrap();

        let reworked = repo.transition_task(&task.id, TaskState::Ready).unwrap();
        assert_eq!(reworked.state, TaskState::Ready);
        assert_eq!(reworked.attempt, 1);
        assert_eq!(
            reworked.failure_reason.as_deref(),
            Some("verification rejected the draft"),
            "the reason for rework must survive so the agent can act on it"
        );
    }

    #[test]
    fn artefacts_are_recorded_against_their_project() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        repo.add_artifact(
            &project.id,
            None,
            "report.md",
            "text/markdown",
            "/data/projects/p/report.md",
            2048,
        )
        .unwrap();
        let artifacts = repo.artifacts(&project.id).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name, "report.md");
        assert_eq!(artifacts[0].byte_size, 2048);
    }

    #[test]
    fn deleting_a_project_removes_its_tasks_and_artefacts() {
        let db = Db::open_in_memory().unwrap();
        let repo = ProjectRepo::new(&db);
        let project = project(&db);
        let t = task(&repo, &project.id, "A", vec![]);
        repo.add_artifact(
            &project.id,
            Some(&t.id),
            "r.md",
            "text/markdown",
            "/p/r.md",
            1,
        )
        .unwrap();
        repo.delete(&project.id).unwrap();

        for table in ["tasks", "artifacts", "task_dependencies"] {
            let count: i64 = db
                .conn()
                .unwrap()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "{table} should be empty");
        }
    }
}
