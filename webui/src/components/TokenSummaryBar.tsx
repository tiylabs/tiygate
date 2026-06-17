import { useTranslation } from "react-i18next";
import type { TokenSummaryData } from "@/api/types";
import { fmtTokens } from "@/lib/format";

interface TokenSummaryBarProps {
  data?: TokenSummaryData | null;
  isLoading?: boolean;
}

export function TokenSummaryBar({ data, isLoading }: TokenSummaryBarProps) {
  const { t } = useTranslation();

  const fmtStreak = (count: number) =>
    `${count} ${count === 1 ? t("tokenActivity.day", "day") : t("tokenActivity.days", "days")}`;

  const items = [
    {
      id: "lifetime",
      value: data ? fmtTokens(data.lifetime_tokens) : "…",
      label: t("tokenActivity.lifetimeTokens", "Lifetime tokens"),
    },
    {
      id: "peak",
      value: data ? fmtTokens(data.peak_day_tokens) : "…",
      label: t("tokenActivity.peakTokens", "Peak day tokens"),
    },
    {
      id: "current-streak",
      value: data ? fmtStreak(data.current_streak) : "…",
      label: t("tokenActivity.currentStreak", "Current streak"),
    },
    {
      id: "longest-streak",
      value: data ? fmtStreak(data.longest_streak) : "…",
      label: t("tokenActivity.longestStreak", "Longest streak"),
    },
  ];

  return (
    <div className="grid h-full grid-cols-2 gap-4">
      {items.map((item) => (
        <div
          key={item.id}
          className="flex flex-col items-center justify-center rounded-lg border border-border bg-surface px-4 py-3"
        >
          <span className={`text-lg font-medium tabular-nums text-text ${isLoading ? "animate-pulse" : ""}`}>
            {item.value}
          </span>
          <span className="text-xs text-text-subtle">{item.label}</span>
        </div>
      ))}
    </div>
  );
}
