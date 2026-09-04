-- Persist target-specific capability planner decisions separately from hop
-- attempts. The table is append/upsert oriented so events may arrive out of
-- order on the asynchronous telemetry bus.
CREATE TABLE IF NOT EXISTS request_capability_plans (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL DEFAULT '',
    target TEXT NOT NULL,
    ts TEXT NOT NULL,
    mode TEXT NOT NULL,
    shape_hash TEXT NOT NULL DEFAULT '',
    planning_micros BIGINT NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    requirements_json TEXT NOT NULL,
    missing_json TEXT NOT NULL,
    unknown_json TEXT NOT NULL,
    transform TEXT,
    UNIQUE(request_id, target)
);

CREATE INDEX IF NOT EXISTS idx_request_capability_plans_request_id
    ON request_capability_plans (request_id);
CREATE INDEX IF NOT EXISTS idx_request_capability_plans_status
    ON request_capability_plans (status);

CREATE TABLE IF NOT EXISTS request_capability_feedback (
    request_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    shape_hash TEXT NOT NULL,
    target TEXT NOT NULL,
    capability TEXT NOT NULL,
    outcome TEXT NOT NULL,
    ts TEXT NOT NULL,
    UNIQUE(request_id, target, capability)
);

CREATE INDEX IF NOT EXISTS idx_request_capability_feedback_shape
    ON request_capability_feedback (route_id, shape_hash, ts);
