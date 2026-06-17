-- Truncation reason column on request_payloads (PostgreSQL).
ALTER TABLE request_payloads ADD COLUMN IF NOT EXISTS truncation_reason TEXT;
