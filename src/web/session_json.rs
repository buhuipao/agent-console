//! JSON shapes for the session list.
//!
//! The list is grouped by working directory, the way the TUI's sidebar groups it: a workspace
//! is the unit people think in, and a flat list of twenty sessions from six repos is not
//! something anyone can navigate on a phone.

use std::path::Path;

use serde::Serialize;

use crate::{
    app::App,
    model::{AgentKind, Decision, Session, SessionStatus, unix_timestamp},
};

#[derive(Serialize)]
pub(crate) struct WorkspacesJson {
    pub workspaces: Vec<WorkspaceJson>,
    /// The TUI header's four counts, over *every* session rather than the filtered subset.
    /// Filtering is one client narrowing its own view; the counts answer "what is happening
    /// on this machine", which does not change because someone typed in a search box.
    pub counts: StatusCountsJson,
    /// The badge the TUI shows for unread alerts. Carried here so a dashboard poll renders
    /// the header in one request instead of pairing every list refresh with a second call.
    pub unread_notifications: usize,
    /// The query this list was filtered by, normalized the way the matcher normalizes it.
    /// Echoed so a client can tell a stale response from a current one.
    pub query: String,
}

#[derive(Serialize)]
pub(crate) struct StatusCountsJson {
    pub working: usize,
    pub waiting: usize,
    pub idle: usize,
    pub failed: usize,
}

#[derive(Serialize)]
pub(crate) struct WorkspaceJson {
    pub path: String,
    pub name: String,
    pub sessions: Vec<SessionJson>,
}

/// `AgentKind`/`SessionStatus` already derive `Serialize` with
/// `#[serde(rename_all = "lowercase")]`, so they serialize the same way the rest of the
/// codebase reasons about them (`"claude"`/`"codex"`, `"working"`, ...).
#[derive(Serialize)]
pub(crate) struct SessionJson {
    pub key: String,
    pub title: String,
    /// The user-set name, when there is one. `title` already falls back to the derived name,
    /// so this exists to tell the two apart: the rename dialog opens on the title either way,
    /// but only a session that carries an explicit name is offered a way to clear it.
    pub alias: Option<String>,
    pub agent: AgentKind,
    pub status: SessionStatus,
    pub cwd: String,
    pub branch: Option<String>,
    pub archived: bool,
    pub managed_alive: bool,
    pub activity_age: String,
    pub updated_at: u64,
    pub summary: SummaryJson,
    pub pending_decisions: Vec<Decision>,
}

/// The parts of `SessionSummary` a conversation view shows. `status` and `needs_user` are
/// deliberately absent: they duplicate the session's own `status` and `pending_decisions`.
#[derive(Serialize)]
pub(crate) struct SummaryJson {
    pub task: String,
    pub current_action: String,
    pub next_step: String,
    pub progress: Vec<String>,
    pub blockers: Vec<String>,
}

impl SessionJson {
    pub(crate) fn from_app(app: &App, session: &Session) -> Self {
        Self {
            key: session.key.clone(),
            title: app.session_title(session),
            alias: app.session_alias(&session.key).map(str::to_owned),
            agent: session.agent,
            status: session.status,
            cwd: session.cwd.display().to_string(),
            branch: session.branch.clone(),
            archived: app.session_archived(session),
            managed_alive: session.managed_alive,
            activity_age: session.activity_age(unix_timestamp()),
            updated_at: session.transcript_modified_at,
            summary: SummaryJson {
                task: session.summary.task.clone(),
                current_action: session.summary.current_action.clone(),
                next_step: session.summary.next_step.clone(),
                progress: session.summary.progress.clone(),
                blockers: session.summary.blockers.clone(),
            },
            pending_decisions: session.pending_decisions.clone(),
        }
    }
}

/// The display title for a session key, for a payload that names a session it cannot
/// otherwise join -- an alert about a session that has since disappeared, say.
pub(crate) fn title_for_key(app: &App, key: &str) -> String {
    app.sessions
        .iter()
        .find(|session| session.key == key)
        .map(|session| app.session_title(session))
        .unwrap_or_else(|| key.to_owned())
}

/// Groups every session by working directory. Workspaces are ordered by their most recently
/// active session and sessions within one by recency, so the thing you touched last is the
/// first thing on screen.
///
/// `query` filters per request, using the TUI's own matching rules but none of its state.
/// The TUI's search dialog writes into a shared `SessionFilter`, so one search reorders the
/// single list everyone is looking at; here the query is an argument and two clients can
/// search for different things at the same time without either noticing the other.
pub(crate) fn workspaces(app: &App, query: &str) -> WorkspacesJson {
    let (working, waiting, idle, failed) = app.status_counts();
    let mut grouped: Vec<(String, Vec<SessionJson>)> = Vec::new();
    for session in app
        .sessions
        .iter()
        .filter(|session| app.session_matches(session, query))
    {
        let path = session.cwd.display().to_string();
        let entry = match grouped.iter_mut().find(|(existing, _)| *existing == path) {
            Some(entry) => entry,
            None => {
                grouped.push((path, Vec::new()));
                grouped.last_mut().expect("just pushed")
            }
        };
        entry.1.push(SessionJson::from_app(app, session));
    }

    let mut workspaces = grouped
        .into_iter()
        .map(|(path, mut sessions)| {
            sessions.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| left.key.cmp(&right.key))
            });
            WorkspaceJson {
                name: workspace_name(&path),
                path,
                sessions,
            }
        })
        .collect::<Vec<_>>();
    workspaces.sort_by(|left, right| {
        latest_activity(right)
            .cmp(&latest_activity(left))
            .then_with(|| left.path.cmp(&right.path))
    });
    WorkspacesJson {
        workspaces,
        counts: StatusCountsJson {
            working,
            waiting,
            idle,
            failed,
        },
        unread_notifications: app.unread_notification_count(),
        query: query.trim().to_lowercase(),
    }
}

fn latest_activity(workspace: &WorkspaceJson) -> u64 {
    workspace
        .sessions
        .iter()
        .map(|session| session.updated_at)
        .max()
        .unwrap_or(0)
}

/// The directory's own name, which is what a person calls the workspace. Falls back to the
/// full path for a root-level directory that has no final component.
fn workspace_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_json_serializes_agent_and_status_as_lowercase_strings() {
        let app = App::test_fixture();
        let session = &app.sessions[0];

        let json = SessionJson::from_app(&app, session);
        let value = serde_json::to_value(&json).unwrap();

        assert_eq!(value["key"], session.key);
        assert_eq!(value["agent"], session.agent.label());
        assert_eq!(value["status"], session.status.label());
        assert_eq!(value["managed_alive"], session.managed_alive);
        assert_eq!(value["archived"], false);
        assert_eq!(value["updated_at"], session.transcript_modified_at);
        assert_eq!(value["summary"]["task"], session.summary.task);
        assert!(value["summary"]["progress"].is_array());
        assert!(value["pending_decisions"].is_array());
        assert!(
            value["summary"].get("needs_user").is_none(),
            "pending decisions are reported once, at the session level"
        );
    }

    #[test]
    fn sessions_group_by_workspace_with_the_most_recent_work_first() {
        let mut app = App::test_fixture();
        let template = app.sessions[0].clone();

        let mut old_backend = template.clone();
        old_backend.key = "codex:old".into();
        old_backend.cwd = "/tmp/backend-api".into();
        old_backend.transcript_modified_at = 100;

        let mut new_backend = template.clone();
        new_backend.key = "claude:new".into();
        new_backend.cwd = "/tmp/backend-api".into();
        new_backend.transcript_modified_at = 300;

        let mut frontend = template.clone();
        frontend.key = "codex:frontend".into();
        frontend.cwd = "/tmp/frontend".into();
        frontend.transcript_modified_at = 200;

        app.sessions = vec![old_backend, frontend, new_backend];

        let grouped = workspaces(&app, "");
        let paths = grouped
            .workspaces
            .iter()
            .map(|workspace| workspace.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/tmp/backend-api", "/tmp/frontend"]);
        assert_eq!(grouped.workspaces[0].name, "backend-api");

        let keys = grouped.workspaces[0]
            .sessions
            .iter()
            .map(|session| session.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["claude:new", "codex:old"]);
    }

    #[test]
    fn the_grouped_response_matches_the_shape_the_web_ui_consumes() {
        let app = App::test_fixture();
        let value = serde_json::to_value(workspaces(&app, "")).unwrap();

        let workspace = &value["workspaces"][0];
        assert!(workspace["path"].is_string());
        assert!(workspace["name"].is_string());
        assert_eq!(workspace["sessions"][0]["key"], app.sessions[0].key);
    }

    /// Two sessions that differ in every searchable field, so one query can be shown to hit
    /// exactly one of them.
    fn app_with_two_searchable_sessions() -> App {
        let mut app = App::test_fixture();
        let template = app.sessions[0].clone();

        let mut backend = template.clone();
        backend.key = "codex:backend".into();
        backend.name = "backend-api".into();
        backend.cwd = "/tmp/backend-api".into();
        backend.branch = Some("feat/tokens".into());
        backend.summary.task = "Implement refresh-token rotation".into();

        let mut frontend = template;
        frontend.key = "claude:frontend".into();
        frontend.agent = AgentKind::Claude;
        frontend.status = SessionStatus::Waiting;
        frontend.name = "web-console".into();
        frontend.cwd = "/tmp/web-console".into();
        frontend.branch = Some("fix/layout".into());
        frontend.search_terms = vec!["responsive".into()];
        frontend.summary.task = "Tidy the sidebar".into();
        frontend.provider_session_id = "abc123".into();

        app.sessions = vec![backend, frontend];
        app
    }

    fn keys_matching(app: &App, query: &str) -> Vec<String> {
        workspaces(app, query)
            .workspaces
            .into_iter()
            .flat_map(|workspace| workspace.sessions)
            .map(|session| session.key)
            .collect()
    }

    /// The same haystack `SessionFilter` searches: alias, summary task, name, the provider's
    /// own search terms, cwd, branch, session id, agent, status, archived state.
    #[test]
    fn filtering_matches_every_field_the_tui_search_matches() {
        let mut app = app_with_two_searchable_sessions();
        app.set_session_alias("codex:backend", Some("urgent release"))
            .unwrap();

        for query in [
            "urgent",      // alias
            "rotation",    // summary task
            "backend-api", // name and cwd
            "feat/tokens", // branch
            "codex",       // agent label
        ] {
            assert_eq!(
                keys_matching(&app, query),
                vec!["codex:backend"],
                "{query:?} should have matched only the backend session"
            );
        }

        for query in [
            "responsive", // provider search terms
            "abc123",     // provider session id
            "waiting",    // status label
            "claude",     // agent label
        ] {
            assert_eq!(
                keys_matching(&app, query),
                vec!["claude:frontend"],
                "{query:?} should have matched only the frontend session"
            );
        }
    }

    #[test]
    fn matching_ignores_case_and_surrounding_whitespace_like_the_dialog_does() {
        let app = app_with_two_searchable_sessions();

        assert_eq!(
            keys_matching(&app, "  BACKEND-API  "),
            vec!["codex:backend"]
        );
        assert_eq!(keys_matching(&app, "Rotation"), vec!["codex:backend"]);
    }

    #[test]
    fn an_empty_query_filters_nothing_and_a_miss_filters_everything() {
        let app = app_with_two_searchable_sessions();

        assert_eq!(keys_matching(&app, "").len(), 2);
        assert_eq!(keys_matching(&app, "   ").len(), 2);
        assert!(keys_matching(&app, "nothing-matches-this").is_empty());
    }

    /// A filtered list is one client's view. The header counts describe the machine, so they
    /// must not shrink because someone typed in a search box on their phone.
    #[test]
    fn the_status_counts_cover_every_session_even_when_the_list_is_filtered() {
        let app = app_with_two_searchable_sessions();

        let filtered = workspaces(&app, "backend-api");

        assert_eq!(filtered.workspaces.len(), 1, "the list itself is narrowed");
        assert_eq!(filtered.counts.working, 1);
        assert_eq!(filtered.counts.waiting, 1);
        assert_eq!(filtered.counts.idle, 0);
        assert_eq!(filtered.counts.failed, 0);
        assert_eq!(filtered.query, "backend-api", "the query is echoed back");
    }

    #[test]
    fn the_response_carries_the_header_the_dashboard_draws_beside_the_list() {
        let app = App::test_fixture();

        let value = serde_json::to_value(workspaces(&app, "")).unwrap();

        assert!(value["counts"]["working"].is_number());
        assert!(value["counts"]["waiting"].is_number());
        assert!(value["counts"]["idle"].is_number());
        assert!(value["counts"]["failed"].is_number());
        assert_eq!(value["unread_notifications"], 0);
        assert_eq!(value["query"], "");
    }

    #[test]
    fn a_session_reports_its_alias_separately_from_the_title_it_falls_back_to() {
        let mut app = App::test_fixture();
        let key = app.sessions[0].key.clone();

        let before = serde_json::to_value(SessionJson::from_app(&app, &app.sessions[0])).unwrap();
        assert!(before["alias"].is_null());
        assert!(before["title"].is_string());

        app.set_session_alias(&key, Some("urgent release")).unwrap();
        let after = serde_json::to_value(SessionJson::from_app(&app, &app.sessions[0])).unwrap();
        assert_eq!(after["alias"], "urgent release");
        assert_eq!(after["title"], "urgent release");
    }
}
