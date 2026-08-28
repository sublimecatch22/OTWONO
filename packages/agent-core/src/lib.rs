//! Agent behaviour: templates, prompt assembly, tools, orchestration,
//! verification and structured multi-agent sessions.

pub mod executor;
pub mod lab;
pub mod orchestrator;
pub mod prompt;
pub mod seed;
pub mod session;
pub mod templates;
pub mod tools;
pub mod verify;

pub use executor::{AgentExecutor, AgentOutcome, AgentTurn, ProviderExecutor};
pub use orchestrator::{Orchestrator, RunReport};
pub use templates::{AgentTemplate, TEMPLATES};
pub use tools::{Tool, ToolCall, ToolContext, ToolResult, ToolRunner};
pub use verify::{Verdict, Verification};
