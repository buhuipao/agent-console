//! Per-session dashboard actions: rename, retry the summary, take the input lease.
//!
//! Each of these is a TUI key binding that acts on "the selected session". Selection is a
//! single shared cursor, so re-using it here would mean one browser's rename moved another
//! browser's list. Every route below names its own session key instead, backed by keyed
//! variants of the same `App` methods the key bindings call.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{post, put},
};
use serde::{Deserialize, Serialize};

use crate::{
    pty::LeaseOutcome,
    web::{AppState, session_json::SessionJson},
};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/sessions/{key}/alias", put(set_alias))
        .route("/api/sessions/{key}/summary/retry", post(retry_summary))
        .route("/api/sessions/{key}/lease", post(acquire_lease))
}

#[derive(Deserialize)]
pub(crate) struct AliasRequest {
    /// `null` or an empty/whitespace string clears the alias, which is what the TUI's rename
    /// dialog does when you submit it empty. The title then falls back to the first prompt.
    alias: Option<String>,
}

/// Renames a session, writing through the same `StateStore` the TUI reads, so the new title
/// shows up in a running TUI without either surface knowing about the other.
pub(crate) async fn set_alias(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<AliasRequest>,
) -> Result<Json<SessionJson>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    app.set_session_alias(&key, body.alias.as_deref())
        .map_err(|error| (status_for(&error), format!("{error}\n")))?;
    let session = app
        .sessions
        .iter()
        .find(|session| session.key == key)
        .expect("set_session_alias rejects an unknown key");
    Ok(Json(SessionJson::from_app(&app, session)))
}

#[derive(Serialize)]
pub(crate) struct RetryJson {
    queued: bool,
}

/// Re-queues one session's summary at the front, clearing its backoff and its provider's
/// circuit breaker -- the same thing the TUI's "retry summary" key does. The work happens on
/// the summary worker, so this answers as soon as the job is queued, not when it finishes.
pub(crate) async fn retry_summary(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<RetryJson>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    app.retry_summary(&key)
        .map_err(|error| (status_for(&error), format!("{error}\n")))?;
    Ok(Json(RetryJson { queued: true }))
}

/// `App::retry_summary` and `App::set_session_alias` both report a missing session as prose,
/// which is the only failure that is not the server's fault.
fn status_for(error: &str) -> StatusCode {
    if error.starts_with("no session with key") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[derive(Default, Deserialize)]
pub(crate) struct LeaseRequest {
    /// False first: that reports who holds the lease without disturbing them, so the UI can
    /// name the conflict before asking the user to confirm. True is the TUI's takeover key.
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
pub(crate) struct LeaseJson {
    granted: bool,
    /// Who is holding it, present only when `granted` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<HolderJson>,
}

#[derive(Serialize)]
pub(crate) struct HolderJson {
    pid: u32,
    instance_id: String,
    started_at: u64,
}

/// Claims this session's input lease for the web server.
///
/// Writing to an agent that a TUI has open is refused by the PTY daemon, and until now the
/// browser had no way past it -- the TUI's takeover key lives inside its full-screen attach
/// loop, which the web never enters. This is that takeover, on its own.
///
/// A denial is a 200, not an error: asking is a legitimate operation and the answer -- who
/// holds it -- is the payload. The frontend flow is "POST with `force: false`; if `granted`
/// is false, show the holder's PID and offer a button that POSTs again with `force: true`".
///
/// The claim is deliberately not released. The daemon treats a lease as stale once its owner
/// has not revalidated for half a second, and the web server never revalidates, so a TUI can
/// take the session back by simply opening it -- no forcing, no cleanup call, and no way for
/// a closed browser tab to lock a session out.
pub(crate) async fn acquire_lease(
    State(state): State<AppState>,
    Path(key): Path<String>,
    body: Option<Json<LeaseRequest>>,
) -> Result<Json<LeaseJson>, (StatusCode, String)> {
    let force = body.map(|Json(body)| body.force).unwrap_or_default();
    let mut app = state.app.lock().unwrap();
    if !app.sessions.iter().any(|session| session.key == key) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no session with key {key}\n"),
        ));
    }
    let outcome = app
        .terminals
        .acquire_lease(&key, &state.current_exe, force)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{error}\n")))?;
    Ok(Json(match outcome {
        LeaseOutcome::Granted => LeaseJson {
            granted: true,
            holder: None,
        },
        LeaseOutcome::Denied(holder) => LeaseJson {
            granted: false,
            holder: Some(HolderJson {
                pid: holder.pid,
                instance_id: holder.instance_id,
                started_at: holder.started_at,
            }),
        },
    }))
}

#[cfg(test)]
mod tests {
    use crate::app::App;

    use super::*;

    #[test]
    fn an_alias_round_trips_through_the_store_the_tui_reads() {
        let mut app = App::test_fixture();
        let key = app.sessions[0].key.clone();
        let original = app.session_title(&app.sessions[0]);

        app.set_session_alias(&key, Some("  urgent release  "))
            .unwrap();

        assert_eq!(app.session_alias(&key), Some("urgent release"));
        assert_eq!(
            app.session_title(&app.sessions[0]),
            "urgent release",
            "the alias has to win over the derived title, as it does in the TUI"
        );

        app.set_session_alias(&key, Some("   ")).unwrap();
        assert_eq!(
            app.session_alias(&key),
            None,
            "an empty value clears the alias rather than storing whitespace"
        );
        assert_eq!(app.session_title(&app.sessions[0]), original);
    }

    #[test]
    fn clearing_with_a_null_alias_is_the_same_as_clearing_with_an_empty_one() {
        let mut app = App::test_fixture();
        let key = app.sessions[0].key.clone();
        app.set_session_alias(&key, Some("named")).unwrap();

        app.set_session_alias(&key, None).unwrap();

        assert_eq!(app.session_alias(&key), None);
    }

    #[test]
    fn renaming_or_retrying_an_unknown_session_is_a_404_rather_than_a_server_error() {
        let mut app = App::test_fixture();

        let rename = app
            .set_session_alias("codex:missing", Some("x"))
            .unwrap_err();
        let retry = app.retry_summary("codex:missing").unwrap_err();

        assert_eq!(status_for(&rename), StatusCode::NOT_FOUND);
        assert_eq!(status_for(&retry), StatusCode::NOT_FOUND);
    }

    #[test]
    fn retrying_a_summary_by_key_matches_the_selection_based_binding() {
        let mut app = App::test_fixture();
        let key = app.sessions[0].key.clone();

        app.retry_summary(&key).unwrap();

        assert_eq!(app.banner.as_deref(), Some("summary retry queued"));
    }

    #[test]
    fn a_denied_lease_serializes_the_holder_the_takeover_button_names() {
        let json = serde_json::to_value(LeaseJson {
            granted: false,
            holder: Some(HolderJson {
                pid: 4321,
                instance_id: "9f2c1d3a".into(),
                started_at: 1_700_000_000,
            }),
        })
        .unwrap();

        assert_eq!(json["granted"], false);
        assert_eq!(json["holder"]["pid"], 4321);

        let granted = serde_json::to_value(LeaseJson {
            granted: true,
            holder: None,
        })
        .unwrap();
        assert_eq!(granted["granted"], true);
        assert!(
            granted.get("holder").is_none(),
            "a granted lease has no holder to name"
        );
    }

    #[test]
    fn a_lease_request_defaults_to_asking_rather_than_forcing() {
        let asked: LeaseRequest = serde_json::from_str("{}").unwrap();
        let forced: LeaseRequest = serde_json::from_str(r#"{"force":true}"#).unwrap();

        assert!(
            !asked.force,
            "an unqualified request must never evict another surface"
        );
        assert!(forced.force);
    }
}
