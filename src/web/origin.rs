//! Who is allowed to open a websocket to this server.
//!
//! `/ws/*` is a full PTY control channel: whatever gets one can type into the session. Auth
//! guards it, but auth is not enough on its own for a *browser*, because a browser attaches
//! credentials it already has to a handshake some other page asked for. The session cookie is
//! `SameSite=Strict` and so stays behind, but cached HTTP Basic credentials are replayed on a
//! cross-site `ws://127.0.0.1:<port>/...` handshake, and a hostile page the user happens to
//! have open can start one. So the handshake is also checked for being same-origin.
//!
//! The comparison is `Origin` against `Host`, not against the configured bind address, and
//! that is deliberate. One server answers to several names -- `127.0.0.1`, `localhost`, a LAN
//! address, a reverse-proxy hostname -- and a page's `Origin` is always the authority it was
//! itself fetched from, which is the same authority it puts in `Host`. Comparing the two is
//! precisely "same origin" without this layer having to enumerate its own names.

use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Whether a websocket handshake may proceed.
///
/// A request with **no** `Origin` is allowed. RFC 6455 requires a browser to send one on every
/// handshake it makes, so a header-less handshake is not a browser -- it is curl, a websockets
/// library or a native client, none of which carry the ambient credentials this check exists
/// to protect. Refusing them would break every non-browser client for no gain; they still have
/// to satisfy `require_auth` like everything else.
pub(crate) fn websocket_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = header_str(headers, &header::ORIGIN) else {
        return true;
    };
    // `null` (a sandboxed iframe, a `file://` page, some cross-site redirects) and anything
    // that is not a plain `http`/`https` origin fall out here with no authority to compare.
    let Some(claimed) = authority_of(origin) else {
        return false;
    };
    header_str(headers, &header::HOST).is_some_and(|host| claimed.eq_ignore_ascii_case(host))
}

/// Refuses a cross-origin handshake before anything else looks at the request.
///
/// A layer rather than a line in the handler: the handler body only runs once every extractor
/// has succeeded, so a check written there would be skipped by exactly the malformed requests
/// least worth trusting. Here the answer is decided first, and a plain status is something a
/// browser reports as a failed connection rather than a socket that quietly closes.
pub(crate) async fn refuse_cross_origin(request: Request, next: Next) -> Response {
    if websocket_allowed(request.headers()) {
        return next.run(request).await;
    }
    (
        StatusCode::FORBIDDEN,
        "cross-origin websocket handshakes are refused; open the console from its own address\n",
    )
        .into_response()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &header::HeaderName) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// The `host[:port]` of an `http`/`https` origin.
///
/// The scheme is not part of the comparison: a reverse proxy terminating TLS leaves the page
/// on `https://` while this server still sees that same authority in `Host`. The port is,
/// because a different port on the same host is a different origin to a browser too.
fn authority_of(origin: &str) -> Option<&str> {
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))?;
    (!authority.is_empty() && !authority.contains('/')).then_some(authority)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_handshake_with_no_origin_is_allowed_so_non_browser_clients_still_work() {
        assert!(websocket_allowed(&headers(&[("host", "127.0.0.1:7878")])));
    }

    #[test]
    fn the_page_the_console_serves_can_open_its_own_socket() {
        assert!(websocket_allowed(&headers(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "http://127.0.0.1:7878"),
        ])));
    }

    #[test]
    fn a_tls_terminating_proxy_still_counts_as_the_same_origin() {
        assert!(websocket_allowed(&headers(&[
            ("host", "console.example"),
            ("origin", "https://console.example"),
        ])));
    }

    #[test]
    fn the_host_comparison_ignores_case() {
        assert!(websocket_allowed(&headers(&[
            ("host", "LocalHost:7878"),
            ("origin", "http://localhost:7878"),
        ])));
    }

    #[test]
    fn another_site_is_refused_even_though_it_reaches_the_same_server() {
        assert!(!websocket_allowed(&headers(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "http://evil.example"),
        ])));
    }

    #[test]
    fn a_different_port_on_this_machine_is_a_different_origin() {
        assert!(!websocket_allowed(&headers(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "http://127.0.0.1:9999"),
        ])));
    }

    #[test]
    fn localhost_and_the_loopback_literal_are_not_interchangeable() {
        assert!(!websocket_allowed(&headers(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "http://localhost:7878"),
        ])));
    }

    #[test]
    fn an_opaque_origin_is_refused_rather_than_treated_as_absent() {
        assert!(!websocket_allowed(&headers(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "null"),
        ])));
    }

    #[test]
    fn an_origin_carrying_a_path_does_not_get_to_smuggle_the_host_past_the_check() {
        assert!(!websocket_allowed(&headers(&[
            ("host", "127.0.0.1:7878"),
            ("origin", "http://evil.example/127.0.0.1:7878"),
        ])));
    }

    #[test]
    fn an_origin_with_no_host_to_compare_against_is_refused() {
        assert!(!websocket_allowed(&headers(&[(
            "origin",
            "http://127.0.0.1:7878",
        )])));
    }
}
