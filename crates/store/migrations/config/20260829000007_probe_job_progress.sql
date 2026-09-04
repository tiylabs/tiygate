-- Durable progress cursor for interruptible probe bundles.  A worker can
-- persist completed outcomes and resume at the next probe after shutdown.
ALTER TABLE target_probe_jobs
    ADD COLUMN next_probe_index INTEGER NOT NULL DEFAULT 0;
