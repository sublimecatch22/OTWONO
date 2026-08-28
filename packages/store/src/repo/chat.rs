//! Conversations and messages.

use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::chat::{Attachment, Citation, Conversation, Message, Role};

use crate::Db;

const CONV_COLUMNS: &str = "id, title, workspace_id, agent_id, provider_connection_id, model, \
    knowledge_source_ids, pinned, archived, created_at, updated_at";

const MSG_COLUMNS: &str = "id, conversation_id, role, content, citations, attachments, model, \
    provider_connection_id, token_estimate, stopped_reason, created_at";

fn parse_role(value: &str) -> Role {
    match value {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

fn map_conversation(row: &Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: row.get(0)?,
        title: row.get(1)?,
        workspace_id: row.get(2)?,
        agent_id: row.get(3)?,
        provider_connection_id: row.get(4)?,
        model: row.get(5)?,
        knowledge_source_ids: crate::json_column(row.get(6)?),
        pinned: row.get::<_, i64>(7)? != 0,
        archived: row.get::<_, i64>(8)? != 0,
        created_at: crate::parse_ts(&row.get::<_, String>(9)?),
        updated_at: crate::parse_ts(&row.get::<_, String>(10)?),
    })
}

fn map_message(row: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        role: parse_role(&row.get::<_, String>(2)?),
        content: row.get(3)?,
        citations: crate::json_column(row.get(4)?),
        attachments: crate::json_column(row.get(5)?),
        model: row.get(6)?,
        provider_connection_id: row.get(7)?,
        token_estimate: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        stopped_reason: row.get(9)?,
        created_at: crate::parse_ts(&row.get::<_, String>(10)?),
    })
}

#[derive(Debug, Clone, Default)]
pub struct NewConversation {
    pub title: String,
    pub workspace_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider_connection_id: Option<String>,
    pub model: Option<String>,
    pub knowledge_source_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub conversation_id: String,
    pub role: Role,
    pub content: String,
    pub citations: Vec<Citation>,
    pub attachments: Vec<Attachment>,
    pub model: Option<String>,
    pub provider_connection_id: Option<String>,
    pub token_estimate: Option<u32>,
    pub stopped_reason: Option<String>,
}

impl NewMessage {
    pub fn user(conversation_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            role: Role::User,
            content: content.into(),
            citations: Vec::new(),
            attachments: Vec::new(),
            model: None,
            provider_connection_id: None,
            token_estimate: None,
            stopped_reason: None,
        }
    }

    pub fn assistant(conversation_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            ..Self::user(conversation_id, content)
        }
    }
}

pub struct ChatRepo<'a> {
    db: &'a Db,
}

impl<'a> ChatRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn create_conversation(&self, new: NewConversation) -> Result<Conversation> {
        let id = otwono_types::new_id("cnv");
        let now = crate::now_str();
        let title = if new.title.trim().is_empty() {
            "New chat".to_string()
        } else {
            new.title.trim().to_string()
        };
        self.db.conn()?.execute(
            "INSERT INTO conversations
               (id, title, workspace_id, agent_id, provider_connection_id, model,
                knowledge_source_ids, pinned, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, ?8, ?8)",
            params![
                id,
                title,
                new.workspace_id,
                new.agent_id,
                new.provider_connection_id,
                new.model,
                crate::to_json(&new.knowledge_source_ids),
                now
            ],
        )?;
        self.get_conversation(&id)?
            .ok_or_else(|| anyhow::anyhow!("conversation not found after creation"))
    }

    pub fn get_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {CONV_COLUMNS} FROM conversations WHERE id = ?1"),
                [id],
                map_conversation,
            )
            .optional()?)
    }

    pub fn list_conversations(
        &self,
        workspace_id: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<Conversation>> {
        let conn = self.db.conn()?;
        let mut sql = format!("SELECT {CONV_COLUMNS} FROM conversations WHERE 1 = 1");
        let mut binds: Vec<String> = Vec::new();
        if let Some(workspace) = workspace_id {
            sql.push_str(" AND workspace_id = ?");
            binds.push(workspace.to_string());
        }
        if !include_archived {
            sql.push_str(" AND archived = 0");
        }
        sql.push_str(" ORDER BY pinned DESC, updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), map_conversation)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update_conversation(&self, conversation: &Conversation) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE conversations SET title = ?2, workspace_id = ?3, agent_id = ?4,
                    provider_connection_id = ?5, model = ?6, knowledge_source_ids = ?7,
                    pinned = ?8, archived = ?9, updated_at = ?10
              WHERE id = ?1",
            params![
                conversation.id,
                conversation.title,
                conversation.workspace_id,
                conversation.agent_id,
                conversation.provider_connection_id,
                conversation.model,
                crate::to_json(&conversation.knowledge_source_ids),
                conversation.pinned as i64,
                conversation.archived as i64,
                crate::now_str()
            ],
        )?;
        Ok(())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        self.db
            .conn()?
            .execute("DELETE FROM conversations WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn append_message(&self, new: NewMessage) -> Result<Message> {
        let id = otwono_types::new_id("msg");
        let now = crate::now_str();
        self.db.transaction(|tx| {
            let ordinal: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM messages WHERE conversation_id = ?1",
                [&new.conversation_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO messages
                   (id, conversation_id, role, content, citations, attachments, model,
                    provider_connection_id, token_estimate, stopped_reason, ordinal, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    new.conversation_id,
                    new.role.as_str(),
                    new.content,
                    crate::to_json(&new.citations),
                    crate::to_json(&new.attachments),
                    new.model,
                    new.provider_connection_id,
                    new.token_estimate.map(|v| v as i64),
                    new.stopped_reason,
                    ordinal,
                    now
                ],
            )?;
            tx.execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
                params![new.conversation_id, now],
            )?;
            Ok(())
        })?;
        self.get_message(&id)?
            .ok_or_else(|| anyhow::anyhow!("message not found after append"))
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {MSG_COLUMNS} FROM messages WHERE id = ?1"),
                [id],
                map_message,
            )
            .optional()?)
    }

    pub fn messages(&self, conversation_id: &str) -> Result<Vec<Message>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {MSG_COLUMNS} FROM messages WHERE conversation_id = ?1 ORDER BY ordinal"
        ))?;
        let rows = stmt.query_map([conversation_id], map_message)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace an in-flight assistant message with its final content. Used when
    /// a stream completes, is stopped, or fails.
    pub fn finalise_message(
        &self,
        id: &str,
        content: &str,
        citations: &[Citation],
        token_estimate: Option<u32>,
        stopped_reason: Option<&str>,
    ) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE messages SET content = ?2, citations = ?3, token_estimate = ?4,
                    stopped_reason = ?5 WHERE id = ?1",
            params![
                id,
                content,
                crate::to_json(&citations),
                token_estimate.map(|v| v as i64),
                stopped_reason
            ],
        )?;
        Ok(())
    }

    /// Delete a message and everything after it. Backs "edit and resend": the
    /// edited turn becomes the new tail of the conversation.
    pub fn truncate_from(&self, conversation_id: &str, message_id: &str) -> Result<u32> {
        let removed = self.db.transaction(|tx| {
            let ordinal: Option<i64> = tx
                .query_row(
                    "SELECT ordinal FROM messages WHERE id = ?1 AND conversation_id = ?2",
                    params![message_id, conversation_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(ordinal) = ordinal else {
                return Ok(0);
            };
            let count = tx.execute(
                "DELETE FROM messages WHERE conversation_id = ?1 AND ordinal >= ?2",
                params![conversation_id, ordinal],
            )?;
            Ok(count)
        })?;
        Ok(removed as u32)
    }

    /// A conversation's first user message, trimmed, makes a better title than
    /// "New chat". Applied once, only while the title is still the default.
    pub fn autotitle(&self, conversation_id: &str) -> Result<Option<String>> {
        let conversation = match self.get_conversation(conversation_id)? {
            Some(c) if c.title == "New chat" => c,
            _ => return Ok(None),
        };
        let conn = self.db.conn()?;
        let first: Option<String> = conn
            .query_row(
                "SELECT content FROM messages WHERE conversation_id = ?1 AND role = 'user'
                  ORDER BY ordinal LIMIT 1",
                [conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        drop(conn);
        let Some(first) = first else { return Ok(None) };

        let title: String = first
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("New chat")
            .trim()
            .chars()
            .take(60)
            .collect();
        let title = title.trim().to_string();
        if title.is_empty() {
            return Ok(None);
        }
        let mut updated = conversation;
        updated.title = title.clone();
        self.update_conversation(&updated)?;
        Ok(Some(title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(db: &Db) -> Conversation {
        ChatRepo::new(db)
            .create_conversation(NewConversation {
                title: String::new(),
                ..Default::default()
            })
            .unwrap()
    }

    #[test]
    fn a_conversation_defaults_to_a_placeholder_title() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(conversation(&db).title, "New chat");
    }

    #[test]
    fn messages_keep_their_order_across_reloads() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        repo.append_message(NewMessage::user(&chat.id, "first"))
            .unwrap();
        repo.append_message(NewMessage::assistant(&chat.id, "second"))
            .unwrap();
        repo.append_message(NewMessage::user(&chat.id, "third"))
            .unwrap();

        let messages = repo.messages(&chat.id).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn appending_a_message_touches_the_conversation() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        std::thread::sleep(std::time::Duration::from_millis(5));
        repo.append_message(NewMessage::user(&chat.id, "hello"))
            .unwrap();
        let reloaded = repo.get_conversation(&chat.id).unwrap().unwrap();
        assert!(reloaded.updated_at >= chat.updated_at);
    }

    #[test]
    fn citations_survive_the_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        let message = repo
            .append_message(NewMessage {
                citations: vec![Citation {
                    source_id: "src_1".into(),
                    document_id: "doc_1".into(),
                    file_name: "handbook.pdf".into(),
                    file_path: "/home/u/handbook.pdf".into(),
                    chunk_index: 2,
                    locator: Some("page 7".into()),
                    excerpt: "The policy states…".into(),
                    score: 0.77,
                }],
                ..NewMessage::assistant(&chat.id, "According to the handbook…")
            })
            .unwrap();

        let loaded = repo.get_message(&message.id).unwrap().unwrap();
        assert_eq!(loaded.citations.len(), 1);
        assert_eq!(loaded.citations[0].locator.as_deref(), Some("page 7"));
        assert_eq!(loaded.citations[0].file_name, "handbook.pdf");
    }

    #[test]
    fn a_stopped_generation_records_why_it_stopped() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        let message = repo
            .append_message(NewMessage::assistant(&chat.id, ""))
            .unwrap();
        repo.finalise_message(
            &message.id,
            "partial answer",
            &[],
            Some(14),
            Some("stopped_by_user"),
        )
        .unwrap();

        let loaded = repo.get_message(&message.id).unwrap().unwrap();
        assert_eq!(loaded.content, "partial answer");
        assert_eq!(loaded.stopped_reason.as_deref(), Some("stopped_by_user"));
        assert_eq!(loaded.token_estimate, Some(14));
    }

    #[test]
    fn edit_and_resend_truncates_from_the_edited_turn() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        repo.append_message(NewMessage::user(&chat.id, "one"))
            .unwrap();
        let second = repo
            .append_message(NewMessage::assistant(&chat.id, "two"))
            .unwrap();
        repo.append_message(NewMessage::user(&chat.id, "three"))
            .unwrap();

        let removed = repo.truncate_from(&chat.id, &second.id).unwrap();
        assert_eq!(removed, 2);
        let remaining = repo.messages(&chat.id).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].content, "one");

        // Appending after a truncation continues the numbering cleanly.
        repo.append_message(NewMessage::assistant(&chat.id, "new two"))
            .unwrap();
        assert_eq!(repo.messages(&chat.id).unwrap().len(), 2);
    }

    #[test]
    fn truncating_from_an_unknown_message_removes_nothing() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        repo.append_message(NewMessage::user(&chat.id, "one"))
            .unwrap();
        assert_eq!(repo.truncate_from(&chat.id, "msg_absent").unwrap(), 0);
        assert_eq!(repo.messages(&chat.id).unwrap().len(), 1);
    }

    #[test]
    fn a_conversation_titles_itself_from_the_first_user_message_once() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        assert_eq!(
            repo.autotitle(&chat.id).unwrap(),
            None,
            "nothing to title yet"
        );

        repo.append_message(NewMessage::user(
            &chat.id,
            "Summarise the quarterly report\nand list the risks",
        ))
        .unwrap();
        assert_eq!(
            repo.autotitle(&chat.id).unwrap().as_deref(),
            Some("Summarise the quarterly report")
        );
        assert_eq!(repo.autotitle(&chat.id).unwrap(), None, "does not re-title");
    }

    #[test]
    fn deleting_a_conversation_removes_its_messages() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let chat = conversation(&db);
        repo.append_message(NewMessage::user(&chat.id, "one"))
            .unwrap();
        repo.delete_conversation(&chat.id).unwrap();

        let count: i64 = db
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn archived_and_pinned_conversations_sort_and_filter_correctly() {
        let db = Db::open_in_memory().unwrap();
        let repo = ChatRepo::new(&db);
        let a = conversation(&db);
        let b = conversation(&db);

        let mut pinned = b.clone();
        pinned.pinned = true;
        pinned.title = "Pinned".into();
        repo.update_conversation(&pinned).unwrap();

        let listed = repo.list_conversations(None, false).unwrap();
        assert_eq!(listed[0].title, "Pinned");

        let mut archived = a;
        archived.archived = true;
        repo.update_conversation(&archived).unwrap();
        assert_eq!(repo.list_conversations(None, false).unwrap().len(), 1);
        assert_eq!(repo.list_conversations(None, true).unwrap().len(), 2);
    }
}
