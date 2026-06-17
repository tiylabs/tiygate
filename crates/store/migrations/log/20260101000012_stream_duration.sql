-- stream_duration_ms
--
-- Duration of the streaming body transfer in milliseconds, measured
-- from upstream response-header arrival to stream EOF / error /
-- timeout. Only populated for SSE stream responses; NULL for
-- non-stream exchanges.
--
-- Populated by the OLTP sink's `write_capture` background task
-- (alongside usage write-back) via an order-independent upsert.
--
-- Used to compute output token rate:
--   tokens_per_second = completion_tokens / (stream_duration_ms / 1000)

ALTER TABLE request_logs ADD COLUMN stream_duration_ms INTEGER;
