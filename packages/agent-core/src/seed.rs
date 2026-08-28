//! First-run seeding: the shipped agent templates, and demo data the user can
//! ask for. Seeding is idempotent — running it twice adds nothing.

use anyhow::Result;

use otwono_store::repo::agents::{AgentRepo, NewAgent};
use otwono_store::repo::workspaces::{NewWorkspace, WorkspaceRepo};
use otwono_store::Db;
use otwono_types::agent::Agent;
use otwono_types::workspace::WorkspaceKind;

use crate::templates::TEMPLATES;

/// Create any shipped template that is not already present. Returns the agents
/// created by this call.
pub fn seed_templates(db: &Db) -> Result<Vec<Agent>> {
    let repo = AgentRepo::new(db);
    let mut created = Vec::new();

    for template in TEMPLATES {
        if repo.get_by_template_key(template.key)?.is_some() {
            continue;
        }
        created.push(repo.create(NewAgent {
            name: template.name.into(),
            role: template.role.into(),
            description: template.description.into(),
            icon: template.icon.into(),
            system_instructions: template.system_instructions.into(),
            parameters: template.parameters(),
            capabilities: template.capabilities.to_vec(),
            memory_scope: template.memory_scope,
            approval_policy: template.approval_policy,
            max_steps: template.max_steps,
            timeout_seconds: template.timeout_seconds,
            template_key: Some(template.key.into()),
            is_template: true,
            ..Default::default()
        })?);
    }
    Ok(created)
}

/// Demo workspaces, created only when the user asks for them.
pub fn seed_demo_workspaces(db: &Db) -> Result<Vec<String>> {
    let workspaces = WorkspaceRepo::new(db);
    let agents = AgentRepo::new(db);
    let mut created = Vec::new();

    let definitions: &[(WorkspaceKind, &str, &str, &[&str])] = &[
        (
            WorkspaceKind::Office,
            "Demo Operations Office",
            "A standing team that turns objectives into finished work.",
            &[
                "executive-orchestrator",
                "researcher",
                "writer",
                "verification-agent",
            ],
        ),
        (
            WorkspaceKind::Lab,
            "Demo Prompt Lab",
            "Somewhere to compare prompts and models before changing an Office.",
            &["writer", "researcher"],
        ),
        (
            WorkspaceKind::Boardroom,
            "Demo Decision Boardroom",
            "A structured session ending in a chair's synthesis and a dissent summary.",
            &[
                "security-reviewer",
                "budget-reviewer",
                "executive-orchestrator",
            ],
        ),
        (
            WorkspaceKind::ThinkTank,
            "Demo Research Think Tank",
            "Divergent proposals, critique, then a research brief.",
            &["researcher", "designer", "planner"],
        ),
    ];

    for (kind, name, description, members) in definitions {
        if workspaces
            .list(Some(*kind), true)?
            .iter()
            .any(|workspace| workspace.name == *name)
        {
            continue;
        }
        let workspace = workspaces.create(NewWorkspace {
            kind: *kind,
            name: (*name).to_string(),
            description: (*description).to_string(),
            icon: kind.as_str().to_string(),
            shared_instructions: String::new(),
            knowledge_source_ids: Vec::new(),
        })?;

        for (position, key) in members.iter().enumerate() {
            if let Some(agent) = agents.get_by_template_key(key)? {
                workspaces.add_member(&workspace.id, &agent.id, &agent.role, position == 0)?;
            }
        }
        created.push(workspace.id);
    }

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_creates_every_template_once() {
        let db = Db::open_in_memory().unwrap();
        let first = seed_templates(&db).unwrap();
        assert_eq!(first.len(), TEMPLATES.len());

        let second = seed_templates(&db).unwrap();
        assert!(second.is_empty(), "seeding twice must not duplicate agents");
        assert_eq!(
            AgentRepo::new(&db).list(None, true).unwrap().len(),
            TEMPLATES.len()
        );
    }

    #[test]
    fn seeded_agents_carry_their_templates_settings() {
        let db = Db::open_in_memory().unwrap();
        seed_templates(&db).unwrap();
        let repo = AgentRepo::new(&db);

        let verifier = repo
            .get_by_template_key("verification-agent")
            .unwrap()
            .unwrap();
        assert_eq!(verifier.name, "Verification Agent");
        assert_eq!(verifier.parameters.temperature, Some(0.0));
        assert!(verifier.is_template);
        assert!(
            verifier.provider_connection_id.is_none(),
            "a seeded agent has no connection until the user chooses one"
        );
    }

    #[test]
    fn a_deleted_template_is_recreated_by_the_next_seed() {
        let db = Db::open_in_memory().unwrap();
        seed_templates(&db).unwrap();
        let repo = AgentRepo::new(&db);
        let planner = repo.get_by_template_key("planner").unwrap().unwrap();
        repo.delete(&planner.id).unwrap();

        let created = seed_templates(&db).unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].template_key.as_deref(), Some("planner"));
    }

    #[test]
    fn demo_workspaces_cover_every_kind_that_needs_one_and_are_idempotent() {
        let db = Db::open_in_memory().unwrap();
        seed_templates(&db).unwrap();
        let created = seed_demo_workspaces(&db).unwrap();
        assert_eq!(created.len(), 4);
        assert!(seed_demo_workspaces(&db).unwrap().is_empty());

        let workspaces = WorkspaceRepo::new(&db);
        for kind in [
            WorkspaceKind::Office,
            WorkspaceKind::Lab,
            WorkspaceKind::Boardroom,
            WorkspaceKind::ThinkTank,
        ] {
            assert_eq!(
                workspaces.list(Some(kind), true).unwrap().len(),
                1,
                "{kind}"
            );
        }
    }

    #[test]
    fn a_demo_office_has_a_coordinator_and_a_team() {
        let db = Db::open_in_memory().unwrap();
        seed_templates(&db).unwrap();
        seed_demo_workspaces(&db).unwrap();

        let workspaces = WorkspaceRepo::new(&db);
        let office = workspaces
            .list(Some(WorkspaceKind::Office), true)
            .unwrap()
            .remove(0);
        let members = workspaces.members(&office.id).unwrap();
        assert_eq!(members.len(), 4);
        assert_eq!(members.iter().filter(|m| m.is_coordinator).count(), 1);
        assert!(office.coordinator_agent_id.is_some());
    }
}
