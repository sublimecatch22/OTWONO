//! Boardroom and Think Tank sessions end to end.

use otwono_agent_core::executor::scripted::ScriptedExecutor;
use otwono_agent_core::executor::AgentOutcome;
use otwono_agent_core::seed::seed_templates;
use otwono_agent_core::session::SessionRunner;
use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::workspaces::{NewWorkspace, WorkspaceRepo};
use otwono_store::Db;
use otwono_types::workspace::{ClaimKind, SessionStage, WorkspaceKind};

fn workspace_with_three_agents(db: &Db, kind: WorkspaceKind, name: &str) -> String {
    seed_templates(db).unwrap();
    let agents = AgentRepo::new(db);
    for mut agent in agents.list(None, true).unwrap() {
        agent.model = Some("test-model".into());
        agents.update(&agent, None).unwrap();
    }

    let workspaces = WorkspaceRepo::new(db);
    let workspace = workspaces.create(NewWorkspace::named(kind, name)).unwrap();
    for (position, key) in [
        "executive-orchestrator",
        "security-reviewer",
        "budget-reviewer",
    ]
    .iter()
    .enumerate()
    {
        let agent = agents.get_by_template_key(key).unwrap().unwrap();
        workspaces
            .add_member(&workspace.id, &agent.id, &agent.role, position == 0)
            .unwrap();
    }
    workspace.id
}

const SYNTHESIS: &str = "\
## Synthesis

The group concluded that shipping should wait until the audit closes.

## Dissent

Budget Reviewer preferred shipping on Friday and accepting the audit risk.

## Unresolved questions

- Who signs off the audit?
- What is the rollback plan?

## Recommended decision

Delay the release to Monday.";

#[tokio::test]
async fn a_boardroom_runs_positions_critique_and_synthesis() {
    let db = Db::open_in_memory().unwrap();
    let workspace_id = workspace_with_three_agents(&db, WorkspaceKind::Boardroom, "Board");
    let workspaces = WorkspaceRepo::new(&db);
    let session = workspaces
        .create_session(&workspace_id, "Should we ship on Friday?", None)
        .unwrap();
    assert_eq!(session.stage, SessionStage::Positions);

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("You are chairing this session") {
            AgentOutcome::text(SYNTHESIS)
        } else if last.contains("BEGIN POSITIONS") {
            AgentOutcome::text("SPECULATION: I still think Monday is safer.")
        } else {
            AgentOutcome::text("SOURCED: audit-plan.md (page 2) says the audit closes Monday.")
        })
    });

    let finished = SessionRunner::new(&db, &executor)
        .run(&session.id)
        .await
        .unwrap();

    assert_eq!(finished.stage, SessionStage::Completed);
    assert!(finished
        .synthesis
        .unwrap()
        .contains("wait until the audit closes"));
    assert!(finished
        .dissent_summary
        .unwrap()
        .contains("Budget Reviewer preferred shipping on Friday"));
    assert_eq!(finished.unresolved_questions.len(), 2);
    assert_eq!(
        finished.recommended_decision.as_deref(),
        Some("Delay the release to Monday.")
    );

    // Three positions, three critiques, one synthesis.
    let contributions = workspaces.contributions(&session.id).unwrap();
    assert_eq!(contributions.len(), 7);
    assert_eq!(
        contributions
            .iter()
            .filter(|c| c.stage == SessionStage::Positions)
            .count(),
        3
    );
    assert_eq!(
        contributions
            .iter()
            .filter(|c| c.stage == SessionStage::Critique)
            .count(),
        3
    );
    assert_eq!(
        contributions
            .iter()
            .filter(|c| c.stage == SessionStage::Synthesis)
            .count(),
        1
    );
}

#[tokio::test]
async fn positions_are_taken_before_anyone_has_seen_another_position() {
    let db = Db::open_in_memory().unwrap();
    let workspace_id = workspace_with_three_agents(&db, WorkspaceKind::Boardroom, "Board");
    let session = WorkspaceRepo::new(&db)
        .create_session(&workspace_id, "Ship?", None)
        .unwrap();

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("You are chairing this session") {
            AgentOutcome::text(SYNTHESIS)
        } else {
            AgentOutcome::text("A view.")
        })
    });
    SessionRunner::new(&db, &executor)
        .run(&session.id)
        .await
        .unwrap();

    let calls = executor.calls.lock().unwrap();
    // The first three calls are the position round; none may contain another
    // participant's answer.
    for turn in calls.iter().take(3) {
        let prompt = turn.messages.last().unwrap().content.clone();
        assert!(
            !prompt.contains("BEGIN POSITIONS"),
            "a position was asked for with other positions in view"
        );
        assert!(prompt.contains("before hearing anyone else's"));
    }
}

#[tokio::test]
async fn contributions_record_whether_they_were_sourced_or_speculation() {
    let db = Db::open_in_memory().unwrap();
    let workspace_id = workspace_with_three_agents(&db, WorkspaceKind::ThinkTank, "Tank");
    let workspaces = WorkspaceRepo::new(&db);
    let session = workspaces
        .create_session(&workspace_id, "What should we research next?", None)
        .unwrap();

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("You are chairing this session") {
            AgentOutcome::text(
                "## Research brief\n\nFocus on retention.\n\n## Open questions\n\n- Which cohort?",
            )
        } else if last.contains("BEGIN POSITIONS") {
            AgentOutcome::text("SPECULATION: retention may matter more than acquisition.")
        } else {
            AgentOutcome::text("SOURCED: metrics.csv (rows 2-51) shows churn rising.")
        })
    });
    SessionRunner::new(&db, &executor)
        .run(&session.id)
        .await
        .unwrap();

    let contributions = workspaces.contributions(&session.id).unwrap();
    let positions: Vec<_> = contributions
        .iter()
        .filter(|c| c.stage == SessionStage::Positions)
        .collect();
    assert!(positions.iter().all(|c| c.claim_kind == ClaimKind::Sourced));

    let critiques: Vec<_> = contributions
        .iter()
        .filter(|c| c.stage == SessionStage::Critique)
        .collect();
    assert!(critiques
        .iter()
        .all(|c| c.claim_kind == ClaimKind::Speculation));
}

#[tokio::test]
async fn a_session_needs_at_least_two_participants() {
    let db = Db::open_in_memory().unwrap();
    seed_templates(&db).unwrap();
    let agents = AgentRepo::new(&db);
    let mut agent = agents.get_by_template_key("planner").unwrap().unwrap();
    agent.model = Some("test-model".into());
    let agent = agents.update(&agent, None).unwrap();

    let workspaces = WorkspaceRepo::new(&db);
    let workspace = workspaces
        .create(NewWorkspace::named(
            WorkspaceKind::Boardroom,
            "Lonely board",
        ))
        .unwrap();
    let session = workspaces
        .create_session(&workspace.id, "Ship?", None)
        .unwrap();

    let executor = ScriptedExecutor::with_replies(vec![]);
    let runner = SessionRunner::new(&db, &executor);
    let error = runner.run(&session.id).await.unwrap_err().to_string();
    assert!(error.contains("has no agents"), "{error}");

    workspaces
        .add_member(&workspace.id, &agent.id, "Planning", true)
        .unwrap();
    let error = runner.run(&session.id).await.unwrap_err().to_string();
    assert!(error.contains("at least two agents"), "{error}");
}

#[tokio::test]
async fn a_finished_session_cannot_be_run_again() {
    let db = Db::open_in_memory().unwrap();
    let workspace_id = workspace_with_three_agents(&db, WorkspaceKind::Boardroom, "Board");
    let session = WorkspaceRepo::new(&db)
        .create_session(&workspace_id, "Ship?", None)
        .unwrap();

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("You are chairing this session") {
            AgentOutcome::text(SYNTHESIS)
        } else {
            AgentOutcome::text("A view.")
        })
    });
    let runner = SessionRunner::new(&db, &executor);
    runner.run(&session.id).await.unwrap();

    let error = runner.run(&session.id).await.unwrap_err().to_string();
    assert!(error.contains("already finished"), "{error}");
}

#[tokio::test]
async fn the_chair_defaults_to_the_workspace_coordinator() {
    let db = Db::open_in_memory().unwrap();
    let workspace_id = workspace_with_three_agents(&db, WorkspaceKind::Boardroom, "Board");
    let workspaces = WorkspaceRepo::new(&db);
    let session = workspaces
        .create_session(&workspace_id, "Ship?", None)
        .unwrap();
    assert!(session.chair_agent_id.is_none());

    let executor = ScriptedExecutor::responding(|turn| {
        let last = turn.messages.last().unwrap().content.clone();
        Ok(if last.contains("You are chairing this session") {
            AgentOutcome::text(SYNTHESIS)
        } else {
            AgentOutcome::text("A view.")
        })
    });
    let finished = SessionRunner::new(&db, &executor)
        .run(&session.id)
        .await
        .unwrap();

    let coordinator = workspaces
        .get(&workspace_id)
        .unwrap()
        .unwrap()
        .coordinator_agent_id;
    assert_eq!(finished.chair_agent_id, coordinator);
}
