//! TiyGate Admin — REST API for managing providers, routes, and API keys.

pub mod auth;
pub mod handlers;
pub mod oauth;
pub mod state;

use axum::{middleware, Router};

pub use state::AdminState;

/// Build the Admin REST router. The router is *unauthenticated* at
/// this level — the [`auth::require_admin_token`] middleware is
/// applied by [`build_router_with_auth`]. Callers that want a
/// public-facing prefix (e.g. for `/healthz`) can mount the public
/// subset separately.
pub fn build_router(state: AdminState) -> Router {
    build_router_with_auth(state, true)
}

pub fn build_router_with_auth(state: AdminState, require_auth: bool) -> Router {
    // The OAuth admin handlers live in their own `Router<()>`;
    // `Router::merge` on two routers with different state types
    // erases the state to `()`. We re-attach the state on the
    // merged router before applying the auth middleware.
    let handlers_router = handlers::router().with_state(state.clone());
    let oauth_router = oauth::router().with_state(state.clone());
    let inner = handlers_router.merge(oauth_router);
    if require_auth {
        // The middleware reads TIYGATE_ADMIN_TOKEN at request
        // time. For tests, set the env *inside* the middleware
        // invocation by passing a custom check; the production
        // path uses the env var.
        inner.layer(middleware::from_fn_with_state(
            state,
            auth::require_admin_token,
        ))
    } else {
        inner
    }
}
