ALTER TABLE request_payloads ADD COLUMN payload_archive_status TEXT NOT NULL DEFAULT 'archive_ready';
ALTER TABLE request_payloads ADD COLUMN payload_archive_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE request_payloads ADD COLUMN payload_archive_last_error TEXT;
ALTER TABLE request_payloads ADD COLUMN payload_archive_locked_at TEXT;
ALTER TABLE request_payloads ADD COLUMN payload_archived_at TEXT;
ALTER TABLE request_payloads ADD COLUMN payload_archive_manifest_json TEXT;

UPDATE request_payloads
SET payload_archive_status = 'archive_ready'
WHERE payload_archive_status = 'pending';

CREATE INDEX IF NOT EXISTS idx_request_payloads_archive_status_captured_at
    ON request_payloads(payload_archive_status, captured_at);
