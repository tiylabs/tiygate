import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw, ShieldCheck, Trash2 } from "lucide-react";
import { useTranslation } from "react-i18next";

import {
  capabilitiesApi,
  providersApi,
  routesApi,
  settingsApi,
} from "@/api/resources";
import type {
  CapabilityProbeRunDetail,
  CapabilityRequirement,
  CapabilityState,
} from "@/api/types";
import {
  Badge,
  Button,
  Card,
  CardBody,
  CardHeader,
  Drawer,
  ErrorBox,
  Field,
  Input,
  JsonViewer,
  Metric,
  Select,
  Table,
  TableSkeleton,
  Textarea,
  Td,
  Th,
  Tr,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { Pagination } from "@/components/Pagination";

const DEFAULT_TARGET_PAGE_SIZE = 10;
const TARGET_PAGE_SIZE_OPTIONS = [10, 20, 50] as const;
const DEFAULT_PROBE_JOB_PAGE_SIZE = 10;
const PROBE_JOB_PAGE_SIZE_OPTIONS = [10, 20, 50] as const;
const PROFILE_PANEL_CLASS =
  "flex min-h-0 flex-col overflow-hidden xl:max-h-[calc(100vh-10rem)]";

const stateOptions = [
  { value: "supported", label: "Supported" },
  { value: "unsupported", label: "Unsupported" },
  { value: "constrained", label: "Constrained" },
];

function constrainedOverrideValue(
  valueKind: string | undefined,
  setValue: string,
  rangeMin: string,
  rangeMax: string,
  booleanValue: string,
  opaqueValue: string,
): unknown {
  if (!valueKind) throw new Error("Select a registered capability before adding a constraint.");
  if (["enum_set", "string_set", "schema_keyword_set"].includes(valueKind)) {
    const values = setValue.split(",").map((value) => value.trim()).filter(Boolean);
    if (values.length === 0) throw new Error("Enter at least one allowed value.");
    return { kind: valueKind, value: [...new Set(values)] };
  }
  if (valueKind === "integer_range" || valueKind === "decimal_range") {
    if (!rangeMin.trim() && !rangeMax.trim()) throw new Error("Enter a minimum or maximum value.");
    const parse = (raw: string): number | null => {
      if (!raw.trim()) return null;
      const parsed = Number(raw);
      if (!Number.isFinite(parsed)) throw new Error("Range bounds must be valid numbers.");
      if (valueKind === "integer_range" && !Number.isSafeInteger(parsed)) {
        throw new Error("Integer range bounds must be safe integers.");
      }
      return parsed;
    };
    const min = parse(rangeMin);
    const max = parse(rangeMax);
    if (min !== null && max !== null && min > max) {
      throw new Error("Minimum cannot be greater than maximum.");
    }
    return { kind: valueKind, value: { min, max } };
  }
  if (valueKind === "bool") {
    return { kind: "bool", value: booleanValue === "true" };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(opaqueValue);
  } catch (error) {
    throw new Error(`Enter valid JSON: ${(error as Error).message}`);
  }
  return { kind: "opaque", value: parsed };
}

function statusTone(status: string): "success" | "warning" | "danger" | "neutral" {
  if (status === "ready" || status === "complete") return "success";
  if (status === "stale" || status === "partial" || status === "pending") {
    return "warning";
  }
  if (status === "running") return "warning";
  if (status === "error" || status === "failed") return "danger";
  return "neutral";
}

function probeStatusTone(status: string): "success" | "warning" | "danger" | "neutral" {
  if (status === "complete") return "success";
  if (status === "failed") return "danger";
  if (status === "pending" || status === "running" || status === "partial") {
    return "warning";
  }
  return "neutral";
}

function capabilityTone(state: string): "success" | "danger" | "warning" | "neutral" {
  if (state === "supported") return "success";
  if (state === "unsupported") return "danger";
  if (state === "constrained") return "warning";
  return "neutral";
}

function jsonText(value: unknown): string {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function ProbeRunDetailView({ detail }: { detail: CapabilityProbeRunDetail }) {
  const { t, i18n } = useTranslation();
  const exchanges = detail.details?.exchanges ?? [];
  const judgment = detail.details?.judgment;
  const probeLabel = (group: string, value: string) => {
    const key = `capabilities.${group}.${value}`;
    return i18n.exists(key) ? t(key) : value;
  };
  return (
    <div className="mt-2 space-y-2 rounded border border-primary/30 bg-primary-soft/20 p-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-text-muted">
        <span className="font-medium text-text">{t("capabilities.probeRunDetail")}</span>
        <span>{t("capabilities.probeRunOutcome")}: {probeLabel("probeOutcomes", detail.outcome)}</span>
        <span>{t("capabilities.probeRunTime")}: {fmtTime(detail.ts)}</span>
      </div>
      {judgment ? (
        <section>
          <div className="mb-1 font-medium text-text">{t("capabilities.probeJudgment")}</div>
          <JsonViewer value={jsonText(judgment)} className="max-h-80" />
        </section>
      ) : null}
      {exchanges.length === 0 ? (
        <div className="text-text-muted">{t("capabilities.probeExchangesEmpty")}</div>
      ) : (
        <section className="space-y-2">
          <div className="font-medium text-text">{t("capabilities.probeExchanges")}</div>
          {exchanges.map((exchange, index) => (
            <div key={`${exchange.request_path}:${index}`} className="rounded border border-border bg-surface px-2 py-2">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-text-muted">
                <span className="font-medium text-text">{t("capabilities.probeExchange", { index: index + 1 })}</span>
                <code>{exchange.request_path}</code>
                {exchange.response_status !== undefined && exchange.response_status !== null ? (
                  <span>{t("capabilities.probeResponseStatus")}: {exchange.response_status}</span>
                ) : null}
                {exchange.response_content_type ? <span>{exchange.response_content_type}</span> : null}
              </div>
              <div className="mt-2 text-text-subtle">{t("capabilities.probeRequestHeaders")}</div>
              <JsonViewer value={jsonText(exchange.request_headers)} className="mt-1" />
              <div className="mt-2 text-text-subtle">{t("capabilities.probeRequest")}</div>
              <JsonViewer value={jsonText(exchange.request_body)} className="mt-1" />
              {exchange.response_body ? (
                <>
                  <div className="mt-2 text-text-subtle">{t("capabilities.probeResponse")}</div>
                  <JsonViewer value={exchange.response_body} className="mt-1" />
                </>
              ) : null}
              {exchange.error ? (
                <div className="mt-2 rounded border border-danger/30 bg-danger/5 px-2 py-1 text-danger">
                  {t("capabilities.probeExchangeError")}: {exchange.error}
                </div>
              ) : null}
            </div>
          ))}
        </section>
      )}
      {detail.details?.truncated ? (
        <div className="text-warning">{t("capabilities.probeDetailTruncated")}</div>
      ) : null}
    </div>
  );
}

export default function CapabilitiesPage() {
  const { t, i18n } = useTranslation();
  const toast = useToast();
  const qc = useQueryClient();
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [targetPageSize, setTargetPageSize] = useState(DEFAULT_TARGET_PAGE_SIZE);
  const [targetOffset, setTargetOffset] = useState(0);
  const [probeJobPageSize, setProbeJobPageSize] = useState(DEFAULT_PROBE_JOB_PAGE_SIZE);
  const [probeJobOffset, setProbeJobOffset] = useState(0);
  const [expandedProbeJobId, setExpandedProbeJobId] = useState<string | null>(null);
  const [selectedProbeRunId, setSelectedProbeRunId] = useState<string | null>(null);
  const [overrideCapability, setOverrideCapability] = useState("");
  const [overrideState, setOverrideState] = useState<CapabilityState>("supported");
  const [overrideReason, setOverrideReason] = useState("");
  const [overrideExpiresAt, setOverrideExpiresAt] = useState("");
  const [overrideSetValue, setOverrideSetValue] = useState("");
  const [overrideRangeMin, setOverrideRangeMin] = useState("");
  const [overrideRangeMax, setOverrideRangeMax] = useState("");
  const [overrideBooleanValue, setOverrideBooleanValue] = useState("true");
  const [overrideOpaqueValue, setOverrideOpaqueValue] = useState("");
  const [overrideValueError, setOverrideValueError] = useState<string | null>(null);
  const [selectedRouteId, setSelectedRouteId] = useState("");
  const [admissionShape, setAdmissionShape] = useState("");
  const [admissionCapabilities, setAdmissionCapabilities] = useState("");
  const [admissionRequirementsJson, setAdmissionRequirementsJson] = useState("");
  const [admissionMode, setAdmissionMode] = useState<"shadow" | "enforce">("shadow");
  const [admissionReason, setAdmissionReason] = useState("");
  const [lowTrafficException, setLowTrafficException] = useState(false);

  const profilesQuery = useQuery({
    queryKey: ["target-capabilities", targetPageSize, targetOffset],
    queryFn: () =>
      capabilitiesApi.list({ limit: targetPageSize, offset: targetOffset }),
  });
  const detailQuery = useQuery({
    queryKey: ["target-capability", selectedKey],
    queryFn: () => capabilitiesApi.get(selectedKey ?? ""),
    enabled: selectedKey !== null,
    refetchInterval: selectedKey !== null ? 3000 : false,
  });
  const probeJobsQuery = useQuery({
    queryKey: ["target-capability-probe-jobs", selectedKey, probeJobPageSize, probeJobOffset],
    queryFn: () =>
      capabilitiesApi.probeJobs(selectedKey ?? "", {
        limit: probeJobPageSize,
        offset: probeJobOffset,
      }),
    enabled: selectedKey !== null,
    refetchInterval: selectedKey !== null ? 3000 : false,
  });
  const probeJobRunsQuery = useQuery({
    queryKey: ["target-capability-probe-job-runs", selectedKey, expandedProbeJobId],
    queryFn: () =>
      capabilitiesApi.probeJobRuns(selectedKey ?? "", expandedProbeJobId ?? "", {
        limit: 50,
      }),
    enabled: selectedKey !== null && expandedProbeJobId !== null,
    refetchInterval: selectedKey !== null && expandedProbeJobId !== null ? 3000 : false,
  });
  const probeRunDetailQuery = useQuery({
    queryKey: ["target-capability-probe-run", selectedKey, selectedProbeRunId],
    queryFn: () => capabilitiesApi.probeRun(selectedKey ?? "", selectedProbeRunId ?? ""),
    enabled: selectedKey !== null && selectedProbeRunId !== null,
  });
  const registryQuery = useQuery({
    queryKey: ["capability-registry"],
    queryFn: () => capabilitiesApi.registryAll(),
  });
  const routesQuery = useQuery({
    queryKey: ["routes", "capability-admissions"],
    queryFn: () => routesApi.listAll(),
  });
  const providersQuery = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });
  const settingsQuery = useQuery({
    queryKey: ["settings", "capability-probes"],
    queryFn: () => settingsApi.list(),
  });
  const admissionsQuery = useQuery({
    queryKey: ["capability-admissions", selectedRouteId],
    queryFn: () => capabilitiesApi.admissionsAll(selectedRouteId),
    enabled: selectedRouteId !== "",
  });
  const metricsQuery = useQuery({
    queryKey: ["capability-metrics", selectedRouteId],
    queryFn: () => capabilitiesApi.metricsAll({ route_id: selectedRouteId }),
    enabled: selectedRouteId !== "",
  });
  const currentAdmissions = admissionsQuery.data?.entries ?? [];
  const enforceAvailable = currentAdmissions.length > 0 && !admissionsQuery.error;

  const probeMutation = useMutation({
    mutationFn: (targetKey: string) => capabilitiesApi.probe(targetKey),
    onSuccess: (_job, targetKey) => {
      toast.success(t("capabilities.probeQueued"));
      void qc.invalidateQueries({ queryKey: ["target-capabilities"] });
      void qc.invalidateQueries({ queryKey: ["target-capability", targetKey] });
      void qc.invalidateQueries({ queryKey: ["target-capability-probe-jobs", targetKey] });
    },
    onError: (error: Error) => toast.error(t("capabilities.probeFailed"), error.message),
  });

  const overrideMutation = useMutation({
    mutationFn: () => {
      if (!selectedKey) throw new Error(t("capabilities.selectTarget"));
      const descriptor = (registryQuery.data?.entries ?? registryQuery.data?.items ?? []).find(
        (entry) => entry.id === overrideCapability.trim(),
      );
      let value: unknown;
      if (overrideState === "constrained") {
        try {
          value = constrainedOverrideValue(
            descriptor?.value_kind,
            overrideSetValue,
            overrideRangeMin,
            overrideRangeMax,
            overrideBooleanValue,
            overrideOpaqueValue,
          );
          setOverrideValueError(null);
        } catch (error) {
          const message = (error as Error).message;
          setOverrideValueError(message);
          throw new Error(message);
        }
      }
      return capabilitiesApi.override(selectedKey, {
        capability_id: overrideCapability.trim(),
        state: overrideState,
        value,
        reason: overrideReason.trim(),
        expires_at: overrideExpiresAt
          ? new Date(overrideExpiresAt).toISOString()
          : null,
      });
    },
    onSuccess: () => {
      setOverrideCapability("");
      setOverrideReason("");
      setOverrideExpiresAt("");
      setOverrideSetValue("");
      setOverrideRangeMin("");
      setOverrideRangeMax("");
      setOverrideOpaqueValue("");
      setOverrideValueError(null);
      toast.success(t("capabilities.overrideSaved"));
      void qc.invalidateQueries({ queryKey: ["target-capabilities"] });
      void qc.invalidateQueries({ queryKey: ["target-capability", selectedKey] });
    },
    onError: (error: Error) => toast.error(t("capabilities.overrideFailed"), error.message),
  });

  const removeOverrideMutation = useMutation({
    mutationFn: (capabilityId: string) =>
      capabilitiesApi.removeOverride(selectedKey ?? "", capabilityId),
    onSuccess: () => {
      toast.success(t("capabilities.overrideRemoved"));
      void qc.invalidateQueries({ queryKey: ["target-capabilities"] });
      void qc.invalidateQueries({ queryKey: ["target-capability", selectedKey] });
    },
    onError: (error: Error) => toast.error(t("capabilities.overrideFailed"), error.message),
  });

  const admissionMutation = useMutation({
    mutationFn: () => {
      if (!selectedRouteId) throw new Error(t("capabilities.selectRoute"));
      const required = admissionCapabilities
        .split(",")
        .map((value) => value.trim())
        .filter(Boolean);
      if (required.length === 0 || !admissionReason.trim()) {
        throw new Error(t("capabilities.admissionReasonPlaceholder"));
      }
      if (admissionMode === "enforce" && !enforceAvailable) {
        throw new Error(t("capabilities.enforceUnavailable"));
      }
      let required_requirements: CapabilityRequirement[] | undefined;
      if (admissionRequirementsJson.trim()) {
        try {
          const parsed: unknown = JSON.parse(admissionRequirementsJson);
          if (!Array.isArray(parsed)) throw new Error("must be a JSON array");
          required_requirements = parsed as CapabilityRequirement[];
        } catch (error) {
          throw new Error(`required_requirements JSON: ${(error as Error).message}`);
        }
      }
      const existing = admissions.find(
        (admission) =>
          (admissionShape.trim() && admission.capability_shape_hash === admissionShape.trim()) ||
          (!admissionShape.trim() &&
            !admissionRequirementsJson.trim() &&
            admission.required_capabilities.slice().sort().join(",") === required.slice().sort().join(",")),
      );
      return capabilitiesApi.upsertAdmission(selectedRouteId, {
        shape_hash: admissionShape.trim() || existing?.capability_shape_hash,
        required_capabilities: required,
        required_requirements,
        mode: admissionMode,
        expected_revision: existing?.revision,
        low_traffic_exception: lowTrafficException,
        reason: admissionReason.trim(),
      });
    },
    onSuccess: () => {
      setAdmissionReason("");
      setAdmissionRequirementsJson("");
      setLowTrafficException(false);
      toast.success(t("capabilities.admissionSaved"));
      void qc.invalidateQueries({ queryKey: ["capability-admissions", selectedRouteId] });
    },
    onError: (error: Error) => toast.error(t("capabilities.admissionFailed"), error.message),
  });

  const removeAdmissionMutation = useMutation({
    mutationFn: (input: { shapeHash: string; revision: number }) =>
      capabilitiesApi.removeAdmission(selectedRouteId, input.shapeHash, input.revision),
    onSuccess: () => {
      toast.success(t("capabilities.admissionRemoved"));
      void qc.invalidateQueries({ queryKey: ["capability-admissions", selectedRouteId] });
    },
    onError: (error: Error) => toast.error(t("capabilities.admissionFailed"), error.message),
  });

  const probeWorkerMutation = useMutation({
    mutationFn: (input: { enabled: boolean; reason: string }) =>
      capabilitiesApi.setProbeWorker(input.enabled, input.reason),
    onSuccess: () => {
      toast.success(t("capabilities.probeWorkerUpdated"));
      void settingsQuery.refetch();
    },
    onError: (error: Error) => toast.error(t("capabilities.probeWorkerFailed"), error.message),
  });

  const profiles = profilesQuery.data?.entries ?? [];
  const targetTotal = profilesQuery.data?.total ?? 0;
  const targetPage = Math.floor(targetOffset / targetPageSize) + 1;
  const targetPageCount =
    targetTotal === 0 ? 1 : Math.ceil(targetTotal / targetPageSize);
  const probeJobTotal = probeJobsQuery.data?.total ?? 0;
  const probeJobPage = Math.floor(probeJobOffset / probeJobPageSize) + 1;
  const probeJobPageCount =
    probeJobTotal === 0 ? 1 : Math.ceil(probeJobTotal / probeJobPageSize);
  const routes = routesQuery.data?.entries ?? [];
  const providerNameById = useMemo(
    () =>
      new Map(
        (providersQuery.data ?? []).map((provider) => [provider.id, provider.name]),
      ),
    [providersQuery.data],
  );
  const selectedSummary = profiles.find(
    (profile) => profile.target_key === selectedKey,
  );
  const registryEntries = registryQuery.data?.entries ?? registryQuery.data?.items ?? [];
  const overrideDescriptor = registryEntries.find(
    (descriptor) => descriptor.id === overrideCapability.trim(),
  );
  const admissions = currentAdmissions;
  const metrics = metricsQuery.data?.entries ?? [];
  const probeWorkerEnabled =
    settingsQuery.data?.settings["gateway.capabilities.probe_enabled"] !== "false";
  const detail = detailQuery.data;
  const probeJobs = probeJobsQuery.data?.entries ?? probeJobsQuery.data?.items ?? [];
  const probeJobRuns =
    probeJobRunsQuery.data?.entries ?? probeJobRunsQuery.data?.items ?? [];
  const probeRunDetail = probeRunDetailQuery.data;
  const entries = useMemo(
    () => Object.entries(detail?.profile.resolved_capabilities ?? {}),
    [detail],
  );
  const inconclusiveObservations = useMemo(
    () =>
      (detail?.profile.observations ?? []).filter(
        (observation) => observation.reason_code === "inconclusive",
      ),
    [detail],
  );

  function providerName(providerId: string) {
    return providerNameById.get(providerId) ?? providerId;
  }

  useEffect(() => {
    setExpandedProbeJobId(null);
    setSelectedProbeRunId(null);
    setProbeJobOffset(0);
  }, [selectedKey]);

  function registryLabel(group: string, value: string) {
    const key = `capabilities.registry.${group}.${value.replace(/\./g, "__")}`;
    return i18n.exists(key) ? t(key) : value;
  }

  function probeLabel(group: string, value: string) {
    const key = `capabilities.${group}.${value}`;
    return i18n.exists(key) ? t(key) : value;
  }

  function changeTargetPage(next: number) {
    const clamped = Math.max(1, Math.min(targetPageCount, next));
    setTargetOffset((clamped - 1) * targetPageSize);
    setSelectedKey(null);
  }

  function changeTargetPageSize(next: number) {
    setTargetPageSize(next);
    setTargetOffset(0);
    setSelectedKey(null);
  }

  function changeProbeJobPage(next: number) {
    const clamped = Math.max(1, Math.min(probeJobPageCount, next));
    setProbeJobOffset((clamped - 1) * probeJobPageSize);
    setExpandedProbeJobId(null);
    setSelectedProbeRunId(null);
  }

  function changeProbeJobPageSize(next: number) {
    setProbeJobPageSize(next);
    setProbeJobOffset(0);
    setExpandedProbeJobId(null);
    setSelectedProbeRunId(null);
  }

  return (
    <div>
      <PageHeader
        title={t("capabilities.title")}
        description={t("capabilities.description")}
      />
      {profilesQuery.error ? (
        <ErrorBox
          message={(profilesQuery.error as Error).message}
          onRetry={() => void profilesQuery.refetch()}
          retryLabel={t("common.retry")}
        />
      ) : (
        <div>
          <Card className={PROFILE_PANEL_CLASS}>
            <CardHeader
              title={t("capabilities.targets")}
              description={probeWorkerEnabled ? t("capabilities.probeWorkerRunning") : t("capabilities.probeWorkerPaused")}
              action={
                <div className="flex items-center gap-2">
                  <Button
                    variant="ghost"
                    size="sm"
                    icon={<RefreshCw size={14} />}
                    onClick={() => void profilesQuery.refetch()}
                  >
                    {t("common.refresh")}
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    loading={probeWorkerMutation.isPending}
                    onClick={() => {
                      const reason = window.prompt(t("capabilities.probeWorkerReason"));
                      if (!reason?.trim()) return;
                      probeWorkerMutation.mutate({ enabled: !probeWorkerEnabled, reason: reason.trim() });
                    }}
                  >
                    {probeWorkerEnabled ? t("capabilities.pauseProbes") : t("capabilities.resumeProbes")}
                  </Button>
                </div>
              }
            />
            {profilesQuery.isLoading ? (
              <TableSkeleton rows={6} rowHeight="h-12" />
            ) : profiles.length === 0 ? (
              <CardBody className="text-sm text-text-muted">
                {t("capabilities.empty")}
              </CardBody>
            ) : (
              <Table className="min-h-0 flex-1" tableClassName="min-w-max">
                <thead>
                  <tr>
                    <Th>{t("capabilities.target")}</Th>
                    <Th>{t("capabilities.dialect")}</Th>
                    <Th>{t("capabilities.status")}</Th>
                    <Th>{t("capabilities.counts")}</Th>
                    <Th>{t("common.actions")}</Th>
                  </tr>
                </thead>
                <tbody>
                  {profiles.map((profile) => {
                    const displayProvider = providerName(profile.provider_id);
                    const displayTarget = `${displayProvider} / ${profile.model_id}`;
                    return (
                      <Tr
                        key={profile.target_key}
                        className={selectedKey === profile.target_key ? "bg-primary-soft/40" : undefined}
                      >
                        <Td>
                          <button
                            type="button"
                            className="block max-w-64 text-left text-primary hover:underline"
                            aria-haspopup="dialog"
                            onClick={() => setSelectedKey(profile.target_key)}
                            title={`${displayTarget}\nTarget Key: ${profile.target_key}`}
                          >
                            <span className="block truncate text-sm font-medium">
                              {displayProvider}
                            </span>
                            <span className="block truncate text-xs text-text-muted">
                              {profile.model_id}
                            </span>
                          </button>
                        </Td>
                        <Td className="text-xs text-text-muted">{profile.dialect_id}</Td>
                        <Td>
                          <Badge tone={statusTone(profile.profile_status)}>
                            {profile.profile_status}
                          </Badge>
                        </Td>
                        <Td className="text-xs text-text-muted">
                          {profile.supported}/{profile.constrained}/{profile.unsupported}/{profile.unknown}
                        </Td>
                        <Td>
                          <Button
                            variant="ghost"
                            size="sm"
                            loading={probeMutation.isPending && probeMutation.variables === profile.target_key}
                            icon={<RefreshCw size={14} />}
                            onClick={() => probeMutation.mutate(profile.target_key)}
                          >
                            {t("capabilities.probe")}
                          </Button>
                        </Td>
                      </Tr>
                    );
                  })}
                </tbody>
              </Table>
            )}
            {targetTotal > 0 ? (
              <Pagination
                page={targetPage}
                pageCount={targetPageCount}
                total={targetTotal}
                limit={targetPageSize}
                offset={targetOffset}
                pageSizeOptions={TARGET_PAGE_SIZE_OPTIONS}
                onPageChange={changeTargetPage}
                onPageSizeChange={changeTargetPageSize}
                labels={{
                  pageSizeLabel: t("capabilities.pageSizeLabel"),
                  pageSizeOption: t("capabilities.pageSizeOption"),
                  total: t("capabilities.total"),
                  range: t("capabilities.range"),
                  pageOf: t("capabilities.pageOf"),
                  first: t("capabilities.firstPage"),
                  prev: t("capabilities.prevPage"),
                  next: t("capabilities.nextPage"),
                  last: t("capabilities.lastPage"),
                  goTo: t("capabilities.goToPage"),
                  go: t("capabilities.go"),
                }}
              />
            ) : null}
          </Card>

          <Drawer
            open={selectedKey !== null}
            onOpenChange={(open) => {
              if (!open) {
                setSelectedKey(null);
                setExpandedProbeJobId(null);
                setSelectedProbeRunId(null);
              }
            }}
            title={t("capabilities.details")}
            description={
              selectedSummary
                ? `${providerName(selectedSummary.provider_id)} / ${selectedSummary.model_id}`
                : detail
                  ? `${providerName(detail.profile.provider_id)} / ${detail.profile.model_id}`
                  : t("capabilities.selectTarget")
            }
            closeLabel={t("common.close")}
            footer={
              <Button variant="secondary" onClick={() => setSelectedKey(null)}>
                {t("common.close")}
              </Button>
            }
          >
            {!selectedKey ? (
              <CardBody className="text-sm text-text-muted">{t("capabilities.selectTarget")}</CardBody>
            ) : detailQuery.isLoading ? (
              <CardBody>{t("common.loading")}</CardBody>
            ) : detailQuery.error ? (
              <CardBody>
                <ErrorBox message={(detailQuery.error as Error).message} onRetry={() => void detailQuery.refetch()} retryLabel={t("common.retry")} />
              </CardBody>
            ) : detail ? (
              <CardBody className="space-y-4">
                <div className="grid grid-cols-2 gap-2 text-xs">
                  <div><span className="text-text-subtle">{t("capabilities.provider")}</span><div className="truncate font-medium" title={detail.profile.provider_id}>{providerName(detail.profile.provider_id)}</div></div>
                  <div><span className="text-text-subtle">{t("capabilities.model")}</span><div className="font-medium">{detail.profile.model_id}</div></div>
                  <div><span className="text-text-subtle">{t("capabilities.dialect")}</span><div className="font-medium">{detail.profile.dialect_id}</div></div>
                  <div><span className="text-text-subtle">{t("capabilities.status")}</span><div><Badge tone={statusTone(detail.profile.profile_status)}>{detail.profile.profile_status}</Badge></div></div>
                  <div><span className="text-text-subtle">{t("capabilities.probeJob")}</span><div>{detail.probe_job ? <Badge tone={statusTone(detail.probe_job.status)}>{detail.probe_job.status}</Badge> : "—"}</div></div>
                  <div><span className="text-text-subtle">{t("capabilities.probeAttempts")}</span><div>{detail.probe_job ? `${detail.probe_job.attempt_count}/${detail.probe_job.max_attempts}` : "—"}</div></div>
                  <div><span className="text-text-subtle">probe progress</span><div>{detail.probe_job ? `${detail.probe_job.next_probe_index}/${detail.probe_job.probe_set.length}` : "—"}</div></div>
                  <div><span className="text-text-subtle">{t("capabilities.freshUntil")}</span><div>{fmtTime(detail.profile.fresh_until)}</div></div>
                  <div><span className="text-text-subtle">{t("capabilities.staleUntil")}</span><div>{fmtTime(detail.profile.stale_until)}</div></div>
                  <div><span className="text-text-subtle">registry/baseline</span><div>{detail.profile.registry_version}/{detail.profile.baseline_version}</div></div>
                  <div><span className="text-text-subtle">probe suite/judge</span><div>{detail.profile.last_probe_suite_version ?? "—"}/{detail.profile.last_probe_judge_version ?? "—"}</div></div>
                </div>
                {detail.probe_job ? (
                  <div className="rounded border border-border px-2.5 py-2 text-xs text-text-muted">
                    {t("capabilities.nextProbe")}: {fmtTime(detail.probe_job.next_attempt_at)}
                    {detail.probe_job.lease_until ? ` · ${t("capabilities.leaseUntil")}: ${fmtTime(detail.probe_job.lease_until)}` : ""}
                  </div>
                ) : null}
                <div className="rounded border border-border px-2.5 py-2 text-xs">
                  <div className="font-medium text-text">{t("capabilities.probeJobsTitle")}</div>
                  {probeJobsQuery.error ? (
                    <div className="mt-1 text-danger">{(probeJobsQuery.error as Error).message}</div>
                  ) : probeJobsQuery.isLoading ? (
                    <div className="mt-1 text-text-muted">{t("common.loading")}</div>
                  ) : probeJobs.length === 0 ? (
                    <div className="mt-1 text-text-muted">{t("capabilities.probeJobsEmpty")}</div>
                  ) : (
                    <div className="mt-1 space-y-1.5">
                      {probeJobs.map((job) => {
                        const expanded = expandedProbeJobId === job.id;
                        return (
                          <div key={job.id} className="rounded border border-border/70">
                            <button
                              type="button"
                              className="flex min-h-11 w-full items-center justify-between gap-2 px-2 py-1.5 text-left hover:bg-surface-muted"
                              aria-expanded={expanded}
                              onClick={() => {
                                setExpandedProbeJobId(expanded ? null : job.id);
                                setSelectedProbeRunId(null);
                              }}
                            >
                              <span className="min-w-0">
                                <span className="block truncate font-mono text-text" title={job.probe_set.join(", ")}>
                                  {job.probe_set.join(", ")}
                                </span>
                                <span className="block truncate text-text-subtle">
                                  {t("capabilities.probeJobAttempts")}: {job.attempt_count}/{job.max_attempts} · {t("capabilities.probeJobProgress")}: {job.next_probe_index}/{job.probe_set.length}
                                </span>
                              </span>
                              <Badge tone={probeStatusTone(job.status)}>{probeLabel("probeStatuses", job.status)}</Badge>
                            </button>
                            {expanded ? (
                              <div className="border-t border-border/70 px-2 py-2">
                                {probeJobRunsQuery.error ? (
                                  <div className="text-danger">{(probeJobRunsQuery.error as Error).message}</div>
                                ) : probeJobRunsQuery.isLoading ? (
                                  <div className="text-text-muted">{t("common.loading")}</div>
                                ) : probeJobRuns.length === 0 ? (
                                  <div className="text-text-muted">{t("capabilities.probeJobRunsEmpty")}</div>
                                ) : (
                                  <div className="space-y-1">
                                    <div className="text-text-subtle">{t("capabilities.probeJobRuns")}</div>
                                    {probeJobRuns.map((run) => (
                                      <button
                                        key={run.run_id}
                                        type="button"
                                        className="flex min-h-10 w-full items-center justify-between gap-2 rounded px-1.5 py-1 text-left hover:bg-surface-muted"
                                        aria-expanded={selectedProbeRunId === run.run_id}
                                        onClick={() => setSelectedProbeRunId(
                                          selectedProbeRunId === run.run_id ? null : run.run_id,
                                        )}
                                      >
                                        <span className="min-w-0 truncate">
                                          <span className="font-mono text-text">{run.probe_id}</span>
                                          <span className="ml-2 text-text-subtle">{fmtTime(run.ts)}</span>
                                        </span>
                                        <span className="shrink-0 text-text-muted">
                                          {probeLabel("probeOutcomes", run.outcome)} · {Math.round(run.duration_micros / 1000)}ms
                                        </span>
                                      </button>
                                    ))}
                                  </div>
                                )}
                                {selectedProbeRunId && probeRunDetail ? (
                                  <ProbeRunDetailView
                                    detail={probeRunDetail}
                                  />
                                ) : selectedProbeRunId && probeRunDetailQuery.isLoading ? (
                                  <div className="mt-2 text-text-muted">{t("common.loading")}</div>
                                ) : selectedProbeRunId && probeRunDetailQuery.error ? (
                                  <div className="mt-2 text-danger">{(probeRunDetailQuery.error as Error).message}</div>
                                ) : null}
                              </div>
                            ) : null}
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>
                {probeJobTotal > 0 ? (
                  <Pagination
                    page={probeJobPage}
                    pageCount={probeJobPageCount}
                    total={probeJobTotal}
                    limit={probeJobPageSize}
                    offset={probeJobOffset}
                    pageSizeOptions={PROBE_JOB_PAGE_SIZE_OPTIONS}
                    onPageChange={changeProbeJobPage}
                    onPageSizeChange={changeProbeJobPageSize}
                    labels={{
                      pageSizeLabel: t("capabilities.pageSizeLabel"),
                      pageSizeOption: t("capabilities.pageSizeOption"),
                      total: t("capabilities.probeJobsTotal"),
                      range: t("capabilities.range"),
                      pageOf: t("capabilities.pageOf"),
                      first: t("capabilities.firstPage"),
                      prev: t("capabilities.prevPage"),
                      next: t("capabilities.nextPage"),
                      last: t("capabilities.lastPage"),
                      goTo: t("capabilities.goToPage"),
                      go: t("capabilities.go"),
                    }}
                  />
                ) : null}
                {detail.profile.last_probe_error_class ? (
                  <div className="rounded border border-danger/30 bg-danger/5 px-2.5 py-2 text-xs text-danger">
                    {detail.profile.last_probe_error_class}: {detail.profile.last_probe_error_redacted ?? ""}
                  </div>
                ) : null}
                {inconclusiveObservations.length > 0 ? (
                  <div className="rounded border border-warning/30 bg-warning/5 px-2.5 py-2 text-xs text-text-muted">
                    <div className="font-medium text-warning">{t("capabilities.inconclusiveTitle")}</div>
                    <div className="mt-1 space-y-1">
                      {inconclusiveObservations.map((observation) => (
                        <div key={`${observation.capability_id}:${observation.observed_at}`} className="truncate" title={observation.redacted_detail ?? undefined}>
                          <span className="font-mono">{observation.capability_id}</span>: {observation.redacted_detail ?? t("capabilities.inconclusiveFallback")}
                        </div>
                      ))}
                    </div>
                  </div>
                ) : null}
                <div className="space-y-1.5">
                  {entries.length === 0 ? <p className="text-sm text-text-muted">{t("capabilities.noObservations")}</p> : entries.map(([id, value]) => (
                    <div key={id} className="rounded border border-border px-2.5 py-1.5 text-xs">
                      <div className="flex items-center justify-between gap-2">
                        <span className="truncate font-mono" title={id}>{id}</span>
                        <Badge tone={capabilityTone(value.state)}>{value.state}</Badge>
                      </div>
                      {value.value !== undefined ? (
                        <div className="mt-1 truncate text-text-muted" title={JSON.stringify(value.value)}>
                          value: {JSON.stringify(value.value)}
                        </div>
                      ) : null}
                      {value.observation && typeof value.observation === "object" ? (
                        <div className="mt-1 truncate text-text-subtle">
                          evidence: {String((value.observation as { source?: unknown }).source ?? "unknown")}
                          {value.observation.reason_code ? ` · ${value.observation.reason_code}` : ""}
                          {value.observation.expires_at ? ` · TTL ${fmtTime(value.observation.expires_at)}` : ""}
                        </div>
                      ) : null}
                    </div>
                  ))}
                </div>
                <div className="space-y-2 border-t border-border pt-3">
                  <div className="flex items-center gap-2 text-xs font-medium"><ShieldCheck size={14} />{t("capabilities.overrideTitle")}</div>
                  <Field label={t("capabilities.capabilityId")} controlId="override-capability-id">
                    <Input
                      id="override-capability-id"
                      list="capability-registry-options"
                      value={overrideCapability}
                      onChange={(event) => {
                        setOverrideCapability(event.target.value);
                        setOverrideValueError(null);
                      }}
                      placeholder="tools.function"
                    />
                    <datalist id="capability-registry-options">
                      {registryEntries.map((descriptor) => (
                        <option key={descriptor.id} value={descriptor.id} />
                      ))}
                    </datalist>
                  </Field>
                  <Field label={t("capabilities.state")}>
                    <Select
                      value={overrideState}
                      onValueChange={(value) => {
                        setOverrideState(value as CapabilityState);
                        setOverrideValueError(null);
                      }}
                      options={stateOptions}
                      ariaLabel={t("capabilities.state")}
                    />
                  </Field>
                  {overrideState === "constrained" ? (
                    <Field
                      label={t("capabilities.overrideValue")}
                      hint={overrideDescriptor
                        ? t("capabilities.overrideValueKind", { kind: overrideDescriptor.value_kind })
                        : t("capabilities.overrideValueSelectCapability")}
                      error={overrideValueError}
                      controlId={overrideDescriptor?.value_kind === "bool" ? undefined : "override-capability-value"}
                      errorId="override-capability-value-error"
                      required
                    >
                      {["enum_set", "string_set", "schema_keyword_set"].includes(
                        overrideDescriptor?.value_kind ?? "",
                      ) ? (
                        <Input
                          id="override-capability-value"
                          value={overrideSetValue}
                          onChange={(event) => {
                            setOverrideSetValue(event.target.value);
                            setOverrideValueError(null);
                          }}
                          placeholder={t("capabilities.overrideSetPlaceholder")}
                          aria-invalid={Boolean(overrideValueError)}
                          aria-describedby={overrideValueError ? "override-capability-value-error" : undefined}
                        />
                      ) : ["integer_range", "decimal_range"].includes(
                          overrideDescriptor?.value_kind ?? "",
                        ) ? (
                        <div className="grid grid-cols-2 gap-2">
                          <Input
                            id="override-capability-value"
                            type="number"
                            value={overrideRangeMin}
                            onChange={(event) => {
                              setOverrideRangeMin(event.target.value);
                              setOverrideValueError(null);
                            }}
                            placeholder={t("capabilities.overrideRangeMin")}
                            aria-label={t("capabilities.overrideRangeMin")}
                            aria-invalid={Boolean(overrideValueError)}
                          />
                          <Input
                            type="number"
                            value={overrideRangeMax}
                            onChange={(event) => {
                              setOverrideRangeMax(event.target.value);
                              setOverrideValueError(null);
                            }}
                            placeholder={t("capabilities.overrideRangeMax")}
                            aria-label={t("capabilities.overrideRangeMax")}
                            aria-invalid={Boolean(overrideValueError)}
                          />
                        </div>
                      ) : overrideDescriptor?.value_kind === "bool" ? (
                        <Select
                          value={overrideBooleanValue}
                          onValueChange={(value) => {
                            setOverrideBooleanValue(value);
                            setOverrideValueError(null);
                          }}
                          options={[
                            { value: "true", label: "true" },
                            { value: "false", label: "false" },
                          ]}
                          ariaLabel={t("capabilities.overrideValue")}
                        />
                      ) : (
                        <Textarea
                          id="override-capability-value"
                          rows={3}
                          value={overrideOpaqueValue}
                          onChange={(event) => {
                            setOverrideOpaqueValue(event.target.value);
                            setOverrideValueError(null);
                          }}
                          placeholder={t("capabilities.overrideJsonPlaceholder")}
                          aria-invalid={Boolean(overrideValueError)}
                          aria-describedby={overrideValueError ? "override-capability-value-error" : undefined}
                        />
                      )}
                    </Field>
                  ) : null}
                  <Field label={t("capabilities.reason")}>
                    <Input value={overrideReason} onChange={(event) => setOverrideReason(event.target.value)} placeholder={t("capabilities.reasonPlaceholder")} />
                  </Field>
                  <Field label={t("capabilities.expiresAt")}>
                    <Input
                      type="datetime-local"
                      value={overrideExpiresAt}
                      onChange={(event) => setOverrideExpiresAt(event.target.value)}
                    />
                  </Field>
                  <Button
                    variant="primary"
                    size="sm"
                    disabled={!overrideCapability.trim() || !overrideReason.trim()}
                    loading={overrideMutation.isPending}
                    onClick={() => {
                      if (
                        overrideState === "supported" &&
                        !window.confirm(t("capabilities.confirmSupportedOverride"))
                      ) {
                        return;
                      }
                      overrideMutation.mutate();
                    }}
                  >
                    {t("capabilities.saveOverride")}
                  </Button>
                  {detail.overrides.length > 0 ? (
                    <div className="space-y-1 pt-2">
                      {detail.overrides.map((override) => (
                        <div key={override.capability_id} className="flex items-center justify-between gap-2 text-xs text-text-muted">
                          <span className="truncate font-mono">{override.capability_id}: {override.state}</span>
                          <Button variant="ghost" size="sm" loading={removeOverrideMutation.isPending} onClick={() => removeOverrideMutation.mutate(override.capability_id)} aria-label={t("capabilities.removeOverride")}><Trash2 size={13} /></Button>
                        </div>
                      ))}
                    </div>
                  ) : null}
                </div>
              </CardBody>
            ) : null}
          </Drawer>
        </div>
      )}
      <Card className="mt-4">
        <CardHeader
          title={t("capabilities.admissionsTitle")}
          description={t("capabilities.admissionsDescription")}
        />
        <CardBody className="space-y-4">
          <Field label={t("capabilities.route")}>
            <Select
              value={selectedRouteId}
              onValueChange={setSelectedRouteId}
              ariaLabel={t("capabilities.selectRoute")}
              options={[
                { value: "", label: t("capabilities.selectRoute") },
                ...routes.map((route) => ({
                  value: route.id,
                  label: `${route.virtual_model} (${route.id})`,
                })),
              ]}
            />
          </Field>
          {selectedRouteId ? (
            <>
              {(() => {
                const selectedRoute = routes.find((route) => route.id === selectedRouteId);
                return selectedRoute ? (
                  <div className="rounded border border-border bg-surface-muted px-3 py-2 text-xs text-text-muted">
                    {t("capabilities.routeMode")}: {selectedRoute.capability_routing_mode ?? "inherit"}
                  </div>
                ) : null;
              })()}
              {metricsQuery.error ? (
                <ErrorBox
                  message={(metricsQuery.error as Error).message}
                  onRetry={() => void metricsQuery.refetch()}
                  retryLabel={t("common.retry")}
                />
              ) : metrics.length > 0 ? (
                <div className="grid gap-3 md:grid-cols-3">
                  {metrics.map((metric) => (
                    <div key={metric.shape_hash}>
                      <Metric
                        label={`${t("capabilities.compatibleCoverage")} · ${metric.shape_hash}`}
                        value={`${Math.round(metric.compatible_shape_coverage * 100)}%`}
                        caption={`${t("capabilities.profileCoverage")} ${Math.round(metric.profile_resolution_coverage * 100)}% · ${t("capabilities.unknownRate")} ${Math.round(metric.planner_unknown_rate * 100)}% · ${t("capabilities.disagreement")} ${metric.verified_success_disagreements}`}
                      />
                      {metric.telemetry_gap || metric.planner_internal_error_rate > 0 ? (
                        <div className="text-xs text-danger">
                          {metric.telemetry_gap ? "telemetry gap" : "planner error"}
                        </div>
                      ) : null}
                    </div>
                  ))}
                </div>
              ) : null}
              {admissionsQuery.error ? (
                <ErrorBox
                  message={(admissionsQuery.error as Error).message}
                  onRetry={() => void admissionsQuery.refetch()}
                  retryLabel={t("common.retry")}
                />
              ) : admissionsQuery.isLoading ? (
                <p className="text-sm text-text-muted">{t("common.loading")}</p>
              ) : admissions.length === 0 ? (
                <p className="text-sm text-text-muted">{t("capabilities.noAdmissions")}</p>
              ) : (
                <Table tableClassName="min-w-max">
                  <thead>
                    <tr>
                      <Th>{t("capabilities.admissionShape")}</Th>
                      <Th>{t("capabilities.admissionMode")}</Th>
                      <Th>{t("capabilities.profileCoverage")}</Th>
                      <Th>{t("capabilities.compatibleCoverage")}</Th>
                      <Th>{t("capabilities.constraints")}</Th>
                      <Th>{t("common.actions")}</Th>
                    </tr>
                  </thead>
                  <tbody>
                    {admissions.map((admission) => {
                      const report = admission.report;
                      return (
                        <Tr key={admission.capability_shape_hash}>
                          <Td className="max-w-48 truncate font-mono text-xs" title={admission.capability_shape_hash}>
                            {admission.capability_shape_hash}
                          </Td>
                          <Td><Badge tone={admission.mode === "enforce" ? "success" : "warning"}>{admission.mode}</Badge></Td>
                          <Td className="text-xs text-text-muted">
                            {typeof report.profile_resolution_coverage === "number"
                              ? `${Math.round(report.profile_resolution_coverage * 100)}%`
                              : "—"}
                          </Td>
                          <Td className="text-xs text-text-muted">
                            {typeof report.compatible_shape_coverage === "number"
                              ? `${Math.round(report.compatible_shape_coverage * 100)}%`
                              : "—"}
                          </Td>
                          <Td
                            className="max-w-64 truncate text-xs text-text-muted"
                            title={JSON.stringify(admission.required_requirements ?? [])}
                          >
                            {admission.required_requirements?.some(
                              (requirement) => requirement.value !== undefined,
                            )
                              ? JSON.stringify(admission.required_requirements)
                              : t("capabilities.unconstrainedShape")}
                          </Td>
                          <Td>
                            <Button
                              variant="ghost"
                              size="sm"
                              loading={removeAdmissionMutation.isPending}
                              icon={<Trash2 size={13} />}
                              onClick={() => {
                                if (!window.confirm(t("capabilities.confirmRemoveAdmission"))) {
                                  return;
                                }
                                removeAdmissionMutation.mutate({
                                  shapeHash: admission.capability_shape_hash,
                                  revision: admission.revision,
                                });
                              }}
                            >
                              {t("capabilities.removeAdmission")}
                            </Button>
                          </Td>
                        </Tr>
                      );
                    })}
                  </tbody>
                </Table>
              )}
              <div className="grid gap-3 md:grid-cols-2">
                <Field label={t("capabilities.admissionShape")} hint={t("capabilities.admissionShapeHint")}>
                  <Input value={admissionShape} onChange={(event) => setAdmissionShape(event.target.value)} placeholder="shape/v1:… (optional)" />
                </Field>
                <Field label={t("capabilities.admissionCapabilities")}>
                  <Input value={admissionCapabilities} onChange={(event) => setAdmissionCapabilities(event.target.value)} placeholder="tools.function,tools.namespace" />
                </Field>
                <Field label={t("capabilities.constraints")} hint={t("capabilities.constraintsHint")}>
                  <Textarea
                    rows={3}
                    value={admissionRequirementsJson}
                    onChange={(event) => setAdmissionRequirementsJson(event.target.value)}
                    placeholder='[{"id":"tools.namespace","strength":"required","value":{"kind":"enum_set","value":["functions"]}}]'
                  />
                </Field>
                <Field label={t("capabilities.admissionMode")}>
                  <Select
                    value={admissionMode}
                    onValueChange={(value) => setAdmissionMode(value as "shadow" | "enforce")}
                    ariaLabel={t("capabilities.admissionMode")}
                    options={[
                      { value: "shadow", label: "Shadow" },
                      { value: "enforce", label: "Enforce", disabled: !enforceAvailable },
                    ]}
                  />
                </Field>
                <Field label={t("capabilities.admissionReason")}>
                  <Input value={admissionReason} onChange={(event) => setAdmissionReason(event.target.value)} placeholder={t("capabilities.admissionReasonPlaceholder")} />
                </Field>
              </div>
              <label className="flex items-center gap-2 text-xs text-text-muted">
                <input type="checkbox" checked={lowTrafficException} onChange={(event) => setLowTrafficException(event.target.checked)} />
                {t("capabilities.lowTrafficException")}
              </label>
              <Button
                variant="primary"
                size="sm"
                loading={admissionMutation.isPending}
                disabled={
                  Boolean(admissionsQuery.error) ||
                  !admissionCapabilities.trim() ||
                  !admissionReason.trim() ||
                  (admissionMode === "enforce" && !enforceAvailable)
                }
                onClick={() => {
                  if (
                    admissionMode === "enforce" &&
                    !window.confirm(t("capabilities.confirmEnforceAdmission"))
                  ) {
                    return;
                  }
                  if (
                    lowTrafficException &&
                    !window.confirm(t("capabilities.confirmLowTrafficException"))
                  ) {
                    return;
                  }
                  admissionMutation.mutate();
                }}
              >
                {t("capabilities.saveAdmission")}
              </Button>
            </>
          ) : null}
        </CardBody>
      </Card>
      {registryQuery.data ? (
        <Card className="mt-4">
          <CardHeader
            title={t("capabilities.registryTitle")}
            description={t("capabilities.registryDescription", {
              count: registryEntries.length,
            })}
          />
          <CardBody className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
            {registryEntries.map((descriptor) => {
              const capabilityName = registryLabel("capabilityNames", descriptor.id);
              return (
                <div key={descriptor.id} className="rounded border border-border px-2.5 py-2 text-xs">
                  <div className="flex items-start justify-between gap-2">
                    <div className="min-w-0">
                      {capabilityName !== descriptor.id ? (
                        <div className="truncate font-medium text-text" title={capabilityName}>
                          {capabilityName}
                        </div>
                      ) : null}
                      <div className="truncate font-mono text-text-muted" title={descriptor.id}>
                        {descriptor.id}
                      </div>
                    </div>
                    <Badge
                      tone={descriptor.routing_eligibility === "enforce_eligible" ? "success" : "neutral"}
                      title={descriptor.routing_eligibility}
                    >
                      {registryLabel("routingEligibility", descriptor.routing_eligibility)}
                    </Badge>
                  </div>
                  <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1 text-text-subtle">
                    <span>
                      {t("capabilities.registryValueKind")}:{" "}
                      <code className="text-text">{descriptor.value_kind}</code>
                    </span>
                    <span>
                      {t("capabilities.registryMatcher")}:{" "}
                      <span className="text-text" title={descriptor.matcher}>
                        {registryLabel("matcher", descriptor.matcher)}
                      </span>
                    </span>
                    <span>
                      {t("capabilities.registryImplementationStatus")}:{" "}
                      <span className="text-text" title={descriptor.implementation_status}>
                        {registryLabel("implementationStatus", descriptor.implementation_status)}
                      </span>
                    </span>
                  </div>
                </div>
              );
            })}
          </CardBody>
        </Card>
      ) : null}
    </div>
  );
}
