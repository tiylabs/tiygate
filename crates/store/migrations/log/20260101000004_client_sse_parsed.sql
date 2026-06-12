-- Best-effort merged/structured SSE parse result for the Gateway ->
-- Client (g->c) response direction. Mirrors `sse_parsed_json` (which
-- carries the Provider -> Gateway upstream parse) so the request-log
-- detail view can show a parsed result for both directions.
--
-- Populated by the OLTP sink on the telemetry background task from
-- `client_resp_body` when the exchange used a streaming (SSE)
-- response. NULL for non-stream exchanges and for rows captured
-- before this column existed.

ALTER TABLE request_payloads ADD COLUMN client_sse_parsed_json TEXT;
