import { useTranslation } from "react-i18next";
import { setLanguage } from "@/i18n";
import { cn } from "@/lib/cn";

export function LanguageSwitcher() {
  const { i18n } = useTranslation();
  const current = i18n.language.startsWith("zh") ? "zh" : "en";
  const itemClass = (active: boolean) =>
    cn(
      "rounded px-1.5 py-0.5 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
      active ? "font-semibold text-text" : "text-text-subtle hover:text-text",
    );
  return (
    <div className="flex items-center gap-1 text-xs">
      <button
        type="button"
        className={itemClass(current === "en")}
        onClick={() => setLanguage("en")}
      >
        EN
      </button>
      <span className="text-border">/</span>
      <button
        type="button"
        className={itemClass(current === "zh")}
        onClick={() => setLanguage("zh")}
      >
        中文
      </button>
    </div>
  );
}
