-- Add cost rollups to pre-aggregated token statistics.
-- Cost is stored in micro-USD, matching request_logs.cost.

ALTER TABLE token_daily_stats ADD COLUMN IF NOT EXISTS total_cost INTEGER NOT NULL DEFAULT 0;
ALTER TABLE token_summary ADD COLUMN IF NOT EXISTS lifetime_cost INTEGER NOT NULL DEFAULT 0;
ALTER TABLE token_summary ADD COLUMN IF NOT EXISTS peak_day_cost INTEGER NOT NULL DEFAULT 0;

-- Backfill existing daily rows from request_logs. NULL costs represent
-- historical or unpriced requests and are treated as zero.
UPDATE token_daily_stats SET
    total_cost = COALESCE((
        SELECT SUM(cost)
        FROM request_logs
        WHERE to_char(request_logs.ts::date, 'YYYY-MM-DD') = token_daily_stats.day
    ), 0);

UPDATE token_summary SET
    lifetime_cost = COALESCE((SELECT SUM(total_cost) FROM token_daily_stats), 0),
    peak_day_cost = COALESCE((SELECT MAX(total_cost) FROM token_daily_stats), 0),
    updated_at = NOW()::text
WHERE id = 1;
