import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en";
import zh from "./locales/zh";

const STORAGE_KEY = "tiygate.lang";

const saved = window.localStorage.getItem(STORAGE_KEY);
const fallback = navigator.language.startsWith("zh") ? "zh" : "en";

void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
  },
  lng: saved ?? fallback,
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

export function setLanguage(lang: "en" | "zh"): void {
  window.localStorage.setItem(STORAGE_KEY, lang);
  void i18n.changeLanguage(lang);
}

export default i18n;
