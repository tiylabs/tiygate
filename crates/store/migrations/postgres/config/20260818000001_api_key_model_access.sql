-- Optional allow-list of client-facing virtual model names for each inbound
-- API key. NULL means unrestricted for backwards compatibility; an empty JSON
-- array means the key cannot access any model.
ALTER TABLE api_keys ADD COLUMN allowed_models_json TEXT NULL;
