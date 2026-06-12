import { useTranslation } from "react-i18next";
import * as RDropdown from "@radix-ui/react-dropdown-menu";
import { Check, Palette } from "lucide-react";
import { THEMES, useTheme, type ThemeMode } from "@/lib/theme";
import { cn } from "@/lib/cn";

const contentClass =
  "z-40 min-w-[12rem] overflow-hidden rounded-md border border-border bg-surface p-1 shadow-md " +
  "data-[state=open]:animate-overlay-in";

const itemClass =
  "flex cursor-pointer items-center justify-between gap-3 rounded-sm px-2.5 py-1.5 text-sm text-text outline-none " +
  "transition-colors duration-[var(--duration-fast)] focus:bg-surface-muted data-[highlighted]:bg-surface-muted";

const groupLabelClass =
  "px-2.5 pb-1 pt-1.5 text-[11px] font-medium uppercase tracking-[0.04em] text-text-subtle";

export function ThemeSwitcher() {
  const { t } = useTranslation();
  const { theme, setTheme } = useTheme();

  const groups: Array<{ mode: ThemeMode; labelKey: string }> = [
    { mode: "light", labelKey: "app.themeGroupLight" },
    { mode: "dark", labelKey: "app.themeGroupDark" },
  ];

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
          {groups.map((group, gi) => (
            <RDropdown.Group key={group.mode}>
              {gi > 0 ? (
                <RDropdown.Separator className="my-1 h-px bg-border" />
              ) : null}
              <RDropdown.Label className={groupLabelClass}>
                {t(group.labelKey)}
              </RDropdown.Label>
              {THEMES.filter((item) => item.mode === group.mode).map((item) => {
                const active = item.id === theme;
                return (
                  <RDropdown.Item
                    key={item.id}
                    onSelect={() => setTheme(item.id)}
                    className={cn(itemClass, active && "font-medium")}
                  >
                    <span>{t(item.labelKey)}</span>
                    {active ? (
                      <Check size={14} className="text-primary" aria-hidden />
                    ) : null}
                  </RDropdown.Item>
                );
              })}
            </RDropdown.Group>
          ))}
        </RDropdown.Content>
      </RDropdown.Portal>
    </RDropdown.Root>
  );
}
