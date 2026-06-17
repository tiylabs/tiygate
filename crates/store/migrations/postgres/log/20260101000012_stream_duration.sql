-- stream_duration_ms column on request_logs (PostgreSQL).
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS stream_duration_ms INTEGER;
