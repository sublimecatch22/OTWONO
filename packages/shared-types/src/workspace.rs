//! Workspaces: Chat, Office, Lab, Boardroom, Think Tank.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Chat,
    Office,
    Lab,
    Boardroom,
    ThinkTank,
}

impl WorkspaceKind {
    pub const ALL: [WorkspaceKind; 5] = [
        WorkspaceKind::Chat,
        WorkspaceKind::Office,
        WorkspaceKind::Lab,
        WorkspaceKind::Boardroom,
        WorkspaceKind::ThinkTank,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Office => "office",
            Self::Lab => "lab",
            Self::Boardroom => "boardroom",
            Self::ThinkTank => "think_tank",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Office => "Office",
            Self::Lab => "Lab",
            Self::Boardroom => "Boardroom",
            Self::ThinkTank => "Think Tank",
        }
    }

    pub const fn purpose(self) -> &'static str {
        match self {
            Self::Chat => "A conversation with one selected model or agent.",
            Self::Office => "A standing team of agents doing repeated operational work.",
            Self::Lab => "A place to test prompts, models and agent settings safely.",
            Self::Boardroom => "A structured decision session ending in a chair's synthesis.",
            Self::ThinkTank => "Research and ideation, separating sourced claims from speculation.",
        }
    }

    /// Whether the workspace runs a structured multi-agent session rather than
    /// a free conversation.
    pub const fn is_session_based(self) -> bool {
        matches!(self, Self::Boardroom | Self::ThinkTank)
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == value)
            .ok_or_else(|| {
                DomainError::validation("workspace_kind", format!("unknown kind {value:?}"))
            })
    }
}

impl fmt::Display for WorkspaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub kind: WorkspaceKind,
    pub name: String,
    pub description: String,
    pub icon: String,
    /// Instructions shared by every agent in this workspace.
    pub shared_instructions: String,
    pub knowledge_source_ids: Vec<String>,
    /// The Office executive / Boardroom chair / Think Tank editor.
    pub coordinator_agent_id: Option<String>,
    pub favorite: bool,
    pub archived: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMember {
    pub workspace_id: String,
    pub agent_id: String,
    /// The job title this agent holds in this workspace.
    pub job_role: String,
    pub is_coordinator: bool,
    pub ordinal: u32,
}

/// The stages a Boardroom or Think Tank session moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStage {
    /// Each participant states an independent position or proposal.
    Positions,
    /// Participants challenge each other's assumptions.
    Critique,
    /// The chair or editor writes the synthesis.
    Synthesis,
    Completed,
    Failed,
}

impl SessionStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Positions => "positions",
            Self::Critique => "critique",
            Self::Synthesis => "synthesis",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub const fn next(self) -> Option<Self> {
        match self {
            Self::Positions => Some(Self::Critique),
            Self::Critique => Some(Self::Synthesis),
            Self::Synthesis => Some(Self::Completed),
            Self::Completed | Self::Failed => None,
        }
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        match value {
            "positions" => Ok(Self::Positions),
            "critique" => Ok(Self::Critique),
            "synthesis" => Ok(Self::Synthesis),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(DomainError::validation(
                "stage",
                format!("unknown stage {other:?}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace_id: String,
    pub question: String,
    pub stage: SessionStage,
    pub chair_agent_id: Option<String>,
    /// Written by the chair at the Synthesis stage.
    pub synthesis: Option<String>,
    pub dissent_summary: Option<String>,
    pub unresolved_questions: Vec<String>,
    pub recommended_decision: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// Backed by a citation from an authorised source.
    Sourced,
    /// The agent's own reasoning, explicitly not a sourced fact.
    Speculation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContribution {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub stage: SessionStage,
    pub content: String,
    pub claim_kind: ClaimKind,
    #[serde(default)]
    pub citations: Vec<crate::chat::Citation>,
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_workspace_kind_is_described() {
        for kind in WorkspaceKind::ALL {
            assert!(!kind.display_name().is_empty());
            assert!(kind.purpose().ends_with('.'));
            assert_eq!(WorkspaceKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn sessions_run_positions_then_critique_then_synthesis() {
        let mut stage = SessionStage::Positions;
        let mut seen = vec![stage];
        while let Some(next) = stage.next() {
            stage = next;
            seen.push(stage);
        }
        assert_eq!(
            seen,
            vec![
                SessionStage::Positions,
                SessionStage::Critique,
                SessionStage::Synthesis,
                SessionStage::Completed
            ]
        );
        assert!(SessionStage::Failed.next().is_none());
    }

    #[test]
    fn only_boardrooms_and_think_tanks_run_sessions() {
        assert!(WorkspaceKind::Boardroom.is_session_based());
        assert!(WorkspaceKind::ThinkTank.is_session_based());
        assert!(!WorkspaceKind::Office.is_session_based());
        assert!(!WorkspaceKind::Chat.is_session_based());
    }
}
