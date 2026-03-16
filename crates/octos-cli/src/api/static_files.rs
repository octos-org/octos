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

/// Fallback handler: serves embedded static files, falls back to admin/index.html for SPA routing.
/// The admin dashboard SPA handles all UI routes (login, profiles, users, etc.).
///
/// The React SPA uses `basename="/admin"`, so all UI paths must start with `/admin/`.
/// Non-admin paths that don't match a real asset are redirected to `/admin/` so that
/// React Router can handle them properly.
pub async fn static_handler(State(_state): State<Arc<AppState>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Root "/" → redirect to /admin/
    if path.is_empty() {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(header::LOCATION, "/admin/")],
            "",
        )
            .into_response();
    }

    // Serve exact embedded asset (e.g. admin/assets/index-xxx.js)
    if let Some(file) = Assets::get(path) {
        return serve_file(path, &file.data);
    }

    // Try under admin/ prefix (e.g. /assets/foo.js → admin/assets/foo.js)
    let admin_path = format!("admin/{path}");
    if let Some(file) = Assets::get(&admin_path) {
        return serve_file(&admin_path, &file.data);
    }

    // SPA fallback: only serve index.html for paths under /admin/
    // Non-admin paths redirect to /admin/ so React Router (basename="/admin") can handle them.
    if path.starts_with("admin") {
        if let Some(file) = Assets::get("admin/index.html") {
            return serve_file("admin/index.html", &file.data);
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
