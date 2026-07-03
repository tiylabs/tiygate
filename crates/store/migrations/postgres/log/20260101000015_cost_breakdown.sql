-- Cost breakdown columns on request_logs (PostgreSQL).
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS input_cost INTEGER;
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS output_cost INTEGER;
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS cache_read_cost INTEGER;
ALTER TABLE request_logs ADD COLUMN IF NOT EXISTS cache_write_cost INTEGER;
