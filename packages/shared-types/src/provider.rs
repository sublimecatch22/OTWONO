//! Provider vocabulary: what kind of runtime is connected and what it can do.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{DomainError, DomainResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Native Ollama HTTP API.
    Ollama,
    /// LM Studio's OpenAI-compatible server.
    LmStudio,
    /// Any other OpenAI-compatible endpoint (llama.cpp, vLLM, LocalAI, hosted).
    OpenAiCompatible,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LmStudio => "lmstudio",
            Self::OpenAiCompatible => "openai_compatible",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::LmStudio => "LM Studio",
            Self::OpenAiCompatible => "OpenAI-compatible endpoint",
        }
    }

    /// Default loopback endpoint used by the detection wizard.
    pub const fn default_endpoint(self) -> &'static str {
        match self {
            Self::Ollama => "http://127.0.0.1:11434",
            Self::LmStudio => "http://127.0.0.1:1234",
            Self::OpenAiCompatible => "http://127.0.0.1:8080",
        }
    }

    /// Whether this kind reaches outside the machine by default. Only
    /// `OpenAiCompatible` can, and only if the user points it off-device.
    pub const fn is_local_by_default(self) -> bool {
        !matches!(self, Self::OpenAiCompatible)
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        match value {
            "ollama" => Ok(Self::Ollama),
            "lmstudio" => Ok(Self::LmStudio),
            "openai_compatible" => Ok(Self::OpenAiCompatible),
            other => Err(DomainError::validation(
                "provider_kind",
                format!("unknown provider {other:?}"),
            )),
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a specific model on a specific provider can actually do. Absent
/// capabilities cause the UI to disable the feature rather than fail later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub chat: bool,
    pub streaming: bool,
    pub tool_calling: bool,
    pub structured_output: bool,
    pub vision: bool,
    pub embeddings: bool,
    pub context_length: Option<u32>,
}

impl Default for Capabilities {
    /// The conservative baseline: text chat only. Everything else must be
    /// proven by discovery before the UI offers it.
    fn default() -> Self {
        Self {
            chat: true,
            streaming: false,
            tool_calling: false,
            structured_output: false,
            vision: false,
            embeddings: false,
            context_length: None,
        }
    }
}

/// How a capability was established. Shown in the UI so the user can tell a
/// probed fact from an educated guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// The runtime itself told us.
    Reported,
    /// We asked the runtime to do it and it worked.
    Probed,
    /// Inferred from the model's name or family. May be wrong.
    Inferred,
}

impl CapabilitySource {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Reported => "reported by the runtime",
            Self::Probed => "confirmed by a test request",
            Self::Inferred => "inferred from the model name; may be inaccurate",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub capabilities: Capabilities,
    pub capability_source: CapabilitySource,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConnection {
    pub id: String,
    pub kind: ProviderKind,
    pub label: String,
    pub endpoint: String,
    /// True when a credential for this connection exists in the OS vault.
    /// The credential itself is never carried in this struct.
    pub has_credential: bool,
    /// Online providers stay disabled until the user supplies credentials.
    pub enabled: bool,
    pub default_model: Option<String>,
    pub default_embedding_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionHealth {
    Unknown,
    Reachable,
    Unreachable,
    AuthenticationRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTest {
    pub health: ConnectionHealth,
    pub detail: String,
    pub models: Vec<ModelInfo>,
    pub latency_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_assume_nothing_beyond_chat() {
        let c = Capabilities::default();
        assert!(c.chat);
        assert!(!c.streaming && !c.tool_calling && !c.vision && !c.embeddings);
        assert!(c.context_length.is_none());
    }

    #[test]
    fn provider_kinds_round_trip() {
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::LmStudio,
            ProviderKind::OpenAiCompatible,
        ] {
            assert_eq!(ProviderKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(ProviderKind::parse("chatgpt").is_err());
    }

    #[test]
    fn every_capability_source_explains_itself() {
        for source in [
            CapabilitySource::Reported,
            CapabilitySource::Probed,
            CapabilitySource::Inferred,
        ] {
            assert!(!source.describe().is_empty());
        }
        assert!(CapabilitySource::Inferred
            .describe()
            .contains("may be inaccurate"));
    }

    #[test]
    fn only_generic_endpoints_may_leave_the_device() {
        assert!(ProviderKind::Ollama.is_local_by_default());
        assert!(ProviderKind::LmStudio.is_local_by_default());
        assert!(!ProviderKind::OpenAiCompatible.is_local_by_default());
    }
}
