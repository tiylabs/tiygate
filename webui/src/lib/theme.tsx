import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type PropsWithChildren,
} from "react";

export type ThemeMode = "light" | "dark";

export type Theme =
  | "light"
  | "light-warm"
  | "light-slate"
  | "dark"
  | "dark-dim"
  | "dark-oled";

export interface ThemeMeta {
  id: Theme;
  mode: ThemeMode;
  labelKey: string;
}

export const THEMES: ThemeMeta[] = [
  { id: "light", mode: "light", labelKey: "app.themeLightDefault" },
  { id: "light-warm", mode: "light", labelKey: "app.themeLightWarm" },
  { id: "light-slate", mode: "light", labelKey: "app.themeLightSlate" },
  { id: "dark", mode: "dark", labelKey: "app.themeDarkDefault" },
  { id: "dark-dim", mode: "dark", labelKey: "app.themeDarkDim" },
  { id: "dark-oled", mode: "dark", labelKey: "app.themeDarkOled" },
];

const DEFAULT_LIGHT: Theme = "light";
const DEFAULT_DARK: Theme = "dark";

const STORAGE_KEY = "tiygate.theme";

function isValidTheme(value: string | null): value is Theme {
  return THEMES.some((t) => t.id === value);
}

function modeOf(theme: Theme): ThemeMode {
  return THEMES.find((t) => t.id === theme)?.mode ?? "light";
}

interface ThemeState {
  theme: Theme;
  setTheme: (theme: Theme) => void;
  toggleTheme: () => void;
}

const ThemeContext = createContext<ThemeState | null>(null);

function readInitialTheme(): Theme {
  const saved = window.localStorage.getItem(STORAGE_KEY);
  if (isValidTheme(saved)) return saved;
  const prefersDark = window.matchMedia(
    "(prefers-color-scheme: dark)",
  ).matches;
  return prefersDark ? DEFAULT_DARK : DEFAULT_LIGHT;
}

function applyTheme(theme: Theme): void {
  document.documentElement.setAttribute("data-theme", theme);
}

export function ThemeProvider({ children }: PropsWithChildren) {
  const [theme, setThemeState] = useState<Theme>(() => readInitialTheme());

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  const setTheme = useCallback((next: Theme) => {
    if (!isValidTheme(next)) return;
    window.localStorage.setItem(STORAGE_KEY, next);
    setThemeState(next);
  }, []);

  const toggleTheme = useCallback(() => {
    setThemeState((prev) => {
      const next = modeOf(prev) === "dark" ? DEFAULT_LIGHT : DEFAULT_DARK;
      window.localStorage.setItem(STORAGE_KEY, next);
      return next;
    });
  }, []);

  const value = useMemo<ThemeState>(
    () => ({ theme, setTheme, toggleTheme }),
    [theme, setTheme, toggleTheme],
  );

  return (
    <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
  );
}

export function useTheme(): ThemeState {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error("useTheme must be used within ThemeProvider");
  return ctx;
}
