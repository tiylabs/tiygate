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
  | "light-lime"
  | "light-fuchsia"
  | "light-mauve"
  | "dark"
  | "dark-dim"
  | "dark-oled"
  | "dark-lime"
  | "dark-fuchsia"
  | "dark-mauve";

export interface ThemeMeta {
  id: Theme;
  mode: ThemeMode;
  labelKey: string;
  /** Preview swatch background (theme bg) and dot (theme primary). */
  swatchBg: string;
  swatchColor: string;
}

export const THEMES: ThemeMeta[] = [
  {
    id: "light",
    mode: "light",
    labelKey: "app.themeBlue",
    swatchBg: "#f7f8fa",
    swatchColor: "#2e5be6",
  },
  {
    id: "light-warm",
    mode: "light",
    labelKey: "app.themeAmber",
    swatchBg: "#faf7f2",
    swatchColor: "#b4530e",
  },
  {
    id: "light-slate",
    mode: "light",
    labelKey: "app.themeSlate",
    swatchBg: "#eef1f5",
    swatchColor: "#475569",
  },
  {
    id: "light-lime",
    mode: "light",
    labelKey: "app.themeLime",
    swatchBg: "#f6f9f0",
    swatchColor: "#4d7c0f",
  },
  {
    id: "light-fuchsia",
    mode: "light",
    labelKey: "app.themeFuchsia",
    swatchBg: "#fdf4fb",
    swatchColor: "#c026d3",
  },
  {
    id: "light-mauve",
    mode: "light",
    labelKey: "app.themeMauve",
    swatchBg: "#f6f3f7",
    swatchColor: "#8b5c9e",
  },
  {
    id: "dark",
    mode: "dark",
    labelKey: "app.themeBlue",
    swatchBg: "#0a0c14",
    swatchColor: "#84a6ff",
  },
  {
    id: "dark-dim",
    mode: "dark",
    labelKey: "app.themeAmber",
    swatchBg: "#1b1812",
    swatchColor: "#f0915a",
  },
  {
    id: "dark-oled",
    mode: "dark",
    labelKey: "app.themeSlate",
    swatchBg: "#000000",
    swatchColor: "#94a3b8",
  },
  {
    id: "dark-lime",
    mode: "dark",
    labelKey: "app.themeLime",
    swatchBg: "#0c120a",
    swatchColor: "#a3e635",
  },
  {
    id: "dark-fuchsia",
    mode: "dark",
    labelKey: "app.themeFuchsia",
    swatchBg: "#120a11",
    swatchColor: "#e879f9",
  },
  {
    id: "dark-mauve",
    mode: "dark",
    labelKey: "app.themeMauve",
    swatchBg: "#14111a",
    swatchColor: "#c4a7e0",
  },
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
