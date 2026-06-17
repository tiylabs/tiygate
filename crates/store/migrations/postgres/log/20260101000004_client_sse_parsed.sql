-- Client-side SSE parsed JSON column (PostgreSQL).
ALTER TABLE request_payloads ADD COLUMN IF NOT EXISTS client_sse_parsed_json TEXT;
