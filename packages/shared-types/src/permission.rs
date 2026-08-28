//! Permission vocabulary. The engine that evaluates these lives in
//! `otwono-permissions`; the shapes live here so the API and UI agree.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;

/// The set of things an agent can be allowed to do. This is a closed
/// enumeration on purpose: a capability that is not listed here cannot be
/// requested, so a model cannot invent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Read a file inside an authorised knowledge source.
    FileRead,
    /// Write a file inside the project's artefact directory.
    FileWrite,
    /// Search the local knowledge index.
    KnowledgeSearch,
    /// HTTP GET against a host on the project's approved-domain list.
    HttpFetch,
    /// Create a project artefact (report, document, dataset).
    ArtifactCreate,
    /// Record an estimated or approved expense against a budget.
    BudgetRecord,
    /// Publish or modify a marketplace listing.
    MarketplacePublish,
    /// Push approved metadata to the relay API.
    RelaySync,
}

impl Capability {
    pub const ALL: [Capability; 8] = [
        Capability::FileRead,
        Capability::FileWrite,
        Capability::KnowledgeSearch,
        Capability::HttpFetch,
        Capability::ArtifactCreate,
        Capability::BudgetRecord,
        Capability::MarketplacePublish,
        Capability::RelaySync,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileRead => "file_read",
            Self::FileWrite => "file_write",
            Self::KnowledgeSearch => "knowledge_search",
            Self::HttpFetch => "http_fetch",
            Self::ArtifactCreate => "artifact_create",
            Self::BudgetRecord => "budget_record",
            Self::MarketplacePublish => "marketplace_publish",
            Self::RelaySync => "relay_sync",
        }
    }

    /// Sentence shown in the approval prompt. Written for a human, not a log.
    pub const fn human_request(self) -> &'static str {
        match self {
            Self::FileRead => "read a file you have authorised",
            Self::FileWrite => "write a file into this project's output folder",
            Self::KnowledgeSearch => "search your local knowledge index",
            Self::HttpFetch => "fetch a page from an approved website",
            Self::ArtifactCreate => "create a deliverable in this project",
            Self::BudgetRecord => "record an expense against this project's budget",
            Self::MarketplacePublish => "publish or change a marketplace listing",
            Self::RelaySync => "send approved project metadata to your OTWONO account",
        }
    }

    /// Capabilities that can move data off the device always need an explicit
    /// grant and can never be pre-approved by a template.
    pub const fn leaves_device(self) -> bool {
        matches!(self, Self::HttpFetch | Self::RelaySync | Self::MarketplacePublish)
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == value)
            .ok_or_else(|| DomainError::validation("capability", format!("unknown capability {value:?}")))
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a grant applies. Narrower scopes win over broader ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Scope {
    /// Everything the user owns. Only ever created by the user, never requested
    /// by an agent.
    Global,
    Project { project_id: String },
    Workspace { workspace_id: String },
    Agent { agent_id: String },
    /// A filesystem prefix. Comparison is on canonicalised paths.
    Path { path: String },
    /// A single host, matched exactly (no wildcard subdomains).
    Host { host: String },
    Connector { connector_id: String },
}

impl Scope {
    /// Specificity used to resolve conflicting grants: higher wins.
    pub const fn precedence(&self) -> u8 {
        match self {
            Scope::Global => 0,
            Scope::Workspace { .. } => 1,
            Scope::Project { .. } => 2,
            Scope::Connector { .. } => 3,
            Scope::Agent { .. } => 4,
            Scope::Host { .. } => 5,
            Scope::Path { .. } => 6,
        }
    }

    pub fn key(&self) -> String {
        match self {
            Scope::Global => "global".into(),
            Scope::Project { project_id } => format!("project:{project_id}"),
            Scope::Workspace { workspace_id } => format!("workspace:{workspace_id}"),
            Scope::Agent { agent_id } => format!("agent:{agent_id}"),
            Scope::Path { path } => format!("path:{path}"),
            Scope::Host { host } => format!("host:{host}"),
            Scope::Connector { connector_id } => format!("connector:{connector_id}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// One use, consumed on the next matching check.
    AllowOnce,
    /// Persists for the scope until revoked or expired.
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub capability: Capability,
    pub scopes: Vec<Scope>,
    pub decision: Decision,
    /// Optional spending ceiling in minor currency units, for `BudgetRecord`.
    pub spend_limit_minor: Option<i64>,
    pub spend_category: Option<String>,
    pub expires_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub created_by: String,
    pub note: Option<String>,
}

/// A request an agent makes that requires a human answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub capability: Capability,
    pub scopes: Vec<Scope>,
    /// Human-readable sentence assembled for the approval dialog.
    pub summary: String,
    pub requested_by_agent_id: Option<String>,
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub created_at: Timestamp,
    pub resolved_at: Option<Timestamp>,
    pub resolution: Option<Decision>,
}

/// Outcome of a permission check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CheckOutcome {
    Allowed { grant_id: String },
    /// No matching grant: deny by default and ask.
    NeedsApproval { reason: String },
    Denied { reason: String },
    /// The global emergency stop is engaged; nothing runs.
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_has_a_human_sentence() {
        for cap in Capability::ALL {
            let sentence = cap.human_request();
            assert!(!sentence.is_empty());
            assert!(
                sentence.chars().next().unwrap().is_lowercase(),
                "{cap} sentence should read after 'This agent wants to …'"
            );
            assert_eq!(Capability::parse(cap.as_str()).unwrap(), cap);
        }
    }

    #[test]
    fn off_device_capabilities_are_flagged() {
        assert!(Capability::HttpFetch.leaves_device());
        assert!(Capability::RelaySync.leaves_device());
        assert!(!Capability::FileRead.leaves_device());
        assert!(!Capability::KnowledgeSearch.leaves_device());
    }

    #[test]
    fn narrower_scopes_outrank_broader_ones() {
        let path = Scope::Path { path: "/a".into() };
        let project = Scope::Project { project_id: "p".into() };
        assert!(path.precedence() > project.precedence());
        assert!(project.precedence() > Scope::Global.precedence());
    }

    #[test]
    fn unknown_capability_is_rejected() {
        assert!(Capability::parse("run_shell").is_err());
    }
}
