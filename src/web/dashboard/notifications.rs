//! The alert queue, as data rather than as a mode.
//!
//! The TUI treats an alert as something one keypress consumes: `jump_to_next_notification`
//! selects the session *and* marks the entry read, in one shared piece of state. That works
//! when there is exactly one pair of eyes. A server has several -- two phones and a TUI can
//! watch the same sessions -- so consuming an alert on read would let whoever polled first
//! silently swallow it for everyone else.
//!
//! So reading is pure here. `GET` never changes anything, entries keep their place in the
//! queue after being read (up to the runtime's 100-entry cap), and every entry carries a
//! stable `id` and a `created_at`. A client that wants its own notion of "new" tracks the
//! ids it has shown and needs nothing from the server; the shared `read` flag is left for
//! the explicit, idempotent mark-read routes, which exist so that acknowledging an alert in
//! the browser also clears the TUI's badge when that is what the user meant.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    app::App,
    model::SessionStatus,
    web::{AppState, session_json},
};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list))
        .route("/api/notifications/read-all", post(read_all))
        .route("/api/notifications/{id}/read", post(read_one))
}

#[derive(Serialize)]
pub(crate) struct NotificationsJson {
    /// How many entries still carry the shared unread flag. This is the number the TUI puts
    /// in its header badge, so a browser showing it shows the same thing the TUI does.
    unread: usize,
    /// Oldest first -- the order `jump_to_next_notification` walks -- so the first entry with
    /// `read: false` is the one the TUI's "next alert" key would jump to.
    notifications: Vec<NotificationJson>,
}

#[derive(Serialize)]
pub(crate) struct NotificationJson {
    /// Unique for the life of the entry and across restarts, so a client can remember which
    /// alerts it has already shown without a counter that resets to zero.
    id: String,
    session_key: String,
    /// Resolved here rather than in the browser: an alert can outlive the session it names,
    /// and a client cannot join a key it can no longer find in the session list.
    session_title: String,
    /// Always `waiting` or `failed` -- the only two statuses that raise an alert.
    status: SessionStatus,
    message: String,
    created_at: u64,
    /// The shared flag, not this client's. A client with its own read-state should use its
    /// own record and treat this as "the TUI badge counts it".
    read: bool,
}

#[derive(Serialize)]
pub(crate) struct MarkReadJson {
    /// How many entries this call flipped, so an idempotent repeat is visibly a no-op.
    cleared: usize,
    unread: usize,
}

fn snapshot(app: &App) -> NotificationsJson {
    NotificationsJson {
        unread: app.unread_notification_count(),
        notifications: app
            .notifications()
            .map(|notification| NotificationJson {
                id: notification.id.clone(),
                session_key: notification.session_key.clone(),
                session_title: session_json::title_for_key(app, &notification.session_key),
                status: notification.status,
                message: notification.message.clone(),
                created_at: notification.created_at,
                read: notification.is_read(),
            })
            .collect(),
    }
}

pub(crate) async fn list(State(state): State<AppState>) -> Json<NotificationsJson> {
    let app = state.app.lock().unwrap();
    Json(snapshot(&app))
}

/// Marks one alert read. Idempotent: a second call reports `cleared: 0` rather than failing.
/// 404 only for an id this process never had or has already aged out of the queue.
pub(crate) async fn read_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MarkReadJson>, (StatusCode, String)> {
    let mut app = state.app.lock().unwrap();
    let was_unread = app
        .notifications()
        .find(|notification| notification.id == id)
        .map(|notification| !notification.is_read());
    let Some(was_unread) = was_unread else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no notification with id {id}\n"),
        ));
    };
    app.mark_notification_read(&id);
    Ok(Json(MarkReadJson {
        cleared: usize::from(was_unread),
        unread: app.unread_notification_count(),
    }))
}

/// Clears the shared badge in one call, the way a person dismisses a stack of alerts.
pub(crate) async fn read_all(State(state): State<AppState>) -> Json<MarkReadJson> {
    let mut app = state.app.lock().unwrap();
    let cleared = app.mark_all_notifications_read();
    Json(MarkReadJson {
        cleared,
        unread: app.unread_notification_count(),
    })
}

#[cfg(test)]
mod tests {
    use crate::model::SessionStatus;

    use super::*;

    /// Drives the runtime the way a running server does -- tick, change a status, tick again --
    /// so the queue under test is built by the same code path the TUI's alerts come from,
    /// including its rule that only a *transition* into a critical status raises one.
    fn app_with_two_alerts() -> App {
        let mut app = App::test_fixture();
        app.set_selected_notification_suppression(false);
        let mut second = app.sessions[0].clone();
        second.key = "claude:second".into();
        app.sessions.push(second);

        app.tick();
        app.sessions[0].status = SessionStatus::Waiting;
        app.sessions[1].status = SessionStatus::Failed;
        app.tick();
        app
    }

    #[test]
    fn every_alert_carries_a_stable_id_and_a_timestamp_clients_can_order_by() {
        let app = app_with_two_alerts();

        let json = snapshot(&app);
        assert_eq!(
            json.notifications.len(),
            2,
            "both transitions raised alerts"
        );
        assert_eq!(json.unread, 2);
        assert_ne!(
            json.notifications[0].id, json.notifications[1].id,
            "ids have to distinguish two alerts raised in the same second"
        );
        assert!(json.notifications.iter().all(|entry| entry.created_at > 0));
        assert!(json.notifications.iter().all(|entry| !entry.read));
        assert!(
            json.notifications
                .iter()
                .all(|entry| !entry.session_title.is_empty()),
            "an alert has to name its session even for a client that cannot find the key"
        );
    }

    #[test]
    fn the_selected_session_still_raises_an_alert_once_suppression_is_off() {
        let mut app = App::test_fixture();
        app.set_selected_notification_suppression(false);
        app.tick();
        app.selected = 0;
        app.sessions[0].status = SessionStatus::Waiting;
        app.tick();

        assert_eq!(
            snapshot(&app).unread,
            1,
            "the web has no single selected session, so suppressing for it only loses alerts"
        );
    }

    #[test]
    fn a_read_entry_stays_in_the_queue_so_a_second_client_still_sees_it() {
        let mut app = app_with_two_alerts();
        let first = snapshot(&app).notifications[0].id.clone();

        assert!(app.mark_notification_read(&first));

        let json = snapshot(&app);
        assert_eq!(
            json.notifications.len(),
            2,
            "marking read must not delete history a late client has not seen"
        );
        assert_eq!(json.unread, 1);
        assert!(json.notifications[0].read);
    }

    #[test]
    fn marking_read_is_idempotent_and_clearing_all_reports_what_it_changed() {
        let mut app = app_with_two_alerts();
        let first = snapshot(&app).notifications[0].id.clone();

        assert!(app.mark_notification_read(&first));
        assert!(
            app.mark_notification_read(&first),
            "a repeat has to succeed rather than 404 on an id that is still there"
        );
        assert_eq!(app.mark_all_notifications_read(), 1);
        assert_eq!(app.mark_all_notifications_read(), 0);
        assert_eq!(snapshot(&app).unread, 0);
    }

    #[test]
    fn an_unknown_id_is_not_in_the_queue() {
        let mut app = app_with_two_alerts();

        assert!(!app.mark_notification_read("not-an-id"));
    }
}
