-- Truncation reason mirrored to request_logs (PostgreSQL).
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS truncation_reason TEXT;
