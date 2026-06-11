import { useTranslation } from "react-i18next";
import { setLanguage } from "@/i18n";

export function LanguageSwitcher() {
  const { i18n } = useTranslation();
  const current = i18n.language.startsWith("zh") ? "zh" : "en";
  return (
    <div className="flex items-center gap-1 text-xs">
      <button
        className={current === "en" ? "font-semibold text-slate-900" : "text-slate-400"}
        onClick={() => setLanguage("en")}
      >
        EN
      </button>
      <span className="text-slate-300">/</span>
      <button
        className={current === "zh" ? "font-semibold text-slate-900" : "text-slate-400"}
        onClick={() => setLanguage("zh")}
      >
        中文
      </button>
    </div>
  );
}
