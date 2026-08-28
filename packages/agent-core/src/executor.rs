//! Running one agent turn.
//!
//! `AgentExecutor` is the seam between "decide what to ask" and "ask a model".
//! The shipped implementation talks to a provider. Tests substitute a scripted
//! executor so orchestration logic can be verified without a model — the seam
//! is internal, and no fake provider exists in the application itself.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use otwono_providers::{ChatDelta, ChatRequest, ChatTurn, Provider};
use otwono_types::chat::Citation;

/// One request to an agent.
#[derive(Debug, Clone)]
pub struct AgentTurn {
    pub agent_id: String,
    pub agent_name: String,
    pub model: String,
    pub messages: Vec<ChatTurn>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentOutcome {
    pub text: String,
    pub finish_reason: String,
    pub token_estimate: Option<u32>,
    #[serde(default)]
    pub citations: Vec<Citation>,
}

impl AgentOutcome {
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            text: value.into(),
            finish_reason: "stop".into(),
            token_estimate: None,
            citations: Vec::new(),
        }
    }

    /// True when the model stopped because it ran out of room rather than
    /// because it finished. The orchestrator treats these differently.
    pub fn was_truncated(&self) -> bool {
        matches!(
            self.finish_reason.as_str(),
            "length" | "max_tokens" | "incomplete"
        )
    }
}

#[async_trait]
pub trait AgentExecutor: Send + Sync {
    async fn run(&self, turn: AgentTurn) -> Result<AgentOutcome>;
}

/// The real executor: streams from a provider and collects the result.
pub struct ProviderExecutor {
    provider: Arc<dyn Provider>,
}

impl ProviderExecutor {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl AgentExecutor for ProviderExecutor {
    async fn run(&self, turn: AgentTurn) -> Result<AgentOutcome> {
        let request = ChatRequest {
            model: turn.model.clone(),
            messages: turn.messages.clone(),
            temperature: turn.temperature,
            top_p: None,
            max_output_tokens: turn.max_output_tokens,
            stop: Vec::new(),
        };

        let work = async {
            let mut stream = self.provider.chat_stream(request).await?;
            let mut text = String::new();
            let mut finish_reason = "incomplete".to_string();
            let mut token_estimate = None;

            while let Some(item) = stream.next().await {
                match item? {
                    ChatDelta::Text(chunk) => text.push_str(&chunk),
                    ChatDelta::Done {
                        finish_reason: reason,
                        token_estimate: tokens,
                    } => {
                        finish_reason = reason;
                        token_estimate = tokens;
                        break;
                    }
                }
            }

            Ok::<_, anyhow::Error>(AgentOutcome {
                text,
                finish_reason,
                token_estimate,
                citations: Vec::new(),
            })
        };

        // A model that never answers must not hold a task open forever.
        match tokio::time::timeout(
            std::time::Duration::from_secs(turn.timeout_seconds.max(1) as u64),
            work,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "{} did not answer within {} seconds",
                turn.agent_name,
                turn.timeout_seconds
            ),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod scripted {
    //! A scripted executor for tests of orchestration logic. Not compiled into
    //! release builds of the application.

    use super::*;
    use std::sync::Mutex;

    type Responder = Box<dyn Fn(&AgentTurn) -> Result<AgentOutcome> + Send + Sync>;

    #[derive(Default)]
    pub struct ScriptedExecutor {
        responses: Mutex<Vec<Result<AgentOutcome, String>>>,
        responder: Option<Responder>,
        pub calls: Mutex<Vec<AgentTurn>>,
    }

    impl ScriptedExecutor {
        /// Answer each call with the next scripted reply, in order.
        pub fn with_replies(replies: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(
                    replies
                        .into_iter()
                        .map(|text| Ok(AgentOutcome::text(text)))
                        .collect(),
                ),
                responder: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn with_results(results: Vec<Result<AgentOutcome, String>>) -> Self {
            Self {
                responses: Mutex::new(results),
                responder: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        /// Decide each reply from the turn itself.
        pub fn responding(
            responder: impl Fn(&AgentTurn) -> Result<AgentOutcome> + Send + Sync + 'static,
        ) -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                responder: Some(Box::new(responder)),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        pub fn last_prompt(&self) -> Option<String> {
            self.calls.lock().unwrap().last().map(|turn| {
                turn.messages
                    .iter()
                    .map(|m| format!("[{}] {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
    }

    #[async_trait]
    impl AgentExecutor for ScriptedExecutor {
        async fn run(&self, turn: AgentTurn) -> Result<AgentOutcome> {
            self.calls.lock().unwrap().push(turn.clone());
            if let Some(responder) = &self.responder {
                return responder(&turn);
            }
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("the scripted executor ran out of replies");
            }
            match responses.remove(0) {
                Ok(outcome) => Ok(outcome),
                Err(message) => anyhow::bail!(message),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scripted::ScriptedExecutor;
    use super::*;

    fn turn() -> AgentTurn {
        AgentTurn {
            agent_id: "agt_1".into(),
            agent_name: "Researcher".into(),
            model: "test-model".into(),
            messages: vec![ChatTurn::user("hello")],
            temperature: Some(0.2),
            max_output_tokens: None,
            timeout_seconds: 30,
        }
    }

    #[tokio::test]
    async fn a_scripted_executor_answers_in_order_and_records_calls() {
        let executor = ScriptedExecutor::with_replies(vec!["first", "second"]);
        assert_eq!(executor.run(turn()).await.unwrap().text, "first");
        assert_eq!(executor.run(turn()).await.unwrap().text, "second");
        assert_eq!(executor.call_count(), 2);
        assert!(
            executor.run(turn()).await.is_err(),
            "running out is an error, not silence"
        );
    }

    #[test]
    fn truncation_is_distinguished_from_a_clean_finish() {
        assert!(!AgentOutcome::text("done").was_truncated());
        for reason in ["length", "max_tokens", "incomplete"] {
            let outcome = AgentOutcome {
                finish_reason: reason.into(),
                ..AgentOutcome::text("cut off")
            };
            assert!(outcome.was_truncated(), "{reason} means truncated");
        }
    }

    #[tokio::test]
    async fn a_scripted_failure_surfaces_as_an_error() {
        let executor = ScriptedExecutor::with_results(vec![Err("the model refused".into())]);
        let error = executor.run(turn()).await.unwrap_err().to_string();
        assert_eq!(error, "the model refused");
    }
}
