-- Optional provider-specific secret used by the Admin API to query
-- subscription usage. It is separate from the upstream API key because the
-- ZenMux Management API key has a different scope and purpose.
ALTER TABLE providers
    ADD COLUMN encrypted_usage_management_key TEXT NOT NULL DEFAULT '';
