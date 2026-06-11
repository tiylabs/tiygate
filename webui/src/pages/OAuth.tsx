import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery } from "@tanstack/react-query";
import { ExternalLink, RefreshCw, Play } from "lucide-react";
import { oauthApi, providersApi } from "@/api/resources";
import {
  Button,
  Card,
  CardHeader,
  ErrorBox,
  Field,
  Select,
  Spinner,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";

export default function OAuth() {
  const { t } = useTranslation();
  const { data: providers, isLoading } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });

  const [providerId, setProviderId] = useState("");
  const [authUrl, setAuthUrl] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const startMutation = useMutation({
    mutationFn: () => oauthApi.start(providerId),
    onSuccess: (res) => {
      setError(null);
      setAuthUrl(res.url);
      setMessage(t("oauth.started"));
    },
    onError: (e: Error) => {
      setError(e.message);
      setAuthUrl(null);
      setMessage(null);
    },
  });

  const refreshMutation = useMutation({
    mutationFn: () => oauthApi.refresh(providerId),
    onSuccess: (res) => {
      setError(null);
      setMessage(t("oauth.refreshed", { provider: res.provider_id }));
    },
    onError: (e: Error) => {
      setError(e.message);
      setMessage(null);
    },
  });

  const hasProviders = (providers ?? []).length > 0;

  return (
    <div>
      <PageHeader title={t("oauth.title")} />
      <Card className="max-w-xl">
        <CardHeader title={t("oauth.subtitle")} />
        <div className="space-y-4 p-4">
          {isLoading ? (
            <Spinner />
          ) : !hasProviders ? (
            <p className="text-sm text-slate-400">{t("oauth.noProviders")}</p>
          ) : (
            <>
              <Field label={t("oauth.selectProvider")}>
                <Select
                  value={providerId}
                  onChange={(e) => {
                    setProviderId(e.target.value);
                    setAuthUrl(null);
                    setMessage(null);
                    setError(null);
                  }}
                >
                  <option value="">—</option>
                  {(providers ?? []).map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name} ({p.id})
                    </option>
                  ))}
                </Select>
              </Field>

              <div className="flex gap-2">
                <Button
                  variant="primary"
                  disabled={!providerId || startMutation.isPending}
                  onClick={() => startMutation.mutate()}
                >
                  <Play size={16} />
                  {t("oauth.start")}
                </Button>
                <Button
                  disabled={!providerId || refreshMutation.isPending}
                  onClick={() => refreshMutation.mutate()}
                >
                  <RefreshCw size={16} />
                  {t("oauth.refresh")}
                </Button>
              </div>

              {error ? <ErrorBox message={error} /> : null}
              {message ? (
                <div className="rounded-md border border-green-200 bg-green-50 px-4 py-3 text-sm text-green-700">
                  {message}
                </div>
              ) : null}

              {authUrl ? (
                <Field label={t("oauth.authorizeUrl")}>
                  <div className="flex items-center gap-2">
                    <code className="flex-1 break-all rounded-md bg-slate-100 px-3 py-2 text-xs">
                      {authUrl}
                    </code>
                    <a href={authUrl} target="_blank" rel="noreferrer">
                      <Button variant="secondary">
                        <ExternalLink size={14} />
                        {t("oauth.openUrl")}
                      </Button>
                    </a>
                  </div>
                </Field>
              ) : null}
            </>
          )}
        </div>
      </Card>
    </div>
  );
}
