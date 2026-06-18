//! Header-based API key authentication applier.

use http::HeaderName;
use tiygate_core::{AuthApplier, Error, RoutingTarget};

/// Writes the routing target's effective API key into a configurable
/// header (e.g. `x-api-key`, `x-goog-api-key`).
pub struct HeaderApiKeyAuthApplier {
    pub header_name: String,
}

#[async_trait::async_trait]
impl AuthApplier for HeaderApiKeyAuthApplier {
    async fn apply(
        &self,
        headers: &mut http::HeaderMap,
        target: &RoutingTarget,
    ) -> Result<(), Error> {
        let key = target.effective_api_key();
        let header_name = HeaderName::from_bytes(self.header_name.as_bytes())
            .map_err(|e| Error::Auth(format!("Invalid header name: {e}")))?;
        let header_value = http::HeaderValue::from_str(key)
            .map_err(|e| Error::Auth(format!("Invalid header value: {e}")))?;
        headers.insert(header_name, header_value);
        Ok(())
    }
}
