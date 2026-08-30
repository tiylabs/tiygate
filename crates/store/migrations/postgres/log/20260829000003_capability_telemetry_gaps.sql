-- Durable markers for capability-plan/feedback events that could not be
-- accepted by the primary telemetry queue.  A gap keeps the affected
-- Route × capability shape out of enforce until the observation window is
-- repaired.
CREATE TABLE IF NOT EXISTS request_capability_telemetry_gaps (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    shape_hash TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL,
    dropped_count BIGINT NOT NULL DEFAULT 1,
    first_ts TEXT NOT NULL,
    last_ts TEXT NOT NULL,
    PRIMARY KEY (request_id, route_id, shape_hash, target, reason)
);

CREATE INDEX IF NOT EXISTS idx_request_capability_gaps_shape
    ON request_capability_telemetry_gaps (route_id, shape_hash, last_ts);
