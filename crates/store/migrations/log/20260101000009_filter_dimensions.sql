-- Request filter dimension dictionary.
--
-- Maintained by the OLTP sink when completed RequestEvent rows are
-- persisted. The admin request-log filter-options endpoint reads this
-- small dictionary table instead of running SELECT DISTINCT over the
-- large request_logs table on every page load.

CREATE TABLE IF NOT EXISTS request_filter_dimensions (
    dimension TEXT NOT NULL,
    value TEXT NOT NULL,
    first_seen_ts TEXT NOT NULL,
    last_seen_ts TEXT NOT NULL,
    use_count INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (dimension, value)
);

CREATE INDEX IF NOT EXISTS idx_request_filter_dimensions_lookup
    ON request_filter_dimensions (dimension, last_seen_ts);

-- ============================================================
-- One-time backfill: seed all low-cardinality dimensions from
-- existing request_logs so current and future filter options have
-- historical values immediately after migration.
-- ============================================================

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'virtual_model', virtual_model, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE virtual_model IS NOT NULL AND trim(virtual_model) <> ''
GROUP BY virtual_model;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'resolved_provider', resolved_provider, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE resolved_provider IS NOT NULL AND trim(resolved_provider) <> ''
GROUP BY resolved_provider;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'resolved_model', resolved_model, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE resolved_model IS NOT NULL AND trim(resolved_model) <> ''
GROUP BY resolved_model;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'account_label', account_label, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE account_label IS NOT NULL AND trim(account_label) <> ''
GROUP BY account_label;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'ingress_protocol', ingress_protocol, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE ingress_protocol IS NOT NULL AND trim(ingress_protocol) <> ''
GROUP BY ingress_protocol;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'egress_protocol', egress_protocol, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE egress_protocol IS NOT NULL AND trim(egress_protocol) <> ''
GROUP BY egress_protocol;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'cache_hit', cache_hit, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE cache_hit IS NOT NULL AND trim(cache_hit) <> ''
GROUP BY cache_hit;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'status', status, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE status IS NOT NULL AND trim(status) <> ''
GROUP BY status;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'error_class', error_class, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE error_class IS NOT NULL AND trim(error_class) <> ''
GROUP BY error_class;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'http_status', CAST(http_status AS TEXT), MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE http_status IS NOT NULL
GROUP BY http_status;

INSERT OR IGNORE INTO request_filter_dimensions
    (dimension, value, first_seen_ts, last_seen_ts, use_count)
SELECT 'error_source', error_source, MIN(ts), MAX(ts), COUNT(*)
FROM request_logs
WHERE error_source IS NOT NULL AND trim(error_source) <> ''
GROUP BY error_source;
