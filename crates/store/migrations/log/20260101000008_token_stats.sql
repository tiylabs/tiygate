-- Pre-aggregated token statistics tables for the Dashboard
-- "Token Activity" panel. Populated asynchronously by a background
-- task every 5 minutes (space-for-time trade-off: O(1) API reads).

-- Per-day aggregated token counts, used for the heatmap and trend chart.
CREATE TABLE IF NOT EXISTS token_daily_stats (
    day TEXT NOT NULL PRIMARY KEY,          -- ISO date "2026-06-15"
    request_count INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    peak_single_request INTEGER NOT NULL DEFAULT 0,  -- max tokens in a single request that day
    longest_task_ms INTEGER NOT NULL DEFAULT 0,       -- longest total_latency_ms that day
    updated_at TEXT NOT NULL
);

-- Single-row summary table for the top metric cards.
-- Always contains exactly one row with id=1.
CREATE TABLE IF NOT EXISTS token_summary (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    lifetime_tokens INTEGER NOT NULL DEFAULT 0,
    peak_day_tokens INTEGER NOT NULL DEFAULT 0,
    longest_task_ms INTEGER NOT NULL DEFAULT 0,
    current_streak INTEGER NOT NULL DEFAULT 0,
    longest_streak INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

-- Seed the summary row so the API can always SELECT without
-- worrying about an empty table.
INSERT OR IGNORE INTO token_summary (id, lifetime_tokens, peak_day_tokens, longest_task_ms, current_streak, longest_streak, updated_at)
VALUES (1, 0, 0, 0, 0, 0, '1970-01-01T00:00:00Z');

-- ============================================================
-- One-time backfill: aggregate ALL existing request_logs into
-- token_daily_stats so the heatmap is immediately populated
-- after migration, without waiting for the background task.
-- ============================================================
INSERT OR IGNORE INTO token_daily_stats
    (day, request_count, total_tokens, prompt_tokens, completion_tokens,
     reasoning_tokens, peak_single_request, longest_task_ms, updated_at)
SELECT
    DATE(ts) AS day,
    COUNT(*) AS request_count,
    COALESCE(SUM(total_tokens), 0) AS total_tokens,
    COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
    COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
    COALESCE(SUM(reasoning_tokens), 0) AS reasoning_tokens,
    COALESCE(MAX(total_tokens), 0) AS peak_single_request,
    COALESCE(MAX(total_latency_ms), 0) AS longest_task_ms,
    datetime('now') AS updated_at
FROM request_logs
WHERE ts IS NOT NULL
GROUP BY DATE(ts);

-- Backfill the summary row from the just-populated daily stats.
UPDATE token_summary SET
    lifetime_tokens = COALESCE((SELECT SUM(total_tokens) FROM token_daily_stats), 0),
    peak_day_tokens = COALESCE((SELECT MAX(total_tokens) FROM token_daily_stats), 0),
    longest_task_ms = COALESCE((SELECT MAX(longest_task_ms) FROM token_daily_stats), 0),
    updated_at = datetime('now')
WHERE id = 1;
