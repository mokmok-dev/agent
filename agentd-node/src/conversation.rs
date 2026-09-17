//! The agent's `CloudEvents` and the conversation read model.
//!
//! The agent is event-driven like every other component: a turn begins when an
//! [`AGENT_INBOX`] event (a user prompt) is applied, and every finalized message
//! is published back as an event. The volatile token stream is never an event;
//! only the finalized [`Message`]s are.
//!
//! [`Conversation`] is the [`SqliteReducer`] that stores those messages. It is a
//! projection over the log, so it can be rebuilt with `--rebuild`.

use agentd_events::{LogEntry, Seq};
use agentd_inference::{Message, ToolCall};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::Value;
use std::path::Path;

use crate::error::AgentError;

/// A session began: the conversation id it runs and where.
pub const AGENT_SESSION_STARTED: &str = "agent.session.started";
/// A user prompt that starts a turn.
pub const AGENT_INBOX: &str = "agent.inbox";
/// A finalized assistant message.
pub const AGENT_MESSAGE: &str = "agent.message";
/// The result of a tool the assistant called.
pub const AGENT_TOOL_RESULT: &str = "agent.tool_result";
/// A turn began.
pub const AGENT_TURN_STARTED: &str = "agent.turn.started";
/// A turn completed without a provider error.
pub const AGENT_TURN_COMPLETED: &str = "agent.turn.completed";
/// A turn failed, for example because the provider returned an error.
pub const AGENT_TURN_FAILED: &str = "agent.turn.failed";

/// The conversation read model.
///
/// One row per finalized message; `seq` is the log position, so applying the
/// same event twice is a no-op.
#[derive(Debug)]
pub struct Conversation;

impl super::SqliteReducer for Conversation {
    type Error = AgentError;

    fn migrate(conn: &Connection) -> Result<(), Self::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_messages (
                seq INTEGER PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_calls TEXT,
                tool_call_id TEXT
            );
            CREATE INDEX IF NOT EXISTS agent_messages_conversation
                ON agent_messages (conversation_id, seq);
            CREATE TABLE IF NOT EXISTS agent_sessions (
                conversation_id TEXT PRIMARY KEY,
                workdir TEXT NOT NULL,
                model TEXT NOT NULL,
                seq INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS agent_sessions_workdir
                ON agent_sessions (workdir, seq);",
        )?;
        Ok(())
    }

    fn reduce(
        tx: &Transaction<'_>,
        entry: &LogEntry,
    ) -> Result<(), Self::Error> {
        let data = &entry.event.data;
        let conversation_id = field(data, "conversation_id").unwrap_or_default();
        if entry.event.r#type == AGENT_SESSION_STARTED {
            if conversation_id.is_empty() {
                return Ok(());
            }
            tx.execute(
                "INSERT INTO agent_sessions (conversation_id, workdir, model, seq) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(conversation_id) DO UPDATE SET \
                     workdir = excluded.workdir, model = excluded.model, seq = excluded.seq",
                rusqlite::params![
                    conversation_id,
                    field(data, "workdir").unwrap_or_default(),
                    field(data, "model").unwrap_or_default(),
                    crate::projection::seq_to_sql(entry.seq)?,
                ],
            )?;
            return Ok(());
        }
        let content = field(data, "content").unwrap_or_default();
        let (role, tool_calls, tool_call_id) = match entry.event.r#type.as_str() {
            AGENT_INBOX => (String::from("user"), None, None),
            AGENT_MESSAGE => {
                let tool_calls = data
                    .get("tool_calls")
                    .filter(|value| value.as_array().is_some_and(|calls| !calls.is_empty()))
                    .map(serde_json::to_string)
                    .transpose()?;
                (String::from("assistant"), tool_calls, None)
            },
            AGENT_TOOL_RESULT => (String::from("tool"), None, field(data, "tool_call_id")),
            _ => return Ok(()),
        };
        tx.execute(
            "INSERT OR REPLACE INTO agent_messages \
             (seq, conversation_id, role, content, tool_calls, tool_call_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                crate::projection::seq_to_sql(entry.seq)?,
                conversation_id,
                role,
                content,
                tool_calls,
                tool_call_id,
            ],
        )?;
        Ok(())
    }
}

impl Conversation {
    /// Reads the conversation's messages, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Sqlite`] if the query fails and
    /// [`AgentError::Json`] if a stored message cannot be decoded.
    pub fn history(
        conn: &Connection,
        conversation_id: &str,
    ) -> Result<Vec<Message>, AgentError> {
        let mut statement = conn.prepare(
            "SELECT role, content, tool_calls, tool_call_id FROM agent_messages \
             WHERE conversation_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = statement.query_map([conversation_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (role, content, tool_calls, tool_call_id) = row?;
            decode_message(&role, content, tool_calls.as_deref(), tool_call_id)
        })
        .collect()
    }

    /// Reads the conversation's last message and its log position.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Sqlite`] if the query fails and
    /// [`AgentError::Json`] if the stored message cannot be decoded.
    pub fn tail(
        conn: &Connection,
        conversation_id: &str,
    ) -> Result<Option<(Seq, Message)>, AgentError> {
        let mut statement = conn.prepare_cached(
            "SELECT seq, role, content, tool_calls, tool_call_id FROM agent_messages \
             WHERE conversation_id = ?1 ORDER BY seq DESC LIMIT 1",
        )?;
        let row = statement
            .query_row([conversation_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()?;
        let Some((seq, role, content, tool_calls, tool_call_id)) = row else {
            return Ok(None);
        };
        let seq = u64::try_from(seq)
            .map_err(|_| AgentError::History(format!("the stored position {seq} is negative")))?;
        let message = decode_message(&role, content, tool_calls.as_deref(), tool_call_id)?;
        Ok(Some((seq, message)))
    }

    /// The most recent session recorded for `workdir`, if any.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Sqlite`] if the query fails.
    pub fn latest_session(
        conn: &Connection,
        workdir: &str,
    ) -> Result<Option<String>, AgentError> {
        conn.query_row(
            "SELECT conversation_id FROM agent_sessions WHERE workdir = ?1 \
             ORDER BY seq DESC LIMIT 1",
            [workdir],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(AgentError::from)
    }
}

/// The stable key a session is scoped by: the canonical workdir path.
#[must_use]
pub fn session_key(workdir: &Path) -> String {
    workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf())
        .display()
        .to_string()
}

/// Rebuilds a [`Message`] from its stored columns.
fn decode_message(
    role: &str,
    content: String,
    tool_calls: Option<&str>,
    tool_call_id: Option<String>,
) -> Result<Message, AgentError> {
    let calls: Vec<ToolCall> = tool_calls
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let message = match role {
        "user" => Message::user(content),
        "assistant" if calls.is_empty() => Message::assistant(content),
        "assistant" => Message::with_tool_calls(content, calls),
        "tool" => Message::tool(
            tool_call_id.ok_or_else(|| {
                AgentError::History(String::from("a tool message has no tool_call_id"))
            })?,
            content,
        ),
        other => {
            return Err(AgentError::History(format!(
                "unknown message role {other:?}"
            )));
        },
    };
    Ok(message)
}

/// Reads a string field from an event's data, if it is present and a string.
fn field<'a>(
    data: &'a Value,
    name: &str,
) -> Option<&'a str> {
    data.get(name).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_INBOX, AGENT_MESSAGE, AGENT_SESSION_STARTED, AGENT_TOOL_RESULT, Conversation,
    };
    use crate::error::AgentError;
    use crate::projection::SqliteProjection;
    use agentd_events::{Event, LogEntry};
    use agentd_inference::{Message, ToolCall};
    use serde_json::json;

    fn entry(
        seq: u64,
        r#type: &str,
        data: serde_json::Value,
    ) -> LogEntry {
        LogEntry::new(seq, Event::new(r#type, data))
    }

    fn projection() -> (tempfile::TempDir, SqliteProjection<Conversation>) {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let projection = SqliteProjection::<Conversation>::open(dir.path().join("node.db"))
            .expect("projection should open");
        (dir, projection)
    }

    #[test]
    fn reduces_a_conversation_and_rebuilds_the_history() {
        let (_dir, mut projection) = projection();

        projection
            .apply(entry(
                1,
                AGENT_INBOX,
                json!({ "conversation_id": "c1", "content": "list files" }),
            ))
            .expect("inbox should apply");
        projection
            .apply(entry(
                2,
                AGENT_MESSAGE,
                json!({
                    "conversation_id": "c1",
                    "content": "",
                    "tool_calls": [{ "id": "call-1", "name": "shell", "arguments": "{\"command\":\"ls\"}" }],
                }),
            ))
            .expect("message should apply");
        projection
            .apply(entry(
                3,
                AGENT_TOOL_RESULT,
                json!({
                    "conversation_id": "c1",
                    "tool_call_id": "call-1",
                    "content": "exit code: 0\nsrc",
                }),
            ))
            .expect("tool result should apply");

        let history =
            Conversation::history(projection.connection(), "c1").expect("history should load");

        assert_eq!(
            history,
            vec![
                Message::user("list files"),
                Message::with_tool_calls(
                    "",
                    vec![ToolCall {
                        id: String::from("call-1"),
                        name: String::from("shell"),
                        arguments: String::from(r#"{"command":"ls"}"#),
                    }],
                ),
                Message::tool("call-1", "exit code: 0\nsrc"),
            ]
        );
    }

    #[test]
    fn history_is_scoped_to_one_conversation() {
        let (_dir, mut projection) = projection();

        projection
            .apply(entry(
                1,
                AGENT_INBOX,
                json!({ "conversation_id": "c1", "content": "one" }),
            ))
            .expect("apply should succeed");
        projection
            .apply(entry(
                2,
                AGENT_INBOX,
                json!({ "conversation_id": "c2", "content": "two" }),
            ))
            .expect("apply should succeed");

        let history =
            Conversation::history(projection.connection(), "c2").expect("history should load");
        assert_eq!(history, vec![Message::user("two")]);
    }

    #[test]
    fn sessions_are_recorded_and_resolved_by_workdir() {
        let (_dir, mut projection) = projection();

        for (seq, conversation, workdir) in [(1, "c1", "/a"), (2, "c2", "/b"), (3, "c3", "/a")] {
            projection
                .apply(entry(
                    seq,
                    AGENT_SESSION_STARTED,
                    json!({
                        "conversation_id": conversation,
                        "workdir": workdir,
                        "model": "m",
                    }),
                ))
                .expect("session should apply");
        }

        assert_eq!(
            Conversation::latest_session(projection.connection(), "/a")
                .expect("query")
                .as_deref(),
            Some("c3")
        );
        assert_eq!(
            Conversation::latest_session(projection.connection(), "/b")
                .expect("query")
                .as_deref(),
            Some("c2")
        );
        assert_eq!(
            Conversation::latest_session(projection.connection(), "/z").expect("query"),
            None
        );
    }

    #[test]
    fn an_unknown_role_is_reported() {
        let error = super::decode_message("wizard", String::new(), None, None)
            .expect_err("an unknown role should fail");

        assert!(matches!(error, AgentError::History(_)));
    }
}
