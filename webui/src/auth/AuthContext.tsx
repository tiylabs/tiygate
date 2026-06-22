import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type PropsWithChildren,
} from "react";
import { useQueryClient } from "@tanstack/react-query";
import { setUnauthorizedHandler, probeToken } from "@/api/client";
import { clearToken, getToken, setToken } from "./token";
import {
  isTauri,
  tauriGetAdminToken,
  checkIsFirstRun,
  tauriGetActiveInstance,
} from "./setup";

interface AuthState {
  token: string | null;
  isAuthenticated: boolean;
  /** Whether the app is running inside the Tauri desktop client. */
  isTauri: boolean;
  /**
   * `true` until the initial Tauri auto-login check completes. In
   * non-Tauri environments this is always `false`.
   */
  isInitializing: boolean;
  /** Whether the user is in passwordless mode (no logout button). */
  isPasswordless: boolean;
  setPasswordless: (v: boolean) => void;
  login: (token: string, remember: boolean) => void;
  logout: () => void;
}

const AuthContext = createContext<AuthState | null>(null);

export function AuthProvider({ children }: PropsWithChildren) {
  const tauri = isTauri();
  const [instanceKey, setInstanceKey] = useState<string>("");
  const [token, setTokenState] = useState<string | null>(() =>
    tauri ? null : getToken(""),
  );
  const [isInitializing, setIsInitializing] = useState(tauri);
  const [isPasswordless, setIsPasswordless] = useState(false);
  const queryClient = useQueryClient();

  const logout = useCallback(() => {
    clearToken(instanceKey);
    setTokenState(null);
    queryClient.clear();
  }, [queryClient, instanceKey]);

  const login = useCallback(
    (newToken: string, remember: boolean) => {
      setToken(newToken, remember, instanceKey);
      setTokenState(newToken);
    },
    [instanceKey],
  );

  // In Tauri environments, attempt to auto-login on mount:
  // - Always inspect the active instance first. A remote instance may be
  //   selected even while the local sidecar's first-run setup is still
  //   incomplete.
  // - Local sidecar with completed setup → fetch the stored token and
  //   auto-login (passwordless flow). Mark it so the logout button is hidden.
  // - Remote instance → check for a remembered per-instance token in
  //   localStorage. If found, auto-login; otherwise show the login page.
  useEffect(() => {
    if (!tauri) {
      setIsInitializing(false);
      return;
    }
    let cancelled = false;
    (async () => {
      try {
        const [firstRun, active] = await Promise.all([
          checkIsFirstRun(),
          tauriGetActiveInstance(),
        ]);
        const key = active?.id ?? "local";
        setInstanceKey(key);
        if (active?.kind === "local" && !firstRun) {
          // Local sidecar: use the Rust-side stored token for
          // passwordless auto-login.
          const storedToken = await tauriGetAdminToken();
          if (storedToken && !cancelled) {
            setToken(storedToken, true, key);
            setTokenState(storedToken);
            setIsPasswordless(true);
          }
        } else if (active?.kind === "remote") {
          // Remote instance: a remembered token must be verified
          // before entering the dashboard. This prevents stale tokens
          // from triggering many parallel dashboard requests and
          // freezing the remote instance due to failed attempts.
          const remembered = getToken(key);
          if (remembered && !cancelled) {
            try {
              await probeToken(remembered);
              if (!cancelled) setTokenState(remembered);
            } catch {
              clearToken(key);
              setTokenState(null);
              queryClient.clear();
            }
          } else {
            clearToken(key);
            setTokenState(null);
            queryClient.clear();
          }
        }
      } catch {
        // Degrade gracefully — user can use the login page manually.
      } finally {
        if (!cancelled) setIsInitializing(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [tauri, queryClient]);

  // Wire the API client's 401 handler so any rejected request drops
  // the session and bounces the user back to login.
  useEffect(() => {
    setUnauthorizedHandler(() => {
      clearToken(instanceKey);
      setTokenState(null);
      queryClient.clear();
    });
    return () => setUnauthorizedHandler(null);
  }, [queryClient, instanceKey]);

  const setPasswordless = useCallback((v: boolean) => setIsPasswordless(v), []);

  const value = useMemo<AuthState>(
    () => ({
      token,
      isAuthenticated: token !== null,
      isTauri: tauri,
      isInitializing,
      isPasswordless,
      setPasswordless,
      login,
      logout,
    }),
    [
      token,
      tauri,
      isInitializing,
      isPasswordless,
      setPasswordless,
      login,
      logout,
    ],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within AuthProvider");
  return ctx;
}
