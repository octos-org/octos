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

    // API / infrastructure paths must NEVER fall through to the SPA
    // redirect. If a request reached this handler with such a prefix the
    // route simply isn't registered — return `404 Not Found` as JSON so
    // API clients see a clean error instead of a `307 -> /admin/` that
    // (a) breaks Playwright's `apiRequestContext` max-redirect cap and
    // (b) hands an HTML body to a caller that asked for `text/event-stream`
    // or JSON. Documented surfaces like `/api/events/harness` hit this
    // branch when the route was planned but never wired.
    if is_api_or_infra_path(path) {
        let body = serde_json::json!({
            "error": "not_found",
            "path": format!("/{path}"),
        });
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response();
    }

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
    //
    // Segment-match rather than `starts_with("swarm")` so sibling paths
    // like `/swarmish` or `/swarm-config` fall through to the admin
    // redirect instead of hijacking the 503 branch.
    if path == "swarm" || path.starts_with("swarm/") {
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

/// Returns true when `path` (already stripped of the leading `/`) targets
/// an API, webhook, or internal surface. Segment-match so sibling names
/// like `apibackup` fall through to the admin redirect rather than being
/// hijacked by the 404 branch.
fn is_api_or_infra_path(path: &str) -> bool {
    for prefix in ["api", "webhook", "internal"] {
        if path == prefix || path.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    false
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

        fn with(entries: &[(&str, &[u8])]) -> Self {
            let mut files = HashMap::new();
            for (k, v) in entries {
                files.insert((*k).to_string(), v.to_vec());
            }
            Self { files }
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

    /// Bug 1: any unmatched `/api/*` path must return a JSON 404 from
    /// the SPA fallback. Previously these were returning
    /// `307 Location: /admin/`, which breaks Playwright's
    /// `apiRequestContext` (max-redirect trip) and hands HTML to a
    /// caller that asked for `text/event-stream`. The live-sweep
    /// surfaced this via a documented-but-unwired
    /// `GET /api/events/harness?kinds=...` endpoint.
    #[tokio::test]
    async fn should_return_404_for_unmatched_api_path_even_without_admin_bundle() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::empty();
        let resp = serve_with(&assets, &state, "/api/does-not-exist").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(ct, "application/json");
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "not_found");
        assert_eq!(body["path"], "/api/does-not-exist");
    }

    /// Even when the admin bundle is present, an unmatched `/api/` path
    /// still must 404 — must NOT serve the SPA `admin/index.html` body
    /// to a JSON/SSE client.
    #[tokio::test]
    async fn should_return_404_for_unmatched_api_path_even_with_admin_bundle() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::with(&[("admin/index.html", b"<html/>")]);
        let resp = serve_with(&assets, &state, "/api/unknown").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// `/webhook/*` is a distinct public surface — an unmatched webhook
    /// route must 404, not get redirected to `/admin/`.
    #[tokio::test]
    async fn should_return_404_for_unmatched_webhook_path() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::empty();
        let resp = serve_with(&assets, &state, "/webhook/unknown/profile").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Segment-match guard: `/apibackup` is a sibling name, NOT an API
    /// surface, so it must fall through to the admin redirect branch
    /// and not be hijacked by the 404 guard.
    #[tokio::test]
    async fn should_not_match_api_prefix_siblings() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::with(&[("admin/index.html", b"<html/>")]);
        let resp = serve_with(&assets, &state, "/apibackup").await;
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "sibling prefix must not be captured by the /api/ 404 guard"
        );
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(location, "/admin/");
    }

    /// C-003: a prefix-match on `"swarm"` catches sibling paths like
    /// `/swarmish` and would hijack them into the 503 branch even
    /// though the operator never asked for the swarm dashboard.
    /// Segment-match (`path == "swarm" || path.starts_with("swarm/")`)
    /// makes `/swarmish` fall through to the admin redirect.
    #[tokio::test]
    async fn should_not_match_swarm_prefix_siblings() {
        let state = AppState::empty_for_tests();
        let assets = StubAssets::empty();
        // `/swarmish` must fall through to the admin redirect (307), NOT
        // the swarm 503 branch. The final catch-all returns a redirect
        // to `/admin/`, so we assert we get exactly that.
        let resp = serve_with(&assets, &state, "/swarmish").await;
        assert_eq!(
            resp.status(),
            StatusCode::TEMPORARY_REDIRECT,
            "sibling prefix must not be captured by the swarm branch"
        );
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(location, "/admin/");
    }
}
