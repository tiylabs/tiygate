-- capability_decisions_json column on request_logs (PostgreSQL).
--
-- JSON array of structured `CapabilityDecision` records emitted by
-- the active provider CapabilityProfile while the gateway prepared
-- the upstream request body (fields stripped / converted / rejected
-- before egress). NULL for requests that did not run a profile.
--
-- Populated by the OLTP sink via the `CapabilityChecked` pipeline
-- event with an order-independent upsert (the terminal
-- `RequestEvent` insert's ON CONFLICT clause does not list the
-- column). Last write wins when a fallback retry re-runs the
-- sanitize pass for a second target.
--
-- Consumed by the Admin request-log detail view to explain why the
-- upstream body differs from the client's payload.

ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS capability_decisions_json TEXT;
