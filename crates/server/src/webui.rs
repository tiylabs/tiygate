//! Embedded WebUI asset serving.
//!
//! The admin SPA (built under `webui/dist`) is compiled into the
//! binary with `rust-embed` and served behind the `/admin/ui` prefix.
//! This keeps TiyGate a single self-contained binary — no static
//! directory has to ship alongside it.
//!
//! Routing contract:
//! * `/admin/ui` permanently redirects to `/admin/ui/` so the
//!   browser resolves the SPA's relative asset URLs and react-router
//!   `basename` against the trailing slash.
//! * `/admin/ui/` returns `index.html`.
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
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
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
        .route("/admin/ui", get(redirect_to_slash))
        .route("/admin/ui/", get(serve_index))
        .route("/admin/ui/*path", get(serve_asset))
}

/// Redirect the bare `/admin/ui` (no trailing slash) to `/admin/ui/`.
///
/// The SPA's `index.html` references its assets with relative URLs
/// (`./assets/…`, `./icon.svg`) and react-router resolves its
/// `basename` against the trailing slash. Serving `index.html`
/// directly at `/admin/ui` makes the browser resolve those relative
/// URLs against `/admin/` (the parent), breaking asset loading and
/// client-side routing. A permanent redirect that preserves the
/// query string keeps deep links working. The query is forwarded so
/// links like `/admin/ui?foo=bar` survive the hop.
async fn redirect_to_slash(uri: Uri) -> Response {
    let target = match uri.query() {
        Some(q) if !q.is_empty() => format!("/admin/ui/?{q}"),
        _ => "/admin/ui/".to_string(),
    };
    Redirect::permanent(&target).into_response()
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
