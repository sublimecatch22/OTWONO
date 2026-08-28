//! Agents: configuration, version history, templates and package import/export.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::agent::{Agent, AgentPackage, ApprovalPolicy, MemoryScope, ModelParameters};
use otwono_types::permission::Capability;

use crate::Db;

const COLUMNS: &str = "id, name, role, description, icon, system_instructions, \
    provider_connection_id, model, parameters, capabilities, knowledge_source_ids, \
    memory_scope, approval_policy, max_steps, timeout_seconds, workspace_id, version, \
    is_template, template_key, created_at, updated_at";

fn parse_memory_scope(value: &str) -> MemoryScope {
    match value {
        "none" => MemoryScope::None,
        "project" => MemoryScope::Project,
        "workspace" => MemoryScope::Workspace,
        _ => MemoryScope::Conversation,
    }
}

fn parse_approval_policy(value: &str) -> ApprovalPolicy {
    match value {
        "always" => ApprovalPolicy::Always,
        "standing" => ApprovalPolicy::Standing,
        _ => ApprovalPolicy::OffDeviceOnly,
    }
}

fn approval_policy_str(policy: ApprovalPolicy) -> &'static str {
    match policy {
        ApprovalPolicy::Always => "always",
        ApprovalPolicy::OffDeviceOnly => "off_device_only",
        ApprovalPolicy::Standing => "standing",
    }
}

fn map(row: &Row<'_>) -> rusqlite::Result<Agent> {
    let capability_names: Vec<String> = crate::json_column(row.get(9)?);
    Ok(Agent {
        id: row.get(0)?,
        name: row.get(1)?,
        role: row.get(2)?,
        description: row.get(3)?,
        icon: row.get(4)?,
        system_instructions: row.get(5)?,
        provider_connection_id: row.get(6)?,
        model: row.get(7)?,
        parameters: crate::json_column::<Option<ModelParameters>>(row.get(8)?).unwrap_or_default(),
        // An unknown capability name is dropped rather than failing the load;
        // this is how a downgrade stays safe.
        capabilities: capability_names
            .iter()
            .filter_map(|c| Capability::parse(c).ok())
            .collect(),
        knowledge_source_ids: crate::json_column(row.get(10)?),
        memory_scope: parse_memory_scope(&row.get::<_, String>(11)?),
        approval_policy: parse_approval_policy(&row.get::<_, String>(12)?),
        max_steps: row.get::<_, i64>(13)? as u32,
        timeout_seconds: row.get::<_, i64>(14)? as u32,
        workspace_id: row.get(15)?,
        version: row.get::<_, i64>(16)? as u32,
        is_template: row.get::<_, i64>(17)? != 0,
        template_key: row.get(18)?,
        created_at: crate::parse_ts(&row.get::<_, String>(19)?),
        updated_at: crate::parse_ts(&row.get::<_, String>(20)?),
    })
}

#[derive(Debug, Clone)]
pub struct NewAgent {
    pub name: String,
    pub role: String,
    pub description: String,
    pub icon: String,
    pub system_instructions: String,
    pub provider_connection_id: Option<String>,
    pub model: Option<String>,
    pub parameters: ModelParameters,
    pub capabilities: Vec<Capability>,
    pub knowledge_source_ids: Vec<String>,
    pub memory_scope: MemoryScope,
    pub approval_policy: ApprovalPolicy,
    pub max_steps: u32,
    pub timeout_seconds: u32,
    pub workspace_id: Option<String>,
    pub template_key: Option<String>,
    pub is_template: bool,
}

impl Default for NewAgent {
    fn default() -> Self {
        Self {
            name: String::new(),
            role: String::new(),
            description: String::new(),
            icon: "agent".into(),
            system_instructions: String::new(),
            provider_connection_id: None,
            model: None,
            parameters: ModelParameters::default(),
            capabilities: Vec::new(),
            knowledge_source_ids: Vec::new(),
            memory_scope: MemoryScope::Conversation,
            approval_policy: ApprovalPolicy::OffDeviceOnly,
            max_steps: 12,
            timeout_seconds: 120,
            workspace_id: None,
            template_key: None,
            is_template: false,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentVersion {
    pub id: String,
    pub agent_id: String,
    pub version: u32,
    pub snapshot: serde_json::Value,
    pub note: Option<String>,
    pub created_at: String,
}

pub struct AgentRepo<'a> {
    db: &'a Db,
}

impl<'a> AgentRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    fn validate(new: &NewAgent) -> Result<()> {
        if new.name.trim().is_empty() {
            bail!("an agent needs a name");
        }
        if new.name.chars().count() > 120 {
            bail!("agent names must be 120 characters or fewer");
        }
        if new.system_instructions.len() > 32_768 {
            bail!("system instructions must be 32768 characters or fewer");
        }
        if new.max_steps == 0 || new.max_steps > 200 {
            bail!("max_steps must be between 1 and 200");
        }
        if new.timeout_seconds == 0 || new.timeout_seconds > 3_600 {
            bail!("timeout_seconds must be between 1 and 3600");
        }
        Ok(())
    }

    pub fn create(&self, new: NewAgent) -> Result<Agent> {
        Self::validate(&new)?;
        let id = otwono_types::new_id("agt");
        let now = crate::now_str();
        let capability_names: Vec<&str> = new.capabilities.iter().map(|c| c.as_str()).collect();
        self.db.conn()?.execute(
            "INSERT INTO agents
               (id, name, role, description, icon, system_instructions, provider_connection_id,
                model, parameters, capabilities, knowledge_source_ids, memory_scope,
                approval_policy, max_steps, timeout_seconds, workspace_id, version,
                is_template, template_key, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 1,
                     ?17, ?18, 0, ?19, ?19)",
            params![
                id,
                new.name.trim(),
                new.role,
                new.description,
                new.icon,
                new.system_instructions,
                new.provider_connection_id,
                new.model,
                crate::to_json(&new.parameters),
                crate::to_json(&capability_names),
                crate::to_json(&new.knowledge_source_ids),
                new.memory_scope.as_str(),
                approval_policy_str(new.approval_policy),
                new.max_steps as i64,
                new.timeout_seconds as i64,
                new.workspace_id,
                new.is_template as i64,
                new.template_key,
                now,
            ],
        )?;
        let agent = self
            .get(&id)?
            .ok_or_else(|| anyhow::anyhow!("agent not found after creation"))?;
        self.snapshot(&agent, Some("created"))?;
        Ok(agent)
    }

    pub fn get(&self, id: &str) -> Result<Option<Agent>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agents WHERE id = ?1"),
                [id],
                map,
            )
            .optional()?)
    }

    pub fn get_by_template_key(&self, key: &str) -> Result<Option<Agent>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM agents WHERE template_key = ?1"),
                [key],
                map,
            )
            .optional()?)
    }

    pub fn list(&self, workspace_id: Option<&str>, include_archived: bool) -> Result<Vec<Agent>> {
        let conn = self.db.conn()?;
        let mut sql = format!("SELECT {COLUMNS} FROM agents WHERE 1 = 1");
        let mut binds: Vec<String> = Vec::new();
        if let Some(workspace) = workspace_id {
            sql.push_str(" AND workspace_id = ?");
            binds.push(workspace.to_string());
        }
        if !include_archived {
            sql.push_str(" AND archived = 0");
        }
        sql.push_str(" ORDER BY name COLLATE NOCASE");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), map)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Save an edit. The version counter advances and a snapshot of the *new*
    /// state is recorded so the history can be inspected and restored.
    pub fn update(&self, agent: &Agent, note: Option<&str>) -> Result<Agent> {
        let capability_names: Vec<&str> = agent.capabilities.iter().map(|c| c.as_str()).collect();
        let next_version = agent.version.saturating_add(1);
        self.db.conn()?.execute(
            "UPDATE agents SET name = ?2, role = ?3, description = ?4, icon = ?5,
                    system_instructions = ?6, provider_connection_id = ?7, model = ?8,
                    parameters = ?9, capabilities = ?10, knowledge_source_ids = ?11,
                    memory_scope = ?12, approval_policy = ?13, max_steps = ?14,
                    timeout_seconds = ?15, workspace_id = ?16, version = ?17, updated_at = ?18
              WHERE id = ?1",
            params![
                agent.id,
                agent.name,
                agent.role,
                agent.description,
                agent.icon,
                agent.system_instructions,
                agent.provider_connection_id,
                agent.model,
                crate::to_json(&agent.parameters),
                crate::to_json(&capability_names),
                crate::to_json(&agent.knowledge_source_ids),
                agent.memory_scope.as_str(),
                approval_policy_str(agent.approval_policy),
                agent.max_steps as i64,
                agent.timeout_seconds as i64,
                agent.workspace_id,
                next_version as i64,
                crate::now_str(),
            ],
        )?;
        let updated = self
            .get(&agent.id)?
            .ok_or_else(|| anyhow::anyhow!("agent {} disappeared during update", agent.id))?;
        self.snapshot(&updated, note)?;
        Ok(updated)
    }

    pub fn archive(&self, id: &str, archived: bool) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE agents SET archived = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, archived as i64, crate::now_str()],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM agents WHERE id = ?1", [id])?;
        Ok(())
    }

    fn snapshot(&self, agent: &Agent, note: Option<&str>) -> Result<()> {
        let snapshot = serde_json::to_string(agent)?;
        self.db.conn()?.execute(
            "INSERT INTO agent_versions (id, agent_id, version, snapshot, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(agent_id, version) DO UPDATE SET snapshot = excluded.snapshot",
            params![
                otwono_types::new_id("agv"),
                agent.id,
                agent.version as i64,
                snapshot,
                note,
                crate::now_str()
            ],
        )?;
        Ok(())
    }

    pub fn versions(&self, agent_id: &str) -> Result<Vec<AgentVersion>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, agent_id, version, snapshot, note, created_at
               FROM agent_versions WHERE agent_id = ?1 ORDER BY version DESC",
        )?;
        let rows = stmt.query_map([agent_id], |row| {
            Ok(AgentVersion {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                version: row.get::<_, i64>(2)? as u32,
                snapshot: crate::json_column(row.get(3)?),
                note: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Restore a previous version as a *new* version, so history is never lost.
    pub fn restore_version(&self, agent_id: &str, version: u32) -> Result<Agent> {
        let conn = self.db.conn()?;
        let snapshot: String = conn
            .query_row(
                "SELECT snapshot FROM agent_versions WHERE agent_id = ?1 AND version = ?2",
                params![agent_id, version as i64],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow::anyhow!("version {version} of agent {agent_id} does not exist")
            })?;
        drop(conn);

        let mut restored: Agent =
            serde_json::from_str(&snapshot).context("reading the stored agent snapshot")?;
        let current = self
            .get(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} no longer exists"))?;
        restored.version = current.version;
        self.update(&restored, Some(&format!("restored from version {version}")))
    }

    // ---- packages

    /// Build the exportable package. `provider_hint` is derived by the caller
    /// from the connection's *kind*, never its id or endpoint.
    pub fn export(&self, agent_id: &str, provider_hint: Option<String>) -> Result<AgentPackage> {
        let agent = self
            .get(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent {agent_id} does not exist"))?;
        let package = agent.to_package(provider_hint);
        // Validate on the way out as well as on the way in: a future field
        // added to `Agent` must never reach a shared file unnoticed.
        package
            .validate()
            .map_err(|e| anyhow::anyhow!("refusing to export this agent: {e}"))?;
        Ok(package)
    }

    /// Import a package as a new agent. The package is validated first, and no
    /// provider connection is attached — the user chooses one after import,
    /// because connections are local to a machine.
    pub fn import(&self, package: &AgentPackage, workspace_id: Option<String>) -> Result<Agent> {
        package
            .validate()
            .map_err(|e| anyhow::anyhow!("refusing to import this agent package: {e}"))?;
        self.create(NewAgent {
            name: package.name.clone(),
            role: package.role.clone(),
            description: package.description.clone(),
            icon: package.icon.clone(),
            system_instructions: package.system_instructions.clone(),
            provider_connection_id: None,
            model: package.model_hint.clone(),
            parameters: package.parameters.clone(),
            capabilities: package.capabilities.clone(),
            knowledge_source_ids: Vec::new(),
            memory_scope: package.memory_scope,
            approval_policy: package.approval_policy,
            max_steps: package.max_steps,
            timeout_seconds: package.timeout_seconds,
            workspace_id,
            template_key: None,
            is_template: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str) -> NewAgent {
        NewAgent {
            name: name.into(),
            role: "Research".into(),
            description: "Finds sources.".into(),
            system_instructions: "You research carefully.".into(),
            capabilities: vec![Capability::KnowledgeSearch],
            ..Default::default()
        }
    }

    #[test]
    fn agents_round_trip_through_the_database() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let created = repo.create(sample("Researcher")).unwrap();
        assert_eq!(created.version, 1);
        let loaded = repo.get(&created.id).unwrap().unwrap();
        assert_eq!(loaded.name, "Researcher");
        assert_eq!(loaded.capabilities, vec![Capability::KnowledgeSearch]);
        assert_eq!(loaded.memory_scope, MemoryScope::Conversation);
    }

    #[test]
    fn saving_an_edit_advances_the_version_and_keeps_history() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let mut agent = repo.create(sample("Researcher")).unwrap();
        agent.system_instructions = "You research exhaustively.".into();
        let updated = repo.update(&agent, Some("tightened instructions")).unwrap();
        assert_eq!(updated.version, 2);

        let versions = repo.versions(&agent.id).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 2);
        assert_eq!(versions[0].note.as_deref(), Some("tightened instructions"));
        assert_eq!(versions[1].version, 1);
    }

    #[test]
    fn a_previous_version_can_be_restored_without_losing_history() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let mut agent = repo.create(sample("Researcher")).unwrap();
        let original_instructions = agent.system_instructions.clone();

        agent.system_instructions = "Something worse.".into();
        let v2 = repo.update(&agent, None).unwrap();
        assert_eq!(v2.system_instructions, "Something worse.");

        let restored = repo.restore_version(&agent.id, 1).unwrap();
        assert_eq!(restored.system_instructions, original_instructions);
        assert_eq!(restored.version, 3, "restoring creates a new version");
        assert_eq!(repo.versions(&agent.id).unwrap().len(), 3);
    }

    #[test]
    fn an_exported_package_carries_no_local_identifiers_or_secrets() {
        let db = Db::open_in_memory().unwrap();
        let connection = crate::repo::providers::ProviderRepo::new(&db)
            .create(crate::repo::providers::NewProvider {
                kind: otwono_types::provider::ProviderKind::Ollama,
                label: "Ollama".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                default_model: None,
                default_embedding_model: None,
                enabled: true,
            })
            .unwrap();
        let repo = AgentRepo::new(&db);
        let agent = repo
            .create(NewAgent {
                provider_connection_id: Some(connection.id.clone()),
                model: Some("llama3.1".into()),
                ..sample("Researcher")
            })
            .unwrap();

        let package = repo.export(&agent.id, Some("ollama".into())).unwrap();
        let json = serde_json::to_string(&package).unwrap();
        assert!(!json.contains(&connection.id), "{json}");
        assert!(
            !json.contains("127.0.0.1"),
            "endpoints are local; they must not travel"
        );
        assert!(
            !json.contains(&agent.id),
            "the local agent id must not travel"
        );
        assert_eq!(package.provider_hint.as_deref(), Some("ollama"));
        assert_eq!(package.model_hint.as_deref(), Some("llama3.1"));
    }

    #[test]
    fn importing_a_package_creates_an_unconnected_agent() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let source = repo.create(sample("Researcher")).unwrap();
        let package = repo.export(&source.id, Some("ollama".into())).unwrap();

        let imported = repo.import(&package, None).unwrap();
        assert_ne!(imported.id, source.id);
        assert_eq!(imported.name, "Researcher");
        assert_eq!(imported.capabilities, vec![Capability::KnowledgeSearch]);
        assert!(
            imported.provider_connection_id.is_none(),
            "an imported agent must not claim a connection from another machine"
        );
    }

    #[test]
    fn a_package_containing_a_credential_is_refused_on_import() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let source = repo.create(sample("Researcher")).unwrap();
        let mut package = repo.export(&source.id, None).unwrap();
        package.parameters.extra.insert(
            "api_key".into(),
            serde_json::Value::String("sk-live-oops".into()),
        );

        let err = repo.import(&package, None).unwrap_err().to_string();
        assert!(err.contains("must not contain secrets"), "{err}");
        assert_eq!(
            repo.list(None, false).unwrap().len(),
            1,
            "nothing was created"
        );
    }

    #[test]
    fn invalid_agents_are_refused_before_they_are_written() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        assert!(repo
            .create(NewAgent {
                name: "  ".into(),
                ..sample("x")
            })
            .is_err());
        assert!(repo
            .create(NewAgent {
                max_steps: 0,
                ..sample("x")
            })
            .is_err());
        assert!(repo
            .create(NewAgent {
                max_steps: 5_000,
                ..sample("x")
            })
            .is_err());
        assert!(repo
            .create(NewAgent {
                timeout_seconds: 0,
                ..sample("x")
            })
            .is_err());
        assert!(repo.list(None, false).unwrap().is_empty());
    }

    #[test]
    fn archived_agents_leave_the_default_list_but_stay_readable() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let agent = repo.create(sample("Researcher")).unwrap();
        repo.archive(&agent.id, true).unwrap();
        assert!(repo.list(None, false).unwrap().is_empty());
        assert_eq!(repo.list(None, true).unwrap().len(), 1);
        assert!(repo.get(&agent.id).unwrap().is_some());
    }

    #[test]
    fn unknown_capability_names_in_storage_are_dropped_not_fatal() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        let agent = repo.create(sample("Researcher")).unwrap();
        db.conn()
            .unwrap()
            .execute(
                r#"UPDATE agents SET capabilities = '["knowledge_search","run_shell"]' WHERE id = ?1"#,
                [&agent.id],
            )
            .unwrap();
        let loaded = repo.get(&agent.id).unwrap().unwrap();
        assert_eq!(loaded.capabilities, vec![Capability::KnowledgeSearch]);
    }

    #[test]
    fn template_keys_are_unique() {
        let db = Db::open_in_memory().unwrap();
        let repo = AgentRepo::new(&db);
        repo.create(NewAgent {
            template_key: Some("planner".into()),
            is_template: true,
            ..sample("Planner")
        })
        .unwrap();
        assert!(repo
            .create(NewAgent {
                template_key: Some("planner".into()),
                is_template: true,
                ..sample("Planner 2")
            })
            .is_err());
        assert!(repo.get_by_template_key("planner").unwrap().is_some());
    }
}
