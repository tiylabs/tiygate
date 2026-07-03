import { useTranslation } from "react-i18next";
import type { TokenSummaryData } from "@/api/types";
import { fmtTokens, fmtUsdFromMicros } from "@/lib/format";

interface TokenSummaryBarProps {
  data?: TokenSummaryData | null;
  isLoading?: boolean;
  group?: "all" | "lifetime" | "details";
  className?: string;
}

export function TokenSummaryBar({
  data,
  isLoading,
  group = "all",
  className,
}: TokenSummaryBarProps) {
  const { t } = useTranslation();

  const fmtStreak = (count: number) =>
    `${count} ${count === 1 ? t("tokenActivity.day", "day") : t("tokenActivity.days", "days")}`;

  const lifetimeItems = [
    {
      id: "lifetime",
      value: data ? fmtTokens(data.lifetime_tokens) : "…",
      label: t("tokenActivity.lifetimeTokens", "Lifetime tokens"),
    },
    {
      id: "lifetime-cost",
      value: data ? fmtUsdFromMicros(data.lifetime_cost) : "…",
      label: t("tokenActivity.lifetimeCost", "Lifetime cost"),
    },
  ];

  const detailItems = [
    {
      id: "peak",
      value: data ? fmtTokens(data.peak_day_tokens) : "…",
      label: t("tokenActivity.peakTokens", "Peak day tokens"),
    },
    {
      id: "peak-cost",
      value: data ? fmtUsdFromMicros(data.peak_day_cost) : "…",
      label: t("tokenActivity.peakDayCost", "Peak day cost"),
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

  const items =
    group === "lifetime"
      ? lifetimeItems
      : group === "details"
        ? detailItems
        : [
            lifetimeItems[0],
            lifetimeItems[1],
            detailItems[3],
            detailItems[0],
            detailItems[1],
            detailItems[2],
          ];

  const gridClass =
    group === "lifetime"
      ? "grid h-full grid-cols-1 gap-4"
      : group === "details"
        ? "grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4"
        : "grid h-full grid-cols-2 gap-4 lg:grid-cols-3";

  return (
    <div className={`${gridClass} ${className ?? ""}`}>
      {items.map((item) => (
        <div
          key={item.id}
          className="flex flex-col items-center justify-center rounded-lg border border-border bg-surface px-4 py-3"
        >
          <span
            className={`text-lg font-medium tabular-nums text-text ${isLoading ? "animate-pulse" : ""}`}
          >
            {item.value}
          </span>
          <span className="text-xs text-text-subtle">{item.label}</span>
        </div>
      ))}
    </div>
  );
}
