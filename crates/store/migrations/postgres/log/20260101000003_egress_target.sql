-- Egress method and path columns (PostgreSQL).
ALTER TABLE request_payloads ADD COLUMN IF NOT EXISTS egress_method TEXT NOT NULL DEFAULT '';
ALTER TABLE request_payloads ADD COLUMN IF NOT EXISTS egress_path TEXT NOT NULL DEFAULT '';
