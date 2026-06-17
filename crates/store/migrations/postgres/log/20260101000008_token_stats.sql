-- Pre-aggregated token statistics tables (PostgreSQL).

CREATE TABLE IF NOT EXISTS token_daily_stats (
    day TEXT NOT NULL PRIMARY KEY,
    request_count INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0,
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    peak_single_request INTEGER NOT NULL DEFAULT 0,
    longest_task_ms INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS token_summary (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    lifetime_tokens INTEGER NOT NULL DEFAULT 0,
    peak_day_tokens INTEGER NOT NULL DEFAULT 0,
    longest_task_ms INTEGER NOT NULL DEFAULT 0,
    current_streak INTEGER NOT NULL DEFAULT 0,
    longest_streak INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

-- Seed the summary row.
INSERT INTO token_summary (id, lifetime_tokens, peak_day_tokens, longest_task_ms, current_streak, longest_streak, updated_at)
VALUES (1, 0, 0, 0, 0, 0, '1970-01-01T00:00:00Z')
ON CONFLICT (id) DO NOTHING;

-- Backfill daily stats from existing request_logs.
INSERT INTO token_daily_stats
    (day, request_count, total_tokens, prompt_tokens, completion_tokens,
     reasoning_tokens, peak_single_request, longest_task_ms, updated_at)
SELECT
    to_char(ts::date, 'YYYY-MM-DD') AS day,
    COUNT(*) AS request_count,
    COALESCE(SUM(total_tokens), 0) AS total_tokens,
    COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
    COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
    COALESCE(SUM(reasoning_tokens), 0) AS reasoning_tokens,
    COALESCE(MAX(total_tokens), 0) AS peak_single_request,
    COALESCE(MAX(total_latency_ms), 0) AS longest_task_ms,
    NOW()::text AS updated_at
FROM request_logs
WHERE ts IS NOT NULL
GROUP BY ts::date
ON CONFLICT (day) DO NOTHING;

-- Backfill the summary row.
UPDATE token_summary SET
    lifetime_tokens = COALESCE((SELECT SUM(total_tokens) FROM token_daily_stats), 0),
    peak_day_tokens = COALESCE((SELECT MAX(total_tokens) FROM token_daily_stats), 0),
    longest_task_ms = COALESCE((SELECT MAX(longest_task_ms) FROM token_daily_stats), 0),
    updated_at = NOW()::text
WHERE id = 1;
