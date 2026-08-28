//! Agent configuration and the declarative package format used for
//! export/import. Packages are data only: they never carry credentials and
//! never carry executable content.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;
use crate::permission::Capability;
use crate::PACKAGE_SCHEMA_VERSION;

/// How much an agent remembers between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Nothing survives the current message exchange.
    None,
    /// The current conversation only.
    Conversation,
    /// Shared notes within one project.
    Project,
    /// Shared notes across the agent's workspace.
    Workspace,
}

impl MemoryScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Conversation => "conversation",
            Self::Project => "project",
            Self::Workspace => "workspace",
        }
    }
}

/// What the agent must do before acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Every capability use is confirmed by the human.
    Always,
    /// Only capabilities that can move data off the device are confirmed.
    OffDeviceOnly,
    /// Rely on standing grants; still refused when no grant exists.
    Standing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelParameters {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub stop: Vec<String>,
    /// Parameters this build does not model explicitly, passed through to
    /// providers that understand them.
    #[serde(default)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for ModelParameters {
    fn default() -> Self {
        Self {
            temperature: Some(0.7),
            top_p: None,
            max_output_tokens: None,
            stop: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub role: String,
    pub description: String,
    /// A short icon token (emoji or built-in glyph name). Never a URL or path.
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
    pub version: u32,
    pub is_template: bool,
    pub template_key: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The exported form of an agent. Deliberately a different type from `Agent`
/// so that adding an internal field cannot leak it into a shared file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPackage {
    pub schema_version: u32,
    pub kind: String,
    pub name: String,
    pub role: String,
    pub description: String,
    pub icon: String,
    pub system_instructions: String,
    /// Provider *kind* hint only — never a connection id or endpoint, because
    /// those are local to the exporting machine.
    pub provider_hint: Option<String>,
    pub model_hint: Option<String>,
    pub parameters: ModelParameters,
    pub capabilities: Vec<Capability>,
    pub memory_scope: MemoryScope,
    pub approval_policy: ApprovalPolicy,
    pub max_steps: u32,
    pub timeout_seconds: u32,
    pub exported_at: Timestamp,
    pub exported_by_app_version: String,
}

/// Key names that are refused outright when they appear on their own, once
/// punctuation is stripped. Kept separate from the substring list because a
/// bare `token` is a credential while `max_output_tokens` is a model parameter.
pub const FORBIDDEN_EXACT_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passphrase",
    "credential",
    "credentials",
    "auth",
    "authorization",
    "bearer",
    "cookie",
    "session",
    "key",
    "jwt",
    "pat",
];

/// Fragments that are a credential wherever they appear inside a key name.
pub const FORBIDDEN_KEY_FRAGMENTS: &[&str] = &[
    "apikey",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "authtoken",
    "bearertoken",
    "sessiontoken",
    "privatekey",
    "secretkey",
    "clientsecret",
    "apisecret",
];

/// Normalise a key for comparison: lowercase, punctuation and spacing removed,
/// so `API-Key`, `api_key` and `apiKey` all collapse to the same form.
fn normalise_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Reason a key is refused, or `None` when it is acceptable.
fn credential_key_reason(key: &str) -> Option<String> {
    let normalised = normalise_key(key);
    if FORBIDDEN_EXACT_KEYS.contains(&normalised.as_str()) {
        return Some(format!("{key:?} names a credential"));
    }
    FORBIDDEN_KEY_FRAGMENTS
        .iter()
        .find(|fragment| normalised.contains(*fragment))
        .map(|fragment| format!("{key:?} contains {fragment:?}"))
}

/// Reject a package that contains anything resembling a secret. Applied to the
/// serialised JSON so nested and unknown fields are covered too.
pub fn assert_package_has_no_secrets(value: &serde_json::Value) -> DomainResult<()> {
    fn walk(value: &serde_json::Value, path: &str) -> DomainResult<()> {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    if let Some(reason) = credential_key_reason(key) {
                        return Err(DomainError::Refused(format!(
                            "agent package field {path}{key} is not allowed: {reason}; \
                             packages must not contain secrets"
                        )));
                    }
                    walk(child, &format!("{path}{key}."))?;
                }
                Ok(())
            }
            serde_json::Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{path}{index}."))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(value, "")
}

impl AgentPackage {
    pub const KIND: &'static str = "otwono.agent";

    pub fn validate(&self) -> DomainResult<()> {
        if self.schema_version != PACKAGE_SCHEMA_VERSION {
            return Err(DomainError::UnsupportedSchema {
                found: self.schema_version,
                supported: PACKAGE_SCHEMA_VERSION,
            });
        }
        if self.kind != Self::KIND {
            return Err(DomainError::validation(
                "kind",
                format!("expected {:?}, found {:?}", Self::KIND, self.kind),
            ));
        }
        if self.name.trim().is_empty() {
            return Err(DomainError::validation("name", "must not be empty"));
        }
        if self.name.chars().count() > 120 {
            return Err(DomainError::validation(
                "name",
                "must be 120 characters or fewer",
            ));
        }
        if self.system_instructions.len() > 32_768 {
            return Err(DomainError::validation(
                "system_instructions",
                "must be 32768 characters or fewer",
            ));
        }
        if self.max_steps == 0 || self.max_steps > 200 {
            return Err(DomainError::validation(
                "max_steps",
                "must be between 1 and 200",
            ));
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > 3_600 {
            return Err(DomainError::validation(
                "timeout_seconds",
                "must be between 1 and 3600",
            ));
        }
        let json = serde_json::to_value(self)
            .map_err(|e| DomainError::validation("package", e.to_string()))?;
        assert_package_has_no_secrets(&json)
    }
}

impl Agent {
    pub fn to_package(&self, provider_hint: Option<String>) -> AgentPackage {
        AgentPackage {
            schema_version: PACKAGE_SCHEMA_VERSION,
            kind: AgentPackage::KIND.to_string(),
            name: self.name.clone(),
            role: self.role.clone(),
            description: self.description.clone(),
            icon: self.icon.clone(),
            system_instructions: self.system_instructions.clone(),
            provider_hint,
            model_hint: self.model.clone(),
            parameters: self.parameters.clone(),
            capabilities: self.capabilities.clone(),
            memory_scope: self.memory_scope,
            approval_policy: self.approval_policy,
            max_steps: self.max_steps,
            timeout_seconds: self.timeout_seconds,
            exported_at: crate::ids::now(),
            exported_by_app_version: crate::APP_VERSION.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> AgentPackage {
        AgentPackage {
            schema_version: PACKAGE_SCHEMA_VERSION,
            kind: AgentPackage::KIND.into(),
            name: "Researcher".into(),
            role: "Research".into(),
            description: "Finds and cites sources.".into(),
            icon: "search".into(),
            system_instructions: "You research carefully.".into(),
            provider_hint: Some("ollama".into()),
            model_hint: Some("llama3.1".into()),
            parameters: ModelParameters::default(),
            capabilities: vec![Capability::KnowledgeSearch],
            memory_scope: MemoryScope::Project,
            approval_policy: ApprovalPolicy::OffDeviceOnly,
            max_steps: 12,
            timeout_seconds: 120,
            exported_at: crate::ids::now(),
            exported_by_app_version: crate::APP_VERSION.into(),
        }
    }

    #[test]
    fn a_well_formed_package_validates() {
        sample().validate().expect("should validate");
    }

    #[test]
    fn packages_from_a_future_schema_are_refused() {
        let mut pkg = sample();
        pkg.schema_version = PACKAGE_SCHEMA_VERSION + 1;
        assert!(matches!(
            pkg.validate(),
            Err(DomainError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn secret_shaped_fields_are_refused_at_any_depth() {
        for probe in [
            json!({ "api_key": "sk-live-123" }),
            json!({ "parameters": { "extra": { "Authorization": "Bearer x" } } }),
            json!({ "list": [ { "nested": { "refresh_token": "x" } } ] }),
            json!({ "OPENAI_API_KEY": "x" }),
        ] {
            assert!(
                assert_package_has_no_secrets(&probe).is_err(),
                "should have refused {probe}"
            );
        }
        for benign in [
            json!({ "name": "ok", "role": "ok" }),
            json!({ "parameters": { "max_output_tokens": 512, "token_budget": 10 } }),
            json!({ "keywords": ["a", "b"], "monkey": true }),
        ] {
            assert!(
                assert_package_has_no_secrets(&benign).is_ok(),
                "should have accepted {benign}"
            );
        }
    }

    #[test]
    fn step_and_timeout_budgets_are_bounded() {
        let mut pkg = sample();
        pkg.max_steps = 0;
        assert!(pkg.validate().is_err());
        pkg.max_steps = 10_000;
        assert!(pkg.validate().is_err());
        pkg.max_steps = 10;
        pkg.timeout_seconds = 0;
        assert!(pkg.validate().is_err());
    }

    #[test]
    fn packages_round_trip_through_json() {
        let pkg = sample();
        let text = serde_json::to_string_pretty(&pkg).unwrap();
        assert!(!text.contains("connection_id"), "no local ids in packages");
        let back: AgentPackage = serde_json::from_str(&text).unwrap();
        assert_eq!(pkg, back);
    }
}
