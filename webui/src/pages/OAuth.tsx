import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery } from "@tanstack/react-query";
import { ExternalLink, Info, RefreshCw, Play, Copy } from "lucide-react";
import { oauthApi, providersApi } from "@/api/resources";
import {
  Button,
  Card,
  CardBody,
  CardHeader,
  EmptyState,
  ErrorBox,
  Field,
  Select,
  Spinner,
  Tooltip,
  Alert,
  useToast,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";

export default function OAuth() {
  const { t } = useTranslation();
  const toast = useToast();
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
      toast.success(t("oauth.refreshed", { provider: res.provider_id }));
    },
    onError: (e: Error) => {
      setError(e.message);
      setMessage(null);
    },
  });

  async function copyUrl() {
    if (!authUrl) return;
    try {
      await navigator.clipboard.writeText(authUrl);
      toast.success(t("oauth.urlCopied"));
    } catch {
      toast.error(t("common.copyFailed"));
    }
  }

  const hasProviders = (providers ?? []).length > 0;
  const providerOptions = [
    { value: "", label: t("oauth.selectPlaceholder") },
    ...(providers ?? []).map((p) => ({
      value: p.id,
      label: `${p.name} (${p.id})`,
    })),
  ];

  return (
    <div>
      <PageHeader title={t("oauth.title")} />
      <Card className="max-w-xl">
        <CardHeader title={t("oauth.subtitle")} />
        {isLoading ? (
          <Spinner />
        ) : !hasProviders ? (
          <EmptyState title={t("oauth.noProviders")} />
        ) : (
          <CardBody className="space-y-4">
            <Field
              label={
                <span className="inline-flex items-center gap-1.5">
                  {t("oauth.selectProvider")}
                  <Tooltip content={t("oauth.requirements")} side="top">
                    <button
                      type="button"
                      aria-label={t("oauth.requirements")}
                      className="inline-flex h-5 w-5 items-center justify-center rounded text-text-muted transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-primary/40"
                    >
                      <Info size={14} />
                    </button>
                  </Tooltip>
                </span>
              }
            >
              <Select
                value={providerId}
                onValueChange={(v) => {
                  setProviderId(v);
                  setAuthUrl(null);
                  setMessage(null);
                  setError(null);
                }}
                ariaLabel={t("oauth.selectProvider")}
                placeholder={t("oauth.selectPlaceholder")}
                options={providerOptions}
              />
            </Field>

            <div className="flex flex-wrap gap-2">
              <Button
                variant="primary"
                icon={<Play size={16} />}
                disabled={!providerId}
                loading={startMutation.isPending}
                onClick={() => startMutation.mutate()}
              >
                {t("oauth.start")}
              </Button>
              <Button
                variant="secondary"
                icon={<RefreshCw size={16} />}
                disabled={!providerId}
                loading={refreshMutation.isPending}
                onClick={() => refreshMutation.mutate()}
              >
                {t("oauth.refresh")}
              </Button>
            </div>

            {error ? <ErrorBox message={error} /> : null}
            {message ? <Alert tone="success">{message}</Alert> : null}

            {authUrl ? (
              <Field label={t("oauth.authorizeUrl")}>
                <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
                  <code className="min-w-0 flex-1 break-all rounded-md bg-surface-muted px-3 py-2 font-mono text-xs text-text">
                    {authUrl}
                  </code>
                  <div className="flex gap-2">
                    <Button
                      variant="secondary"
                      icon={<Copy size={14} />}
                      onClick={copyUrl}
                    >
                      {t("oauth.copyUrl")}
                    </Button>
                    <a href={authUrl} target="_blank" rel="noreferrer">
                      <Button variant="accent" icon={<ExternalLink size={14} />}>
                        {t("oauth.openUrl")}
                      </Button>
                    </a>
                  </div>
                </div>
              </Field>
            ) : null}
          </CardBody>
        )}
      </Card>
    </div>
  );
}
