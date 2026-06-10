//! Unit tests for the server ingress helpers (header extraction, etc.).
//! These are pure functions so they can be tested without spinning up a server.

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use http::HeaderMap;
    use http::HeaderValue;

    /// Re-implementation of `extract_retry_after` for testing.
    fn extract_retry_after(headers: &HeaderMap) -> Option<String> {
        headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    /// Re-implementation of `extract_rate_limit_headers` for testing.
    fn extract_rate_limit_headers(headers: &HeaderMap) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in &[
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ] {
            if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
                out.push((name.to_string(), v.to_string()));
            }
        }
        out
    }

    #[test]
    fn test_retry_after_extraction() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(extract_retry_after(&headers), Some("30".to_string()));
    }

    #[test]
    fn test_retry_after_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn test_rate_limit_headers_extraction() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit", HeaderValue::from_static("100"));
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1700000000"));
        let extracted = extract_rate_limit_headers(&headers);
        assert_eq!(extracted.len(), 3);
        let map: std::collections::HashMap<_, _> = extracted.into_iter().collect();
        assert_eq!(map.get("x-ratelimit-limit").unwrap(), "100");
        assert_eq!(map.get("x-ratelimit-remaining").unwrap(), "42");
        assert_eq!(map.get("x-ratelimit-reset").unwrap(), "1700000000");
    }

    #[test]
    fn test_rate_limit_headers_partial() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        let extracted = extract_rate_limit_headers(&headers);
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].0, "x-ratelimit-remaining");
        assert_eq!(extracted[0].1, "0");
    }

    #[test]
    fn test_rate_limit_headers_empty() {
        let headers = HeaderMap::new();
        let extracted = extract_rate_limit_headers(&headers);
        assert!(extracted.is_empty());
    }
}
