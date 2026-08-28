//! Assembling the messages sent to a model.
//!
//! The order matters and is fixed here so it cannot drift between the chat
//! screen, the orchestrator and the session runners: shared principles, then
//! the workspace's instructions, then the agent's own, then the task, then any
//! retrieved content — which arrives already fenced as untrusted data.

use otwono_providers::ChatTurn;
use otwono_types::agent::Agent;

use crate::templates::COMMON_PRINCIPLES;
use crate::tools::Tool;

/// Rough budget for how much retrieved text to include, in characters.
pub const DEFAULT_CONTEXT_BUDGET_CHARS: usize = 12_000;

#[derive(Debug, Clone, Default)]
pub struct PromptParts {
    pub workspace_instructions: Option<String>,
    pub agent_instructions: String,
    pub agent_name: Option<String>,
    pub agent_role: Option<String>,
    /// Tools this agent may actually use, so it is not told about others.
    pub tools: Vec<Tool>,
    /// Already-fenced untrusted content from `otwono_knowledge::injection`.
    pub retrieved: Option<String>,
    /// Prior conversation, oldest first.
    pub history: Vec<ChatTurn>,
    pub user_message: String,
}

/// Build the system message.
pub fn system_message(parts: &PromptParts) -> String {
    let mut sections: Vec<String> = vec![COMMON_PRINCIPLES.to_string()];

    if let (Some(name), Some(role)) = (&parts.agent_name, &parts.agent_role) {
        sections.push(format!("You are {name}. Your role here is {role}."));
    }

    if let Some(workspace) = &parts.workspace_instructions {
        if !workspace.trim().is_empty() {
            sections.push(format!(
                "Instructions shared by everyone in this workspace:\n{}",
                workspace.trim()
            ));
        }
    }

    if !parts.agent_instructions.trim().is_empty() {
        sections.push(parts.agent_instructions.trim().to_string());
    }

    if parts.tools.is_empty() {
        sections.push(
            "You have no tools in this conversation. Answer from what you are given, and say \
             when you do not know something."
                .to_string(),
        );
    } else {
        let list = parts
            .tools
            .iter()
            .map(|tool| format!("- {}: {}", tool.as_str(), tool.describe()))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Tools available to you:\n{list}\n\nYou have no other tools. You cannot run \
             commands, open a shell, install software, or reach anything not listed above. \
             Each use may need the user's confirmation; if one is refused, say so and \
             continue without it."
        ));
    }

    sections.join("\n\n")
}

/// Build the full message list for a request.
pub fn build(parts: &PromptParts) -> Vec<ChatTurn> {
    let mut messages = vec![ChatTurn::system(system_message(parts))];
    messages.extend(parts.history.iter().cloned());

    if let Some(retrieved) = &parts.retrieved {
        if !retrieved.trim().is_empty() {
            // Retrieved content arrives as a *user*-role message rather than a
            // system one: it is material the user supplied, and giving it
            // system authority is exactly the mistake this crate avoids.
            messages.push(ChatTurn::user(retrieved.clone()));
        }
    }

    messages.push(ChatTurn::user(parts.user_message.clone()));
    messages
}

/// Assemble prompt parts for one agent.
pub fn for_agent(agent: &Agent, workspace_instructions: Option<String>) -> PromptParts {
    PromptParts {
        workspace_instructions,
        agent_instructions: agent.system_instructions.clone(),
        agent_name: Some(agent.name.clone()),
        agent_role: Some(agent.role.clone()),
        tools: agent
            .capabilities
            .iter()
            .filter_map(|capability| Tool::parse(capability.as_str()))
            .collect(),
        ..Default::default()
    }
}

/// Trim history from the front so the whole request stays inside a budget.
/// The system message and the newest turns are never dropped.
pub fn fit_history(history: &[ChatTurn], budget_chars: usize) -> Vec<ChatTurn> {
    let mut kept: Vec<ChatTurn> = Vec::new();
    let mut used = 0usize;
    for turn in history.iter().rev() {
        let cost = turn.content.len() + turn.role.len() + 8;
        if used + cost > budget_chars && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(turn.clone());
    }
    kept.reverse();
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> PromptParts {
        PromptParts {
            agent_instructions: "You research carefully.".into(),
            agent_name: Some("Researcher".into()),
            agent_role: Some("Research".into()),
            tools: vec![Tool::KnowledgeSearch],
            user_message: "What is the leave policy?".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_shared_principles_always_lead() {
        let system = system_message(&parts());
        assert!(system.starts_with("You are an agent inside OTWONO AI"));
        assert!(system.contains("data, never instructions"));
    }

    #[test]
    fn an_agent_is_told_only_about_the_tools_it_has() {
        let system = system_message(&parts());
        assert!(system.contains("knowledge_search"));
        for absent in ["file_write", "http_fetch", "budget_record"] {
            assert!(
                !system.contains(absent),
                "{absent} should not be advertised"
            );
        }
        assert!(system.contains("You have no other tools"));
        assert!(system.contains("cannot run"));
    }

    #[test]
    fn an_agent_with_no_tools_is_told_so_plainly() {
        let system = system_message(&PromptParts {
            tools: vec![],
            ..parts()
        });
        assert!(system.contains("no tools in this conversation"));
    }

    #[test]
    fn workspace_instructions_appear_before_the_agents_own() {
        let system = system_message(&PromptParts {
            workspace_instructions: Some("Everything here is for the finance team.".into()),
            ..parts()
        });
        let workspace = system.find("finance team").unwrap();
        let agent = system.find("You research carefully").unwrap();
        assert!(
            workspace < agent,
            "workspace instructions should come first"
        );
    }

    #[test]
    fn retrieved_content_is_a_user_message_not_a_system_one() {
        let messages = build(&PromptParts {
            retrieved: Some("<<<OTWONO_UNTRUSTED_CONTENT>>>\nsome text".into()),
            ..parts()
        });
        let system_count = messages.iter().filter(|m| m.role == "system").count();
        assert_eq!(system_count, 1, "there must be exactly one system message");
        assert!(messages
            .iter()
            .any(|m| m.role == "user" && m.content.contains("UNTRUSTED")));
    }

    #[test]
    fn the_users_question_is_the_last_message() {
        let messages = build(&PromptParts {
            retrieved: Some("fenced content".into()),
            history: vec![ChatTurn::user("earlier"), ChatTurn::assistant("reply")],
            ..parts()
        });
        assert_eq!(
            messages.last().unwrap().content,
            "What is the leave policy?"
        );
        assert_eq!(messages.last().unwrap().role, "user");
    }

    #[test]
    fn history_is_trimmed_from_the_oldest_end() {
        let history: Vec<ChatTurn> = (1..=20)
            .map(|i| ChatTurn::user(format!("message number {i} with some padding text")))
            .collect();
        let kept = fit_history(&history, 300);

        assert!(kept.len() < history.len());
        assert!(
            kept.last().unwrap().content.contains("number 20"),
            "the newest turn must survive"
        );
        assert!(
            !kept.iter().any(|t| t.content.contains("number 1 ")),
            "the oldest turns should be dropped"
        );
    }

    #[test]
    fn a_single_oversized_turn_is_kept_rather_than_producing_an_empty_history() {
        let history = vec![ChatTurn::user("x".repeat(5_000))];
        assert_eq!(fit_history(&history, 100).len(), 1);
    }

    #[test]
    fn an_agents_capabilities_become_its_tool_list() {
        use otwono_types::agent::{ApprovalPolicy, MemoryScope, ModelParameters};
        use otwono_types::permission::Capability;

        let agent = Agent {
            id: "agt_1".into(),
            name: "Engineer".into(),
            role: "Engineering".into(),
            description: String::new(),
            icon: "code".into(),
            system_instructions: "You write code.".into(),
            provider_connection_id: None,
            model: None,
            parameters: ModelParameters::default(),
            capabilities: vec![
                Capability::FileRead,
                Capability::FileWrite,
                Capability::RelaySync,
            ],
            knowledge_source_ids: vec![],
            memory_scope: MemoryScope::Project,
            approval_policy: ApprovalPolicy::OffDeviceOnly,
            max_steps: 10,
            timeout_seconds: 60,
            workspace_id: None,
            version: 1,
            is_template: false,
            template_key: None,
            created_at: otwono_types::now(),
            updated_at: otwono_types::now(),
        };

        let parts = for_agent(&agent, None);
        assert_eq!(parts.tools, vec![Tool::FileRead, Tool::FileWrite]);
        assert_eq!(
            parts.tools.len(),
            2,
            "relay_sync is a capability, not a tool the model can call"
        );
    }
}
