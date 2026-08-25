use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

/// The whole PWA shell (HTML/CSS/JS, manifest, service worker, icons, vendored xterm.js) is
/// embedded into the binary so `agent-console web` is a single self-contained executable.
#[derive(RustEmbed)]
#[folder = "assets/web/"]
struct Assets;

/// Serves an embedded asset for its exact path, falling back to `index.html` for anything
/// else (`/`, and any path that isn't a known asset) so the app shell always loads.
pub(crate) async fn serve_asset(uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    if let Some(file) = Assets::get(path) {
        return asset_response(path, file.data.into_owned());
    }
    if let Some(file) = Assets::get("index.html") {
        return asset_response("index.html", file.data.into_owned());
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn asset_response(path: &str, body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type(path)),
            (header::CACHE_CONTROL, cache_control(path).to_owned()),
        ],
        body,
    )
        .into_response()
}

fn content_type(path: &str) -> String {
    if path.ends_with(".webmanifest") {
        return "application/manifest+json".to_owned();
    }
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

/// The shell files must always revalidate so a rebuilt binary takes effect immediately.
/// A new binary changes their bytes but not their URLs, so a cached copy would otherwise
/// outlive an upgrade -- and the service worker would refresh itself straight from that
/// stale cache. Only the vendored libraries and icons, which change when their pinned
/// version does, are worth caching hard.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("vendor/") || path.starts_with("icons/") {
        "public, max-age=3600"
    } else {
        "no-cache"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webmanifest_gets_the_manifest_mime_type() {
        assert_eq!(
            content_type("manifest.webmanifest"),
            "application/manifest+json"
        );
    }

    #[test]
    fn javascript_gets_a_javascript_mime_type() {
        assert!(content_type("app.js").contains("javascript"));
    }

    #[test]
    fn unversioned_shell_assets_revalidate_so_an_upgrade_is_not_masked_by_a_stale_cache() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("service-worker.js"), "no-cache");
        assert_eq!(cache_control("app.js"), "no-cache");
        assert_eq!(cache_control("app.css"), "no-cache");
        assert_eq!(cache_control("manifest.webmanifest"), "no-cache");
        assert_eq!(cache_control("vendor/xterm.js"), "public, max-age=3600");
        assert_eq!(cache_control("icons/icon-192.png"), "public, max-age=3600");
    }

    #[test]
    fn embedded_shell_assets_exist() {
        assert!(Assets::get("index.html").is_some());
        assert!(Assets::get("app.js").is_some());
        assert!(Assets::get("app.css").is_some());
        assert!(Assets::get("manifest.webmanifest").is_some());
        assert!(Assets::get("service-worker.js").is_some());
        assert!(Assets::get("js/views/shell.js").is_some());
        assert!(Assets::get("js/views/termview.js").is_some());
        // The dashboard-capability modules and their styles: a file the service worker lists
        // but the binary does not carry is a 404 the moment the PWA is installed offline.
        assert!(Assets::get("js/notifications.js").is_some());
        assert!(Assets::get("js/lease.js").is_some());
        assert!(Assets::get("js/clipboard.js").is_some());
        assert!(Assets::get("js/views/alerts.js").is_some());
        assert!(Assets::get("js/views/doctor.js").is_some());
        assert!(Assets::get("js/views/overview.js").is_some());
        assert!(Assets::get("js/dialogs/rename.js").is_some());
        assert!(Assets::get("css/alerts.css").is_some());
        assert!(Assets::get("css/overview.css").is_some());
        assert!(Assets::get("css/doctor.css").is_some());
        assert!(Assets::get("vendor/xterm.js").is_some());
        assert!(Assets::get("vendor/xterm.css").is_some());
        assert!(Assets::get("vendor/xterm-addon-fit.js").is_some());
        assert!(Assets::get("icons/icon-192.png").is_some());
        assert!(Assets::get("icons/icon-512.png").is_some());
    }
}
