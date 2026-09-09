-- capabilities_json
--
-- Provider-declared capability profile overlay (ADR-0001). JSON
-- object: { "endpoint": {...}, "fields": [FieldRule...] }. When the
-- endpoint matches the request's egress protocol suite, the overlay's
-- field rules REPLACE the built-in profile's rules wholesale; the
-- built-in structure hook (e.g. the DeepSeek tool allow-list) is
-- retained. Empty string = no overlay (pure built-in behaviour).
--
-- Validated by the Admin API at save time (must parse as a
-- CapabilityProfileOverride); the route-table builder tolerates and
-- ignores invalid JSON so a bad row cannot break request routing.

ALTER TABLE providers ADD COLUMN capabilities_json TEXT NOT NULL DEFAULT '';
