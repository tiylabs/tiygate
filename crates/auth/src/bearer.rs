//! Bearer token authentication applier.

use tiygate_core::{AuthApplier, Error, RoutingTarget};

/// Writes `Authorization: Bearer <key>` using the routing target's
/// effective API key.
pub struct BearerAuthApplier;

#[async_trait::async_trait]
impl AuthApplier for BearerAuthApplier {
    async fn apply(
        &self,
        headers: &mut http::HeaderMap,
        target: &RoutingTarget,
    ) -> Result<(), Error> {
        let key = target.effective_api_key();
        let header_value = http::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|e| Error::Auth(format!("Invalid header value: {e}")))?;
        headers.insert(http::header::AUTHORIZATION, header_value);
        Ok(())
    }
}
