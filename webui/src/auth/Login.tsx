import { useState, type FormEvent } from "react";
import { useTranslation } from "react-i18next";
import { useNavigate } from "react-router-dom";
import { probeToken, ApiError } from "@/api/client";
import { useAuth } from "./AuthContext";
import { Button, Card, ErrorBox, Field, Input } from "@/components/ui";
import { LanguageSwitcher } from "@/components/LanguageSwitcher";

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
    <div className="flex min-h-full items-center justify-center bg-slate-50 px-4">
      <div className="w-full max-w-sm">
        <div className="mb-6 flex items-center justify-between">
          <h1 className="text-lg font-semibold text-slate-800">
            {t("app.title")}
          </h1>
          <LanguageSwitcher />
        </div>
        <Card className="p-6">
          <h2 className="text-base font-semibold text-slate-800">
            {t("login.title")}
          </h2>
          <p className="mt-1 text-sm text-slate-500">{t("login.subtitle")}</p>
          <form className="mt-5 space-y-4" onSubmit={onSubmit}>
            <Field label={t("login.tokenLabel")}>
              <Input
                type="password"
                autoFocus
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder={t("login.tokenPlaceholder")}
              />
            </Field>
            <label className="flex items-center gap-2 text-sm text-slate-600">
              <input
                type="checkbox"
                checked={remember}
                onChange={(e) => setRemember(e.target.checked)}
              />
              {t("login.remember")}
            </label>
            {error ? <ErrorBox message={error} /> : null}
            <Button
              type="submit"
              variant="primary"
              className="w-full"
              disabled={busy || !token.trim()}
            >
              {busy ? t("login.verifying") : t("login.submit")}
            </Button>
          </form>
        </Card>
      </div>
    </div>
  );
}
