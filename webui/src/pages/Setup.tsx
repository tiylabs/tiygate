import { useEffect, useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { useAuth } from "@/auth/AuthContext";
import {
  tauriSetAdminToken,
  tauriEnablePasswordless,
  tauriGetMasterKey,
  tauriApplyMasterKey,
} from "@/auth/setup";
import {
  Alert,
  Button,
  Card,
  ErrorBox,
  Field,
  PasswordInput,
  Spinner,
} from "@/components/ui";
import { LanguageSwitcher } from "@/components/LanguageSwitcher";
import { cn } from "@/lib/cn";

type Mode = "choose" | "set-token" | "busy" | "show-master-key";

export default function Setup() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { login, setPasswordless } = useAuth();
  const [mode, setMode] = useState<Mode>("choose");
  const [token, setToken] = useState("");
  const [confirmToken, setConfirmToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [masterKey, setMasterKey] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  // Tracks the auto-generated token from passwordless mode so we can
  // auto-login after the master-key step without a page reload.
  const [passwordlessToken, setPasswordlessToken] = useState<string | null>(null);

  // Redirect to home if not in Tauri environment — the setup wizard is
  // only meaningful for the desktop client.
  useEffect(() => {
    const tauriInternals = "__TAURI_INTERNALS__" in window;
    if (!tauriInternals) {
      navigate("/", { replace: true });
    }
  }, [navigate]);

  async function handleSetToken(e: FormEvent) {
    e.preventDefault();
    if (!token.trim()) return;
    if (token !== confirmToken) {
      setError(t("setup.tokenMismatch"));
      return;
    }
    setMode("busy");
    setError(null);
    try {
      await tauriSetAdminToken(token.trim());
      await waitForSidecar(token.trim());
      // Fetch master key to show in the next step.
      const key = await tauriGetMasterKey();
      setMasterKey(key);
      setMode("show-master-key");
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(t("setup.restartFailed", { message: msg }));
      setMode("set-token");
    }
  }

  async function handlePasswordless() {
    setMode("busy");
    setError(null);
    try {
      const autoToken = await tauriEnablePasswordless();
      setPasswordlessToken(autoToken);
      // No sidecar restart needed — the token was already injected at
      // startup. Skip waitForSidecar and go straight to master key.
      const key = await tauriGetMasterKey();
      setMasterKey(key);
      setMode("show-master-key");
    } catch (err) {
      setError(t("setup.restartFailed", { message: String(err) }));
      setMode("choose");
    }
  }

  async function regenerateKey() {
    // Generate 32 random bytes on the frontend using Web Crypto API.
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    const hex = Array.from(buf)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    setMasterKey(hex);
    setCopied(false);
  }

  async function handleMasterKeyDone() {
    if (!masterKey) return;
    setMode("busy");
    setError(null);
    try {
      // Persist the master key and restart the sidecar so the new
      // TIYGATE_MASTER_KEY takes effect.
      await tauriApplyMasterKey(masterKey);
      if (passwordlessToken) {
        // Passwordless flow: wait for sidecar, then auto-login.
        await waitForSidecar(passwordlessToken);
        setPasswordless(true);
        login(passwordlessToken, true);
        navigate("/", { replace: true });
      } else {
        // Set-token flow: wait for sidecar, then go to login.
        await waitForSidecar(token.trim());
        navigate("/login", { replace: true });
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(t("setup.restartFailed", { message: msg }));
      setMode("show-master-key");
    }
  }

  async function copyMasterKey() {
    if (!masterKey) return;
    try {
      await navigator.clipboard.writeText(masterKey);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard not available — user can manually select & copy.
    }
  }

  if (mode === "busy") {
    return (
      <div className="flex min-h-full items-center justify-center bg-bg px-4 py-10">
        <div className="flex flex-col items-center gap-3">
          <Spinner />
          <p className="text-sm text-text-muted">
            {t("setup.starting")}
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-full items-center justify-center bg-bg px-4 py-10 sm:py-16">
      <div className="w-full max-w-[460px] animate-content-in">
        {/* Brand */}
        <div className="mb-7 flex flex-col items-center text-center">
          <span className="flex h-12 w-12 items-center justify-center overflow-hidden rounded-lg">
            <img src="./icon.svg" alt="" aria-hidden className="h-12 w-12" />
          </span>
          <h1 className="mt-3 text-lg font-semibold tracking-[-0.01em] text-text">
            {t("setup.title")}
          </h1>
          <p className="mt-1 text-sm text-text-muted">
            {t("setup.subtitle")}
          </p>
        </div>

        <Card className="p-6">
          <div className="mb-5 flex items-start justify-between gap-3">
            <div>
              <h2 className="text-base font-semibold text-text">
                {mode === "show-master-key"
                  ? t("setup.masterKeyTitle")
                  : t("setup.heading")}
              </h2>
              <p className="mt-1 text-sm text-text-muted">
                {mode === "show-master-key"
                  ? t("setup.masterKeySubtitle")
                  : t("setup.description")}
              </p>
            </div>
            <LanguageSwitcher className="shrink-0" />
          </div>

          {mode === "choose" && (
            <div className="space-y-3">
              <button
                type="button"
                onClick={() => {
                  setMode("set-token");
                  setError(null);
                }}
                className={cn(
                  "flex w-full items-start gap-3 rounded-md border border-border p-4 text-left",
                  "transition-colors hover:border-accent hover:bg-accent/5",
                  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                )}
              >
                <div className="flex-1">
                  <p className="font-medium text-text">
                    {t("setup.setTokenTitle")}
                  </p>
                  <p className="mt-1 text-sm text-text-muted">
                    {t("setup.setTokenDesc")}
                  </p>
                </div>
              </button>
              <button
                type="button"
                onClick={handlePasswordless}
                className={cn(
                  "flex w-full items-start gap-3 rounded-md border border-border p-4 text-left",
                  "transition-colors hover:border-accent hover:bg-accent/5",
                  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                )}
              >
                <div className="flex-1">
                  <p className="font-medium text-text">
                    {t("setup.passwordlessTitle")}
                  </p>
                  <p className="mt-1 text-sm text-text-muted">
                    {t("setup.passwordlessDesc")}
                  </p>
                </div>
              </button>
              {error ? <ErrorBox message={error} /> : null}
            </div>
          )}

          {mode === "set-token" && (
            <form className="space-y-4" onSubmit={handleSetToken}>
              <Field label={t("setup.tokenLabel")}>
                <PasswordInput
                  autoFocus
                  autoComplete="new-password"
                  value={token}
                  onChange={(e) => setToken(e.target.value)}
                  placeholder={t("setup.tokenPlaceholder")}
                  toggleLabel={t("setup.tokenLabel")}
                />
              </Field>
              <Field label={t("setup.confirmTokenLabel")}>
                <PasswordInput
                  autoComplete="new-password"
                  value={confirmToken}
                  onChange={(e) => setConfirmToken(e.target.value)}
                  placeholder={t("setup.confirmTokenPlaceholder")}
                  toggleLabel={t("setup.confirmTokenLabel")}
                />
              </Field>
              {error ? <ErrorBox message={error} /> : null}
              <div className="flex gap-2">
                <Button
                  type="button"
                  variant="secondary"
                  onClick={() => {
                    setMode("choose");
                    setError(null);
                  }}
                >
                  {t("common.cancel")}
                </Button>
                <Button
                  type="submit"
                  variant="primary"
                  className="flex-1"
                  disabled={!token.trim() || !confirmToken.trim()}
                >
                  {t("setup.confirm")}
                </Button>
              </div>
            </form>
          )}

          {mode === "show-master-key" && (
            <div className="space-y-4">
              <Alert tone="warning">
                {t("setup.masterKeyWarning")}
              </Alert>
              <div>
                <label className="mb-1 block text-sm font-medium text-text">
                  {t("setup.masterKeyLabel")}
                </label>
                <div className="flex items-center gap-2">
                  <code className="flex-1 overflow-hidden overflow-ellipsis whitespace-nowrap rounded-md border border-border bg-surface-muted px-3 py-2 text-xs text-text">
                    {masterKey}
                  </code>
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    onClick={regenerateKey}
                  >
                    {t("setup.masterKeyRegenerate")}
                  </Button>
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    onClick={copyMasterKey}
                  >
                    {copied ? t("common.copied") : t("common.copy")}
                  </Button>
                </div>
              </div>
              <Button
                type="button"
                variant="primary"
                className="w-full"
                onClick={handleMasterKeyDone}
              >
                {t("setup.masterKeyConfirm")}
              </Button>
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}

/**
 * Wait for the sidecar to become reachable after a restart. Polls the
 * `/healthz` endpoint directly (bypassing the API client) to avoid
 * depending on CORS or token resolution. The port is fetched from the
 * Tauri backend.
 */
async function waitForSidecar(token: string): Promise<void> {
  // Fetch the sidecar port via Tauri command.
  let port: number | null = null;
  try {
    const mod = await import("@tauri-apps/api/core");
    port = await mod.invoke<number>("get_server_port");
  } catch {
    // Not in Tauri — can't wait.
    return;
  }
  if (!port || port === 0) return;

  const healthUrl = `http://127.0.0.1:${port}/healthz`;
  const probeUrl = `http://127.0.0.1:${port}/admin/v1/audit?limit=1`;
  const maxAttempts = 50;
  const delayMs = 300;
  for (let i = 0; i < maxAttempts; i++) {
    // First check liveness (no auth, no CORS preflight on GET /healthz
    // in most WKWebView versions). Then probe with the token.
    try {
      const healthRes = await fetch(healthUrl);
      if (healthRes.ok) {
        // Sidecar is up — now verify the token works.
        try {
          const probeRes = await fetch(probeUrl, {
            headers: { Authorization: `Bearer ${token}` },
          });
          if (probeRes.ok) return;
        } catch {
          // Token probe failed (CORS?) but healthz passed — the
          // sidecar is running, so consider it ready.
          return;
        }
      }
    } catch {
      // Sidecar not yet listening — retry.
    }
    await new Promise((r) => setTimeout(r, delayMs));
  }
  throw new Error("sidecar did not become reachable");
}
