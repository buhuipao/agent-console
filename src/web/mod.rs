mod agent;
mod api;
mod assets;
mod auth;
mod control;
mod dashboard;
mod dialog;
mod messages;
mod origin;
mod screen;
mod session_json;
pub(crate) mod settings;
mod shells;
mod transcript;
mod ws;

use std::{
    env, io,
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{Arc, Mutex, TryLockError},
    thread,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};

use crate::app::App;

pub(crate) use auth::AuthMode;
pub(crate) use settings::{WebEnv, WebOverrides, WebSettings};

/// Mirrors the TUI main loop's tick cadence (`event::poll(Duration::from_millis(100))` in
/// `main.rs`), so live session discovery keeps working with no TUI attached.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// How long a request waits for the App-wide lock before it gives up and says so.
///
/// The dashboard shares that lock now, and it holds it for the whole time a session workspace
/// is attached. Blocking a request for minutes would park a tokio worker per request and wedge
/// the whole server -- including the static shell -- with no explanation, so requests fail
/// fast and visibly instead.
const APP_LOCK_TIMEOUT: Duration = Duration::from_millis(400);
const APP_LOCK_RETRY: Duration = Duration::from_millis(20);

const BUSY_MESSAGE: &str = "the dashboard has a session workspace open and is holding the shared session state; \
     return it to the dashboard, or run `agent-console web` as its own process\n";

#[derive(Clone)]
pub(crate) struct AppState {
    app: Arc<Mutex<App>>,
    /// This layer's own screen state, kept separate from the terminals' shared parsers so
    /// reading a screen never disturbs the TUI or an attached websocket.
    screens: Arc<Mutex<screen::ScreenTrackers>>,
    auth: AuthMode,
    current_exe: PathBuf,
}

/// What a started server is, to whoever started it.
#[derive(Clone, Debug)]
pub struct WebRunning {
    /// The address to open, carrying the token when the server runs in token mode.
    pub url: String,
    /// Which credential the server is asking for. Never contains a password.
    pub auth: String,
    /// Whether the bound address is reachable from beyond this machine. The dashboard takes
    /// over the terminal immediately, so the warning printed to stderr scrolls out of sight
    /// and this is what lets it say so on screen instead.
    pub exposed: bool,
}

/// `agent-console web`: this process serves the web UI and nothing else.
pub fn run_web(settings: &WebSettings) -> io::Result<()> {
    let startup_cwd = env::current_dir()?;
    let mut app = App::load(startup_cwd)?;
    // The TUI drops an alert for the session it already has on screen. This process has no
    // screen and several clients, so `selected` here is just wherever the last request left
    // it -- suppressing for it would silently lose alerts nobody ever saw.
    app.set_selected_notification_suppression(false);
    let app = Arc::new(Mutex::new(app));
    let (state, listener, running) = bind(Arc::clone(&app), settings)?;
    // Nothing else advances discovery, event polling or summaries in this process.
    spawn_tick_thread(app);
    println!("Agent Console web UI: {}", running.url);
    println!("Authentication: {}", running.auth);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        axum::serve(listener, build_router(state)).await
    })
}

/// Starts the web server beside a running dashboard, on the dashboard's own `App`.
///
/// One `App`, so both surfaces see the same sessions, one discovery worker, and one summary
/// worker. No tick thread is started: the dashboard already ticks, both on its main loop and
/// once per frame while a workspace is attached, and a second ticker would only contend for
/// the same lock.
///
/// Binding happens on the caller's thread and before the terminal is taken over, so a port
/// conflict is an ordinary `Err` the dashboard can report and carry on from.
pub fn start_embedded(app: Arc<Mutex<App>>, settings: &WebSettings) -> io::Result<WebRunning> {
    let (state, listener, running) = bind(app, settings)?;
    thread::Builder::new()
        .name("agent-console-web".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    crate::diagnostics::record(&format!("web runtime failed to start: {error}"));
                    return;
                }
            };
            let served = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)?;
                axum::serve(listener, build_router(state)).await
            });
            if let Err(error) = served {
                crate::diagnostics::record(&format!("web server stopped: {error}"));
            }
        })?;
    Ok(running)
}

/// Resolves the host, binds the port, and assembles everything the router needs.
///
/// The listener is a blocking `std` one on purpose: it is created before any tokio runtime
/// exists, which is what lets both callers surface a bind failure as a plain `io::Error`.
fn bind(
    app: Arc<Mutex<App>>,
    settings: &WebSettings,
) -> io::Result<(AppState, TcpListener, WebRunning)> {
    let address = settings::resolve_bind(&settings.host, settings.port)?;
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    warn_if_not_loopback(&address, &settings.host);

    let auth = AuthMode::new(settings.credentials.clone());
    let running = WebRunning {
        url: url_for(&settings.host, settings.port, auth.token()),
        auth: auth.description(),
        exposed: !settings::is_loopback_bind(&address),
    };
    let state = AppState {
        app,
        screens: Arc::new(Mutex::new(screen::ScreenTrackers::default())),
        auth,
        current_exe: env::current_exe()?,
    };
    Ok((state, listener, running))
}

/// The address to open, with the token appended in token mode.
///
/// An IPv6 literal is bracketed: `http://::1:7878` is not a URL any browser can parse.
fn url_for(host: &str, port: u16, token: Option<&str>) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    match token {
        Some(token) => format!("http://{host}:{port}/?token={token}"),
        None => format!("http://{host}:{port}/"),
    }
}

/// Runs `App::tick()` on the same cadence the TUI's event loop uses, so discovery, event
/// polling, and summary scheduling keep advancing with no TUI attached.
fn spawn_tick_thread(app: Arc<Mutex<App>>) {
    thread::Builder::new()
        .name("agent-console-web-tick".into())
        .spawn(move || {
            loop {
                app.lock().unwrap().tick();
                thread::sleep(TICK_INTERVAL);
            }
        })
        .expect("failed to spawn the web tick thread");
}

fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        .route(
            "/api/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        .route("/api/sessions/{key}/archive", post(api::archive_session))
        .route("/api/sessions/{key}", delete(api::delete_session))
        .route(
            "/api/sessions/{key}/messages",
            get(messages::session_messages),
        )
        .route("/api/sessions/{key}/prompt", post(control::send_prompt))
        .route("/api/sessions/{key}/interrupt", post(control::interrupt))
        .route(
            "/api/sessions/{key}/prompt-status",
            get(control::blocking_prompt),
        )
        .route("/api/sessions/{key}/answer", post(control::answer_prompt))
        .route(
            "/api/sessions/{key}/shells",
            get(shells::list_shells).post(shells::create_shell),
        )
        .route(
            "/api/sessions/{key}/shells/{id}",
            delete(shells::delete_shell),
        )
        .route("/api/fs/complete", get(api::complete_path))
        .merge(sockets())
        .merge(dashboard::routes())
        // Inner first: nothing gets to probe how busy the dashboard is without credentials.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            reject_while_the_dashboard_holds_the_app,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let public = Router::new()
        .route("/api/health", get(api::health))
        .fallback(assets::serve_asset);

    Router::new()
        .merge(protected)
        .merge(public)
        .with_state(state)
}

/// The PTY control channels, behind an origin check of their own.
///
/// They are merged into the protected router, so they still sit behind the credential check
/// and the busy check like everything else; this adds the one thing credentials cannot supply.
/// A browser attaches cached Basic credentials to a handshake *another page* started, so
/// "has credentials" and "is the console's own page" are different questions and the socket
/// has to ask both.
fn sockets() -> Router<AppState> {
    Router::new()
        .route("/ws/sessions/{key}", get(ws::ws_handler))
        .route("/ws/sessions/{key}/shells/{id}", get(ws::shell_ws_handler))
        .layer(axum::middleware::from_fn(origin::refuse_cross_origin))
}

/// Answers 503 rather than blocking a worker thread for as long as the dashboard keeps the
/// App-wide lock. Probing here, once, keeps every handler below written as straight-line code
/// against `state.app.lock()`.
async fn reject_while_the_dashboard_holds_the_app(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let deadline = Instant::now() + APP_LOCK_TIMEOUT;
    loop {
        match probe_app_lock(&state) {
            AppLock::Free => return next.run(request).await,
            AppLock::Poisoned => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the session state is poisoned; restart agent-console\n",
                )
                    .into_response();
            }
            AppLock::Held => {
                if Instant::now() >= deadline {
                    return (StatusCode::SERVICE_UNAVAILABLE, BUSY_MESSAGE).into_response();
                }
                tokio::time::sleep(APP_LOCK_RETRY).await;
            }
        }
    }
}

enum AppLock {
    Free,
    Held,
    Poisoned,
}

/// Deliberately its own synchronous function: binding the guard anywhere in an `async fn`
/// keeps it in that future's state across the next `.await`, which costs the whole middleware
/// its `Send` bound even with an explicit `drop`.
fn probe_app_lock(state: &AppState) -> AppLock {
    match state.app.try_lock() {
        Ok(_) => AppLock::Free,
        Err(TryLockError::WouldBlock) => AppLock::Held,
        Err(TryLockError::Poisoned(_)) => AppLock::Poisoned,
    }
}

fn warn_if_not_loopback(address: &SocketAddr, host: &str) {
    if settings::is_loopback_bind(address) {
        return;
    }
    eprintln!(
        "warning: agent-console web is bound to {host} ({address}), which is not localhost-only.\n\
         The PTY control endpoint is reachable from the network -- anyone who gets past the\n\
         credential check gets full shell access to this machine. There is no built-in\n\
         HTTPS/TLS; if you expose this beyond localhost, put a trusted reverse proxy with TLS\n\
         in front of it."
    );
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use super::{auth::Credentials, *};

    pub(super) fn state_with(auth: AuthMode) -> AppState {
        AppState {
            app: Arc::new(Mutex::new(App::test_fixture())),
            screens: Arc::new(Mutex::new(screen::ScreenTrackers::default())),
            auth,
            current_exe: PathBuf::from("/usr/bin/true"),
        }
    }

    fn test_state() -> AppState {
        state_with(AuthMode::Token("secret".into()))
    }

    async fn respond(state: AppState, method: Method, uri: &str) -> axum::response::Response {
        build_router(state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Untokened requests separate the two outcomes that matter here: a mounted route is
    /// behind the auth layer and answers 401, while anything unmounted falls through to the
    /// app-shell fallback and answers 200 with `index.html`.
    async fn status_without_token(method: Method, uri: &str) -> StatusCode {
        respond(test_state(), method, uri).await.status()
    }

    /// A typo in a route pattern does not fail loudly -- the request lands on the SPA
    /// fallback and the browser reports "not implemented by this server build" instead.
    #[tokio::test]
    async fn the_shell_routes_are_mounted_and_token_guarded() {
        assert_eq!(
            status_without_token(Method::GET, "/api/sessions/claude:one/shells").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_without_token(Method::POST, "/api/sessions/claude:one/shells").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_without_token(Method::DELETE, "/api/sessions/claude:one/shells/abc").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_without_token(
                Method::GET,
                "/ws/sessions/claude:one/shells/abc?cols=80&rows=24"
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn the_agent_socket_keeps_its_own_path_beside_the_shell_ones() {
        assert_eq!(
            status_without_token(Method::GET, "/ws/sessions/claude:one").await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The two ways a websocket handshake reaches the origin check, end to end through the
    /// real router: credentials alone are not enough, and a header-less client is not
    /// collateral damage.
    async fn websocket_status(headers: &[(&str, &str)]) -> StatusCode {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/ws/sessions/claude:one?token=secret");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        build_router(test_state())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// `/ws/*` is a PTY control channel, and a browser replays cached Basic credentials on a
    /// handshake another page started. The session cookie is `SameSite=Strict` and stays
    /// behind; the credentials do not, so the handshake is refused on its origin instead.
    #[tokio::test]
    async fn a_cross_origin_websocket_handshake_is_refused_even_with_valid_credentials() {
        assert_eq!(
            websocket_status(&[
                ("host", "127.0.0.1:7878"),
                ("origin", "http://evil.example")
            ])
            .await,
            StatusCode::FORBIDDEN
        );
    }

    /// Refused *before* the upgrade, which is what separates it from a same-origin handshake:
    /// that one gets past the check and only then fails for want of the upgrade headers this
    /// hand-built request has no way to carry.
    #[tokio::test]
    async fn a_same_origin_handshake_gets_past_the_origin_check() {
        let status = websocket_status(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "http://127.0.0.1:7878"),
        ])
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN);
        assert_ne!(status, StatusCode::UNAUTHORIZED);
    }

    /// curl and every websockets library send no `Origin` at all. They are not browsers, carry
    /// no ambient credentials, and still have to pass the auth layer, so they are let through.
    #[tokio::test]
    async fn a_client_that_sends_no_origin_is_not_refused() {
        let status = websocket_status(&[("host", "127.0.0.1:7878")]).await;
        assert_ne!(status, StatusCode::FORBIDDEN);
        assert_ne!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unmounted_path_falls_through_to_the_app_shell() {
        assert_eq!(
            status_without_token(Method::GET, "/api/sessions/claude:one/not-a-route").await,
            StatusCode::OK,
            "the 401s above only prove a route exists because a missing one answers 200"
        );
    }

    /// The websocket routes matter most: a browser cannot put a header on the `WebSocket`
    /// constructor, so if these were not covered by the same layer as `/api/*` the terminal
    /// would be an unauthenticated shell on a bound port.
    #[tokio::test]
    async fn basic_mode_challenges_every_api_and_websocket_route() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        for uri in [
            "/api/sessions",
            "/api/notifications",
            "/ws/sessions/claude:one",
            "/ws/sessions/claude:one/shells/abc",
        ] {
            let response = respond(
                state_with(AuthMode::new(Some(credentials.clone()))),
                Method::GET,
                uri,
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
            assert!(
                response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("Basic ")),
                "{uri} must challenge with Basic so the browser prompts"
            );
        }
    }

    /// The token that would work in token mode is not a second way in when Basic is active.
    #[tokio::test]
    async fn basic_mode_does_not_also_accept_a_query_token() {
        let response = respond(
            state_with(AuthMode::new(Some(
                Credentials::parse("alice:hunter2", "--auth").unwrap(),
            ))),
            Method::GET,
            "/api/sessions?token=hunter2",
        )
        .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_app_shell_stays_public_in_basic_mode_so_the_page_can_load() {
        let response = respond(
            state_with(AuthMode::new(Some(
                Credentials::parse("alice:hunter2", "--auth").unwrap(),
            ))),
            Method::GET,
            "/index.html",
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Holds the App-wide lock on another thread, the way a dashboard with a session
    /// workspace attached does, and releases it when the returned handle is dropped.
    struct HeldApp {
        release: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl HeldApp {
        fn new(app: &Arc<Mutex<App>>) -> Self {
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let (taken, confirmed) = std::sync::mpsc::channel::<()>();
            let app = Arc::clone(app);
            let thread = thread::spawn(move || {
                let _guard = app.lock().unwrap();
                taken.send(()).expect("the test outlives this thread");
                let _ = wait.recv();
            });
            confirmed.recv().expect("the lock was never taken");
            Self {
                release: Some(release),
                thread: Some(thread),
            }
        }
    }

    impl Drop for HeldApp {
        fn drop(&mut self) {
            self.release.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The dashboard holds the App-wide lock for as long as it has a session workspace
    /// attached. Blocking there would park a tokio worker per request until it came back and
    /// take the whole server down with it, so a request that cannot get the lock is answered.
    #[tokio::test]
    async fn a_request_is_answered_rather_than_parked_while_the_dashboard_holds_the_app() {
        let state = test_state();
        let held = HeldApp::new(&state.app);

        let response = respond(state.clone(), Method::GET, "/api/sessions?token=secret").await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
        assert_eq!(
            respond(state, Method::GET, "/api/sessions?token=secret")
                .await
                .status(),
            StatusCode::OK,
            "the same request has to succeed once the dashboard lets go"
        );
    }

    /// The app shell and the health probe never touch the App, so they keep working while it
    /// is held -- which is what lets the page render the explanation at all.
    #[tokio::test]
    async fn the_shell_and_health_survive_a_held_app_lock() {
        let state = test_state();
        let _held = HeldApp::new(&state.app);

        assert_eq!(
            respond(state.clone(), Method::GET, "/index.html")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            respond(state, Method::GET, "/api/health").await.status(),
            StatusCode::OK
        );
    }

    /// Answering "busy" before checking the credential would let anyone on the port learn
    /// whether a dashboard has a workspace open.
    #[tokio::test]
    async fn an_uncredentialed_request_is_refused_before_the_busy_check_runs() {
        let state = test_state();
        let _held = HeldApp::new(&state.app);

        assert_eq!(
            respond(state, Method::GET, "/api/sessions").await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn an_ipv6_literal_is_bracketed_so_the_printed_url_parses() {
        assert_eq!(
            url_for("::1", 7878, None),
            "http://[::1]:7878/",
            "an unbracketed IPv6 host is not a URL"
        );
        assert_eq!(url_for("127.0.0.1", 7878, None), "http://127.0.0.1:7878/");
    }

    #[test]
    fn token_mode_puts_the_token_in_the_url_and_basic_mode_does_not() {
        assert_eq!(
            url_for("127.0.0.1", 7878, Some("abc")),
            "http://127.0.0.1:7878/?token=abc"
        );
        assert_eq!(url_for("127.0.0.1", 7878, None), "http://127.0.0.1:7878/");
    }
}
