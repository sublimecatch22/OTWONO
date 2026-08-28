//! Workspaces (Chat, Office, Lab, Boardroom, Think Tank), their members, and
//! the structured sessions Boardrooms and Think Tanks run.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use otwono_types::workspace::{
    ClaimKind, Session, SessionContribution, SessionStage, Workspace, WorkspaceKind,
    WorkspaceMember,
};

use crate::Db;

const WS_COLUMNS: &str = "id, kind, name, description, icon, shared_instructions, \
    knowledge_source_ids, coordinator_agent_id, favorite, archived, created_at, updated_at";

fn map_workspace(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: row.get(0)?,
        kind: WorkspaceKind::parse(&row.get::<_, String>(1)?).unwrap_or(WorkspaceKind::Chat),
        name: row.get(2)?,
        description: row.get(3)?,
        icon: row.get(4)?,
        shared_instructions: row.get(5)?,
        knowledge_source_ids: crate::json_column(row.get(6)?),
        coordinator_agent_id: row.get(7)?,
        favorite: row.get::<_, i64>(8)? != 0,
        archived: row.get::<_, i64>(9)? != 0,
        created_at: crate::parse_ts(&row.get::<_, String>(10)?),
        updated_at: crate::parse_ts(&row.get::<_, String>(11)?),
    })
}

#[derive(Debug, Clone)]
pub struct NewWorkspace {
    pub kind: WorkspaceKind,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub shared_instructions: String,
    pub knowledge_source_ids: Vec<String>,
}

impl NewWorkspace {
    pub fn named(kind: WorkspaceKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            description: String::new(),
            icon: kind.as_str().to_string(),
            shared_instructions: String::new(),
            knowledge_source_ids: Vec::new(),
        }
    }
}

/// One configuration under test in a Lab.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabVariant {
    pub id: String,
    pub label: String,
    pub agent_id: Option<String>,
    pub provider_connection_id: Option<String>,
    pub model: Option<String>,
    pub system_instructions: Option<String>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabResult {
    pub variant_id: String,
    pub output: String,
    pub error: Option<String>,
    pub latency_ms: u64,
    pub token_estimate: Option<u32>,
    pub ran_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabExperiment {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub prompt: String,
    pub variants: Vec<LabVariant>,
    pub results: Vec<LabResult>,
    pub promoted_variant: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct WorkspaceRepo<'a> {
    db: &'a Db,
}

impl<'a> WorkspaceRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn create(&self, new: NewWorkspace) -> Result<Workspace> {
        if new.name.trim().is_empty() {
            bail!("a workspace needs a name");
        }
        let id = otwono_types::new_id("wsp");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO workspaces
               (id, kind, name, description, icon, shared_instructions, knowledge_source_ids,
                favorite, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, ?8, ?8)",
            params![
                id,
                new.kind.as_str(),
                new.name.trim(),
                new.description,
                new.icon,
                new.shared_instructions,
                crate::to_json(&new.knowledge_source_ids),
                now
            ],
        )?;
        self.get(&id)?
            .ok_or_else(|| anyhow::anyhow!("workspace not found after creation"))
    }

    pub fn get(&self, id: &str) -> Result<Option<Workspace>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {WS_COLUMNS} FROM workspaces WHERE id = ?1"),
                [id],
                map_workspace,
            )
            .optional()?)
    }

    pub fn list(
        &self,
        kind: Option<WorkspaceKind>,
        include_archived: bool,
    ) -> Result<Vec<Workspace>> {
        let conn = self.db.conn()?;
        let mut sql = format!("SELECT {WS_COLUMNS} FROM workspaces WHERE 1 = 1");
        let mut binds: Vec<String> = Vec::new();
        if let Some(kind) = kind {
            sql.push_str(" AND kind = ?");
            binds.push(kind.as_str().to_string());
        }
        if !include_archived {
            sql.push_str(" AND archived = 0");
        }
        sql.push_str(" ORDER BY favorite DESC, updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), map_workspace)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update(&self, workspace: &Workspace) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE workspaces SET name = ?2, description = ?3, icon = ?4,
                    shared_instructions = ?5, knowledge_source_ids = ?6,
                    coordinator_agent_id = ?7, favorite = ?8, archived = ?9, updated_at = ?10
              WHERE id = ?1",
            params![
                workspace.id,
                workspace.name,
                workspace.description,
                workspace.icon,
                workspace.shared_instructions,
                crate::to_json(&workspace.knowledge_source_ids),
                workspace.coordinator_agent_id,
                workspace.favorite as i64,
                workspace.archived as i64,
                crate::now_str(),
            ],
        )?;
        Ok(())
    }

    /// Copy a workspace and its membership under a new name.
    pub fn duplicate(&self, id: &str, new_name: &str) -> Result<Workspace> {
        let source = self
            .get(id)?
            .ok_or_else(|| anyhow::anyhow!("workspace {id} does not exist"))?;
        let copy = self.create(NewWorkspace {
            kind: source.kind,
            name: new_name.to_string(),
            description: source.description.clone(),
            icon: source.icon.clone(),
            shared_instructions: source.shared_instructions.clone(),
            knowledge_source_ids: source.knowledge_source_ids.clone(),
        })?;
        for member in self.members(id)? {
            self.add_member(
                &copy.id,
                &member.agent_id,
                &member.job_role,
                member.is_coordinator,
            )?;
        }
        Ok(copy)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- membership

    pub fn add_member(
        &self,
        workspace_id: &str,
        agent_id: &str,
        job_role: &str,
        is_coordinator: bool,
    ) -> Result<()> {
        let conn = self.db.conn()?;
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM workspace_members WHERE workspace_id = ?1",
            [workspace_id],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO workspace_members (workspace_id, agent_id, job_role, is_coordinator, ordinal)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(workspace_id, agent_id)
             DO UPDATE SET job_role = excluded.job_role, is_coordinator = excluded.is_coordinator",
            params![workspace_id, agent_id, job_role, is_coordinator as i64, next],
        )?;
        if is_coordinator {
            // Exactly one coordinator per workspace.
            conn.execute(
                "UPDATE workspace_members SET is_coordinator = 0
                  WHERE workspace_id = ?1 AND agent_id <> ?2",
                params![workspace_id, agent_id],
            )?;
            conn.execute(
                "UPDATE workspaces SET coordinator_agent_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![workspace_id, agent_id, crate::now_str()],
            )?;
        }
        Ok(())
    }

    pub fn remove_member(&self, workspace_id: &str, agent_id: &str) -> Result<()> {
        let conn = self.db.conn()?;
        conn.execute(
            "DELETE FROM workspace_members WHERE workspace_id = ?1 AND agent_id = ?2",
            params![workspace_id, agent_id],
        )?;
        conn.execute(
            "UPDATE workspaces SET coordinator_agent_id = NULL
              WHERE id = ?1 AND coordinator_agent_id = ?2",
            params![workspace_id, agent_id],
        )?;
        Ok(())
    }

    pub fn members(&self, workspace_id: &str) -> Result<Vec<WorkspaceMember>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT workspace_id, agent_id, job_role, is_coordinator, ordinal
               FROM workspace_members WHERE workspace_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map([workspace_id], |row| {
            Ok(WorkspaceMember {
                workspace_id: row.get(0)?,
                agent_id: row.get(1)?,
                job_role: row.get(2)?,
                is_coordinator: row.get::<_, i64>(3)? != 0,
                ordinal: row.get::<_, i64>(4)? as u32,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- sessions

    pub fn create_session(
        &self,
        workspace_id: &str,
        question: &str,
        chair_agent_id: Option<&str>,
    ) -> Result<Session> {
        let workspace = self
            .get(workspace_id)?
            .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} does not exist"))?;
        if !workspace.kind.is_session_based() {
            bail!(
                "{} workspaces do not run structured sessions",
                workspace.kind.display_name()
            );
        }
        let id = otwono_types::new_id("ses");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO sessions (id, workspace_id, question, stage, chair_agent_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'positions', ?4, ?5, ?5)",
            params![id, workspace_id, question, chair_agent_id, now],
        )?;
        self.get_session(&id)?
            .ok_or_else(|| anyhow::anyhow!("session not found after creation"))
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                "SELECT id, workspace_id, question, stage, chair_agent_id, synthesis,
                        dissent_summary, unresolved_questions, recommended_decision,
                        created_at, updated_at
                   FROM sessions WHERE id = ?1",
                [id],
                |row| {
                    Ok(Session {
                        id: row.get(0)?,
                        workspace_id: row.get(1)?,
                        question: row.get(2)?,
                        stage: SessionStage::parse(&row.get::<_, String>(3)?)
                            .unwrap_or(SessionStage::Positions),
                        chair_agent_id: row.get(4)?,
                        synthesis: row.get(5)?,
                        dissent_summary: row.get(6)?,
                        unresolved_questions: crate::json_column(row.get(7)?),
                        recommended_decision: row.get(8)?,
                        created_at: crate::parse_ts(&row.get::<_, String>(9)?),
                        updated_at: crate::parse_ts(&row.get::<_, String>(10)?),
                    })
                },
            )
            .optional()?)
    }

    pub fn list_sessions(&self, workspace_id: &str) -> Result<Vec<Session>> {
        let conn = self.db.conn()?;
        let mut stmt = conn
            .prepare("SELECT id FROM sessions WHERE workspace_id = ?1 ORDER BY created_at DESC")?;
        let ids: Vec<String> = stmt
            .query_map([workspace_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        ids.iter()
            .filter_map(|id| self.get_session(id).transpose())
            .collect()
    }

    pub fn update_session(&self, session: &Session) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE sessions SET stage = ?2, chair_agent_id = ?3, synthesis = ?4,
                    dissent_summary = ?5, unresolved_questions = ?6,
                    recommended_decision = ?7, updated_at = ?8
              WHERE id = ?1",
            params![
                session.id,
                session.stage.as_str(),
                session.chair_agent_id,
                session.synthesis,
                session.dissent_summary,
                crate::to_json(&session.unresolved_questions),
                session.recommended_decision,
                crate::now_str(),
            ],
        )?;
        Ok(())
    }

    pub fn add_contribution(
        &self,
        session_id: &str,
        agent_id: &str,
        agent_name: &str,
        stage: SessionStage,
        content: &str,
        claim_kind: ClaimKind,
        citations: &[otwono_types::chat::Citation],
    ) -> Result<SessionContribution> {
        let id = otwono_types::new_id("con");
        let conn = self.db.conn()?;
        let ordinal: i64 = conn.query_row(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM session_contributions WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )?;
        let now = crate::now_str();
        conn.execute(
            "INSERT INTO session_contributions
               (id, session_id, agent_id, agent_name, stage, content, claim_kind, citations, ordinal, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                session_id,
                agent_id,
                agent_name,
                stage.as_str(),
                content,
                match claim_kind {
                    ClaimKind::Sourced => "sourced",
                    ClaimKind::Speculation => "speculation",
                },
                crate::to_json(&citations),
                ordinal,
                now
            ],
        )?;
        Ok(SessionContribution {
            id,
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            agent_name: agent_name.into(),
            stage,
            content: content.into(),
            claim_kind,
            citations: citations.to_vec(),
            created_at: crate::parse_ts(&now),
        })
    }

    pub fn contributions(&self, session_id: &str) -> Result<Vec<SessionContribution>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, agent_id, agent_name, stage, content, claim_kind, citations, created_at
               FROM session_contributions WHERE session_id = ?1 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(SessionContribution {
                id: row.get(0)?,
                session_id: row.get(1)?,
                agent_id: row.get(2)?,
                agent_name: row.get(3)?,
                stage: SessionStage::parse(&row.get::<_, String>(4)?)
                    .unwrap_or(SessionStage::Positions),
                content: row.get(5)?,
                claim_kind: if row.get::<_, String>(6)? == "sourced" {
                    ClaimKind::Sourced
                } else {
                    ClaimKind::Speculation
                },
                citations: crate::json_column(row.get(7)?),
                created_at: crate::parse_ts(&row.get::<_, String>(8)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- lab experiments

    pub fn create_experiment(
        &self,
        workspace_id: &str,
        name: &str,
        prompt: &str,
        variants: &[LabVariant],
    ) -> Result<LabExperiment> {
        let id = otwono_types::new_id("exp");
        let now = crate::now_str();
        self.db.conn()?.execute(
            "INSERT INTO lab_experiments (id, workspace_id, name, prompt, variants, results, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?6)",
            params![id, workspace_id, name, prompt, crate::to_json(&variants), now],
        )?;
        self.get_experiment(&id)?
            .ok_or_else(|| anyhow::anyhow!("experiment not found after creation"))
    }

    pub fn get_experiment(&self, id: &str) -> Result<Option<LabExperiment>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                "SELECT id, workspace_id, name, prompt, variants, results, promoted_variant, created_at, updated_at
                   FROM lab_experiments WHERE id = ?1",
                [id],
                |row| {
                    Ok(LabExperiment {
                        id: row.get(0)?,
                        workspace_id: row.get(1)?,
                        name: row.get(2)?,
                        prompt: row.get(3)?,
                        variants: crate::json_column(row.get(4)?),
                        results: crate::json_column(row.get(5)?),
                        promoted_variant: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_experiments(&self, workspace_id: &str) -> Result<Vec<LabExperiment>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM lab_experiments WHERE workspace_id = ?1 ORDER BY created_at DESC",
        )?;
        let ids: Vec<String> = stmt
            .query_map([workspace_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        drop(conn);
        ids.iter()
            .filter_map(|id| self.get_experiment(id).transpose())
            .collect()
    }

    pub fn save_experiment_results(
        &self,
        id: &str,
        results: &[LabResult],
        promoted_variant: Option<&str>,
    ) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE lab_experiments SET results = ?2, promoted_variant = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, crate::to_json(&results), promoted_variant, crate::now_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_row(db: &Db, id: &str, name: &str) {
        db.conn()
            .unwrap()
            .execute(
                "INSERT INTO agents (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![id, name, crate::now_str()],
            )
            .unwrap();
    }

    #[test]
    fn all_five_workspace_kinds_can_be_created_and_listed() {
        let db = Db::open_in_memory().unwrap();
        let repo = WorkspaceRepo::new(&db);
        for kind in WorkspaceKind::ALL {
            repo.create(NewWorkspace::named(
                kind,
                format!("My {}", kind.display_name()),
            ))
            .unwrap();
        }
        assert_eq!(repo.list(None, false).unwrap().len(), 5);
        for kind in WorkspaceKind::ALL {
            assert_eq!(repo.list(Some(kind), false).unwrap().len(), 1);
        }
    }

    #[test]
    fn workspaces_persist_edits_and_archive_out_of_the_default_list() {
        let db = Db::open_in_memory().unwrap();
        let repo = WorkspaceRepo::new(&db);
        let mut office = repo
            .create(NewWorkspace::named(WorkspaceKind::Office, "Ops"))
            .unwrap();
        office.description = "Daily operations".into();
        office.shared_instructions = "Be concise.".into();
        office.favorite = true;
        repo.update(&office).unwrap();

        let reloaded = repo.get(&office.id).unwrap().unwrap();
        assert_eq!(reloaded.description, "Daily operations");
        assert_eq!(reloaded.shared_instructions, "Be concise.");
        assert!(reloaded.favorite);

        let mut archived = reloaded;
        archived.archived = true;
        repo.update(&archived).unwrap();
        assert!(repo.list(None, false).unwrap().is_empty());
        assert_eq!(repo.list(None, true).unwrap().len(), 1);
    }

    #[test]
    fn a_workspace_has_at_most_one_coordinator() {
        let db = Db::open_in_memory().unwrap();
        agent_row(&db, "agt_1", "Exec");
        agent_row(&db, "agt_2", "Planner");
        let repo = WorkspaceRepo::new(&db);
        let office = repo
            .create(NewWorkspace::named(WorkspaceKind::Office, "Ops"))
            .unwrap();

        repo.add_member(&office.id, "agt_1", "Executive", true)
            .unwrap();
        repo.add_member(&office.id, "agt_2", "Planner", true)
            .unwrap();

        let members = repo.members(&office.id).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members.iter().filter(|m| m.is_coordinator).count(), 1);
        assert_eq!(
            repo.get(&office.id)
                .unwrap()
                .unwrap()
                .coordinator_agent_id
                .as_deref(),
            Some("agt_2")
        );
    }

    #[test]
    fn removing_the_coordinator_clears_the_workspace_pointer() {
        let db = Db::open_in_memory().unwrap();
        agent_row(&db, "agt_1", "Exec");
        let repo = WorkspaceRepo::new(&db);
        let office = repo
            .create(NewWorkspace::named(WorkspaceKind::Office, "Ops"))
            .unwrap();
        repo.add_member(&office.id, "agt_1", "Executive", true)
            .unwrap();
        repo.remove_member(&office.id, "agt_1").unwrap();
        assert!(repo
            .get(&office.id)
            .unwrap()
            .unwrap()
            .coordinator_agent_id
            .is_none());
        assert!(repo.members(&office.id).unwrap().is_empty());
    }

    #[test]
    fn duplicating_a_workspace_copies_its_team() {
        let db = Db::open_in_memory().unwrap();
        agent_row(&db, "agt_1", "Exec");
        agent_row(&db, "agt_2", "Writer");
        let repo = WorkspaceRepo::new(&db);
        let office = repo
            .create(NewWorkspace::named(WorkspaceKind::Office, "Ops"))
            .unwrap();
        repo.add_member(&office.id, "agt_1", "Executive", true)
            .unwrap();
        repo.add_member(&office.id, "agt_2", "Writer", false)
            .unwrap();

        let copy = repo.duplicate(&office.id, "Ops (copy)").unwrap();
        assert_ne!(copy.id, office.id);
        assert_eq!(copy.name, "Ops (copy)");
        assert_eq!(repo.members(&copy.id).unwrap().len(), 2);
    }

    #[test]
    fn only_boardrooms_and_think_tanks_accept_sessions() {
        let db = Db::open_in_memory().unwrap();
        let repo = WorkspaceRepo::new(&db);
        let office = repo
            .create(NewWorkspace::named(WorkspaceKind::Office, "Ops"))
            .unwrap();
        let err = repo
            .create_session(&office.id, "Should we ship?", None)
            .unwrap_err();
        assert!(
            err.to_string().contains("do not run structured sessions"),
            "{err}"
        );

        let board = repo
            .create(NewWorkspace::named(WorkspaceKind::Boardroom, "Board"))
            .unwrap();
        let session = repo
            .create_session(&board.id, "Should we ship?", None)
            .unwrap();
        assert_eq!(session.stage, SessionStage::Positions);
    }

    #[test]
    fn a_session_records_contributions_synthesis_and_dissent() {
        let db = Db::open_in_memory().unwrap();
        let repo = WorkspaceRepo::new(&db);
        agent_row(&db, "agt_chair", "Chair");
        let board = repo
            .create(NewWorkspace::named(WorkspaceKind::Boardroom, "Board"))
            .unwrap();
        let mut session = repo
            .create_session(&board.id, "Ship on Friday?", Some("agt_chair"))
            .unwrap();

        repo.add_contribution(
            &session.id,
            "agt_1",
            "Security",
            SessionStage::Positions,
            "Not until the audit closes.",
            ClaimKind::Sourced,
            &[],
        )
        .unwrap();
        repo.add_contribution(
            &session.id,
            "agt_2",
            "Delivery",
            SessionStage::Positions,
            "Friday is feasible.",
            ClaimKind::Speculation,
            &[],
        )
        .unwrap();

        session.stage = SessionStage::Completed;
        session.synthesis = Some("Ship Monday.".into());
        session.dissent_summary = Some("Delivery preferred Friday.".into());
        session.unresolved_questions = vec!["Who signs off the audit?".into()];
        session.recommended_decision = Some("Delay to Monday".into());
        repo.update_session(&session).unwrap();

        let reloaded = repo.get_session(&session.id).unwrap().unwrap();
        assert_eq!(reloaded.synthesis.as_deref(), Some("Ship Monday."));
        assert_eq!(
            reloaded.dissent_summary.as_deref(),
            Some("Delivery preferred Friday.")
        );
        assert_eq!(reloaded.unresolved_questions.len(), 1);

        let contributions = repo.contributions(&session.id).unwrap();
        assert_eq!(contributions.len(), 2);
        assert_eq!(contributions[0].agent_name, "Security");
        assert_eq!(contributions[0].claim_kind, ClaimKind::Sourced);
        assert_eq!(contributions[1].claim_kind, ClaimKind::Speculation);
        assert_eq!(repo.list_sessions(&board.id).unwrap().len(), 1);
    }

    #[test]
    fn lab_experiments_store_variants_results_and_a_promotion() {
        let db = Db::open_in_memory().unwrap();
        let repo = WorkspaceRepo::new(&db);
        let lab = repo
            .create(NewWorkspace::named(WorkspaceKind::Lab, "Prompt lab"))
            .unwrap();
        let variants = vec![
            LabVariant {
                id: "v1".into(),
                label: "Terse".into(),
                agent_id: None,
                provider_connection_id: None,
                model: Some("m".into()),
                system_instructions: Some("Be terse.".into()),
                temperature: Some(0.2),
            },
            LabVariant {
                id: "v2".into(),
                label: "Verbose".into(),
                agent_id: None,
                provider_connection_id: None,
                model: Some("m".into()),
                system_instructions: Some("Explain fully.".into()),
                temperature: Some(0.9),
            },
        ];
        let experiment = repo
            .create_experiment(&lab.id, "Tone test", "Summarise this.", &variants)
            .unwrap();
        assert_eq!(experiment.variants.len(), 2);

        repo.save_experiment_results(
            &experiment.id,
            &[LabResult {
                variant_id: "v1".into(),
                output: "Short.".into(),
                error: None,
                latency_ms: 120,
                token_estimate: Some(8),
                ran_at: crate::now_str(),
            }],
            Some("v1"),
        )
        .unwrap();

        let reloaded = repo.get_experiment(&experiment.id).unwrap().unwrap();
        assert_eq!(reloaded.results.len(), 1);
        assert_eq!(reloaded.promoted_variant.as_deref(), Some("v1"));
        assert_eq!(repo.list_experiments(&lab.id).unwrap().len(), 1);
    }

    #[test]
    fn a_workspace_needs_a_name() {
        let db = Db::open_in_memory().unwrap();
        let repo = WorkspaceRepo::new(&db);
        assert!(repo
            .create(NewWorkspace::named(WorkspaceKind::Chat, "   "))
            .is_err());
    }
}
