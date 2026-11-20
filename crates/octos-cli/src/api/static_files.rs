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

/// The three SPA entry files, shared by the serve-time branches below and
/// the doctor introspection so the two surfaces cannot drift apart.
const WEB_INDEX: &str = "web/index.html";
const ADMIN_INDEX: &str = "admin/index.html";
const SWARM_INDEX: &str = "swarm/index.html";

/// Doctor introspection (#2384): one web UI bundle's availability to this
/// binary, with the script that produces it — the same remediation the 503
/// responses below hand out at serve time.
pub(crate) struct UiBundleStatus {
    /// Human-readable bundle name with its serve route, e.g. `web (/app)`.
    pub(crate) label: &'static str,
    /// Build script that produces the bundle, relative to the repo root.
    pub(crate) build_script: &'static str,
    /// Whether the bundle is available to this binary. Compile-time embed in
    /// release; debug builds fall back to reading the folder on disk at
    /// runtime (rust-embed's default), so there the flag reflects the
    /// checkout's current `static/` contents.
    pub(crate) embedded: bool,
}

/// Which web UI bundles this binary can serve. The SPAs live in gitignored
/// `static/{web,admin,swarm}/` and only exist when their build scripts ran
/// before the cargo build, so a fresh-clone build has none of the three UI
/// bundles and every UI route serves the 503 JSONs below — doctor surfaces
/// that before the first `octos serve` instead of at it (#2384).
pub(crate) fn embedded_ui_bundles() -> [UiBundleStatus; 3] {
    ui_bundles_from_store(&EmbeddedAssets)
}

fn ui_bundles_from_store(assets: &dyn AssetStore) -> [UiBundleStatus; 3] {
    let mut admin = bundle_status(
        assets,
        "admin (/admin)",
        "./scripts/build-dashboard.sh",
        ADMIN_INDEX,
    );
    // Serve-time parity with the Bug-3 guard (#958): an index.html that
    // references hashed assets missing from the bundle 503s at every /admin
    // path, so present-but-inconsistent must not count as available here
    // either.
    if admin.embedded
        && let Some(index) = assets.get(ADMIN_INDEX)
    {
        admin.embedded = admin_index_missing_assets(assets, &index).is_none();
    }
    [
        bundle_status(
            assets,
            "web (/app)",
            "./scripts/build-web-app.sh",
            WEB_INDEX,
        ),
        admin,
        bundle_status(
            assets,
            "swarm (/swarm)",
            "./scripts/build-swarm-app.sh",
            SWARM_INDEX,
        ),
    ]
}

fn bundle_status(
    assets: &dyn AssetStore,
    label: &'static str,
    build_script: &'static str,
    index: &str,
) -> UiBundleStatus {
    UiBundleStatus {
        label,
        build_script,
        embedded: assets.get(index).is_some(),
    }
}

/// Fallback handler: serves embedded static files, falls back to admin/index.html for SPA routing.
/// The admin dashboard SPA handles all UI routes (login, profiles, users, etc.).
///
/// The React SPA uses `basename="/admin"`, so all UI paths must start with `/admin/`.
/// The swarm-app SPA uses `basename="/swarm"` and is served from the parallel
/// `/swarm/` mount for the M7.6 PM+supervisor orchestrator. Non-matching
/// paths are redirected to the default UI — `/app/` (octos-web) when its
/// bundle is embedded, `/admin/` otherwise — so that React Router can
/// handle them.
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

    // Root "/" → serve landing page only in cloud mode, otherwise
    // redirect to the default UI: /app/ (octos-web) when its bundle is
    // embedded, /admin/ as fallback (or 503 when no bundle is present —
    // see Bug 2 below). The README quickstart onboards users through
    // /app/'s one-step local sign-in; landing them on the admin
    // dashboard's token login was the top onboarding complaint.
    if path.is_empty() {
        if matches!(state.deployment_mode, crate::config::DeploymentMode::Cloud) {
            if let Some(data) = assets.get("landing.html") {
                return serve_file("landing.html", &data);
            }
        }
        return redirect_to_default_ui_or_503(assets);
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
        if let Some(data) = assets.get(SWARM_INDEX) {
            return serve_file(SWARM_INDEX, &data);
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

    // octos-web SPA: under /app/* with its own asset tree (built by
    // scripts/build-web-app.sh into static/web/ with BASE_URL=/app/). Same
    // pattern as the swarm branch: segment-match so `/application` etc. fall
    // through to admin; serve the embedded asset, else the SPA index for
    // client-side routes, else a 503 naming the build script when the bundle
    // wasn't embedded. The app is same-origin (`API_BASE=""`) so it talks to
    // this server's API/WS with no CORS.
    if path == "app" || path.starts_with("app/") {
        // RFC #3262 step 1 (T-0608): `/app` (octos-web) is DEPRECATED —
        // octoscode-web is the replacement web client. Zero-build injection:
        // only the SPA index gets a deprecation banner; asset files
        // (JS/CSS/assets) are byte-identical and hashed-bundle-friendly.
        // This is removed with the submodule in step 2.
        if path == "app" {
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, "/app/")],
                "",
            )
                .into_response();
        }
        let web_path = format!("web/{}", path.trim_start_matches("app/"));
        if let Some(data) = assets.get(&web_path) {
            // Only the index document carries the banner; assets must stay
            // byte-identical (their names are content-hashed).
            if web_path == WEB_INDEX || web_path == "web/index.html" {
                return serve_html_with_deprecation_banner(&web_path, &data);
            }
            return serve_file(&web_path, &data);
        }
        if let Some(data) = assets.get(WEB_INDEX) {
            return serve_html_with_deprecation_banner(WEB_INDEX, &data);
        }
        let body = serde_json::json!({
            "error": "web_bundle_missing",
            "message":
                "Run ./scripts/build-web-app.sh + rebuild octos-cli to include the octos-web app.",
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

    // Admin SPA fallback. Segment-match rather than `starts_with("admin")`
    // (see the C-003 swarm fix for the same class of issue with
    // `/swarmish`). When the bundle is present, serve `admin/index.html`
    // so React Router can handle client-side routing. When the bundle
    // is missing — Bug 2 against release/coding-blue: binaries shipped
    // without running `scripts/build-dashboard.sh` leave `static/admin/`
    // populated with hashed asset files but no `index.html` — the
    // handler used to fall through to `307 Location: /admin/`, which
    // itself 307'd back here, and the browser hit
    // `ERR_TOO_MANY_REDIRECTS`. Short-circuit to 503 so the failure is
    // diagnosable instead of an infinite loop.
    //
    // Bug 3 (regression #958 → mini5 deploy 2026-05-13): a binary built
    // from a checkout where the committed `admin/index.html` references
    // hash-named JS/CSS that were never produced by a local
    // `build-dashboard.sh` (the tracked `index.html` survives the
    // ephemeral-bundle gitignore) embeds an `index.html` whose `<script
    // src="/admin/assets/index-XXX.js">` resolves to a 200 SPA-fallback
    // body (i.e. itself), so the browser's module loader silently fails
    // and the user sees a blank page. Detect that case at serve time
    // and return 503 with a structured `bundle_inconsistent` body so
    // the operator gets a diagnostic instead of a silent blank UI.
    if path == "admin" || path.starts_with("admin/") {
        if let Some(data) = assets.get(ADMIN_INDEX) {
            if let Some(missing) = admin_index_missing_assets(assets, &data) {
                return admin_bundle_inconsistent_response(missing);
            }
            return serve_file("admin/index.html", &data);
        }
        return admin_bundle_missing_response();
    }

    // Catch-all: redirect stray non-API UI paths to the default UI
    // (same /app/-first preference as the root branch). Must guard on
    // the target bundle's index.html — otherwise the redirect target
    // itself 307s and the browser loops.
    redirect_to_default_ui_or_503(assets)
}

/// Redirect to the default UI: prefer the octos-web app (`/app/`) when its
/// bundle is embedded, fall back to the admin dashboard otherwise. The
/// fallback keeps binaries built without `scripts/build-web-app.sh` (older
/// checkouts, dashboard-only builds) on the previous `/admin/` behavior,
/// including all of its missing-bundle 503 diagnostics.
fn redirect_to_default_ui_or_503<A: AssetStore>(assets: &A) -> Response {
    if assets.get(WEB_INDEX).is_some() {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/app/")],
            "",
        )
            .into_response();
    }
    redirect_to_admin_or_503(assets)
}

/// Scan an embedded `admin/index.html` body for `/admin/assets/index-*.{js,css}`
/// references and return the list of asset paths that are NOT present in the
/// embedded asset store. Returns `None` when every reference resolves
/// (or when there are no asset references, e.g. a stub HTML used in tests).
///
/// Defensive guard against Bug 3 — when the deploy binary's `index.html`
/// references hash-named bundles that were never produced (a tracked
/// `index.html` survives the ephemeral-bundle gitignore even after the
/// matching `assets/*` are untracked), the SPA falls back to itself at
/// 200, the browser's `<script type="module">` errors silently, and the
/// user sees a blank `<div id="root">`. Surfacing the mismatch as 503
/// gives the operator a clear `bundle_inconsistent` diagnostic.
fn admin_index_missing_assets(assets: &dyn AssetStore, html: &[u8]) -> Option<Vec<String>> {
    let html = std::str::from_utf8(html).ok()?;
    let mut missing = Vec::new();
    // We don't pull in `regex` here — the structure of a Vite-emitted
    // `index.html` is stable enough that scan-and-extract on the literal
    // prefix `/admin/assets/` is both sufficient and zero-dep.
    let needle = "/admin/assets/";
    let mut cursor = 0;
    while let Some(rel) = html[cursor..].find(needle) {
        let start = cursor + rel + 1; // strip the leading '/'
        let tail = &html[start..];
        // Scan to the first character that can't appear in an asset path
        // — quote, angle-bracket, whitespace, or end-of-string. The
        // hash-named filenames are `index-<base64ish>.{js,css}`.
        let end = tail
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace())
            .unwrap_or(tail.len());
        let asset_path = &tail[..end];
        if !asset_path.is_empty() && assets.get(asset_path).is_none() {
            missing.push(asset_path.to_string());
        }
        cursor = start + end;
    }
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

fn admin_bundle_inconsistent_response(missing: Vec<String>) -> Response {
    let body = serde_json::json!({
        "error": "admin_bundle_inconsistent",
        "message":
            "The embedded admin/index.html references asset files that are not in the bundle. \
             Run ./scripts/build-dashboard.sh + rebuild octos-cli (do NOT pass --skip-dashboard) \
             so index.html and assets/ are produced together.",
        "missing": missing,
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Redirect to `/admin/` when the admin bundle is present; otherwise
/// return a 503 so the caller sees a diagnosable "admin bundle missing"
/// JSON body instead of being fed into an infinite redirect loop.
fn redirect_to_admin_or_503<A: AssetStore>(assets: &A) -> Response {
    if assets.get("admin/index.html").is_some() {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/admin/")],
            "",
        )
            .into_response();
    }
    admin_bundle_missing_response()
}

fn admin_bundle_missing_response() -> Response {
    let body = serde_json::json!({
        "error": "admin_bundle_missing",
        "message":
            "Run ./scripts/build-dashboard.sh + rebuild octos-cli to include the admin dashboard.",
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
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

/// RFC #3262 step 1 (T-0608): serve the octos-web SPA index with a static
/// deprecation banner injected before `</body>`, announcing that `/app` is
/// deprecated and octoscode-web is the replacement. Text-only injection —
/// no scripts — so the page's own scripts are untouched and the change is
/// removable in step 2 by deleting the submodule. A full document (no
/// `</body>`) is served unmodified rather than mangled.
fn serve_html_with_deprecation_banner(path: &str, data: &[u8]) -> Response {
    const BANNER: &str = concat!(
        r#"<div id="octos-app-deprecation" role="status" "#,
        r#"style="position:fixed;top:0;left:0;right:0;z-index:2147483647;padding:10px 16px;"#,
        r#"background:#b45309;color:#fff;font:14px/1.5 system-ui,sans-serif;"#,
        r#"text-align:center;">"#,
        r#"<strong>Deprecation:</strong> the /app web client (octos-web) is deprecated and will "#,
        r#"be removed in a future release. Use "#,
        r#"<a href="https://github.com/octos-org/octoscode-web" style="color:#fde68a;">"#,
        r#"octoscode-web</a> instead. — <a href='#' "#,
        r#"onclick="this.parentElement.remove();return false;" "#,
        r#"style="color:#fde68a;">dismiss</a></div>"#
    );
    let body = match std::str::from_utf8(data) {
        Ok(text) if text.contains("</body>") => text
            .replacen("</body>", &(BANNER.to_string() + "</body>"), 1)
            .into_bytes(),
        _ => data.to_vec(),
    };
    serve_file(path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC #3262 step 1 (T-0608): the /app index document carries the
    /// deprecation banner exactly once; asset files (hashed JS/CSS) pass
    /// through byte-identical.
    #[test]
    fn should_inject_the_deprecation_banner_into_the_app_index_only() {
        let index = b"<html><head></head><body><div id=\"root\"></div></body></html>".to_vec();
        let response = serve_html_with_deprecation_banner(WEB_INDEX, &index);
        let body = response_body(response);
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.matches("octos-app-deprecation").count(), 1, "{text}");
        assert!(
            text.contains(r#"href="https://github.com/octos-org/octoscode-web""#),
            "banner points at octoscode-web: {text}"
        );
        assert!(
            text.matches("</body>").count() == 1 && text.contains("</div></body>"),
            "{text}"
        );

        // Assets (content-hashed names) stay byte-identical.
        let asset = b"const x=1; /* no html */".to_vec();
        let response = serve_html_with_deprecation_banner("web/assets/index-abc123.js", &asset);
        let body = response_body(response);
        assert_eq!(body, asset, "asset bytes are untouched");
    }

    /// #3262: an index without a closing </body> is served unmodified rather
    /// than mangled.
    #[test]
    fn should_serve_a_body_less_index_unmodified() {
        let index = b"<html><head></head></html>".to_vec();
        let response = serve_html_with_deprecation_banner(WEB_INDEX, &index);
        assert_eq!(response_body(response), index, "no </body> → no injection");
    }

    fn response_body(response: Response) -> Vec<u8> {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
                    .await
                    .expect("collect body")
                    .to_vec()
            })
    }
}
