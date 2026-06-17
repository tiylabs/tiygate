UPDATE request_payloads
SET payload_archive_status = 'archive_ready'
WHERE payload_archive_status = 'pending';
