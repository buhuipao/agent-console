//! Who is allowed to reach `/api/*` and `/ws/*`.
//!
//! Two modes, chosen once at startup and never mixed:
//!
//! * [`AuthMode::Basic`] -- HTTP Basic, active whenever credentials were configured (see
//!   `super::settings`). The browser draws the credential prompt itself, so the app ships no
//!   login form of its own.
//! * [`AuthMode::Token`] -- the historical random per-process token, used only when no
//!   credentials were configured anywhere. The server is never unauthenticated.
//!
//! Static shell assets are served by a separate, unauthenticated router: the page has to load
//! before it can tell the user which of the two it is looking at.

use std::fmt;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use uuid::Uuid;

use super::AppState;

/// The realm the browser shows in its credential prompt.
const REALM: &str = "Agent Console";

/// Name of the cookie a Basic-authenticated response hands back.
///
/// The `WebSocket` constructor cannot set an `Authorization` header, so the socket depends on
/// the browser re-sending something on the handshake. Chrome does replay cached Basic
/// credentials for a same-origin handshake, but that is browser behaviour this code cannot
/// enforce, and a terminal that silently fails to connect is the worst possible failure here.
/// The cookie is the belt to that braces: same-origin, `HttpOnly`, `SameSite=Strict`, issued
/// only to a request that already proved it knows the password.
const SESSION_COOKIE: &str = "agent-console-session";

/// HTTP Basic credentials.
///
/// Basic itself forbids a colon in the username and allows one in the password, which is
/// exactly what `split_once(':')` produces -- so `user:pa:ss` is the password `pa:ss`.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Credentials {
    pub(crate) user: String,
    pub(crate) password: String,
}

/// Written by hand so that no `{:?}` anywhere -- a settings dump, a panic message, a future
/// `dbg!` -- can print the password. A derived `Debug` would.
impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Credentials {
    /// Parses the one `user:password` shape shared by `--auth`, `AGENT_CONSOLE_WEB_AUTH` and
    /// `[web] auth`.
    ///
    /// The error names the source and the rule, never the value: this string is a password on
    /// every path that reaches here.
    pub(crate) fn parse(value: &str, source: &str) -> Result<Self, String> {
        let Some((user, password)) = value.split_once(':') else {
            return Err(format!(
                "{source} must be <user>:<password> (no colon found)"
            ));
        };
        if user.is_empty() {
            return Err(format!("{source} has an empty user before the colon"));
        }
        if password.is_empty() {
            return Err(format!("{source} has an empty password after the colon"));
        }
        Ok(Self {
            user: user.to_owned(),
            password: password.to_owned(),
        })
    }

    fn matches(&self, user: &str, password: &str) -> bool {
        // Both halves are compared, and both comparisons always run: `&` rather than `&&` so
        // a wrong user does not answer faster than a wrong password.
        constant_time_eq(self.user.as_bytes(), user.as_bytes())
            & constant_time_eq(self.password.as_bytes(), password.as_bytes())
    }
}

/// How this server authenticates, decided once at startup.
///
/// Deliberately not `Debug`: every variant carries a secret -- the password, the session
/// cookie, or the token -- and a derived `Debug` is how those end up in a panic message.
/// [`AuthMode::description`] is the printable form.
#[derive(Clone)]
pub(crate) enum AuthMode {
    Basic {
        credentials: Credentials,
        /// Per-process secret handed to a Basic-authenticated browser so its websocket
        /// handshake carries something even if the browser declines to replay the header.
        session: String,
    },
    Token(String),
}

impl AuthMode {
    /// Basic when credentials were configured, otherwise a fresh random token.
    pub(crate) fn new(credentials: Option<Credentials>) -> Self {
        match credentials {
            Some(credentials) => Self::Basic {
                credentials,
                session: Uuid::new_v4().to_string(),
            },
            None => Self::Token(Uuid::new_v4().to_string()),
        }
    }

    /// The token a client must present, or `None` in Basic mode.
    pub(crate) fn token(&self) -> Option<&str> {
        match self {
            Self::Basic { .. } => None,
            Self::Token(token) => Some(token),
        }
    }

    /// The identifier `/api/health` reports so the frontend knows whether to run its own
    /// token bootstrap or stay out of the browser's way.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Basic { .. } => "basic",
            Self::Token(_) => "token",
        }
    }

    /// One line for the startup banner and the dashboard header. Never contains the password.
    pub(crate) fn description(&self) -> String {
        match self {
            Self::Basic { credentials, .. } => {
                format!("HTTP Basic auth as user \"{}\"", credentials.user)
            }
            Self::Token(_) => "random token in the URL (set --auth for HTTP Basic)".to_owned(),
        }
    }
}

/// Rejects any `/api/*` or `/ws/*` request that does not carry the active mode's credential.
pub(crate) async fn require_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    match &state.auth {
        AuthMode::Token(expected) => {
            let provided = extract_token(request.uri().query(), request.headers());
            if provided.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
            {
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "missing or invalid token\n").into_response()
            }
        }
        AuthMode::Basic {
            credentials,
            session,
        } => match classify_basic(request.headers(), credentials, session) {
            BasicOutcome::Rejected => basic_challenge(),
            BasicOutcome::Cookie => next.run(request).await,
            // Only a request that proved the password gets the cookie, and only when it did
            // not already have it -- so the header is not re-sent on every single response.
            BasicOutcome::Header => {
                let mut response = next.run(request).await;
                if let Ok(value) = HeaderValue::from_str(&session_cookie(session)) {
                    response.headers_mut().append(header::SET_COOKIE, value);
                }
                response
            }
        },
    }
}

/// Which credential a Basic-mode request presented, if any.
enum BasicOutcome {
    Header,
    Cookie,
    Rejected,
}

/// The cookie is checked first so an already-established browser is not handed a fresh
/// `Set-Cookie` on every single response; the header is what establishes it in the first
/// place, and what re-establishes it after a restart invalidates the old one.
fn classify_basic(headers: &HeaderMap, credentials: &Credentials, session: &str) -> BasicOutcome {
    if cookie_value(headers, SESSION_COOKIE)
        .is_some_and(|value| constant_time_eq(value.as_bytes(), session.as_bytes()))
    {
        return BasicOutcome::Cookie;
    }
    if let Some((user, password)) = basic_header(headers)
        && credentials.matches(&user, &password)
    {
        return BasicOutcome::Header;
    }
    BasicOutcome::Rejected
}

/// A 401 that names the scheme, which is what makes the browser draw its own prompt instead
/// of leaving the page to invent a login form.
fn basic_challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            format!("Basic realm=\"{REALM}\", charset=\"UTF-8\""),
        )],
        "authentication required\n",
    )
        .into_response()
}

fn session_cookie(session: &str) -> String {
    // No `Secure`: there is no built-in TLS, and marking it `Secure` over plain HTTP would
    // stop the browser from ever storing it. `SameSite=Strict` is what keeps another origin
    // from riding this cookie into the PTY channel.
    format!("{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict")
}

/// Decodes `Authorization: Basic <base64(user:password)>`.
fn basic_header(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))?;
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, password) = decoded.split_once(':')?;
    Some((user.to_owned(), password.to_owned()))
}

/// Compares two secrets without an early exit on the first differing byte.
///
/// The length check is not constant time, and deliberately so: it leaks the length of the
/// expected secret, not its content, which is the same trade every `ct_eq` on variable-length
/// slices makes.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |accumulator, (a, b)| accumulator | (a ^ b))
        == 0
}

fn extract_token(query: Option<&str>, headers: &HeaderMap) -> Option<String> {
    if let Some(token) = query.and_then(|query| query_param(query, "token")) {
        return Some(token.to_owned());
    }
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        return Some(token.to_owned());
    }
    if let Some(token) = cookie_value(headers, "token") {
        return Some(token.to_owned());
    }
    None
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (candidate, value) = pair.split_once('=')?;
        (candidate == key).then_some(value)
    })
}

fn cookie_value<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| {
            header.split(';').find_map(|pair| {
                let (candidate, value) = pair.trim().split_once('=')?;
                (candidate == key).then_some(value)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        headers
    }

    fn basic_value(user: &str, password: &str) -> String {
        format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
    }

    #[test]
    fn credentials_split_on_the_first_colon_so_passwords_may_contain_colons() {
        assert_eq!(
            Credentials::parse("alice:pa:ss:word", "--auth").unwrap(),
            Credentials {
                user: "alice".into(),
                password: "pa:ss:word".into(),
            }
        );
    }

    /// Every rejection has to name the rule without echoing the value: the value is a
    /// password on all three of the paths that reach this parser.
    #[test]
    fn malformed_credentials_are_rejected_without_repeating_the_secret() {
        for (value, reason) in [
            ("alice", "no colon found"),
            (":secret", "empty user"),
            ("alice:", "empty password"),
            ("", "no colon found"),
        ] {
            let error = Credentials::parse(value, "--auth").unwrap_err();
            assert!(
                error.contains(reason),
                "{value:?} should be rejected for {reason}, got {error}"
            );
            assert!(error.starts_with("--auth"), "the source is named: {error}");
            assert!(
                !error.contains("secret"),
                "the rejected value must not be echoed: {error}"
            );
        }
    }

    #[test]
    fn no_configured_credentials_falls_back_to_a_random_token() {
        let mode = AuthMode::new(None);

        assert_eq!(mode.kind(), "token");
        let token = mode.token().expect("token mode must carry a token");
        assert_eq!(token.len(), 36, "a v4 UUID, not an empty or fixed string");
        assert_ne!(
            AuthMode::new(None).token(),
            Some(token),
            "each process gets its own token"
        );
    }

    #[test]
    fn configured_credentials_select_basic_and_expose_no_token() {
        let mode = AuthMode::new(Some(Credentials {
            user: "alice".into(),
            password: "hunter2".into(),
        }));

        assert_eq!(mode.kind(), "basic");
        assert_eq!(mode.token(), None, "there is nothing to put in a URL");
        assert!(mode.description().contains("alice"));
        assert!(
            !mode.description().contains("hunter2"),
            "the password never reaches a log line: {}",
            mode.description()
        );
    }

    #[test]
    fn a_correct_basic_header_authenticates() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        let headers = headers_with(header::AUTHORIZATION, &basic_value("alice", "hunter2"));

        assert!(matches!(
            classify_basic(&headers, &credentials, "session-id"),
            BasicOutcome::Header
        ));
    }

    #[test]
    fn wrong_basic_credentials_are_refused() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        for (user, password) in [("alice", "wrong"), ("bob", "hunter2"), ("", "")] {
            let headers = headers_with(header::AUTHORIZATION, &basic_value(user, password));
            assert!(
                matches!(
                    classify_basic(&headers, &credentials, "session-id"),
                    BasicOutcome::Rejected
                ),
                "{user}:{password} must not authenticate"
            );
        }
    }

    #[test]
    fn a_bearer_token_is_not_accepted_in_basic_mode() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        let headers = headers_with(header::AUTHORIZATION, "Bearer hunter2");

        assert!(matches!(
            classify_basic(&headers, &credentials, "session-id"),
            BasicOutcome::Rejected
        ));
    }

    /// The websocket's fallback: the browser cannot put a header on the handshake, so a
    /// handshake that carries only the cookie issued to an authenticated page still passes.
    #[test]
    fn the_session_cookie_authenticates_a_handshake_that_carries_no_header() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        let headers = headers_with(header::COOKIE, "other=1; agent-console-session=abc; more=2");

        assert!(matches!(
            classify_basic(&headers, &credentials, "abc"),
            BasicOutcome::Cookie
        ));
        assert!(matches!(
            classify_basic(&headers, &credentials, "different"),
            BasicOutcome::Rejected
        ));
    }

    /// A stale cookie -- one from a previous process -- must not lock a browser out: the
    /// header it also sends has to be what gets it back in.
    #[test]
    fn a_stale_cookie_falls_back_to_the_header_rather_than_rejecting() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("agent-console-session=from-a-dead-process"),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&basic_value("alice", "hunter2")).unwrap(),
        );

        assert!(matches!(
            classify_basic(&headers, &credentials, "current-session"),
            BasicOutcome::Header
        ));
    }

    #[test]
    fn the_password_is_redacted_from_debug_output() {
        let credentials = Credentials::parse("alice:hunter2", "--auth").unwrap();
        let rendered = format!("{credentials:?}");

        assert!(rendered.contains("alice"));
        assert!(
            !rendered.contains("hunter2"),
            "a derived Debug would leak the password into any settings dump: {rendered}"
        );
    }

    #[test]
    fn the_session_cookie_is_scoped_and_not_readable_from_script() {
        let cookie = session_cookie("abc");

        assert!(cookie.starts_with("agent-console-session=abc"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Path=/"));
    }

    #[test]
    fn the_challenge_names_basic_so_the_browser_prompts_instead_of_the_app() {
        let response = basic_challenge();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(challenge.starts_with("Basic "), "got {challenge}");
        assert!(challenge.contains("Agent Console"));
    }

    #[test]
    fn constant_time_comparison_still_answers_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn token_extracted_from_query_string() {
        assert_eq!(
            extract_token(Some("cols=80&token=abc123&rows=24"), &HeaderMap::new()),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn token_extracted_from_bearer_header() {
        let headers = headers_with(header::AUTHORIZATION, "Bearer abc123");
        assert_eq!(extract_token(None, &headers), Some("abc123".to_owned()));
    }

    #[test]
    fn token_extracted_from_cookie() {
        let headers = headers_with(header::COOKIE, "other=1; token=abc123; more=2");
        assert_eq!(extract_token(None, &headers), Some("abc123".to_owned()));
    }

    #[test]
    fn missing_token_returns_none() {
        assert_eq!(extract_token(None, &HeaderMap::new()), None);
        assert_eq!(extract_token(Some("cols=80"), &HeaderMap::new()), None);
    }

    #[test]
    fn query_token_takes_precedence_over_header() {
        let headers = headers_with(header::AUTHORIZATION, "Bearer header-token");
        assert_eq!(
            extract_token(Some("token=query-token"), &headers),
            Some("query-token".to_owned())
        );
    }

    #[test]
    fn similar_but_unequal_tokens_do_not_match() {
        assert_ne!(
            extract_token(Some("token=abc123extra"), &HeaderMap::new()),
            Some("abc123".to_owned())
        );
    }
}
