//! A session's shells: real login shells running in the session's working directory.
//!
//! These are not the agent's own terminal. `agent.rs` reaches the Codex/Claude Code TUI, so
//! anything typed there is a prompt for the agent; a shell here runs the command itself,
//! which is what the TUI's own shell panes have always done.
//!
//! Shell terminals live in the PTY daemon under `shell|<session key>|<id>`, not in this
//! process, so the list a browser sees and the panes a TUI shows are views of the same
//! terminals. `TerminalManager::refresh_shells` is what adopts the ones another surface
//! opened, which is why listing goes through it rather than reading a local cache.

use std::{fmt, io};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::Serialize;

use crate::{app::App, model::Session, pty::ShellInfo};

use super::AppState;

/// Size a shell is created at before any viewport is known. A websocket attach resizes it to
/// the client's real size immediately afterwards, so this only shapes the first instant.
const HEADLESS_SIZE: (u16, u16) = (80, 24);

/// Why a shell request could not be served. Separate cases rather than one string so the
/// handler answers with a status instead of pattern-matching on prose.
pub(super) enum ShellError {
    UnknownSession(String),
    UnknownShell(String),
    /// The directory the shell would start in is gone, so spawning would fail obscurely.
    MissingCwd(String),
    Terminal(io::Error),
}

impl ShellError {
    fn status(&self) -> StatusCode {
        match self {
            Self::UnknownSession(_) | Self::UnknownShell(_) => StatusCode::NOT_FOUND,
            Self::MissingCwd(_) => StatusCode::CONFLICT,
            Self::Terminal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession(key) => write!(formatter, "no session with key {key}"),
            Self::UnknownShell(id) => write!(formatter, "no shell with id {id}"),
            Self::MissingCwd(cwd) => write!(
                formatter,
                "cannot open a shell: the working directory {cwd} no longer exists"
            ),
            Self::Terminal(error) => error.fmt(formatter),
        }
    }
}

fn rejection(error: ShellError) -> (StatusCode, String) {
    (error.status(), format!("{error}\n"))
}

#[derive(Serialize)]
pub(crate) struct ShellJson {
    id: String,
    name: String,
}

impl From<ShellInfo> for ShellJson {
    fn from(info: ShellInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
        }
    }
}

fn session_of(app: &App, key: &str) -> Result<Session, ShellError> {
    app.sessions
        .iter()
        .find(|session| session.key == key)
        .cloned()
        .ok_or_else(|| ShellError::UnknownSession(key.to_owned()))
}

/// Every shell open for this session, including ones a TUI opened.
pub(crate) async fn list_shells(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Vec<ShellJson>>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    let session = session_of(&app, &key).map_err(rejection)?;
    let shells = app
        .terminals
        .refresh_shells(&session, &state.current_exe, HEADLESS_SIZE)
        .map_err(|error| rejection(ShellError::Terminal(error)))?;
    Ok(Json(shells.into_iter().map(ShellJson::from).collect()))
}

/// Starts another shell in the session's working directory.
pub(crate) async fn create_shell(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<ShellJson>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    let session = session_of(&app, &key).map_err(rejection)?;
    // The same guard the TUI applies before opening a shell: a session whose directory was
    // deleted or moved would otherwise fail deep inside the pty spawn.
    if !session.cwd.is_dir() {
        return Err(rejection(ShellError::MissingCwd(
            session.cwd.display().to_string(),
        )));
    }
    // `add_shell` spawns through whatever backend the session view is already using, so this
    // has to run first for the shell to land in the daemon rather than in this process.
    app.terminals
        .ensure_session_view(&session, &state.current_exe, HEADLESS_SIZE)
        .map_err(|error| rejection(ShellError::Terminal(error)))?;
    let shell = app
        .terminals
        .add_shell(&session, HEADLESS_SIZE)
        .map_err(|error| rejection(ShellError::Terminal(error)))?;
    Ok(Json(ShellJson::from(shell)))
}

/// Kills one shell. The daemon forgets the terminal, so it leaves every surface at once.
pub(crate) async fn delete_shell(
    State(state): State<AppState>,
    Path((key, id)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    if app.terminals.close_shell(&key, &id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(rejection(ShellError::UnknownShell(id)))
    }
}

/// Points an existing shell at this client's viewport.
///
/// Same reason `agent::attach` resizes: a shell another surface started keeps its original
/// size forever, so attaching from a wide window would render into a narrow strip of it.
pub(super) fn attach(
    state: &AppState,
    key: &str,
    id: &str,
    size: (u16, u16),
) -> Result<(), ShellError> {
    let mut app = state.app.lock().unwrap();
    let session = session_of(&app, key)?;
    // A reload asks for a shell id this process may not have adopted yet -- it could have
    // been opened by a TUI, or by this browser before the server restarted.
    app.terminals
        .refresh_shells(&session, &state.current_exe, size)
        .map_err(ShellError::Terminal)?;
    app.terminals
        .shell(key, id)
        .ok_or_else(|| ShellError::UnknownShell(id.to_owned()))?
        .resize(size.0, size.1)
        .map_err(ShellError::Terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_session_or_shell_is_a_404_and_a_dead_directory_is_a_conflict() {
        assert_eq!(
            ShellError::UnknownSession("claude:one".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ShellError::UnknownShell("abc".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ShellError::MissingCwd("/gone".into()).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ShellError::Terminal(io::Error::other("boom")).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn a_shell_serializes_to_the_id_and_name_the_web_ui_switches_on() {
        let json = serde_json::to_value(ShellJson::from(ShellInfo {
            id: "9a3f".into(),
            name: "shell 2".into(),
        }))
        .unwrap();

        assert_eq!(json["id"], "9a3f");
        assert_eq!(json["name"], "shell 2");
    }
}
