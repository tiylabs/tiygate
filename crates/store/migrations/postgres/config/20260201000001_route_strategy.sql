-- Per-route routing strategy override (PostgreSQL).
ALTER TABLE routes ADD COLUMN IF NOT EXISTS routing_strategy TEXT;
