//! Conversation shapes shared by the API, the UI and the provider adapters.

use serde::{Deserialize, Serialize};

use crate::ids::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Output of a tool call, surfaced to the model as data.
    Tool,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub role: Role,
    pub content: String,
    /// Citations attached to an assistant message that used knowledge.
    #[serde(default)]
    pub citations: Vec<Citation>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    pub model: Option<String>,
    pub provider_connection_id: Option<String>,
    pub token_estimate: Option<u32>,
    /// Set when generation was stopped by the user or by a limit.
    pub stopped_reason: Option<String>,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub source_id: String,
    pub document_id: String,
    /// The file name shown to the user.
    pub file_name: String,
    /// Absolute path, shown only locally and never synchronised.
    pub file_path: String,
    pub chunk_index: u32,
    /// Page for PDFs, line range for text — whichever the parser could supply.
    pub locator: Option<String>,
    pub excerpt: String,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub file_name: String,
    pub media_type: String,
    pub byte_size: u64,
    /// Attachments are copied into the app data directory, not referenced in
    /// place, so that a later move or delete cannot change history.
    pub stored_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub workspace_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider_connection_id: Option<String>,
    pub model: Option<String>,
    pub knowledge_source_ids: Vec<String>,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Frames sent over the streaming endpoint. Kept explicit so the UI can render
/// progress, tool activity and failures rather than only text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Emitted once, before any token.
    Start {
        message_id: String,
        model: String,
        provider: String,
    },
    /// Incremental assistant text.
    Delta { text: String },
    /// A tool the agent invoked, shown in the run drawer and the activity log.
    ToolCall {
        tool: String,
        summary: String,
        status: String,
    },
    /// Knowledge chunks used for this answer.
    Citations { citations: Vec<Citation> },
    /// The run paused for a human decision.
    ApprovalRequired {
        request_id: String,
        summary: String,
    },
    /// Terminal success.
    Done {
        message_id: String,
        finish_reason: String,
        token_estimate: Option<u32>,
    },
    /// Terminal failure. `retryable` tells the UI whether to offer Retry.
    Error { message: String, retryable: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_events_are_tagged_for_the_client() {
        let event = StreamEvent::Delta { text: "hi".into() };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "delta");
        assert_eq!(json["text"], "hi");

        let done = StreamEvent::Done {
            message_id: "msg_1".into(),
            finish_reason: "stop".into(),
            token_estimate: Some(12),
        };
        assert_eq!(serde_json::to_value(&done).unwrap()["type"], "done");
    }

    #[test]
    fn citations_carry_enough_to_reopen_the_source() {
        let c = Citation {
            source_id: "src_1".into(),
            document_id: "doc_1".into(),
            file_name: "handbook.pdf".into(),
            file_path: "/home/u/docs/handbook.pdf".into(),
            chunk_index: 3,
            locator: Some("page 12".into()),
            excerpt: "…".into(),
            score: 0.82,
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["file_name"], "handbook.pdf");
        assert_eq!(json["locator"], "page 12");
    }
}
