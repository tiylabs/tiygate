-- finish_reason column on request_logs (PostgreSQL).
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS finish_reason TEXT;
