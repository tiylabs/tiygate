-- Per-route, per-capability-shape rollout gate. Route-level mode remains the
-- upper bound; an enforce row is required before a shape can filter targets.
CREATE TABLE IF NOT EXISTS capability_route_admissions (
    route_id TEXT NOT NULL,
    capability_shape_hash TEXT NOT NULL,
    required_capabilities_json TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('shadow', 'enforce')),
    gate_policy_version INTEGER NOT NULL,
    report_json TEXT NOT NULL,
    approved_by TEXT,
    approved_at TEXT,
    expires_at TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (route_id, capability_shape_hash)
);

CREATE INDEX IF NOT EXISTS idx_capability_route_admissions_route
    ON capability_route_admissions (route_id, updated_at);
