//! Getting a shell's output out of the shell and into the agent.
//!
//! Two TUI bindings live here: "copy shell output" and "stage shell output". Both start from
//! the same capture and differ only in where it goes.
//!
//! The capture is *not* taken through `TerminalManager::shell_capture`. That reads the
//! terminal's shared parser via `plain_capture`, which syncs it -- advancing the offset and
//! parser the TUI and the terminal websocket render from. This layer instead asks
//! `ManagedTerminal::poll_raw` for the retained output with its own offset, exactly as
//! `web::screen` does for the agent, and decodes it with the same `pty::plain_text` the TUI
//! uses, so the two surfaces produce the same text without sharing mutable state.
//!
//! Which shell is named explicitly, by the id `/api/sessions/{key}/shells` hands out. The
//! TUI can say "the selected shell" because it has one selected pane; a browser with two
//! shell tabs open, and a second browser beside it, does not.

use std::{fmt, io};

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    pty,
    web::{AppState, agent, agent::AgentError},
};

/// Size a shell adopted purely so its output could be read is created at. It is only ever
/// applied when nothing else has claimed the terminal; an attached websocket resizes it to a
/// real viewport, and this must not take that away.
const HEADLESS_SIZE: (u16, u16) = (80, 24);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/sessions/{key}/shells/{id}/capture",
            get(shell_capture),
        )
        .route("/api/sessions/{key}/shells/{id}/stage", post(stage_capture))
}

pub(super) enum CaptureError {
    UnknownSession(String),
    UnknownShell(String),
    /// The shell has produced nothing worth copying. The TUI reports the same thing rather
    /// than putting an empty string on the clipboard.
    Empty,
    Terminal(io::Error),
    Agent(AgentError),
}

impl CaptureError {
    fn status(&self) -> StatusCode {
        match self {
            Self::UnknownSession(_) | Self::UnknownShell(_) => StatusCode::NOT_FOUND,
            Self::Empty => StatusCode::CONFLICT,
            Self::Terminal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Agent(error) => error.status(),
        }
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession(key) => write!(formatter, "no session with key {key}"),
            Self::UnknownShell(id) => write!(
                formatter,
                "no shell with id {id}; open one from the session's Shell tab first"
            ),
            Self::Empty => formatter.write_str("the shell has not produced any output yet"),
            Self::Terminal(error) => error.fmt(formatter),
            Self::Agent(error) => error.fmt(formatter),
        }
    }
}

fn rejection(error: CaptureError) -> (StatusCode, String) {
    (error.status(), format!("{error}\n"))
}

#[derive(Serialize)]
pub(crate) struct CaptureJson {
    /// Plain text, already stripped of escape sequences and bounded the same way the TUI
    /// bounds what it copies. The browser owns the clipboard, so it is returned rather than
    /// written anywhere.
    text: String,
}

#[derive(Serialize)]
pub(crate) struct StagedJson {
    staged: bool,
    /// How much text was pasted, so the UI can say "pasted 2.1 kB of shell output" without
    /// the response carrying a second copy of it.
    bytes: usize,
}

/// The shell's recent output as plain text.
pub(crate) async fn shell_capture(
    State(state): State<AppState>,
    Path((key, id)): Path<(String, String)>,
) -> Result<Json<CaptureJson>, (StatusCode, String)> {
    let text = capture(&state, &key, &id).map_err(rejection)?;
    Ok(Json(CaptureJson { text }))
}

/// Puts the shell's output into the agent's composer, wrapped in the `<shell-output>` markers
/// `pty::staged_shell_text` writes, without submitting a turn.
///
/// The TUI stages this on the session and pastes it the next time someone *enters* the agent,
/// because in a TUI those are separate moments. In a browser they are not: there is no later
/// "enter", the agent view is already open, and a staged string nothing consumes would look
/// like the button did nothing. So this pastes now -- the same bytes, at the point the user
/// asked for them -- and deliberately does not also set `pending_shell_injection`, which
/// would paste the text a second time the next time a TUI opened the session.
pub(crate) async fn stage_capture(
    State(state): State<AppState>,
    Path((key, id)): Path<(String, String)>,
) -> Result<Json<StagedJson>, (StatusCode, String)> {
    let cwd = session_cwd(&state, &key).map_err(rejection)?;
    let text = capture(&state, &key, &id).map_err(rejection)?;
    let staged =
        pty::staged_shell_text(&cwd, &text).ok_or_else(|| rejection(CaptureError::Empty))?;
    // Same gate the prompt endpoint uses: a paste written before the agent is reading input
    // is swallowed with no error anywhere.
    agent::await_input_ready(&state, &key)
        .await
        .map_err(|error| rejection(CaptureError::Agent(error)))?;
    agent::write(&state, &key, &pty::bracketed_paste(&staged))
        .map_err(|error| rejection(CaptureError::Agent(error)))?;
    Ok(Json(StagedJson {
        staged: true,
        bytes: staged.len(),
    }))
}

fn session_cwd(state: &AppState, key: &str) -> Result<std::path::PathBuf, CaptureError> {
    state
        .app
        .lock()
        .unwrap()
        .sessions
        .iter()
        .find(|session| session.key == key)
        .map(|session| session.cwd.clone())
        .ok_or_else(|| CaptureError::UnknownSession(key.to_owned()))
}

/// Reads one shell's retained output without disturbing any other reader.
///
/// Polling from offset 0 asks for everything the daemon still holds. When that is more than
/// its ring buffer keeps, the daemon answers with a checkpoint instead -- a full repaint of
/// the current screen -- which is the honest meaning of "recent output" for a shell that has
/// been running for hours.
fn capture(state: &AppState, key: &str, id: &str) -> Result<String, CaptureError> {
    let mut app = state.app.lock().unwrap();
    let session = app
        .sessions
        .iter()
        .find(|session| session.key == key)
        .cloned()
        .ok_or_else(|| CaptureError::UnknownSession(key.to_owned()))?;
    // The shell may have been opened by a TUI, or by this browser before a server restart,
    // in which case this process has not adopted its terminal yet.
    app.terminals
        .refresh_shells(&session, &state.current_exe, HEADLESS_SIZE)
        .map_err(CaptureError::Terminal)?;
    let terminal = app
        .terminals
        .shell(key, id)
        .ok_or_else(|| CaptureError::UnknownShell(id.to_owned()))?;
    let poll = terminal
        .poll_raw(0, pty::Scrollback::Omit)
        .map_err(CaptureError::Terminal)?;
    let text = pty::plain_text(&raw_bytes(poll));
    if text.trim().is_empty() {
        return Err(CaptureError::Empty);
    }
    Ok(text)
}

/// A checkpoint supersedes everything before it, and `poll_raw` never returns both a
/// checkpoint and bytes that predate it, so concatenating them is the whole retained stream.
fn raw_bytes(poll: pty::RawPoll) -> Vec<u8> {
    let mut bytes = poll.checkpoint.unwrap_or_default();
    bytes.extend(poll.bytes);
    bytes
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_missing_session_or_shell_is_a_404_and_an_empty_shell_is_a_conflict() {
        assert_eq!(
            CaptureError::UnknownSession("claude:one".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            CaptureError::UnknownShell("abc".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(CaptureError::Empty.status(), StatusCode::CONFLICT);
        assert_eq!(
            CaptureError::Terminal(io::Error::other("boom")).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// A lease conflict has to survive the trip through the capture error, or the "Take over"
    /// button never appears for the one action most likely to hit it.
    #[test]
    fn a_lease_conflict_while_staging_stays_a_lease_conflict() {
        assert_eq!(
            CaptureError::Agent(AgentError::LeaseDenied).status(),
            StatusCode::LOCKED
        );
    }

    #[test]
    fn a_checkpoint_and_the_bytes_after_it_are_read_as_one_stream() {
        let poll = pty::RawPoll {
            start: 0,
            end: 12,
            bytes: b" second".to_vec(),
            checkpoint: Some(b"first".to_vec()),
            scrollback: None,
            alive: true,
            exit: None,
        };

        assert_eq!(raw_bytes(poll), b"first second".to_vec());
    }

    /// The escape sequences a checkpoint is made of must not reach the clipboard, and the
    /// text must match what the TUI copies for the same bytes.
    #[test]
    fn the_capture_is_the_same_plain_text_the_tui_copies() {
        let raw = b"\x1b[2J\x1b[H$ cargo test\r\n\x1b[32mok\x1b[0m\r\n";

        assert_eq!(pty::plain_text(raw), "$ cargo test\nok");
    }

    #[test]
    fn staging_wraps_the_capture_in_the_markers_the_agent_is_told_to_read() {
        let staged = pty::staged_shell_text(Path::new("/tmp/backend"), "3 tests passed").unwrap();

        assert!(staged.contains("Shell output from /tmp/backend:"));
        assert!(staged.contains("<shell-output>\n3 tests passed\n</shell-output>"));
        assert!(
            pty::staged_shell_text(Path::new("/tmp/backend"), "   ").is_none(),
            "an empty capture is nothing to stage"
        );
    }

    #[test]
    fn the_staged_payload_is_a_paste_and_never_submits_the_turn() {
        let staged = pty::staged_shell_text(Path::new("/tmp/backend"), "output").unwrap();
        let payload = pty::bracketed_paste(&staged);

        assert!(payload.starts_with(b"\x1b[200~"));
        assert!(payload.ends_with(b"\x1b[201~"));
        assert!(
            !payload.ends_with(b"\r"),
            "the user decides when the shell output becomes a turn"
        );
    }
}
