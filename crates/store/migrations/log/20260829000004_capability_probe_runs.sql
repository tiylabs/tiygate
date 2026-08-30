-- Probe audit and cost records are separate from business request telemetry.
CREATE TABLE IF NOT EXISTS capability_probe_runs (
    run_id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    probe_id TEXT NOT NULL,
    outcome TEXT NOT NULL,
    duration_micros INTEGER NOT NULL DEFAULT 0,
    budget_weight INTEGER NOT NULL DEFAULT 1,
    error_class TEXT,
    ts TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_capability_probe_runs_target_ts
    ON capability_probe_runs (target, ts);
CREATE INDEX IF NOT EXISTS idx_capability_probe_runs_outcome_ts
    ON capability_probe_runs (outcome, ts);
