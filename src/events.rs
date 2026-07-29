use std::{
    collections::BTreeMap,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    model::{AgentKind, Decision, Session, SessionStatus, unix_timestamp},
    store::{atomic_append_jsonl, ensure_private_dir, make_private_file, rotated_jsonl_path},
};

const EVENT_PREFIX_BYTES: u64 = 256;

#[derive(Debug)]
pub struct EventIndex {
    connection: Connection,
}

impl EventIndex {
    pub fn open(state_root: &Path) -> io::Result<Self> {
        ensure_private_dir(state_root)?;
        let path = state_root.join("state.db");
        let connection = Connection::open(&path).map_err(io::Error::other)?;
        make_private_file(&path)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA busy_timeout=2000;
                 CREATE TABLE IF NOT EXISTS event_cursor (
                     source_path TEXT PRIMARY KEY,
                     byte_offset INTEGER NOT NULL,
                     prefix_len INTEGER NOT NULL,
                     prefix_hash TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS normalized_event (
                     source_path TEXT NOT NULL,
                     source_offset INTEGER NOT NULL,
                     provider TEXT NOT NULL,
                     session_id TEXT NOT NULL,
                     event_timestamp INTEGER NOT NULL DEFAULT 0,
                     event_json TEXT NOT NULL,
                     PRIMARY KEY(source_path, source_offset)
                 );",
            )
            .map_err(io::Error::other)?;
        ensure_event_timestamp_column(&connection)?;
        connection
            .execute_batch(
                "DROP INDEX IF EXISTS normalized_event_session;
                 CREATE INDEX normalized_event_session
                     ON normalized_event(provider, session_id, event_timestamp, source_offset);",
            )
            .map_err(io::Error::other)?;
        Ok(Self { connection })
    }

    pub fn refresh_session(
        &mut self,
        path: &Path,
        provider: AgentKind,
        session_id: &str,
    ) -> io::Result<Vec<NormalizedEvent>> {
        self.index_path(&rotated_jsonl_path(path))?;
        self.index_path(path)?;
        self.events(provider, session_id)
    }

    pub fn events(
        &self,
        provider: AgentKind,
        session_id: &str,
    ) -> io::Result<Vec<NormalizedEvent>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_json FROM normalized_event
                 WHERE provider = ?1 AND session_id = ?2
                 ORDER BY event_timestamp, source_path, source_offset",
            )
            .map_err(io::Error::other)?;
        let rows = statement
            .query_map(params![provider.label(), session_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(io::Error::other)?;
        let mut events = Vec::new();
        for row in rows {
            let json = row.map_err(io::Error::other)?;
            if let Ok(event) = serde_json::from_str(&json) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn index_path(&mut self, path: &Path) -> io::Result<()> {
        let source = path.to_string_lossy().into_owned();
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.clear_source(&source)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let cursor = self
            .connection
            .query_row(
                "SELECT byte_offset, prefix_len, prefix_hash FROM event_cursor
                 WHERE source_path = ?1",
                [&source],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?.max(0) as u64,
                        row.get::<_, i64>(1)?.max(0) as u64,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(io::Error::other)?;
        let mut offset = cursor.as_ref().map_or(0, |cursor| cursor.0);
        let prefix_len = cursor
            .as_ref()
            .map_or(metadata.len().min(EVENT_PREFIX_BYTES), |cursor| cursor.1);
        let prefix_hash = hash_prefix(path, prefix_len)?;
        let rotated = cursor
            .as_ref()
            .is_some_and(|cursor| metadata.len() < cursor.0 || cursor.2 != prefix_hash);
        if rotated {
            self.clear_source(&source)?;
            offset = 0;
        }

        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file);
        let mut records = Vec::new();
        loop {
            let record_offset = offset;
            let mut bytes = Vec::new();
            let read = reader.read_until(b'\n', &mut bytes)?;
            if read == 0 || !bytes.ends_with(b"\n") {
                break;
            }
            offset = offset.saturating_add(read as u64);
            if let Ok(event) = serde_json::from_slice::<NormalizedEvent>(&bytes) {
                records.push((record_offset, event));
            }
        }

        self.connection
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(io::Error::other)?;
        let result = (|| {
            for (record_offset, event) in records {
                let json = serde_json::to_string(&event).map_err(io::Error::other)?;
                self.connection
                    .execute(
                        "INSERT OR REPLACE INTO normalized_event
                         (source_path, source_offset, provider, session_id, event_timestamp, event_json)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            source,
                            record_offset as i64,
                            event.provider.label(),
                            event.session_id,
                            event.timestamp as i64,
                            json
                        ],
                    )
                    .map_err(io::Error::other)?;
            }
            self.connection
                .execute(
                    "INSERT INTO event_cursor
                     (source_path, byte_offset, prefix_len, prefix_hash)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(source_path) DO UPDATE SET
                         byte_offset = excluded.byte_offset,
                         prefix_len = excluded.prefix_len,
                         prefix_hash = excluded.prefix_hash",
                    params![source, offset as i64, prefix_len as i64, prefix_hash],
                )
                .map_err(io::Error::other)?;
            Ok::<_, io::Error>(())
        })();
        match result {
            Ok(()) => self
                .connection
                .execute_batch("COMMIT")
                .map_err(io::Error::other),
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn clear_source(&self, source: &str) -> io::Result<()> {
        self.connection
            .execute(
                "DELETE FROM normalized_event WHERE source_path = ?1",
                [source],
            )
            .map_err(io::Error::other)?;
        self.connection
            .execute("DELETE FROM event_cursor WHERE source_path = ?1", [source])
            .map(|_| ())
            .map_err(io::Error::other)
    }

    #[cfg(test)]
    fn cursor_offset(&self, path: &Path) -> u64 {
        self.connection
            .query_row(
                "SELECT byte_offset FROM event_cursor WHERE source_path = ?1",
                [path.to_string_lossy().as_ref()],
                |row| row.get::<_, i64>(0).map(|value| value.max(0) as u64),
            )
            .unwrap_or(0)
    }
}

fn ensure_event_timestamp_column(connection: &Connection) -> io::Result<()> {
    let mut statement = connection
        .prepare("PRAGMA table_info(normalized_event)")
        .map_err(io::Error::other)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(io::Error::other)?;
    let mut has_timestamp = false;
    for column in columns {
        if column.map_err(io::Error::other)? == "event_timestamp" {
            has_timestamp = true;
        }
    }
    if !has_timestamp {
        connection
            .execute(
                "ALTER TABLE normalized_event
                 ADD COLUMN event_timestamp INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(io::Error::other)?;
    }
    Ok(())
}

fn hash_prefix(path: &Path, len: u64) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut bytes = vec![0; len as usize];
    file.read_exact(&mut bytes)?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted,
    UserMessage,
    AgentMessage,
    ToolStarted,
    ToolCompleted,
    ApprovalRequested,
    UserInputRequested,
    TurnCompleted,
    TurnFailed,
    SessionEnded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NormalizedEvent {
    pub provider: AgentKind,
    pub session_id: String,
    pub event_id: String,
    pub timestamp: u64,
    pub kind: EventKind,
    pub text: String,
}

pub fn ingest_hook(provider: AgentKind, input: &Value, events_dir: &Path) -> io::Result<PathBuf> {
    let session_id = input
        .get("session_id")
        .or_else(|| input.get("sessionId"))
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "hook has no session id"))?;
    let event_name = input
        .get("hook_event_name")
        .or_else(|| input.get("hookEventName"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let kind = hook_kind(event_name, input);
    let event_id = input
        .get("request_id")
        .or_else(|| input.get("tool_use_id"))
        .or_else(|| input.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{event_name}-{}", unix_timestamp()));
    let event = NormalizedEvent {
        provider,
        session_id: session_id.to_owned(),
        event_id,
        timestamp: unix_timestamp(),
        kind,
        text: hook_text(input, event_name),
    };
    let path = event_file(events_dir, provider, session_id);
    let line = serde_json::to_vec(&event).map_err(io::Error::other)?;
    atomic_append_jsonl(&path, &line)?;
    Ok(path)
}

pub fn event_file(events_dir: &Path, provider: AgentKind, session_id: &str) -> PathBuf {
    let safe_id = session_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    events_dir.join(format!("{}-{safe_id}.jsonl", provider.label()))
}

#[cfg(test)]
pub fn read_events(path: &Path) -> io::Result<Vec<NormalizedEvent>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect())
}

pub fn apply_events(session: &mut Session, events: &[NormalizedEvent]) {
    let mut pending = BTreeMap::new();
    let mut active = session.managed_alive
        && crate::model::unix_timestamp().saturating_sub(session.transcript_modified_at) < 5;
    let mut turn_failed = session.status == SessionStatus::Failed;
    for event in events {
        match event.kind {
            EventKind::UserMessage | EventKind::AgentMessage | EventKind::ToolStarted => {
                active = true;
                if event.kind == EventKind::UserMessage {
                    pending.clear();
                    turn_failed = false;
                }
            }
            EventKind::ToolCompleted => {
                active = true;
                pending.clear();
            }
            EventKind::ApprovalRequested | EventKind::UserInputRequested => {
                pending.insert(
                    event.event_id.clone(),
                    Decision {
                        id: event.event_id.clone(),
                        question: event.text.clone(),
                    },
                );
            }
            EventKind::TurnCompleted => {
                active = false;
                turn_failed = false;
                pending.clear();
            }
            EventKind::TurnFailed => {
                active = false;
                turn_failed = true;
            }
            EventKind::SessionEnded => {
                active = false;
            }
            EventKind::SessionStarted => active = false,
        }
    }
    session.pending_decisions = pending.into_values().collect();
    session.apply_deterministic_status(active, turn_failed);
}

fn hook_kind(name: &str, input: &Value) -> EventKind {
    match name {
        "SessionStart" => EventKind::SessionStarted,
        "UserPromptSubmit" => EventKind::UserMessage,
        "MessageDisplay" => EventKind::AgentMessage,
        "PreToolUse" => EventKind::ToolStarted,
        "PostToolUse" | "PostToolBatch" => EventKind::ToolCompleted,
        "PermissionRequest" => EventKind::ApprovalRequested,
        "Notification"
            if input.get("notification_type").and_then(Value::as_str)
                == Some("permission_prompt") =>
        {
            EventKind::ApprovalRequested
        }
        "Notification" => EventKind::AgentMessage,
        "Elicitation" => EventKind::UserInputRequested,
        "Stop" | "TaskCompleted" => EventKind::TurnCompleted,
        "StopFailure" | "PostToolUseFailure" => EventKind::TurnFailed,
        "SessionEnd" => EventKind::SessionEnded,
        _ => EventKind::AgentMessage,
    }
}

fn hook_text(input: &Value, event_name: &str) -> String {
    for key in [
        "prompt",
        "message",
        "reason",
        "tool_name",
        "task_subject",
        "error",
    ] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            return clean(text);
        }
    }
    if let Some(tool_input) = input.get("tool_input") {
        let text = serde_json::to_string(tool_input).unwrap_or_default();
        if !text.is_empty() {
            return clean(&text);
        }
    }
    event_name.to_owned()
}

fn clean(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1_000)
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::model::{SessionStatus, SessionSummary};

    fn session() -> Session {
        Session {
            key: "claude:abc".into(),
            provider_session_id: "abc".into(),
            name: "repo".into(),
            search_terms: Vec::new(),
            first_prompt: None,
            agent: AgentKind::Claude,
            status: SessionStatus::Idle,
            cwd: "/tmp".into(),
            branch: None,
            transcript_path: None,
            transcript_modified_at: 0,
            transcript_fingerprint: String::new(),
            summary_fingerprint: String::new(),
            summary_updated_at: None,
            summary_error: None,
            summary: SessionSummary::default(),
            recent_activity: Vec::new(),
            pending_decisions: Vec::new(),
            pending_shell_injection: None,
            managed_alive: true,
            unavailable_reason: None,
            discovered_after_startup: false,
        }
    }

    #[test]
    fn hook_round_trip_and_waiting_precedence() {
        let root = tempdir().unwrap();
        let hook = serde_json::json!({
            "session_id": "abc",
            "hook_event_name": "PermissionRequest",
            "tool_use_id": "tool-1",
            "tool_name": "Bash"
        });
        let path = ingest_hook(AgentKind::Claude, &hook, root.path()).unwrap();
        let events = read_events(&path).unwrap();
        let mut value = session();
        apply_events(&mut value, &events);
        assert_eq!(value.status, SessionStatus::Waiting);
        assert_eq!(value.pending_decisions[0].id, "tool-1");
    }

    #[test]
    fn user_message_resolves_pending_decisions() {
        let mut value = session();
        let events = vec![
            NormalizedEvent {
                provider: AgentKind::Claude,
                session_id: "abc".into(),
                event_id: "approval".into(),
                timestamp: 1,
                kind: EventKind::ApprovalRequested,
                text: "Run?".into(),
            },
            NormalizedEvent {
                provider: AgentKind::Claude,
                session_id: "abc".into(),
                event_id: "prompt".into(),
                timestamp: 2,
                kind: EventKind::UserMessage,
                text: "yes".into(),
            },
        ];
        apply_events(&mut value, &events);
        assert!(value.pending_decisions.is_empty());
        assert_eq!(value.status, SessionStatus::Working);
    }

    #[test]
    fn sqlite_index_reads_only_new_records_and_resets_after_rotation() {
        let root = tempdir().unwrap();
        let path = event_file(root.path(), AgentKind::Claude, "abc");
        let event = |id: &str, text: &str| NormalizedEvent {
            provider: AgentKind::Claude,
            session_id: "abc".into(),
            event_id: id.into(),
            timestamp: 1,
            kind: EventKind::AgentMessage,
            text: text.into(),
        };
        let append = |event: &NormalizedEvent| {
            let line = serde_json::to_vec(event).unwrap();
            atomic_append_jsonl(&path, &line).unwrap();
        };
        append(&event("one", "first"));
        let mut index = EventIndex::open(root.path()).unwrap();
        let first = index
            .refresh_session(&path, AgentKind::Claude, "abc")
            .unwrap();
        let first_offset = index.cursor_offset(&path);
        assert_eq!(first.len(), 1);

        append(&event("two", "second"));
        let second = index
            .refresh_session(&path, AgentKind::Claude, "abc")
            .unwrap();
        assert_eq!(second.len(), 2);
        assert!(index.cursor_offset(&path) > first_offset);

        let mut replacement = serde_json::to_vec(&event("three", "rotated replacement")).unwrap();
        replacement.push(b'\n');
        crate::store::write_private(&path, &replacement).unwrap();
        let rotated = index
            .refresh_session(&path, AgentKind::Claude, "abc")
            .unwrap();
        assert_eq!(
            rotated
                .iter()
                .map(|event| event.event_id.as_str())
                .collect::<Vec<_>>(),
            vec!["three"]
        );
    }
}
