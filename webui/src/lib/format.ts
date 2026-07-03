/** Format a token count with compact units (B / M / K), 2 decimal places. Values < 1000 are shown as-is. */
export function fmtTokens(value?: number | null): string {
  if (value === null || value === undefined) return "—";
  if (value >= 1_000_000_000) return (value / 1_000_000_000).toFixed(2) + "B";
  if (value >= 1_000_000) return (value / 1_000_000).toFixed(2) + "M";
  if (value >= 1_000) return (value / 1_000).toFixed(2) + "K";
  return String(value);
}

/** Format a micro-USD integer amount as USD, preserving extra precision for sub-dollar values. */
export function fmtUsdFromMicros(value?: number | null): string {
  if (value === null || value === undefined) return "—";
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: value > 0 && value < 1_000_000 ? 4 : 2,
  }).format(value / 1_000_000);
}
