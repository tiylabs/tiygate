import { useState, type FormEvent } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "react-router-dom";
import { probeToken, ApiError } from "@/api/client";
import { useAuth } from "./AuthContext";
import { Button, Card, ErrorBox, Field, PasswordInput, Switch } from "@/components/ui";
import { LanguageSwitcher } from "@/components/LanguageSwitcher";
import { cn } from "@/lib/cn";

export default function Login() {
  const { t } = useTranslation();
  const { login } = useAuth();
  const navigate = useNavigate();
  const [token, setToken] = useState("");
  const [remember, setRemember] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    if (!token.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await probeToken(token.trim());
      login(token.trim(), remember);
      navigate("/", { replace: true });
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
            <img
              src="./icon.svg"
              alt=""
              aria-hidden
              className="h-12 w-12"
            />
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
      </div>
    </div>
  );
}
