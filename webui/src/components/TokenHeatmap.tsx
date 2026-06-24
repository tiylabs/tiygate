import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TokenDayActivity } from "@/api/types";
import { fmtTokens } from "@/lib/format";

type Granularity = "daily" | "weekly";

interface TokenHeatmapProps {
  data: TokenDayActivity[];
  /** Total number of days the heatmap should span (default 365). */
  spanDays?: number;
  isLoading?: boolean;
}

/** Parse a "YYYY-MM-DD" string into a UTC Date, avoiding local-timezone shifts. */
function parseDay(s: string): Date {
  const [y, m, d] = s.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d));
}

/** Format a UTC Date back to "YYYY-MM-DD". */
function fmtDay(d: Date): string {
  return d.toISOString().slice(0, 10);
}

/** Map a token count to a color intensity level (0-4). */
function intensityLevel(tokens: number, max: number): number {
  if (tokens === 0 || max === 0) return 0;
  const ratio = tokens / max;
  if (ratio < 0.1) return 1;
  if (ratio < 0.3) return 2;
  if (ratio < 0.6) return 3;
  return 4;
}

/** CSS custom-property names for each intensity level (purple ramp).
 *  Values live in index.css and adapt to light / dark themes. */
const LEVEL_VARS = [
  "var(--heat-0)", // 0: empty / no activity
  "var(--heat-1)", // 1: low
  "var(--heat-2)", // 2: medium-low
  "var(--heat-3)", // 3: medium-high
  "var(--heat-4)", // 4: high
];

/** Short month name from a 0-based month index, respecting the user's locale. */
function shortMonthName(monthIdx: number): string {
  return new Date(Date.UTC(2000, monthIdx, 1)).toLocaleString("default", {
    month: "short",
  });
}

/** Aggregate daily data into weekly buckets. */
function aggregateWeekly(days: TokenDayActivity[]): TokenDayActivity[] {
  const weeks = new Map<string, TokenDayActivity>();
  for (const d of days) {
    const date = parseDay(d.day);
    // Week key = Monday of that week (UTC)
    const dayOfWeek = date.getUTCDay();
    const monday = new Date(date);
    monday.setUTCDate(date.getUTCDate() - ((dayOfWeek + 6) % 7));
    const key = fmtDay(monday);
    const existing = weeks.get(key);
    if (existing) {
      existing.total_tokens += d.total_tokens;
      existing.request_count += d.request_count;
    } else {
      weeks.set(key, { ...d, day: key });
    }
  }
  return Array.from(weeks.values()).sort((a, b) => a.day.localeCompare(b.day));
}

/** Convert daily data to cumulative — kept for potential future use. */
// function toCumulative(days: TokenDayActivity[]): TokenDayActivity[] {
//   let running = 0;
//   return days.map((d) => {
//     running += d.total_tokens;
//     return { ...d, total_tokens: running };
//   });
// }

/** Generate the full grid of cells for the heatmap (7 rows x N columns).
 *  The grid always spans from (today - spanDays) to today, so the layout
 *  stays fixed even when historical data is sparse — just like GitHub's
 *  contribution graph. */
function buildGrid(
  days: TokenDayActivity[],
  spanDays: number,
): {
  cells: Array<{ day: string; tokens: number; col: number; row: number }>;
  columns: number;
  months: Array<{ label: string; col: number }>;
} {
  // Build a map of day -> tokens from whatever data we have
  const dayMap = new Map<string, number>();
  for (const d of days) {
    dayMap.set(d.day, d.total_tokens);
  }

  // Fixed window: today (UTC) back to (today - spanDays)
  const now = new Date();
  const end = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate()));
  const start = new Date(end);
  start.setUTCDate(end.getUTCDate() - spanDays + 1);

  // Align start to Sunday (beginning of week column) — all UTC
  const startDayOfWeek = start.getUTCDay();
  const alignedStart = new Date(start);
  alignedStart.setUTCDate(start.getUTCDate() - startDayOfWeek);

  const cells: Array<{ day: string; tokens: number; col: number; row: number }> = [];
  const months: Array<{ label: string; col: number }> = [];
  let currentMonth = -1;

  const cursor = new Date(alignedStart);
  let col = 0;
  let isFirstWeek = true;

  while (cursor <= end) {
    const row = cursor.getUTCDay(); // 0=Sun, 6=Sat

    // New week starts on Sunday — advance column (skip the very first one)
    if (row === 0) {
      if (!isFirstWeek) col++;
      isFirstWeek = false;

      // Check if this week starts a new month
      const month = cursor.getUTCMonth();
      if (month !== currentMonth) {
        currentMonth = month;
        months.push({
          label: shortMonthName(month),
          col,
        });
      }
    }

    const dayStr = fmtDay(cursor);
    const tokens = dayMap.get(dayStr) ?? 0;
    cells.push({ day: dayStr, tokens, col, row });

    cursor.setUTCDate(cursor.getUTCDate() + 1);
  }

  const columns = col + 1;
  return { cells, columns, months };
}

export function TokenHeatmap({ data, spanDays = 365, isLoading }: TokenHeatmapProps) {
  const { t } = useTranslation();
  const [granularity, setGranularity] = useState<Granularity>("daily");

  const processedData = useMemo(() => {
    switch (granularity) {
      case "weekly":
        return aggregateWeekly(data);
      default:
        return data;
    }
  }, [data, granularity]);

  const { cells, columns, months } = useMemo(
    () => buildGrid(processedData, spanDays),
    [processedData, spanDays],
  );

  // Pre-build a lookup map for O(1) cell access during render
  const cellMap = useMemo(() => {
    const m = new Map<string, (typeof cells)[0]>();
    for (const c of cells) {
      m.set(`${c.col}:${c.row}`, c);
    }
    return m;
  }, [cells]);

  const maxTokens = useMemo(
    () => Math.max(...cells.map((c) => c.tokens), 1),
    [cells],
  );

  const granularityOptions: { key: Granularity; label: string }[] = [
    { key: "daily", label: t("tokenActivity.daily", "Daily") },
    { key: "weekly", label: t("tokenActivity.weekly", "Weekly") },
  ];

  if (isLoading) {
    // Match the real heatmap's intrinsic width: 53 week-columns × 14px
    // (11px cell + 3px gap) − 3px trailing gap ≈ 739px.
    return (
      <div className="space-y-3" style={{ minWidth: 739 }}>
        {/* Header placeholder */}
        <div className="flex items-center justify-between">
          <div className="h-4 w-24 animate-pulse rounded bg-surface-secondary" />
          <div className="h-6 w-24 animate-pulse rounded-md bg-surface-secondary" />
        </div>
        {/* Grid placeholder */}
        <div className="h-[113px] animate-pulse rounded-lg bg-surface-secondary" />
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {/* Header row */}
      <div className="flex items-center justify-between">
        <h3 className="text-sm font-medium text-text">
          {t("tokenActivity.title", "Token activity")}
        </h3>
        <div className="flex gap-1 rounded-md border border-border p-0.5">
          {granularityOptions.map((opt) => (
            <button
              key={opt.key}
              onClick={() => setGranularity(opt.key)}
              className={`rounded px-2.5 py-1 text-xs transition-colors ${
                granularity === opt.key
                  ? "bg-primary text-white"
                  : "text-text-subtle hover:text-text"
              }`}
            >
              {opt.label}
            </button>
          ))}
        </div>
      </div>

      {/* Heatmap grid — GitHub-style contribution graph */}
      <div style={{ overflowX: "auto" }}>
        {/* Month labels */}
        <div style={{ display: "flex", gap: "0px", marginBottom: "4px" }}>
          {months.map((m, i) => {
            const nextCol = i + 1 < months.length ? months[i + 1].col : columns;
            const span = nextCol - m.col;
            const width = span * 14 - 3;
            return (
              <span
                key={`${m.label}-${m.col}`}
                style={{
                  flexShrink: 0,
                  width: `${width}px`,
                  marginLeft: i === 0 ? `${m.col * 14}px` : "3px",
                  fontSize: "10px",
                  lineHeight: 1,
                  color: "var(--color-text-subtle)",
                }}
              >
                {m.label}
              </span>
            );
          })}
        </div>

        {/* Grid cells: horizontal row of week-columns */}
        <div style={{ display: "flex", gap: "3px" }}>
          {Array.from({ length: columns }).map((_, colIdx) => (
            <div
              key={colIdx}
              style={{ display: "flex", flexDirection: "column", flexShrink: 0, gap: "3px" }}
            >
              {Array.from({ length: 7 }).map((_, rowIdx) => {
                const cell = cellMap.get(`${colIdx}:${rowIdx}`);
                if (!cell) {
                  return (
                    <div
                      key={rowIdx}
                      style={{ width: 11, height: 11, borderRadius: 2 }}
                    />
                  );
                }
                const level = intensityLevel(cell.tokens, maxTokens);
                return (
                  <div
                    key={rowIdx}
                    style={{
                      width: 11,
                      height: 11,
                      borderRadius: 2,
                      backgroundColor: LEVEL_VARS[level],
                      transition: "background-color 0.15s",
                    }}
                    title={`${cell.day}: ${fmtTokens(cell.tokens)} tokens`}
                  />
                );
              })}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
