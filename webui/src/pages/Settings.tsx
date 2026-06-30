import { useState, useEffect, useCallback } from "react";
import { useTranslation } from "react-i18next";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useBlocker } from "react-router-dom";
import { Save, RotateCcw, RefreshCw } from "lucide-react";
import { settingsApi, modelCatalogApi } from "@/api/resources";
import type { Settings } from "@/api/types";
import {
  Alert,
  Button,
  Card,
  CardBody,
  CardHeader,
  ConfirmDialog,
  ErrorBox,
  Field,
  Input,
  PasswordInput,
  Select,
  Skeleton,
  Switch,
  useToast,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";

// Setting key constants — must match crates/store/src/settings_keys.rs
const KEYS = {
  retentionInterval: "gateway.retention.interval_secs",
  retentionDays: "gateway.retention.log_retention_days",
  epochPollInterval: "gateway.epoch_poll.interval_secs",
  tokenStatsInterval: "gateway.token_stats.interval_secs",
  tokenStatsLookback: "gateway.token_stats.lookback_days",
  sqliteMaintenanceEnabled: "gateway.sqlite_maintenance.enabled",
  sqliteMaintenanceInterval: "gateway.sqlite_maintenance.interval_secs",
  sqliteMaintenanceVacuumEnabled: "gateway.sqlite_maintenance.vacuum_enabled",
  sqliteMaintenanceMinFreelistPages:
    "gateway.sqlite_maintenance.min_freelist_pages",
  sqliteMaintenanceMinFreeRatioPercent:
    "gateway.sqlite_maintenance.min_free_ratio_percent",
  archiveEnabled: "gateway.archive.enabled",
  archiveS3Endpoint: "gateway.archive.s3_endpoint",
  archiveS3Region: "gateway.archive.s3_region",
  archiveS3Bucket: "gateway.archive.s3_bucket",
  archiveS3AccessKeyId: "gateway.archive.s3_access_key_id",
  archiveS3SecretAccessKey: "gateway.archive.s3_secret_access_key",
  archiveS3Prefix: "gateway.archive.s3_prefix",
  archiveS3ForcePathStyle: "gateway.archive.s3_force_path_style",
  archiveScanInterval: "gateway.archive.scan_interval_secs",
  archiveBatchSize: "gateway.archive.batch_size",
  archiveConcurrency: "gateway.archive.concurrency",
  archiveTimeout: "gateway.archive.timeout_secs",
  archiveMaxRetries: "gateway.archive.max_retries",
  routingStrategy: "gateway.routing.default_strategy",
  ingressMaxBody: "gateway.ingress.max_body_bytes",
  ingressMaxInflight: "gateway.ingress.max_inflight",
  ingressMaxQueueDepth: "gateway.ingress.max_queue_depth",
  ingressAcquireTimeout: "gateway.ingress.acquire_timeout_secs",
  ingressRawMedia: "gateway.ingress.raw_envelope_capture_media",
  ingressRequireApiKey: "gateway.ingress.require_api_key",
  upstreamIdleTimeout: "gateway.upstream.stream_idle_timeout_secs",
  upstreamTotalTimeout: "gateway.upstream.stream_total_timeout_secs",
  upstreamTcpKeepalive: "gateway.upstream.tcp_keepalive_secs",
  upstreamPoolIdleTimeout: "gateway.upstream.pool_idle_timeout_secs",
  upstreamTcpNodelay: "gateway.upstream.tcp_nodelay",
  forwardReqHeaderDeny: "gateway.forward.request_header_deny",
  forwardRespHeaderDeny: "gateway.forward.response_header_deny",
} as const;

const ENCRYPTED_KEYS: string[] = [
  KEYS.archiveS3AccessKeyId,
  KEYS.archiveS3SecretAccessKey,
];

/** Returns true when the value looks like a redacted encrypted blob. */
function isRedacted(value: string | undefined): boolean {
  return !!value && value.startsWith("[encrypted:");
}

export default function SettingsPage() {
  const { t } = useTranslation();
  const toast = useToast();
  const qc = useQueryClient();

  const { data, isLoading, error } = useQuery({
    queryKey: ["settings"],
    queryFn: () => settingsApi.list(),
  });

  // Local editable copy of the settings. Initialized from the server
  // data and updated as the user edits fields. We track which keys
  // have been touched so we only send changed values on save.
  const [local, setLocal] = useState<Settings>({});
  const [dirty, setDirty] = useState<Set<string>>(new Set());

  useEffect(() => {
    if (data?.settings) {
      setLocal({ ...data.settings });
      setDirty(new Set());
    }
  }, [data]);

  const updateField = useCallback((key: string, value: string) => {
    setLocal((prev) => {
      // For encrypted fields, if the user clears the field, don't
      // mark it dirty (leave unchanged on the server).
      if (ENCRYPTED_KEYS.includes(key) && value.trim() === "") {
        const next = { ...prev };
        delete next[key];
        return next;
      }
      return { ...prev, [key]: value };
    });
    setDirty((prev) => {
      const next = new Set(prev);
      // For encrypted fields, don't mark dirty if cleared (leave
      // unchanged). For redacted values, only mark dirty if the
      // user actually typed something new.
      if (ENCRYPTED_KEYS.includes(key) && value.trim() === "") {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  }, []);

  const saveMutation = useMutation({
    mutationFn: (changes: Settings) => settingsApi.update(changes),
    onSuccess: (resp) => {
      setLocal({ ...resp.settings });
      setDirty(new Set());
      toast.success(t("settings.saved"));
      qc.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (e: Error) => {
      toast.error(e.message);
    },
  });

  const handleSave = () => {
    const changes: Settings = {};
    for (const key of dirty) {
      if (local[key] !== undefined) {
        changes[key] = local[key];
      }
    }
    if (Object.keys(changes).length === 0) {
      toast.info(t("settings.noChanges"));
      return;
    }
    saveMutation.mutate(changes);
  };

  const handleReset = () => {
    if (data?.settings) {
      setLocal({ ...data.settings });
      setDirty(new Set());
    }
  };

  // --- Unsaved-changes navigation guard ---
  const hasDirty = dirty.size > 0;

  // Block react-router navigation when there are unsaved changes.
  const blocker = useBlocker(hasDirty);

  // Warn on tab close / page refresh.
  useEffect(() => {
    if (!hasDirty) return;
    const handler = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", handler);
    return () => window.removeEventListener("beforeunload", handler);
  }, [hasDirty]);

  const getField = (key: string, fallback = ""): string =>
    local[key] ?? fallback;

  const isSqliteDatabase = data?.database?.kind === "sqlite";

  if (isLoading) {
    return (
      <div className="space-y-6">
        {/* PageHeader skeleton */}
        <div className="mb-5 flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div className="space-y-2">
            <Skeleton className="h-7 w-48" />
            <Skeleton className="h-4 w-72" />
          </div>
          <div className="flex gap-2">
            <Skeleton className="h-9 w-20" />
            <Skeleton className="h-9 w-20" />
          </div>
        </div>

        {/* Card skeleton: 2-column grid of field placeholders */}
        {[
          { rows: 5, cols: true },
          { rows: 12, cols: true },
          { rows: 7, cols: true },
          { rows: 5, cols: true },
          { rows: 2, cols: false },
        ].map((section, i) => (
          <Card key={i}>
            <div className="border-b border-border px-4 py-3">
              <Skeleton className="h-4 w-40" />
            </div>
            <CardBody
              className={
                section.cols ? "grid gap-4 sm:grid-cols-2" : "grid gap-4"
              }
            >
              {Array.from({ length: section.rows }).map((_, j) => (
                <div key={j} className="space-y-2">
                  <Skeleton className="h-4 w-28" />
                  <Skeleton className="h-9 w-full" />
                </div>
              ))}
            </CardBody>
          </Card>
        ))}
      </div>
    );
  }

  if (error) {
    return <ErrorBox message={error.message} />;
  }

  const strategyOptions = [
    { value: "weighted", label: t("routes.strategyOptions.weighted") },
    { value: "priority", label: t("routes.strategyOptions.priority") },
    { value: "cooldown", label: t("routes.strategyOptions.cooldown") },
    { value: "latency", label: t("routes.strategyOptions.latency") },
  ];

  return (
    <div className="space-y-6">
      <PageHeader
        title={t("settings.title")}
        description={t("settings.description")}
        action={
          <div className="flex gap-2">
            <Button
              variant="secondary"
              onClick={handleReset}
              disabled={dirty.size === 0}
            >
              <RotateCcw className="mr-2 h-4 w-4" />
              {t("common.cancel")}
            </Button>
            <Button
              onClick={handleSave}
              disabled={dirty.size === 0 || saveMutation.isPending}
            >
              <Save className="mr-2 h-4 w-4" />
              {t("common.save")}
            </Button>
          </div>
        }
      />

      {/* Background tasks */}
      <Card>
        <CardHeader title={t("settings.backgroundTasks.title")} />
        <CardBody className="grid gap-4 sm:grid-cols-2">
          <Field
            label={t("settings.backgroundTasks.retentionDays")}
            hint={t("settings.backgroundTasks.retentionDaysHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.retentionDays, "30")}
              onChange={(e) => updateField(KEYS.retentionDays, e.target.value)}
            />
          </Field>
          <Field
            label={t("settings.backgroundTasks.retentionInterval")}
            hint={t("settings.backgroundTasks.retentionIntervalHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.retentionInterval, "3600")}
              onChange={(e) =>
                updateField(KEYS.retentionInterval, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.backgroundTasks.epochPollInterval")}
            hint={t("settings.backgroundTasks.epochPollIntervalHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.epochPollInterval, "2")}
              onChange={(e) =>
                updateField(KEYS.epochPollInterval, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.backgroundTasks.tokenStatsInterval")}
            hint={t("settings.backgroundTasks.tokenStatsIntervalHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.tokenStatsInterval, "300")}
              onChange={(e) =>
                updateField(KEYS.tokenStatsInterval, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.backgroundTasks.tokenStatsLookback")}
            hint={t("settings.backgroundTasks.tokenStatsLookbackHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.tokenStatsLookback, "400")}
              onChange={(e) =>
                updateField(KEYS.tokenStatsLookback, e.target.value)
              }
            />
          </Field>
        </CardBody>
      </Card>

      {/* SQLite Maintenance */}
      {isSqliteDatabase && (
        <Card>
          <CardHeader title={t("settings.sqliteMaintenance.title")} />
          <CardBody className="space-y-4">
            <p className="text-sm text-muted-foreground">
              {t("settings.sqliteMaintenance.description")}
            </p>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field
                label={t("settings.sqliteMaintenance.enabled")}
                hint={t("settings.sqliteMaintenance.enabledHint")}
              >
                <Switch
                  checked={
                    getField(KEYS.sqliteMaintenanceEnabled, "false") === "true"
                  }
                  onCheckedChange={(checked: boolean) =>
                    updateField(KEYS.sqliteMaintenanceEnabled, String(checked))
                  }
                />
              </Field>
              <Field
                label={t("settings.sqliteMaintenance.vacuumEnabled")}
                hint={t("settings.sqliteMaintenance.vacuumEnabledHint")}
              >
                <Switch
                  checked={
                    getField(KEYS.sqliteMaintenanceVacuumEnabled, "false") ===
                    "true"
                  }
                  onCheckedChange={(checked: boolean) =>
                    updateField(
                      KEYS.sqliteMaintenanceVacuumEnabled,
                      String(checked),
                    )
                  }
                />
              </Field>
              <Field
                label={t("settings.sqliteMaintenance.interval")}
                hint={t("settings.sqliteMaintenance.intervalHint")}
              >
                <Input
                  type="number"
                  value={getField(KEYS.sqliteMaintenanceInterval, "86400")}
                  onChange={(e) =>
                    updateField(KEYS.sqliteMaintenanceInterval, e.target.value)
                  }
                />
              </Field>
              <Field
                label={t("settings.sqliteMaintenance.minFreelistPages")}
                hint={t("settings.sqliteMaintenance.minFreelistPagesHint")}
              >
                <Input
                  type="number"
                  value={getField(KEYS.sqliteMaintenanceMinFreelistPages, "1024")}
                  onChange={(e) =>
                    updateField(
                      KEYS.sqliteMaintenanceMinFreelistPages,
                      e.target.value,
                    )
                  }
                />
              </Field>
              <Field
                label={t("settings.sqliteMaintenance.minFreeRatioPercent")}
                hint={t("settings.sqliteMaintenance.minFreeRatioPercentHint")}
              >
                <Input
                  type="number"
                  value={getField(
                    KEYS.sqliteMaintenanceMinFreeRatioPercent,
                    "20",
                  )}
                  onChange={(e) =>
                    updateField(
                      KEYS.sqliteMaintenanceMinFreeRatioPercent,
                      e.target.value,
                    )
                  }
                />
              </Field>
            </div>
            <Alert tone="warning">
              {t("settings.sqliteMaintenance.vacuumWarning")}
            </Alert>
          </CardBody>
        </Card>
      )}

      {/* Payload Archive */}
      <Card>
        <CardHeader title={t("settings.archive.title")} />
        <CardBody className="space-y-4">
          <Field
            label={t("settings.archive.enabled")}
            hint={t("settings.archive.enabledHint")}
          >
            <Switch
              checked={getField(KEYS.archiveEnabled, "false") === "true"}
              onCheckedChange={(checked: boolean) =>
                updateField(KEYS.archiveEnabled, String(checked))
              }
            />
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label={t("settings.archive.s3Endpoint")}>
              <Input
                value={getField(KEYS.archiveS3Endpoint)}
                placeholder="https://s3.example.com"
                onChange={(e) =>
                  updateField(KEYS.archiveS3Endpoint, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.s3Region")}>
              <Input
                value={getField(KEYS.archiveS3Region, "us-east-1")}
                onChange={(e) =>
                  updateField(KEYS.archiveS3Region, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.s3Bucket")}>
              <Input
                value={getField(KEYS.archiveS3Bucket)}
                onChange={(e) =>
                  updateField(KEYS.archiveS3Bucket, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.s3Prefix")}>
              <Input
                value={getField(KEYS.archiveS3Prefix)}
                onChange={(e) =>
                  updateField(KEYS.archiveS3Prefix, e.target.value)
                }
              />
            </Field>
            <Field
              label={t("settings.archive.s3AccessKeyId")}
              hint={
                isRedacted(getField(KEYS.archiveS3AccessKeyId))
                  ? t("settings.archive.encryptedHint")
                  : undefined
              }
            >
              <PasswordInput
                value={
                  isRedacted(getField(KEYS.archiveS3AccessKeyId))
                    ? ""
                    : getField(KEYS.archiveS3AccessKeyId)
                }
                placeholder={
                  isRedacted(getField(KEYS.archiveS3AccessKeyId))
                    ? t("settings.archive.leaveUnchanged")
                    : ""
                }
                onChange={(e) =>
                  updateField(KEYS.archiveS3AccessKeyId, e.target.value)
                }
              />
            </Field>
            <Field
              label={t("settings.archive.s3SecretAccessKey")}
              hint={
                isRedacted(getField(KEYS.archiveS3SecretAccessKey))
                  ? t("settings.archive.encryptedHint")
                  : undefined
              }
            >
              <PasswordInput
                value={
                  isRedacted(getField(KEYS.archiveS3SecretAccessKey))
                    ? ""
                    : getField(KEYS.archiveS3SecretAccessKey)
                }
                placeholder={
                  isRedacted(getField(KEYS.archiveS3SecretAccessKey))
                    ? t("settings.archive.leaveUnchanged")
                    : ""
                }
                onChange={(e) =>
                  updateField(KEYS.archiveS3SecretAccessKey, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.s3ForcePathStyle")}>
              <Switch
                checked={
                  getField(KEYS.archiveS3ForcePathStyle, "true") === "true"
                }
                onCheckedChange={(checked: boolean) =>
                  updateField(KEYS.archiveS3ForcePathStyle, String(checked))
                }
              />
            </Field>
            <Field label={t("settings.archive.scanInterval")}>
              <Input
                type="number"
                value={getField(KEYS.archiveScanInterval, "60")}
                onChange={(e) =>
                  updateField(KEYS.archiveScanInterval, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.batchSize")}>
              <Input
                type="number"
                value={getField(KEYS.archiveBatchSize, "100")}
                onChange={(e) =>
                  updateField(KEYS.archiveBatchSize, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.concurrency")}>
              <Input
                type="number"
                value={getField(KEYS.archiveConcurrency, "4")}
                onChange={(e) =>
                  updateField(KEYS.archiveConcurrency, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.timeout")}>
              <Input
                type="number"
                value={getField(KEYS.archiveTimeout, "30")}
                onChange={(e) =>
                  updateField(KEYS.archiveTimeout, e.target.value)
                }
              />
            </Field>
            <Field label={t("settings.archive.maxRetries")}>
              <Input
                type="number"
                value={getField(KEYS.archiveMaxRetries, "5")}
                onChange={(e) =>
                  updateField(KEYS.archiveMaxRetries, e.target.value)
                }
              />
            </Field>
          </div>
        </CardBody>
      </Card>

      {/* Routing & Ingress */}
      <Card>
        <CardHeader title={t("settings.routingIngress.title")} />
        <CardBody className="grid gap-4 sm:grid-cols-2">
          <Field label={t("settings.routingIngress.defaultStrategy")}>
            <Select
              value={getField(KEYS.routingStrategy, "weighted")}
              options={strategyOptions}
              onValueChange={(v) => updateField(KEYS.routingStrategy, v)}
            />
          </Field>
          <Field
            label={t("settings.routingIngress.maxBodyBytes")}
            hint={t("settings.routingIngress.maxBodyBytesHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.ingressMaxBody, "10485760")}
              onChange={(e) => updateField(KEYS.ingressMaxBody, e.target.value)}
            />
          </Field>
          <Field label={t("settings.routingIngress.maxInflight")}>
            <Input
              type="number"
              value={getField(KEYS.ingressMaxInflight, "128")}
              onChange={(e) =>
                updateField(KEYS.ingressMaxInflight, e.target.value)
              }
            />
          </Field>
          <Field label={t("settings.routingIngress.maxQueueDepth")}>
            <Input
              type="number"
              value={getField(KEYS.ingressMaxQueueDepth, "64")}
              onChange={(e) =>
                updateField(KEYS.ingressMaxQueueDepth, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.routingIngress.acquireTimeout")}
            hint={t("settings.routingIngress.secondsHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.ingressAcquireTimeout, "10")}
              onChange={(e) =>
                updateField(KEYS.ingressAcquireTimeout, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.routingIngress.rawEnvelopeCaptureMedia")}
            hint={t("settings.routingIngress.rawEnvelopeCaptureMediaHint")}
          >
            <Switch
              checked={getField(KEYS.ingressRawMedia, "false") === "true"}
              onCheckedChange={(checked: boolean) =>
                updateField(KEYS.ingressRawMedia, String(checked))
              }
            />
          </Field>
          <Field
            label={t("settings.routingIngress.requireApiKey")}
            hint={t("settings.routingIngress.requireApiKeyHint")}
          >
            <Switch
              checked={getField(KEYS.ingressRequireApiKey, "true") === "true"}
              onCheckedChange={(checked: boolean) =>
                updateField(KEYS.ingressRequireApiKey, String(checked))
              }
            />
          </Field>
        </CardBody>
      </Card>

      {/* Upstream */}
      <Card>
        <CardHeader title={t("settings.upstream.title")} />
        <CardBody className="grid gap-4 sm:grid-cols-2">
          <Field
            label={t("settings.upstream.streamIdleTimeout")}
            hint={t("settings.upstream.secondsHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.upstreamIdleTimeout, "120")}
              onChange={(e) =>
                updateField(KEYS.upstreamIdleTimeout, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.upstream.streamTotalTimeout")}
            hint={t("settings.upstream.secondsHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.upstreamTotalTimeout, "0")}
              onChange={(e) =>
                updateField(KEYS.upstreamTotalTimeout, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.upstream.tcpKeepalive")}
            hint={t("settings.upstream.secondsHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.upstreamTcpKeepalive, "0")}
              onChange={(e) =>
                updateField(KEYS.upstreamTcpKeepalive, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.upstream.poolIdleTimeout")}
            hint={t("settings.upstream.secondsHint")}
          >
            <Input
              type="number"
              value={getField(KEYS.upstreamPoolIdleTimeout, "0")}
              onChange={(e) =>
                updateField(KEYS.upstreamPoolIdleTimeout, e.target.value)
              }
            />
          </Field>
          <Field label={t("settings.upstream.tcpNodelay")}>
            <Switch
              checked={getField(KEYS.upstreamTcpNodelay, "true") === "true"}
              onCheckedChange={(checked: boolean) =>
                updateField(KEYS.upstreamTcpNodelay, String(checked))
              }
            />
          </Field>
        </CardBody>
      </Card>

      {/* Header forwarding */}
      <Card>
        <CardHeader title={t("settings.headerForward.title")} />
        <CardBody className="grid gap-4">
          <Field
            label={t("settings.headerForward.requestDeny")}
            hint={t("settings.headerForward.denyHint")}
          >
            <Input
              value={getField(KEYS.forwardReqHeaderDeny)}
              placeholder="X-Secret,Authorization"
              onChange={(e) =>
                updateField(KEYS.forwardReqHeaderDeny, e.target.value)
              }
            />
          </Field>
          <Field
            label={t("settings.headerForward.responseDeny")}
            hint={t("settings.headerForward.denyHint")}
          >
            <Input
              value={getField(KEYS.forwardRespHeaderDeny)}
              placeholder="X-Internal-Debug,Set-Cookie"
              onChange={(e) =>
                updateField(KEYS.forwardRespHeaderDeny, e.target.value)
              }
            />
          </Field>
        </CardBody>
      </Card>

      {/* Model catalog */}
      <ModelCatalogSection />

      {/* Unsaved-changes navigation confirmation */}
      <ConfirmDialog
        open={blocker.state === "blocked"}
        onOpenChange={(open) => {
          if (!open) blocker.reset?.();
        }}
        title={t("settings.unsavedTitle")}
        description={t("settings.unsavedDescription")}
        confirmLabel={t("settings.unsavedConfirm")}
        cancelLabel={t("settings.unsavedCancel")}
        onConfirm={() => blocker.proceed?.()}
      />
    </div>
  );
}

function ModelCatalogSection() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();

  const modelCatalog = useQuery({
    queryKey: ["model-catalog", "status"],
    queryFn: modelCatalogApi.status,
    staleTime: 5 * 60_000,
  });

  const refreshCatalog = useMutation({
    mutationFn: modelCatalogApi.refresh,
    onSuccess: () => {
      toast.success(
        t("settings.modelCatalogRefreshed", "Model catalog refreshed"),
      );
      qc.invalidateQueries({ queryKey: ["model-catalog", "status"] });
    },
    onError: (e: Error) => {
      toast.error(e.message);
    },
  });

  return (
    <Card>
      <CardHeader
        title={t("settings.modelCatalog", "Model catalog")}
        description={
          modelCatalog.data
            ? `${modelCatalog.data.provider_count} providers · ${modelCatalog.data.model_count} models`
            : t("settings.modelCatalogLoading", "Loading catalog status")
        }
        action={
          <Button
            variant="secondary"
            size="sm"
            onClick={() => refreshCatalog.mutate()}
            disabled={refreshCatalog.isPending}
          >
            <RefreshCw
              className={`h-4 w-4 ${refreshCatalog.isPending ? "animate-spin" : ""}`}
            />
            {t("common.refresh", "Refresh")}
          </Button>
        }
      />
      <CardBody className="grid gap-4 sm:grid-cols-3">
        <Field label={t("settings.catalogChecksum", "Checksum")}>
          <Input
            readOnly
            value={modelCatalog.isLoading ? "…" : modelCatalog.data?.checksum ?? "—"}
          />
        </Field>
        <Field label={t("settings.catalogGeneratedAt", "Generated at")}>
          <Input
            readOnly
            value={
              modelCatalog.data?.generated_at_unix
                ? new Date(
                    modelCatalog.data.generated_at_unix * 1000,
                  ).toLocaleString()
                : "—"
            }
          />
        </Field>
        <Field label={t("settings.catalogSource", "Source")}>
          <Input
            readOnly
            value={modelCatalog.data?.source ?? "models.dev"}
          />
        </Field>
      </CardBody>
    </Card>
  );
}
