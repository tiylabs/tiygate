-- Capture the upstream HTTP method and request path (path-only,
-- not the full URL) for the request-log detail view. Both are
-- produced by `finalize_egress` and stored verbatim. They are not
-- sensitive and are safe to expose in the admin replay payload.

ALTER TABLE request_payloads ADD COLUMN egress_method TEXT NOT NULL DEFAULT '';
ALTER TABLE request_payloads ADD COLUMN egress_path TEXT NOT NULL DEFAULT '';
