-- Optional per-route override for capability-aware routing. NULL inherits the
-- gateway-wide runtime mode.
ALTER TABLE routes ADD COLUMN capability_routing_mode TEXT;
