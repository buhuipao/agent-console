//! `GET /api/sessions/{key}/messages` -- the conversation, read straight from the provider
//! transcript.
//!
//! The App mutex is held only long enough to read the session's agent kind and transcript
//! path; the file itself is read outside it, because a browser refreshing a conversation must
//! never stall the discovery tick that keeps every other session's status current.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;

use super::{
    AppState,
    transcript::{self, DEFAULT_LIMIT, MAX_LIMIT, MessagePage, Position},
};
use crate::model::AgentKind;

#[derive(Deserialize)]
pub(crate) struct MessagesQuery {
    /// Cursor from a previous response; returns messages newer than it.
    #[serde(default)]
    after: Option<String>,
    /// Cursor from a previous response; returns the page of messages older than it.
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(crate) async fn session_messages(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> Result<Json<MessagePage>, (StatusCode, String)> {
    let position = query.position()?;
    let (agent, path) = session_transcript(&state, &key)?;
    let Some(path) = path else {
        // A session created here but never started has no transcript yet. That is an empty
        // conversation, not an error -- the UI should show the composer, not a failure.
        return Ok(Json(MessagePage::empty()));
    };
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    transcript::read_page(&path, agent, position, limit)
        .map(Json)
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read the transcript for {key}: {error}\n"),
            )
        })
}

impl MessagesQuery {
    /// Which direction to read in. Empty strings are treated as absent so a client that always
    /// sends both parameters, one of them blank, still gets the page it meant.
    fn position(&self) -> Result<Position<'_>, (StatusCode, String)> {
        let after = non_empty(self.after.as_deref());
        let before = non_empty(self.before.as_deref());
        match (after, before) {
            (Some(_), Some(_)) => Err((
                StatusCode::BAD_REQUEST,
                "after and before select opposite directions; pass only one\n".to_owned(),
            )),
            (Some(after), None) => Ok(Position::After(after)),
            (None, Some(before)) => Ok(Position::Before(before)),
            (None, None) => Ok(Position::Tail),
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn session_transcript(
    state: &AppState,
    key: &str,
) -> Result<(AgentKind, Option<std::path::PathBuf>), (StatusCode, String)> {
    let app = state.app.lock().unwrap();
    app.sessions
        .iter()
        .find(|session| session.key == key)
        .map(|session| (session.agent, session.transcript_path.clone()))
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("no session with key {key}\n"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(after: Option<&str>, before: Option<&str>) -> MessagesQuery {
        MessagesQuery {
            after: after.map(str::to_owned),
            before: before.map(str::to_owned),
            limit: None,
        }
    }

    #[test]
    fn each_cursor_selects_its_own_direction_and_neither_means_the_tail() {
        assert!(matches!(query(None, None).position(), Ok(Position::Tail)));
        assert!(matches!(
            query(Some("v1.10"), None).position(),
            Ok(Position::After("v1.10"))
        ));
        assert!(matches!(
            query(None, Some("v1.10")).position(),
            Ok(Position::Before("v1.10"))
        ));
    }

    #[test]
    fn asking_for_both_directions_at_once_is_rejected() {
        let error = query(Some("v1.10"), Some("v1.20")).position().unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1.contains("only one"), "{}", error.1);
    }

    #[test]
    fn a_blank_cursor_is_the_same_as_not_sending_one() {
        assert!(matches!(
            query(Some(""), Some("")).position(),
            Ok(Position::Tail)
        ));
        assert!(matches!(
            query(Some(""), Some("v1.10")).position(),
            Ok(Position::Before("v1.10"))
        ));
    }
}
