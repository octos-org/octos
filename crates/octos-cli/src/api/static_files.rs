//! Embedded static file serving for the built-in Web UI.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

use super::AppState;

#[derive(Embed)]
#[folder = "static/"]
struct Assets;

/// Abstraction over the embedded asset store so the route logic can be
/// exercised in tests without rebuilding the binary with a different
/// `static/` tree. Production uses the `rust-embed`-generated [`Assets`].
trait AssetStore {
    fn get(&self, path: &str) -> Option<Vec<u8>>;
}

struct EmbeddedAssets;

impl AssetStore for EmbeddedAssets {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        Assets::get(path).map(|f| f.data.to_vec())
    }
}

/// Fallback handler: serves embedded static files, falls back to admin/index.html for SPA routing.
/// The admin dashboard SPA handles all UI routes (login, profiles, users, etc.).
///
/// The React SPA uses `basename="/admin"`, so all UI paths must start with `/admin/`.
/// The swarm-app SPA uses `basename="/swarm"` and is served from the parallel
/// `/swarm/` mount for the M7.6 PM+supervisor orchestrator. Non-matching
/// paths are redirected to `/admin/` so that React Router can handle them.
///
/// If a `/swarm/*` path is requested but the swarm-app bundle wasn't
/// embedded at build time (i.e. `static/swarm/index.html` is missing),
/// the handler returns `503 Service Unavailable` with a structured JSON
/// body pointing the operator at `scripts/build-swarm-app.sh` rather
/// than silently redirecting to `/admin/`.
pub async fn static_handler(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    serve_with(&EmbeddedAssets, &state, uri.path()).await
}

async fn serve_with<A: AssetStore>(assets: &A, state: &AppState, request_path: &str) -> Response {
    let path = request_path.trim_start_matches('/');

    // Root "/" → serve landing page only in cloud mode,
    // otherwise redirect to /admin/
    if path.is_empty() {
        if matches!(state.deployment_mode, crate::config::DeploymentMode::Cloud) {
            if let Some(data) = assets.get("landing.html") {
                return serve_file("landing.html", &data);
            }
        }
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/admin/")],
            "",
        )
            .into_response();
    }

    // Serve exact embedded asset (e.g. admin/assets/index-xxx.js or
    // swarm/assets/index-xxx.js).
    if let Some(data) = assets.get(path) {
        return serve_file(path, &data);
    }

    // Swarm-app SPA: under /swarm/* with its own asset tree. If the
    // bundle wasn't embedded, short-circuit with a 503 so operators
    // discover the misconfiguration instead of landing on the admin
    // redirect.
    if path.starts_with("swarm") {
        let swarm_path = format!("swarm/{}", path.trim_start_matches("swarm/"));
        if let Some(data) = assets.get(&swarm_path) {
            return serve_file(&swarm_path, &data);
        }
        if let Some(data) = assets.get("swarm/index.html") {
            return serve_file("swarm/index.html", &data);
        }
        let body = serde_json::json!({
            "error": "swarm_bundle_missing",
            "message":
                "Run ./scripts/build-swarm-app.sh + rebuild octos-cli to include the swarm dashboard.",
        });
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response();
    }

    // Try under admin/ prefix (e.g. /assets/foo.js → admin/assets/foo.js)
    let admin_path = format!("admin/{path}");
    if let Some(data) = assets.get(&admin_path) {
        return serve_file(&admin_path, &data);
    }

    // SPA fallback: only serve index.html for paths under /admin/
    // Non-admin paths redirect to /admin/ so React Router (basename="/admin") can handle them.
    if path.starts_with("admin") {
        if let Some(data) = assets.get("admin/index.html") {
            return serve_file("admin/index.html", &data);
        }
    }

    // Redirect unknown paths to /admin/ (e.g. /login → /admin/login won't help since
    // the React Router handles its own routing; just send to /admin/)
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, "/admin/")],
        "",
    )
        .into_response()
}

fn serve_file(path: &str, data: &[u8]) -> Response {
    let mime = match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    };

    // HTML files: no-cache so the browser always fetches the latest index.html
    // Asset files (with content hash in name): cache for 1 year
    let cache_control = if path.ends_with(".html") {
        "no-cache, no-store, must-revalidate"
    } else if path.contains("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, cache_control),
        ],
        data.to_vec(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::collections::HashMap;

    /// In-memory asset store used to simulate bundle presence / absence.
    struct StubAssets {
        files: HashMap<String, Vec<u8>>,
    }

    impl StubAssets {
        fn empty() -> Self {
            Self {
                files: HashMap::new(),
            }
        }
    }

    impl AssetStore for StubAssets {
        fn get(&self, path: &str) -> Option<Vec<u8>> {
            self.files.get(path).cloned()
        }
    }

    #[tokio::test]
    async fn should_return_503_when_swarm_bundle_missing() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::empty();
        let resp = serve_with(&assets, &state, "/swarm").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type, "application/json");
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "swarm_bundle_missing");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or("")
                .contains("build-swarm-app.sh")
        );
    }

    #[tokio::test]
    async fn should_return_503_for_nested_swarm_path_when_bundle_missing() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::empty();
        let resp = serve_with(&assets, &state, "/swarm/assets/index-xyz.js").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
