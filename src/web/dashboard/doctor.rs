//! `agent-console doctor`, as JSON.
//!
//! The checks themselves live in `crate::doctor::report`, which the CLI renders as text and
//! this endpoint serialises verbatim. Neither surface owns the probe list, so a check added
//! for one appears in the other.

use axum::{Router, extract::Json, http::StatusCode, response::Json as JsonResponse, routing::get};

use crate::{doctor::DoctorReport, web::AppState};

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/doctor", get(diagnostics))
}

/// Runs every probe and reports the results.
///
/// Answers 200 even when checks fail: a failing check is the payload, not a server error,
/// and a frontend showing a diagnostics panel needs the detail either way. Only being unable
/// to *run* the checks at all -- no resolvable state directory, no readable config -- is a
/// 500.
///
/// Handed to `spawn_blocking` because it spawns provider binaries with an eight-second
/// timeout each; running it inline would park an async worker for that long. It takes no
/// locks, so it cannot stall the tick thread or any other request while it runs.
pub(crate) async fn diagnostics() -> Result<JsonResponse<DoctorReport>, (StatusCode, String)> {
    let report = tokio::task::spawn_blocking(crate::doctor::report)
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics did not finish: {error}\n"),
            )
        })?
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{error}\n")))?;
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use crate::doctor::{CheckReport, DoctorReport, PathReport, ProviderReport};

    /// The shape the frontend's diagnostics panel is built from. Asserted here so a change to
    /// the report struct that silently drops a field fails a test rather than a page.
    #[test]
    fn the_report_serializes_the_sections_the_diagnostics_panel_renders() {
        let report = DoctorReport {
            version: "0.0.16",
            providers_enabled: vec!["codex", "claude"],
            providers: vec![ProviderReport {
                name: "codex",
                available: true,
                detail: "codex-cli 0.149.0".into(),
                version_support: Some("supported"),
                capabilities: vec![CheckReport {
                    name: "codex resume".into(),
                    ok: true,
                    detail: "supports resume".into(),
                }],
            }],
            discovery: vec![PathReport {
                name: "Codex sessions",
                path: "/home/u/.codex/sessions".into(),
                exists: true,
            }],
            checks: vec![CheckReport {
                name: "clipboard".into(),
                ok: false,
                detail: "no clipboard command is available on PATH".into(),
            }],
            diagnostics_path: Some("/home/u/.local/state/agent-console/agent-console.log".into()),
            failures: 1,
            ok: false,
        };

        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["version"], "0.0.16");
        assert_eq!(value["providers_enabled"][0], "codex");
        assert_eq!(value["providers"][0]["version_support"], "supported");
        assert_eq!(value["providers"][0]["capabilities"][0]["ok"], true);
        assert_eq!(value["discovery"][0]["exists"], true);
        assert_eq!(value["checks"][0]["ok"], false);
        assert_eq!(value["failures"], 1);
        assert_eq!(value["ok"], false);
        assert!(value["diagnostics_path"].is_string());
    }

    /// A provider that is not installed carries no version verdict, so the panel must not be
    /// handed a placeholder it would render as a real one.
    #[test]
    fn an_absent_provider_reports_no_version_verdict() {
        let value = serde_json::to_value(ProviderReport {
            name: "claude",
            available: false,
            detail: "No such file or directory (os error 2)".into(),
            version_support: None,
            capabilities: Vec::new(),
        })
        .unwrap();

        assert_eq!(value["available"], false);
        assert!(value["version_support"].is_null());
        assert!(value["capabilities"].as_array().unwrap().is_empty());
    }
}
