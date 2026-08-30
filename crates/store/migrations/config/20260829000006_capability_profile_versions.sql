-- Version tuple for capability profile resolution.  These columns let a
-- future registry/baseline or probe-judge change invalidate only affected
-- observations without changing the target identity.
ALTER TABLE target_capability_profiles
    ADD COLUMN registry_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE target_capability_profiles
    ADD COLUMN baseline_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE target_capability_profiles
    ADD COLUMN last_probe_judge_version INTEGER;
