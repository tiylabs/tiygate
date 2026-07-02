-- Provider model-discovery endpoint (e.g. GET /v1/models).
-- Backward-compatible: defaults to empty string so existing rows are valid.
ALTER TABLE providers ADD COLUMN models_endpoint TEXT NOT NULL DEFAULT '';
