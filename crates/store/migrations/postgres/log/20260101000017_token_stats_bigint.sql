-- Token and cost counters must not be limited to PostgreSQL's 32-bit INTEGER.
-- A busy gateway can exceed 2,147,483,647 lifetime tokens in a short period.

ALTER TABLE token_daily_stats
    ALTER COLUMN request_count TYPE BIGINT USING request_count::bigint,
    ALTER COLUMN total_tokens TYPE BIGINT USING total_tokens::bigint,
    ALTER COLUMN prompt_tokens TYPE BIGINT USING prompt_tokens::bigint,
    ALTER COLUMN completion_tokens TYPE BIGINT USING completion_tokens::bigint,
    ALTER COLUMN reasoning_tokens TYPE BIGINT USING reasoning_tokens::bigint,
    ALTER COLUMN total_cost TYPE BIGINT USING total_cost::bigint,
    ALTER COLUMN peak_single_request TYPE BIGINT USING peak_single_request::bigint,
    ALTER COLUMN longest_task_ms TYPE BIGINT USING longest_task_ms::bigint;

ALTER TABLE token_summary
    ALTER COLUMN lifetime_tokens TYPE BIGINT USING lifetime_tokens::bigint,
    ALTER COLUMN peak_day_tokens TYPE BIGINT USING peak_day_tokens::bigint,
    ALTER COLUMN longest_task_ms TYPE BIGINT USING longest_task_ms::bigint,
    ALTER COLUMN current_streak TYPE BIGINT USING current_streak::bigint,
    ALTER COLUMN longest_streak TYPE BIGINT USING longest_streak::bigint,
    ALTER COLUMN lifetime_cost TYPE BIGINT USING lifetime_cost::bigint,
    ALTER COLUMN peak_day_cost TYPE BIGINT USING peak_day_cost::bigint;
