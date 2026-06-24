import { useEffect, useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { probeToken, fetchServerInfo, ApiError } from "@/api/client";
import { useAuth } from "./AuthContext";
import { shouldShowLocalSetup } from "./setup";
import {
  Button,
  Card,
  ErrorBox,
  Field,
  PasswordInput,
  Switch,
} from "@/components/ui";
import { LanguageSwitcher } from "@/components/LanguageSwitcher";
import { InstanceIndicator } from "@/components/InstanceIndicator";
import { BootScreen } from "@/components/BootScreen";
import { cn } from "@/lib/cn";

export default function Login() {
  const { t } = useTranslation();
  const { login, isTauri } = useAuth();
  const navigate = useNavigate();
  const [token, setToken] = useState("");
  const [remember, setRemember] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [version, setVersion] = useState<string | null>(null);
  // In Tauri mode, suppress rendering until the first-run check completes
  // to avoid flashing the login page before redirecting to /setup.
  const [tauriCheckDone, setTauriCheckDone] = useState(!isTauri);

  useEffect(() => {
    // In Tauri mode, redirect to setup only when the local sidecar is the
    // active instance and still needs first-run setup. Remote instances must
    // be allowed to show the token login page.
    if (isTauri) {
      shouldShowLocalSetup()
        .then((needsSetup) => {
          if (needsSetup) {
            navigate("/setup", { replace: true });
          } else {
            setTauriCheckDone(true);
          }
        })
        .catch(() => setTauriCheckDone(true));
    }
    fetchServerInfo().then((info) => {
      if (info) setVersion(info.version);
    });
  }, [isTauri, navigate]);

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    if (!token.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await probeToken(token.trim());
      login(token.trim(), remember);
      // In Tauri the SPA root is `/`; in browser it's `/admin/ui/`.
      const tauriInternals = "__TAURI_INTERNALS__" in window;
      window.location.replace(tauriInternals ? "/" : "/admin/ui/");
    } catch (err) {
      if (err instanceof ApiError) {
        if (err.status === 401) setError(t("login.invalid"));
        else if (err.status === 503) setError(t("login.unconfigured"));
        else setError(t("login.error", { message: err.message }));
      } else {
        setError(t("login.error", { message: String(err) }));
      }
    } finally {
      setBusy(false);
    }
  }

  if (isTauri && !tauriCheckDone) {
    return <BootScreen />;
  }

  return (
    <div className="flex min-h-full items-center justify-center bg-bg px-4 py-10 sm:py-16">
      <div className="w-full max-w-[400px] animate-content-in">
        {/* Brand: logo + product name + tagline */}
        <div className="mb-7 flex flex-col items-center text-center">
          <span
            className={cn(
              "flex h-12 w-12 items-center justify-center overflow-hidden rounded-lg",
            )}
          >
            <img src="./icon.svg" alt="" aria-hidden className="h-12 w-12" />
          </span>
          <h1 className="mt-3 text-lg font-semibold tracking-[-0.01em] text-text">
            {t("app.title")}
          </h1>
          <p className="mt-1 text-sm text-text-muted">
            {t("login.brandTagline")}
          </p>
        </div>

        <Card className="p-6">
          <div className="mb-5 flex items-start justify-between gap-3">
            <div>
              <h2 className="text-base font-semibold text-text">
                {t("login.title")}
              </h2>
              <p className="mt-1 text-sm text-text-muted">
                {t("login.subtitle")}
              </p>
            </div>
            <LanguageSwitcher />
          </div>
          <form className="space-y-4" onSubmit={onSubmit}>
            <Field label={t("login.tokenLabel")}>
              <PasswordInput
                autoFocus
                autoComplete="off"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder={t("login.tokenPlaceholder")}
                toggleLabel={t("login.tokenLabel")}
              />
            </Field>
            <Switch
              checked={remember}
              onCheckedChange={setRemember}
              label={t("login.remember")}
            />
            {error ? <ErrorBox message={error} /> : null}
            <Button
              type="submit"
              variant="primary"
              className="w-full"
              loading={busy}
              disabled={!token.trim()}
            >
              {busy ? t("login.verifying") : t("login.submit")}
            </Button>
          </form>
        </Card>

        {(isTauri || version) && (
          <div className="mt-6 flex items-center justify-between gap-3">
            {isTauri ? (
              <InstanceIndicator className="max-w-[280px]" />
            ) : (
              <span />
            )}
            {version ? (
              <p className="shrink-0 text-xs text-text-muted">v{version}</p>
            ) : null}
          </div>
        )}
      </div>
    </div>
  );
}
