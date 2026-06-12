import { useTranslation } from "react-i18next";
import * as RDropdown from "@radix-ui/react-dropdown-menu";
import { Check, Palette } from "lucide-react";
import { THEMES, useTheme, type ThemeMode } from "@/lib/theme";
import { cn } from "@/lib/cn";

const contentClass =
  "z-40 rounded-lg border border-border bg-surface p-3 shadow-md " +
  "data-[state=open]:animate-overlay-in";

const ROWS: Array<{ mode: ThemeMode; labelKey: string }> = [
  { mode: "light", labelKey: "app.themeGroupLight" },
  { mode: "dark", labelKey: "app.themeGroupDark" },
];

export function ThemeSwitcher() {
  const { t } = useTranslation();
  const { theme, setTheme } = useTheme();

  return (
    <RDropdown.Root>
      <RDropdown.Trigger asChild>
        <button
          type="button"
          aria-label={t("app.themeMenu")}
          title={t("app.themeMenu")}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-muted transition-colors hover:bg-surface-muted hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Palette size={16} />
        </button>
      </RDropdown.Trigger>
      <RDropdown.Portal>
        <RDropdown.Content
          align="end"
          side="top"
          sideOffset={6}
          className={contentClass}
        >
          <RDropdown.Label className="px-0.5 pb-2.5 text-[11px] font-medium uppercase tracking-[0.04em] text-text-subtle">
            {t("app.themeMenu")}
          </RDropdown.Label>
          <div className="space-y-2.5">
            {ROWS.map((row) => (
              <div key={row.mode} className="flex items-center gap-2.5">
                <span className="w-9 shrink-0 text-[11px] font-medium text-text-muted">
                  {t(row.labelKey)}
                </span>
                <div className="flex gap-2.5">
                  {THEMES.filter((item) => item.mode === row.mode).map(
                    (item) => {
                      const active = item.id === theme;
                      const label = t(item.labelKey);
                      return (
                        <RDropdown.Item
                          key={item.id}
                          onSelect={(event) => {
                            // Keep the menu open so users can compare themes
                            // back-to-back. preventDefault stops Radix
                            // from auto-closing the dropdown on selection.
                            event.preventDefault();
                            setTheme(item.id);
                          }}
                          aria-label={label}
                          aria-current={active}
                          title={label}
                          className={cn(
                            "relative h-5 w-8 overflow-visible rounded p-0 outline-none",
                            "transition-shadow duration-[var(--duration-fast)]",
                            "ring-1 ring-inset ring-border",
                            "data-[highlighted]:ring-border-strong focus-visible:ring-2 focus-visible:ring-ring",
                            active &&
                              "ring-2 ring-primary ring-offset-2 ring-offset-surface data-[highlighted]:ring-primary",
                          )}
                        >
                          {/* diagonal split chip: upper-left = theme primary, lower-right = theme background */}
                          <span
                            className="block h-full w-full overflow-hidden rounded"
                            style={{
                              background: `linear-gradient(135deg, ${item.swatchColor} 0 50%, ${item.swatchBg} 50% 100%)`,
                            }}
                            aria-hidden
                          />
                          {active ? (
                            <span
                              className="pointer-events-none absolute -right-1.5 -top-1.5 flex h-4 w-4 items-center justify-center rounded-full bg-primary text-on-primary shadow ring-1 ring-surface"
                              aria-hidden
                            >
                              <Check size={10} strokeWidth={3} />
                            </span>
                          ) : null}
                        </RDropdown.Item>
                      );
                    },
                  )}
                </div>
              </div>
            ))}
          </div>
        </RDropdown.Content>
      </RDropdown.Portal>
    </RDropdown.Root>
  );
}
