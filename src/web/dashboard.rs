//! Dashboard-level capability the TUI has always had and the web had no equivalent for:
//! alerts, rename, summary retry, input takeover, shell capture, and diagnostics.
//!
//! One module per concern, all mounted through a single [`routes`] so the router in
//! `mod.rs` grows by one line rather than one per endpoint.
//!
//! The recurring theme is that the TUI's versions of these are *modal and global* -- a search
//! dialog that reorders one shared list, a read flag that one keypress clears for everybody.
//! A server has many clients at once, so each of these is re-cut as a per-request question
//! (`?q=`), or as data plus an explicit, idempotent mutation (the alert queue), instead of a
//! mode the server is in.

mod doctor;
mod notifications;
mod session_ops;
mod shell_capture;

use axum::Router;

use super::AppState;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .merge(notifications::routes())
        .merge(session_ops::routes())
        .merge(shell_capture::routes())
        .merge(doctor::routes())
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
    };
    use tower::ServiceExt;

    use crate::web::AuthMode;

    use super::*;

    fn test_state() -> AppState {
        crate::web::tests::state_with(AuthMode::Token("secret".into()))
    }

    async fn status_without_token(method: Method, uri: &str) -> StatusCode {
        crate::web::build_router(test_state())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    /// Two properties at once, and no handler runs for either: a mounted route is behind the
    /// token layer and answers 401, while a route that was never mounted -- a typo in a path,
    /// or a `merge` that landed after the auth layer instead of before it -- falls through to
    /// the app-shell fallback and answers 200 with `index.html`. A silent 200 is exactly the
    /// failure a browser reports as "this server build does not implement that".
    #[tokio::test]
    async fn every_dashboard_route_is_mounted_behind_the_token_layer() {
        for (method, uri) in [
            (Method::GET, "/api/notifications"),
            (Method::POST, "/api/notifications/read-all"),
            (Method::POST, "/api/notifications/abc/read"),
            (Method::PUT, "/api/sessions/codex:test/alias"),
            (Method::POST, "/api/sessions/codex:test/summary/retry"),
            (Method::POST, "/api/sessions/codex:test/lease"),
            (Method::GET, "/api/sessions/codex:test/shells/abc/capture"),
            (Method::POST, "/api/sessions/codex:test/shells/abc/stage"),
            (Method::GET, "/api/doctor"),
        ] {
            assert_eq!(
                status_without_token(method.clone(), uri).await,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} is not mounted behind the auth layer"
            );
        }
    }

    /// The session list keeps its own path, so adding the dashboard routes underneath
    /// `/api/sessions/{key}/...` did not shadow it.
    #[tokio::test]
    async fn the_session_list_still_answers_on_its_own_path() {
        assert_eq!(
            status_without_token(Method::GET, "/api/sessions?q=backend").await,
            StatusCode::UNAUTHORIZED
        );
    }
}
