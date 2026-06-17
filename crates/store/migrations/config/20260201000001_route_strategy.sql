-- Per-route routing strategy override.
--
-- Adds an optional `routing_strategy` column to the `routes` table. A NULL
-- value means the route inherits the gateway-wide default strategy
-- (`ServerConfig.routing_strategy`, configured via TIYGATE_ROUTING_STRATEGY).
-- Non-NULL values are one of the canonical snake_case tokens:
-- 'weighted' | 'priority' | 'cooldown' | 'latency'.
--
-- Existing rows keep NULL, so the upgrade is fully backward-compatible.
ALTER TABLE routes ADD COLUMN routing_strategy TEXT;
