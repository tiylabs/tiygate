-- Gateway-side stream truncation reason mirrored onto the aggregated
-- request log. `request_payloads.truncation_reason` (migration 0005)
-- is the row-level source of truth, but the request-log list view and
-- its status badge need the signal without a payload join, so we
-- mirror it here.
--
-- Populated by the OLTP sink on the telemetry background task from
-- `ExchangeCapture::truncation_reason` via an order-independent upsert
-- (the capture may arrive before or after the RequestEvent insert).
-- Values: "idle" | "total" | "upstream_error". NULL for a clean
-- end-of-stream, for non-stream exchanges, and for rows captured
-- before this column existed.
--
-- Note: request_logs.status / http_status keep HTTP semantics (a
-- mid-stream truncation still has status="ok" / http_status=200), so
-- this column is the authoritative "status=ok but actually truncated"
-- signal and existing status-based billing/aggregation queries are
-- left untouched.

ALTER TABLE request_logs ADD COLUMN truncation_reason TEXT;
