-- Provider model-discovery endpoint (PostgreSQL).
-- Backward-compatible: defaults to empty string so existing rows are valid.
ALTER TABLE providers ADD COLUMN IF NOT EXISTS models_endpoint TEXT NOT NULL DEFAULT '';
