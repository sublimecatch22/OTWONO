//! Detection of local runtimes.
//!
//! The wizard probes the documented default ports of Ollama and LM Studio.
//! Nothing outside the loopback interface is contacted.

use otwono_types::provider::{ConnectionHealth, ConnectionTest, ProviderKind};

use crate::{adapter_for, Provider};

#[derive(Debug, Clone)]
pub struct Detection {
    pub kind: ProviderKind,
    pub endpoint: String,
    pub test: ConnectionTest,
}

impl Detection {
    pub fn is_usable(&self) -> bool {
        self.test.health == ConnectionHealth::Reachable && !self.test.models.is_empty()
    }
}

/// The endpoints the wizard tries, in order.
pub fn default_candidates() -> Vec<(ProviderKind, &'static str)> {
    vec![
        (ProviderKind::Ollama, "http://127.0.0.1:11434"),
        (ProviderKind::LmStudio, "http://127.0.0.1:1234"),
    ]
}

/// Probe a single endpoint.
pub async fn probe(kind: ProviderKind, endpoint: &str) -> Detection {
    let adapter: Box<dyn Provider> = adapter_for(kind, endpoint, None);
    let test = adapter.test().await;
    Detection {
        kind,
        endpoint: adapter.endpoint().to_string(),
        test,
    }
}

/// Probe every default endpoint concurrently.
pub async fn detect_all() -> Vec<Detection> {
    let futures = default_candidates()
        .into_iter()
        .map(|(kind, endpoint)| probe(kind, endpoint));
    futures_util::future::join_all(futures).await
}

/// A sentence explaining what the user should do when nothing was found.
pub fn nothing_found_guidance() -> &'static str {
    "No local AI runtime was found. OTWONO works without one — you can still create agents, \
     organise projects and index knowledge — but chat needs a model. Install Ollama from \
     ollama.com or LM Studio from lmstudio.ai, start it, then run this test again. If your \
     runtime uses a different port, add it by hand and choose which runtime it is."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_documented_loopback_ports() {
        let candidates = default_candidates();
        assert_eq!(candidates.len(), 2);
        for (_, endpoint) in &candidates {
            assert!(
                endpoint.starts_with("http://127.0.0.1:"),
                "{endpoint} must be loopback only"
            );
        }
        assert!(candidates.iter().any(|(_, e)| e.ends_with(":11434")));
        assert!(candidates.iter().any(|(_, e)| e.ends_with(":1234")));
    }

    #[test]
    fn the_guidance_says_the_app_still_works_without_a_model() {
        let guidance = nothing_found_guidance();
        assert!(guidance.contains("works without one"));
        assert!(guidance.contains("ollama.com"));
        assert!(guidance.contains("lmstudio.ai"));
    }

    #[tokio::test]
    async fn probing_a_closed_port_reports_unreachable_rather_than_failing() {
        // Port 1 is reserved and nothing will be listening on it.
        let detection = probe(ProviderKind::Ollama, "http://127.0.0.1:1").await;
        assert_eq!(detection.test.health, ConnectionHealth::Unreachable);
        assert!(!detection.is_usable());
        assert!(
            detection.test.detail.contains("Start Ollama"),
            "{}",
            detection.test.detail
        );
    }
}
