//! The orchestration engine end to end, with a scripted executor standing in
//! for a model. Everything else — persistence, state machines, verification,
//! the activity log — is the real thing.

use otwono_agent_core::executor::scripted::ScriptedExecutor;
use otwono_agent_core::executor::AgentOutcome;
use otwono_agent_core::seed::seed_templates;
use otwono_agent_core::Orchestrator;
use otwono_store::repo::activity::{ActivityQuery, ActivityRepo};
use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::projects::{NewProject, ProjectRepo};
use otwono_store::Db;
use otwono_types::project::{ProjectState, TaskState};

const PLAN: &str = r#"[
  {"title":"Gather the figures","instructions":"Collect Q3 revenue.",
   "acceptance_criteria":["All three months present"],"depends_on":[],"suggested_role":"Research"},
  {"title":"Write the summary","instructions":"Summarise the figures.",
   "acceptance_criteria":["Under 500 words"],"depends_on":[1],"suggested_role":"Writing"}
]"#;

/// A workspace-free project with the shipped templates available, and every
/// seeded agent given a model so it can be run.
fn project_with_agents(db: &Db) -> String {
    seed_templates(db).unwrap();
    let agents = AgentRepo::new(db);
    for mut agent in agents.list(None, true).unwrap() {
        agent.model = Some("test-model".into());
        agents.update(&agent, None).unwrap();
    }

    let orchestrator = agents
        .get_by_template_key("executive-orchestrator")
        .unwrap()
        .unwrap();
    let verifier = agents
        .get_by_template_key("verification-agent")
        .unwrap()
        .unwrap();

    ProjectRepo::new(db)
        .create(NewProject {
            title: "Quarterly report".into(),
            objective: "Produce the Q3 report".into(),
            acceptance_criteria: vec!["Includes revenue".into()],
            orchestrator_agent_id: Some(orchestrator.id),
            verifier_agent_id: Some(verifier.id),
            max_steps: Some(20),
            max_task_retries: Some(1),
            ..Default::default()
        })
        .unwrap()
        .id
}

#[tokio::test]
async fn a_plan_becomes_tasks_with_dependencies_and_assignments() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);
    let executor = ScriptedExecutor::with_replies(vec![PLAN]);

    let tasks = Orchestrator::new(&db, &executor)
        .plan(&project_id)
        .await
        .unwrap();

    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].title, "Gather the figures");
    assert!(tasks[0].depends_on.is_empty());
    assert_eq!(tasks[1].depends_on, vec![tasks[0].id.clone()]);

    // The planner's suggested roles were matched to agents that exist.
    let agents = AgentRepo::new(&db);
    let assigned = agents
        .get(tasks[0].assigned_agent_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(assigned.name, "Researcher");

    let project = ProjectRepo::new(&db).get(&project_id).unwrap().unwrap();
    assert_eq!(project.state, ProjectState::Planned);

    // Readiness reflects the dependency: only the first task can start.
    assert_eq!(tasks[0].state, TaskState::Ready);
    assert_eq!(tasks[1].state, TaskState::Queued);
}

#[tokio::test]
async fn a_planned_project_runs_to_completion_through_verification() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);

    // plan, task 1, verify 1, task 2, verify 2
    let executor = ScriptedExecutor::with_replies(vec![
        PLAN,
        "Revenue was 1.2m, 1.4m and 1.6m.",
        "VERDICT: pass\n1. Met — all three months are present.",
        "Q3 revenue grew steadily across the quarter.",
        "VERDICT: pass\n1. Met — the summary is 12 words.",
    ]);

    let orchestrator = Orchestrator::new(&db, &executor);
    orchestrator.plan(&project_id).await.unwrap();
    let report = orchestrator.run(&project_id).await.unwrap();

    assert_eq!(report.tasks_completed, 2);
    assert_eq!(report.tasks_failed, 0);
    assert_eq!(report.final_state, "completed");
    assert!(report.stopped_because.contains("verified"));

    let projects = ProjectRepo::new(&db);
    let tasks = projects.tasks(&project_id).unwrap();
    assert!(tasks.iter().all(|task| task.state == TaskState::Completed));
    assert!(tasks[0].output.as_deref().unwrap().contains("1.2m"));
    assert!(tasks[0].verification_notes.is_some());
}

#[tokio::test]
async fn failed_verification_sends_the_task_back_with_instructions() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);

    let executor = ScriptedExecutor::with_replies(vec![
        r#"[{"title":"Write the summary","acceptance_criteria":["Mentions revenue"],"suggested_role":"Writing"}]"#,
        "A summary with no numbers in it.",
        "VERDICT: fail\n1. Not met — no revenue figures.\nREQUIRED CHANGES: Include the revenue figures for each month.",
        "Revenue was 1.2m, 1.4m and 1.6m across the quarter.",
        "VERDICT: pass\n1. Met — the figures are present.",
    ]);

    let orchestrator = Orchestrator::new(&db, &executor);
    orchestrator.plan(&project_id).await.unwrap();
    let report = orchestrator.run(&project_id).await.unwrap();

    assert_eq!(report.tasks_reworked, 1);
    assert_eq!(report.tasks_completed, 1);
    assert_eq!(report.final_state, "completed");

    let task = ProjectRepo::new(&db).tasks(&project_id).unwrap().remove(0);
    assert_eq!(task.attempt, 2);
    assert_eq!(task.state, TaskState::Completed);

    // The second attempt was told exactly what to change.
    let prompts: Vec<String> = executor
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|turn| turn.messages.last().unwrap().content.clone())
        .collect();
    assert!(
        prompts
            .iter()
            .any(|p| p.contains("attempt 2 of 2") && p.contains("Include the revenue figures")),
        "the rework prompt should carry the verifier's instructions"
    );
}

#[tokio::test]
async fn retries_are_bounded_and_the_project_fails_rather_than_looping() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("Produce a plan as a JSON array") {
            AgentOutcome::text(
                r#"[{"title":"Impossible task","acceptance_criteria":["Cannot be met"]}]"#,
            )
        } else if last.contains("VERDICT: pass or fail") {
            AgentOutcome::text("VERDICT: fail\nREQUIRED CHANGES: Everything.")
        } else {
            AgentOutcome::text("An attempt.")
        })
    });

    let orchestrator = Orchestrator::new(&db, &executor);
    orchestrator.plan(&project_id).await.unwrap();
    let report = orchestrator.run(&project_id).await.unwrap();

    assert_eq!(report.tasks_failed, 1);
    assert_eq!(report.final_state, "failed");

    let task = ProjectRepo::new(&db).tasks(&project_id).unwrap().remove(0);
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.attempt, 2, "one attempt plus one retry, as configured");
    assert!(
        report.steps_used < 20,
        "the run stopped without exhausting the step budget"
    );
}

#[tokio::test]
async fn the_step_budget_stops_a_run_that_would_otherwise_continue() {
    let db = Db::open_in_memory().unwrap();
    seed_templates(&db).unwrap();
    let agents = AgentRepo::new(&db);
    for mut agent in agents.list(None, true).unwrap() {
        agent.model = Some("test-model".into());
        agents.update(&agent, None).unwrap();
    }
    let orchestrator_agent = agents
        .get_by_template_key("executive-orchestrator")
        .unwrap()
        .unwrap();
    let verifier = agents
        .get_by_template_key("verification-agent")
        .unwrap()
        .unwrap();

    let project_id = ProjectRepo::new(&db)
        .create(NewProject {
            title: "Big project".into(),
            objective: "Do many things".into(),
            orchestrator_agent_id: Some(orchestrator_agent.id),
            verifier_agent_id: Some(verifier.id),
            max_steps: Some(2),
            max_task_retries: Some(0),
            ..Default::default()
        })
        .unwrap()
        .id;

    let executor = ScriptedExecutor::responding(move |turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("Produce a plan as a JSON array") {
            AgentOutcome::text(
                r#"[{"title":"T1"},{"title":"T2"},{"title":"T3"},{"title":"T4"},{"title":"T5"},{"title":"T6"}]"#,
            )
        } else if last.contains("VERDICT: pass or fail") {
            AgentOutcome::text("VERDICT: pass")
        } else {
            AgentOutcome::text("Done.")
        })
    });

    let engine = Orchestrator::new(&db, &executor);
    engine.plan(&project_id).await.unwrap();
    let report = engine.run(&project_id).await.unwrap();

    assert_eq!(report.steps_used, 2, "the run stopped at its budget");
    assert!(report.stopped_because.contains("limit of 2 steps"));
    assert_eq!(report.final_state, "blocked");

    // Running again continues from where it stopped rather than restarting.
    let second = engine.run(&project_id).await.unwrap();
    assert_eq!(second.steps_used, 2);
    assert_eq!(
        ProjectRepo::new(&db)
            .tasks(&project_id)
            .unwrap()
            .iter()
            .filter(|task| task.state == TaskState::Completed)
            .count(),
        4
    );
}

#[tokio::test]
async fn a_task_that_needs_approval_stops_the_run_until_a_person_answers() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("Produce a plan as a JSON array") {
            AgentOutcome::text(
                r#"[{"title":"Publish the report","requires_approval":true},
                     {"title":"Archive the working notes","depends_on":[1]}]"#,
            )
        } else if last.contains("VERDICT: pass or fail") {
            AgentOutcome::text("VERDICT: pass")
        } else {
            AgentOutcome::text("Done.")
        })
    });

    let engine = Orchestrator::new(&db, &executor);
    engine.plan(&project_id).await.unwrap();
    let report = engine.run(&project_id).await.unwrap();

    assert_eq!(report.awaiting_approval.len(), 1);
    assert_eq!(report.final_state, "awaiting_approval");
    assert!(report.stopped_because.contains("your approval"));
    assert_eq!(report.tasks_completed, 0, "nothing ran past the gate");

    let projects = ProjectRepo::new(&db);
    let tasks = projects.tasks(&project_id).unwrap();
    assert_eq!(tasks[0].state, TaskState::AwaitingApproval);
    assert_eq!(tasks[1].state, TaskState::Queued);

    // Approving lets the run continue.
    engine.approve_task(&tasks[0].id).unwrap();
    let after = engine.run(&project_id).await.unwrap();
    assert_eq!(after.tasks_completed, 2);
    assert_eq!(after.final_state, "completed");
}

#[tokio::test]
async fn declining_a_gated_task_cancels_it_and_blocks_what_depended_on_it() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("Produce a plan as a JSON array") {
            AgentOutcome::text(
                r#"[{"title":"Send the email","requires_approval":true},
                     {"title":"Record that it was sent","depends_on":[1]}]"#,
            )
        } else {
            AgentOutcome::text("Done.")
        })
    });

    let engine = Orchestrator::new(&db, &executor);
    engine.plan(&project_id).await.unwrap();
    engine.run(&project_id).await.unwrap();

    let projects = ProjectRepo::new(&db);
    let gated = projects.tasks(&project_id).unwrap().remove(0);
    engine
        .decline_task(&gated.id, "I do not want to send this")
        .unwrap();

    let report = engine.run(&project_id).await.unwrap();
    let tasks = projects.tasks(&project_id).unwrap();
    assert_eq!(tasks[0].state, TaskState::Cancelled);
    assert!(tasks[0]
        .failure_reason
        .as_deref()
        .unwrap()
        .contains("Declined by the user"));
    assert_eq!(
        tasks[1].state,
        TaskState::Blocked,
        "the dependent task cannot proceed"
    );
    assert!(report.stopped_because.contains("did not complete"));
}

#[tokio::test]
async fn an_interrupted_run_recovers_and_finishes_on_the_next_start() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);
    let projects = ProjectRepo::new(&db);

    let executor = ScriptedExecutor::with_replies(vec![PLAN]);
    Orchestrator::new(&db, &executor)
        .plan(&project_id)
        .await
        .unwrap();

    // Simulate a crash: a task left running and the project left running.
    projects
        .transition(&project_id, ProjectState::Running)
        .unwrap();
    let first = projects.tasks(&project_id).unwrap().remove(0);
    projects
        .transition_task(&first.id, TaskState::Running)
        .unwrap();

    // Start-up recovery.
    let recovered = projects.recover_interrupted().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(
        projects.get(&project_id).unwrap().unwrap().state,
        ProjectState::Blocked
    );

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("VERDICT: pass or fail") {
            AgentOutcome::text("VERDICT: pass")
        } else {
            AgentOutcome::text("Done.")
        })
    });
    let report = Orchestrator::new(&db, &executor)
        .run(&project_id)
        .await
        .unwrap();

    assert_eq!(report.tasks_completed, 2);
    assert_eq!(report.final_state, "completed");
}

#[tokio::test]
async fn a_project_without_a_verifier_does_not_pass_work_by_default() {
    let db = Db::open_in_memory().unwrap();
    seed_templates(&db).unwrap();
    let agents = AgentRepo::new(&db);
    let mut orchestrator_agent = agents
        .get_by_template_key("executive-orchestrator")
        .unwrap()
        .unwrap();
    orchestrator_agent.model = Some("test-model".into());
    let orchestrator_agent = agents.update(&orchestrator_agent, None).unwrap();

    let project_id = ProjectRepo::new(&db)
        .create(NewProject {
            title: "Unverified".into(),
            objective: "Do a thing".into(),
            orchestrator_agent_id: Some(orchestrator_agent.id),
            verifier_agent_id: None,
            max_steps: Some(10),
            max_task_retries: Some(0),
            ..Default::default()
        })
        .unwrap()
        .id;

    let executor = ScriptedExecutor::with_replies(vec![r#"[{"title":"Do it"}]"#, "Done."]);
    let engine = Orchestrator::new(&db, &executor);
    engine.plan(&project_id).await.unwrap();
    let report = engine.run(&project_id).await.unwrap();

    assert_eq!(report.tasks_completed, 0);
    assert_eq!(report.tasks_failed, 1);
    let task = ProjectRepo::new(&db).tasks(&project_id).unwrap().remove(0);
    assert!(
        task.verification_notes
            .as_deref()
            .unwrap()
            .contains("was not checked"),
        "the user must be told the work was not verified"
    );
}

#[tokio::test]
async fn every_step_is_written_to_the_activity_log() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);
    let executor = ScriptedExecutor::with_replies(vec![
        PLAN,
        "Figures.",
        "VERDICT: pass",
        "Summary.",
        "VERDICT: pass",
    ]);

    let engine = Orchestrator::new(&db, &executor);
    engine.plan(&project_id).await.unwrap();
    engine.run(&project_id).await.unwrap();

    let entries = ActivityRepo::new(&db)
        .list(&ActivityQuery {
            project_id: Some(project_id.clone()),
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();

    for expected in [
        "project.plan",
        "task.executed",
        "task.verified",
        "project.run",
    ] {
        assert!(
            actions.contains(&expected),
            "missing {expected} in {actions:?}"
        );
    }
    assert_eq!(
        actions.iter().filter(|a| **a == "task.executed").count(),
        2,
        "one entry per task execution"
    );
}

#[tokio::test]
async fn the_completion_report_shows_the_work_and_its_verification() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);
    let executor = ScriptedExecutor::with_replies(vec![
        PLAN,
        "Revenue was 1.2m, 1.4m and 1.6m.",
        "VERDICT: pass — all three months present.",
        "Q3 revenue grew steadily.",
        "VERDICT: pass — under 500 words.",
    ]);

    let engine = Orchestrator::new(&db, &executor);
    engine.plan(&project_id).await.unwrap();
    engine.run(&project_id).await.unwrap();
    let report = engine.completion_report(&project_id).unwrap();

    assert!(report.contains("# Quarterly report"));
    assert!(report.contains("**State:** completed"));
    assert!(
        report.contains("Includes revenue"),
        "acceptance criteria are shown"
    );
    assert!(report.contains("Gather the figures"));
    assert!(report.contains("1.2m"), "the actual output is included");
    assert!(report.contains("*Verification:*"));
}

#[tokio::test]
async fn a_project_cannot_be_planned_twice() {
    let db = Db::open_in_memory().unwrap();
    let project_id = project_with_agents(&db);
    let executor = ScriptedExecutor::with_replies(vec![PLAN, PLAN]);
    let engine = Orchestrator::new(&db, &executor);

    engine.plan(&project_id).await.unwrap();
    let error = engine.plan(&project_id).await.unwrap_err().to_string();
    assert!(error.contains("cannot be re-planned"), "{error}");
    assert_eq!(ProjectRepo::new(&db).tasks(&project_id).unwrap().len(), 2);
}

#[tokio::test]
async fn a_project_with_no_orchestrator_says_what_to_do_about_it() {
    let db = Db::open_in_memory().unwrap();
    let project_id = ProjectRepo::new(&db)
        .create(NewProject {
            title: "Orphan".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let executor = ScriptedExecutor::with_replies(vec![PLAN]);
    let error = Orchestrator::new(&db, &executor)
        .plan(&project_id)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no orchestrator agent"), "{error}");
    assert!(
        error.contains("project's settings"),
        "the message should say where to fix it"
    );
}
