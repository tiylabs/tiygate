-- Store bounded, redacted probe exchanges and the resulting judge output.
-- Details are written by the asynchronous telemetry sink; request bodies,
-- response bodies, and judgment fields are never collected from business
-- traffic.
ALTER TABLE capability_probe_runs
    ADD COLUMN IF NOT EXISTS job_id TEXT;

ALTER TABLE capability_probe_runs
    ADD COLUMN IF NOT EXISTS details_json TEXT;

CREATE INDEX IF NOT EXISTS idx_capability_probe_runs_job_ts
    ON capability_probe_runs (job_id, ts);
