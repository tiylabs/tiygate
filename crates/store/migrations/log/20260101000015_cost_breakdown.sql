-- Cost breakdown columns on request_logs.
--
-- Per-component cost in micro-USD (1 micro-USD = 1e-6 USD), written
-- by the OLTP sink's `try_compute_cost` alongside the aggregate
-- `cost` column. Each column is NULL for legacy rows and for
-- requests where the corresponding price/token pair was absent.

ALTER TABLE request_logs ADD COLUMN input_cost INTEGER;
ALTER TABLE request_logs ADD COLUMN output_cost INTEGER;
ALTER TABLE request_logs ADD COLUMN cache_read_cost INTEGER;
ALTER TABLE request_logs ADD COLUMN cache_write_cost INTEGER;
