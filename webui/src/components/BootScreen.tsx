import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

/**
 * Full-screen boot/loading screen shown during application startup.
 * Displays the brand logo, an animated progress bar, and a loading hint.
 */
export function BootScreen() {
  const { t } = useTranslation();
  const [progress, setProgress] = useState(8);

  useEffect(() => {
    // Simulate smooth progress towards 95% — never reaches 100%
    // until the actual loading completes and this component unmounts.
    const timer = setInterval(() => {
      setProgress((prev) => {
        if (prev >= 92) return prev;
        // Decelerating approach: larger jumps early, smaller near the end
        const remaining = 95 - prev;
        return Math.min(95, prev + Math.max(1, remaining * 0.15));
      });
    }, 200);
    return () => clearInterval(timer);
  }, []);

  return (
    <div className="flex min-h-full items-center justify-center bg-bg px-4 py-10">
      <div className="flex w-full max-w-[320px] flex-col items-center gap-6">
        {/* Brand logo */}
        <span className="flex h-12 w-12 items-center justify-center overflow-hidden rounded-lg">
          <img
            src="./icon.svg"
            alt=""
            aria-hidden
            className="h-12 w-12"
          />
        </span>

        {/* Progress bar */}
        <div className="w-full">
          <div className="h-1 w-full overflow-hidden rounded-full bg-surface-muted">
            <div
              className="h-full rounded-full bg-primary transition-all duration-300 ease-out"
              style={{ width: `${progress}%` }}
            />
          </div>
        </div>

        {/* Loading hint */}
        <p className="text-sm text-text-muted">
          {t("common.bootLoading")}
        </p>
      </div>
    </div>
  );
}
