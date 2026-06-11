//! Embedded WebUI asset serving.
//!
//! The admin SPA (built under `webui/dist`) is compiled into the
//! binary with `rust-embed` and served behind the `/admin/ui` prefix.
//! This keeps TiyGate a single self-contained binary — no static
//! directory has to ship alongside it.
//!
//! Routing contract:
//! * `/admin/ui` and `/admin/ui/` return `index.html`.
//! * `/admin/ui/<asset>` returns the embedded asset when it exists.
//! * Any other path under `/admin/ui/` falls back to `index.html` so
//!   the client-side router (react-router with `basename="/admin/ui"`)
//!   can resolve deep links on reload.
//!
//! The routes are registered directly on the main router with explicit
//! full paths (rather than `nest`) to avoid axum's trailing-slash gap
//! where `/admin/ui/` would otherwise fall through to the admin auth
//! layer. The `/*path` wildcard is strictly scoped under `/admin/ui/`,
//! so it never intercepts data-plane (`/v1/*`) or admin API
//! (`/admin/v1/*`) routes.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../webui/dist"]
struct WebUiAssets;

/// Merge the `/admin/ui` SPA routes into the given router.
pub fn mount(router: Router) -> Router {
    router
        .route("/admin/ui", get(serve_index))
        .route("/admin/ui/", get(serve_index))
        .route("/admin/ui/*path", get(serve_asset))
}

async fn serve_index() -> Response {
    index_html()
}

async fn serve_asset(Path(path): Path<String>) -> Response {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return index_html();
    }
    match WebUiAssets::get(trimmed) {
        Some(content) => {
            let mime = mime_guess::from_path(trimmed).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                content.data.into_owned(),
            )
                .into_response()
        }
        // SPA fallback: unknown paths return index.html so the client
        // router can handle them.
        None => index_html(),
    }
}

fn index_html() -> Response {
    match WebUiAssets::get("index.html") {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "WebUI assets not embedded. Build the frontend (`cd webui && npm run build`) \
             before compiling with the `webui` feature.",
        )
            .into_response(),
    }
}
