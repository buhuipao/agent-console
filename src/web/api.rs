use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::{DialogField, NewSessionDialog},
    completion::workspace_directory_completions,
    model::AgentKind,
};

use super::{
    AppState,
    session_json::{self, SessionJson, WorkspacesJson},
};

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    ok: bool,
    version: &'static str,
    /// `"basic"` or `"token"`. The one unauthenticated fact the frontend needs: with HTTP
    /// Basic the browser owns the credential prompt, so the page must not open a token
    /// dialog of its own on top of it.
    auth: &'static str,
}

pub(crate) async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        auth: state.auth.kind(),
    })
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    /// The TUI's session search, as a per-request filter. Absent or empty means no filter.
    #[serde(default)]
    q: String,
}

/// The session list, optionally narrowed by `?q=`.
///
/// Filtering is a query parameter rather than server state on purpose: the TUI's search is a
/// modal that rewrites the one list it draws, which is fine for one screen and wrong for a
/// server several clients poll. The matching rules are the TUI's, from `App::session_matches`.
pub(crate) async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Json<WorkspacesJson> {
    let app = state.app.lock().unwrap();
    Json(session_json::workspaces(&app, &query.q))
}

#[derive(Deserialize)]
pub(crate) struct CompleteQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
pub(crate) struct CompleteResponse {
    entries: Vec<String>,
}

/// Directory completion for the new-session dialog, backed by the same function the TUI's own
/// working-directory field uses -- including its `~` handling -- so the two surfaces can never
/// disagree about what a path completes to.
pub(crate) async fn complete_path(Query(query): Query<CompleteQuery>) -> Json<CompleteResponse> {
    Json(CompleteResponse {
        entries: workspace_directory_completions(&query.path, dirs::home_dir().as_deref()),
    })
}

#[derive(Deserialize)]
pub(crate) struct CreateSessionRequest {
    agent: AgentKind,
    cwd: String,
}

/// Creates a session the same way the TUI's "new session" dialog does: populate the same
/// `NewSessionDialog` state `create_from_dialog` already validates (cwd must be a directory)
/// and inserts from, then read back the session it created.
///
/// The dialog is put back the way it was found. A dashboard may be sharing this `App`, and
/// `create_from_dialog` leaves its input in place when it rejects one -- which would open a
/// modal "new session" dialog on somebody else's screen, pre-filled with a browser's typo.
pub(crate) async fn create_session(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<Json<SessionJson>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    let restore = app.dialog.take();
    app.dialog = Some(NewSessionDialog {
        provider: body.agent,
        cwd: body.cwd,
        cwd_cursor: 0,
        cwd_replace_on_input: false,
        cwd_completion_index: 0,
        cwd_completion_accepted: false,
        field: DialogField::Provider,
        error: None,
    });
    let created = app.create_from_dialog();
    app.dialog = restore;
    created.map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let session = SessionJson::from_app(&app, &app.sessions[0]);
    Ok(Json(session))
}

/// Toggles archived state for an arbitrary session key by temporarily pointing the app's
/// `selected` index at it and reusing `toggle_selected_archive` -- the same path the TUI's
/// archive key binding uses.
///
/// The cursor is restored afterwards. It is the dashboard's single selected session as well
/// as this handler's scratch variable, and one browser archiving a session must not move
/// what a dashboard beside it has highlighted.
pub(crate) async fn archive_session(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<SessionJson>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    let Some(index) = app.sessions.iter().position(|session| session.key == key) else {
        return Err((StatusCode::NOT_FOUND, format!("no session with key {key}")));
    };
    let restore = app.selected;
    app.selected = index;
    let toggled = app.toggle_selected_archive();
    // `toggle_selected_archive` normalizes the cursor when the session leaves the visible
    // list, so the restored index is re-normalized rather than trusted.
    app.selected = restore.min(app.sessions.len().saturating_sub(1));
    toggled.map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let session = SessionJson::from_app(&app, &app.sessions[index]);
    Ok(Json(session))
}

pub(crate) async fn delete_session(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> StatusCode {
    state.app.lock().unwrap().terminals.terminate_agent(&key);
    // The screen we tracked belongs to the terminal that just died; keeping it would show a
    // dead agent's last frame to whatever starts next under this key.
    super::agent::forget_screen(&state, &key);
    StatusCode::NO_CONTENT
}
