-- Gateway-side stream truncation reason for the Provider -> Gateway
-- streaming (SSE) response. When the gateway terminates a streaming
-- response itself (instead of seeing a natural end-of-stream) it
-- records why: "idle" (idle timer fired), "total" (total wall-clock
-- budget elapsed), or "upstream_error" (upstream connection errored
-- mid-stream).
--
-- Populated by the OLTP sink on the telemetry background task from
-- `ExchangeCapture::truncation_reason`. NULL for a clean end-of-stream,
-- for non-stream exchanges, and for rows captured before this column
-- existed. Note: request_logs.status / http_status keep HTTP semantics
-- (a mid-stream truncation still has http_status 200), so this column
-- is the authoritative signal for "status=ok but actually truncated".

ALTER TABLE request_payloads ADD COLUMN truncation_reason TEXT;
