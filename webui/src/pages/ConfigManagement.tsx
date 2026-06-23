import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery } from "@tanstack/react-query";
import {
  Download,
  Upload,
  FileJson,
  Info,
  AlertTriangle,
  CheckCircle2,
} from "lucide-react";
import {
  configApi,
  providersApi,
  routesApi,
  apiKeysApi,
  settingsApi,
  statsApi,
} from "@/api/resources";
import { isTauri } from "@/auth/setup";
import type {
  ConfigExport,
  ExportSetting,
  ExportTokenDailyStat,
  ImportReport,
  ImportSelection,
} from "@/api/types";
import {
  Alert,
  Badge,
  Button,
  Card,
  CardBody,
  CardHeader,
  ErrorBox,
  Field,
  PasswordInput,
  useToast,
} from "@/components/ui";
import { PageHeader } from "@/components/PageHeader";

type ScopeKey = "providers" | "routes" | "api_keys" | "settings" | "token_stats";

const ALL_SCOPES: ScopeKey[] = [
  "providers",
  "routes",
  "api_keys",
  "settings",
  "token_stats",
];

/** A single item shown in the restore preview list. */
interface PreviewItem {
  /** Stable identifier within its category (provider/route id or setting key). */
  id: string;
  /** Human-readable label (name / virtual_model / key). */
  label: string;
  /** Secondary info shown next to the label. */
  sub: string;
  /** Whether this id/key already exists in the current instance. */
  exists: boolean;
  /** Whether the value is an encrypted blob (settings only). */
  encrypted: boolean;
}

export default function ConfigManagement() {
  const { t } = useTranslation();
  const toast = useToast();

  // --- Export state ---
  const [exportScope, setExportScope] = useState<Record<ScopeKey, boolean>>({
    providers: true,
    routes: true,
    api_keys: true,
    settings: true,
    token_stats: true,
  });

  const exportMutation = useMutation({
    mutationFn: async () => {
      const bundle = await configApi.export();
      // Filter the bundle by the selected scopes before downloading.
      const filtered: ConfigExport = {
        ...bundle,
        providers: exportScope.providers ? bundle.providers : [],
        routes: exportScope.routes ? bundle.routes : [],
        api_keys: exportScope.api_keys ? bundle.api_keys : [],
        settings: exportScope.settings ? (bundle.settings ?? []) : [],
        token_daily_stats: exportScope.token_stats
          ? (bundle.token_daily_stats ?? [])
          : [],
      };
      const json = JSON.stringify(filtered, null, 2);
      const ts = new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-");
      const filename = `tiygate-backup-${ts}.json`;
      if (isTauri()) {
        // Tauri's macOS WKWebView does not support the `<a download>`
        // + blob URL pattern (NSURLErrorCancelled). Route the save
        // through a native file dialog via the Rust backend instead.
        const mod = await import("@tauri-apps/api/core");
        const saved = await mod.invoke<string | null>(
          "save_backup_file",
          { filename, contents: json },
        );
        return saved !== null;
      }
      const blob = new Blob([json], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = filename;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
      return true;
    },
    onSuccess: (saved) => {
      if (saved) {
        toast.success(t("backup.exportSuccess"));
      }
      // If saved is false the user cancelled the Tauri save dialog.
    },
    onError: (e: Error) => {
      toast.error(e.message);
    },
  });

  // --- Restore state ---
  const [masterKey, setMasterKey] = useState("");
  const [fileName, setFileName] = useState<string | null>(null);
  const [parsedBackup, setParsedBackup] = useState<ConfigExport | null>(null);
  const [parseError, setParseError] = useState<string | null>(null);
  const [restoreResult, setRestoreResult] = useState<ImportReport | null>(null);
  // Per-item checkbox state, keyed by `"<scope>:<id>"`.
  const [checked, setChecked] = useState<Record<string, boolean>>({});

  // Fetch the current instance's existing ids/keys so we can mark
  // existing items and default them to unchecked. This only runs
  // when a backup file has been parsed.
  const existingQuery = useQuery({
    queryKey: ["config-restore-existing"],
    queryFn: async () => {
      const [providers, routesResp, apiKeys, settingsResp, tokenActivity] =
        await Promise.all([
          providersApi.list(),
          routesApi.list({ limit: 500 }),
          apiKeysApi.list(),
          settingsApi.list(),
          statsApi.tokenActivity(730),
        ]);
      return {
        providerIds: new Set(providers.map((p) => p.id)),
        routeIds: new Set(routesResp.entries.map((r) => r.id)),
        apiKeyIds: new Set(apiKeys.map((k) => k.id)),
        settingKeys: new Set(Object.keys(settingsResp.settings)),
        tokenStatsDays: new Set(tokenActivity.days.map((d) => d.day)),
      };
    },
    enabled: parsedBackup !== null,
  });

  // Build the preview items grouped by category. Recomputed when the
  // parsed backup or the existing-id set changes.
  const preview = useMemo(() => {
    if (!parsedBackup) return null;
    const existing = existingQuery.data;
    const buildItems = (
      entries: {
        id: string;
        label: string;
        sub: string;
        encrypted?: boolean;
      }[],
      existingIds: Set<string> | undefined,
    ): PreviewItem[] =>
      entries.map((e) => ({
        id: e.id,
        label: e.label,
        sub: e.sub,
        encrypted: e.encrypted ?? false,
        exists: existingIds?.has(e.id) ?? false,
      }));
    return {
      providers: buildItems(
        parsedBackup.providers.map((p) => ({
          id: p.id,
          label: p.name,
          sub: `${p.vendor} · ${p.api_base}`,
        })),
        existing?.providerIds,
      ),
      routes: buildItems(
        parsedBackup.routes.map((r) => ({
          id: r.id,
          label: r.virtual_model,
          sub: t("backup.routeTargets", { count: r.targets.length }),
        })),
        existing?.routeIds,
      ),
      api_keys: buildItems(
        parsedBackup.api_keys.map((k) => ({
          id: k.id,
          label: k.name,
          sub: k.status,
        })),
        existing?.apiKeyIds,
      ),
      settings: buildItems(
        (parsedBackup.settings ?? []).map((s: ExportSetting) => ({
          id: s.key,
          label: s.key,
          sub: s.encrypted ? t("backup.encryptedSetting") : s.value,
          encrypted: s.encrypted,
        })),
        existing?.settingKeys,
      ),
      token_stats: buildItems(
        (parsedBackup.token_daily_stats ?? []).map(
          (s: ExportTokenDailyStat) => ({
            id: s.day,
            label: s.day,
            sub: t("backup.tokenStatsDay", {
              tokens: s.total_tokens,
              requests: s.request_count,
            }),
          }),
        ),
        existing?.tokenStatsDays,
      ),
    };
  }, [parsedBackup, existingQuery.data, t]);

  // Initialize checkbox defaults when preview becomes available:
  // existing items default unchecked, new items default checked.
  // Runs as an effect (not during render) so StrictMode double-invocation
  // and re-renders do not silently overwrite the user's manual toggles.
  const previewReady = parsedBackup !== null && existingQuery.isSuccess;
  useEffect(() => {
    if (!previewReady || !preview) return;
    const next: Record<string, boolean> = {};
    for (const scope of ALL_SCOPES) {
      for (const item of preview[scope]) {
        // Token stats use additive merge (sum / MAX), which is
        // non-destructive. Default-check all days, including those
        // that already exist, since re-importing accumulates rather
        // than overwrites.
        if (scope === "token_stats") {
          next[`${scope}:${item.id}`] = true;
        } else {
          next[`${scope}:${item.id}`] = !item.exists;
        }
      }
    }
    setChecked(next);
    // Seed exactly once per parsed backup + existing-id snapshot.
  }, [previewReady, preview]);

  function toggleItem(scope: ScopeKey, id: string) {
    const key = `${scope}:${id}`;
    setChecked((prev) => ({ ...prev, [key]: !prev[key] }));
  }

  function toggleAll(scope: ScopeKey, value: boolean) {
    if (!preview) return;
    setChecked((prev) => {
      const next = { ...prev };
      for (const item of preview[scope]) {
        next[`${scope}:${item.id}`] = value;
      }
      return next;
    });
  }

  function handleFileChange(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    if (!file) {
      setParsedBackup(null);
      setFileName(null);
      setParseError(null);
      setChecked({});
      return;
    }
    setFileName(file.name);
    setParseError(null);
    setRestoreResult(null);
    setChecked({});
    const reader = new FileReader();
    reader.onload = () => {
      try {
        const text = String(reader.result);
        const parsed = JSON.parse(text) as ConfigExport;
        if (
          typeof parsed.schema_version !== "number" ||
          !Array.isArray(parsed.providers) ||
          !Array.isArray(parsed.routes) ||
          !Array.isArray(parsed.api_keys)
        ) {
          setParseError(t("backup.invalidFormat"));
          setParsedBackup(null);
          return;
        }
        // Normalize optional settings to an array.
        if (!Array.isArray(parsed.settings)) {
          parsed.settings = [];
        }
        // Normalize optional token_daily_stats to an array.
        if (!Array.isArray(parsed.token_daily_stats)) {
          parsed.token_daily_stats = [];
        }
        setParsedBackup(parsed);
      } catch {
        setParseError(t("backup.invalidFormat"));
        setParsedBackup(null);
      }
    };
    reader.onerror = () => {
      setParseError(t("backup.invalidFormat"));
      setParsedBackup(null);
    };
    reader.readAsText(file);
  }

  function buildSelection(): ImportSelection {
    if (!preview) {
      return {
        providers: [],
        routes: [],
        api_keys: [],
        settings: [],
        token_stats: [],
      };
    }
    const sel: ImportSelection = {
      providers: [],
      routes: [],
      api_keys: [],
      settings: [],
      token_stats: [],
    };
    for (const scope of ALL_SCOPES) {
      for (const item of preview[scope]) {
        if (checked[`${scope}:${item.id}`]) {
          sel[scope].push(item.id);
        }
      }
    }
    return sel;
  }

  const hasAnyChecked = Object.values(checked).some(Boolean);

  const restoreMutation = useMutation({
    mutationFn: () => {
      if (!parsedBackup) {
        throw new Error(t("backup.noFile"));
      }
      const selection = buildSelection();
      return configApi.import(masterKey, parsedBackup, selection);
    },
    onSuccess: (report) => {
      setRestoreResult(report);
      toast.success(t("backup.importSuccess"));
      setParsedBackup(null);
      setFileName(null);
      setMasterKey("");
      setChecked({});
    },
    onError: (e: Error) => {
      toast.error(e.message);
    },
  });

  function renderPreviewGroup(
    scope: ScopeKey,
    title: string,
    items: PreviewItem[],
  ) {
    if (items.length === 0) return null;
    const checkedCount = items.filter(
      (i) => checked[`${scope}:${i.id}`],
    ).length;
    return (
      <div className="space-y-1.5">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <label className="flex cursor-pointer items-center gap-1.5 text-sm font-medium text-text">
              <input
                type="checkbox"
                className="h-4 w-4 rounded border-border-strong accent-primary"
                checked={checkedCount === items.length}
                ref={(el) => {
                  if (el)
                    el.indeterminate =
                      checkedCount > 0 && checkedCount < items.length;
                }}
                onChange={(e) => toggleAll(scope, e.target.checked)}
              />
              {title}
            </label>
            <Badge tone="neutral">{items.length}</Badge>
          </div>
          <span className="text-xs text-text-muted">
            {t("backup.selectedCount", {
              selected: checkedCount,
              total: items.length,
            })}
          </span>
        </div>
        <div className="max-h-48 space-y-1 overflow-y-auto rounded-sm border border-border bg-surface-muted">
          {items.map((item) => {
            const key = `${scope}:${item.id}`;
            const isChecked = checked[key] ?? false;
            return (
              <label
                key={key}
                className="flex cursor-pointer items-start gap-2 px-3 py-1.5 text-xs hover:bg-surface"
              >
                <input
                  type="checkbox"
                  className="mt-0.5 h-4 w-4 rounded border-border-strong accent-primary"
                  checked={isChecked}
                  onChange={() => toggleItem(scope, item.id)}
                />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="truncate font-medium text-text">
                      {item.label}
                    </span>
                    {item.exists &&
                      (scope === "token_stats" ? (
                        <Badge tone="info">
                          {t("backup.mergeBadge")}
                        </Badge>
                      ) : (
                        <Badge tone={isChecked ? "warning" : "neutral"}>
                          {isChecked
                            ? t("backup.overwriteBadge")
                            : t("backup.existsBadge")}
                        </Badge>
                      ))}
                    {item.encrypted && (
                      <Badge tone="info">{t("backup.encryptedBadge")}</Badge>
                    )}
                  </div>
                  <div className="truncate text-text-muted">{item.sub}</div>
                </div>
              </label>
            );
          })}
        </div>
      </div>
    );
  }

  return (
    <div>
      <PageHeader
        title={t("backup.title")}
        description={t("backup.subtitle")}
      />

      <div className="grid gap-5 lg:grid-cols-2">
        {/* Export */}
        <Card>
          <CardHeader title={t("backup.exportTitle")} />
          <CardBody className="space-y-4">
            <p className="text-sm text-text-muted">{t("backup.exportDesc")}</p>
            <Alert tone="info">
              <div className="flex items-start gap-2">
                <Info size={16} className="mt-0.5 shrink-0" />
                <span>{t("backup.exportNote")}</span>
              </div>
            </Alert>
            <Field label={t("backup.exportScope")}>
              <div className="flex flex-wrap gap-4">
                {ALL_SCOPES.map((scope) => (
                  <label
                    key={scope}
                    className="flex cursor-pointer items-center gap-2 text-sm text-text"
                  >
                    <input
                      type="checkbox"
                      className="h-4 w-4 rounded border-border-strong accent-primary"
                      checked={exportScope[scope]}
                      onChange={(e) =>
                        setExportScope((prev) => ({
                          ...prev,
                          [scope]: e.target.checked,
                        }))
                      }
                    />
                    {t(`backup.scope.${scope}`)}
                  </label>
                ))}
              </div>
            </Field>
            <Button
              variant="primary"
              onClick={() => exportMutation.mutate()}
              disabled={
                exportMutation.isPending ||
                !Object.values(exportScope).some(Boolean)
              }
            >
              <Download size={16} />
              {exportMutation.isPending
                ? t("common.loading")
                : t("backup.exportBtn")}
            </Button>
          </CardBody>
        </Card>

        {/* Restore */}
        <Card>
          <CardHeader title={t("backup.importTitle")} />
          <CardBody className="space-y-4">
            <p className="text-sm text-text-muted">{t("backup.importDesc")}</p>

            <Field label={t("backup.selectFile")}>
              <label className="flex cursor-pointer items-center gap-2 rounded-sm border border-dashed border-border-strong bg-surface px-3 py-2 text-sm text-text-muted transition-colors hover:border-primary hover:text-text">
                <FileJson size={16} />
                <span className="truncate">
                  {fileName ?? t("backup.noFileSelected")}
                </span>
                <input
                  type="file"
                  accept="application/json,.json"
                  className="hidden"
                  onChange={handleFileChange}
                />
              </label>
            </Field>

            {parseError && <ErrorBox message={parseError} />}

            {parsedBackup && (
              <div className="rounded-sm bg-surface-muted px-3 py-2 text-xs text-text-muted">
                {t("backup.fileSummary", {
                  providers: parsedBackup.providers.length,
                  routes: parsedBackup.routes.length,
                  apiKeys: parsedBackup.api_keys.length,
                  settings: parsedBackup.settings?.length ?? 0,
                  tokenStats: parsedBackup.token_daily_stats?.length ?? 0,
                  encrypted: parsedBackup.encrypted
                    ? t("common.yes")
                    : t("common.no"),
                })}
              </div>
            )}

            {/* Preview panel */}
            {parsedBackup && existingQuery.isLoading && (
              <p className="text-xs text-text-muted">
                {t("backup.loadingPreview")}
              </p>
            )}
            {parsedBackup && existingQuery.isError && (
              <ErrorBox message={t("backup.previewLoadError")} />
            )}
            {parsedBackup && preview && existingQuery.isSuccess && (
              <div className="space-y-3">
                <div className="flex items-start gap-2 rounded-sm border border-border bg-surface px-3 py-2">
                  <AlertTriangle
                    size={14}
                    className="mt-0.5 shrink-0 text-warning"
                  />
                  <p className="text-xs text-text-muted">
                    {t("backup.previewHint")}
                  </p>
                </div>
                {preview.token_stats.length > 0 && (
                  <div className="flex items-start gap-2 rounded-sm border border-info/30 bg-info/5 px-3 py-2">
                    <Info
                      size={14}
                      className="mt-0.5 shrink-0 text-info"
                    />
                    <p className="text-xs text-text-muted">
                      {t("backup.tokenStatsMergeHint")}
                    </p>
                  </div>
                )}
                {renderPreviewGroup(
                  "providers",
                  t("backup.scope.providers"),
                  preview.providers,
                )}
                {renderPreviewGroup(
                  "routes",
                  t("backup.scope.routes"),
                  preview.routes,
                )}
                {renderPreviewGroup(
                  "api_keys",
                  t("backup.scope.api_keys"),
                  preview.api_keys,
                )}
                {renderPreviewGroup(
                  "settings",
                  t("backup.scope.settings"),
                  preview.settings,
                )}
                {renderPreviewGroup(
                  "token_stats",
                  t("backup.scope.token_stats"),
                  preview.token_stats,
                )}
              </div>
            )}

            <Field
              label={t("backup.masterKey")}
              hint={t("backup.masterKeyHint")}
            >
              <PasswordInput
                value={masterKey}
                onChange={(e) => setMasterKey(e.target.value)}
                placeholder="TIYGATE_MASTER_KEY"
                toggleLabel={t("backup.masterKey")}
                autoComplete="off"
              />
            </Field>

            <Button
              variant="primary"
              onClick={() => restoreMutation.mutate()}
              disabled={
                restoreMutation.isPending ||
                !parsedBackup ||
                !hasAnyChecked ||
                (parsedBackup?.encrypted && !masterKey)
              }
            >
              <Upload size={16} />
              {restoreMutation.isPending
                ? t("common.loading")
                : t("backup.importBtn")}
            </Button>

            {restoreResult && (
              <div className="space-y-2 rounded-sm border border-border bg-surface-muted px-3 py-2">
                <div className="flex items-center gap-2">
                  <CheckCircle2 size={16} className="text-success" />
                  <p className="text-sm font-medium text-text">
                    {t("backup.importResultTitle")}
                  </p>
                </div>
                <ul className="space-y-1 text-xs text-text-muted">
                  <li>
                    {t("backup.providersResult", {
                      imported: restoreResult.providers_imported,
                      skipped: restoreResult.providers_skipped,
                    })}
                  </li>
                  <li>
                    {t("backup.routesResult", {
                      imported: restoreResult.routes_imported,
                      skipped: restoreResult.routes_skipped,
                    })}
                  </li>
                  <li>
                    {t("backup.apiKeysResult", {
                      imported: restoreResult.api_keys_imported,
                      skipped: restoreResult.api_keys_skipped,
                    })}
                  </li>
                  <li>
                    {t("backup.settingsResult", {
                      imported: restoreResult.settings_imported,
                      skipped: restoreResult.settings_skipped,
                    })}
                  </li>
                  <li>
                    {t("backup.tokenStatsResult", {
                      imported: restoreResult.token_stats_imported,
                      skipped: restoreResult.token_stats_skipped,
                    })}
                  </li>
                </ul>
              </div>
            )}
          </CardBody>
        </Card>
      </div>
    </div>
  );
}
