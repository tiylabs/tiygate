import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { RefreshCw, ShieldCheck, Trash2 } from "lucide-react";
import { useTranslation } from "react-i18next";

import { capabilitiesApi, routesApi, settingsApi } from "@/api/resources";
import type { CapabilityRequirement, CapabilityState } from "@/api/types";
import {
  Badge,
  Button,
  Card,
  CardBody,
  CardHeader,
  ErrorBox,
  Field,
  Input,
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

const stateOptions = [
  { value: "supported", label: "Supported" },
  { value: "unsupported", label: "Unsupported" },
  { value: "constrained", label: "Constrained" },
  { value: "unknown", label: "Unknown" },
];

function statusTone(status: string): "success" | "warning" | "danger" | "neutral" {
  if (status === "ready") return "success";
  if (status === "stale" || status === "partial" || status === "pending") {
    return "warning";
  }
  if (status === "error") return "danger";
  return "neutral";
}

function capabilityTone(state: string): "success" | "danger" | "warning" | "neutral" {
  if (state === "supported") return "success";
  if (state === "unsupported") return "danger";
  if (state === "constrained") return "warning";
  return "neutral";
}

export default function CapabilitiesPage() {
  const { t } = useTranslation();
  const toast = useToast();
  const qc = useQueryClient();
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [overrideCapability, setOverrideCapability] = useState("");
  const [overrideState, setOverrideState] = useState<CapabilityState>("supported");
  const [overrideReason, setOverrideReason] = useState("");
  const [overrideExpiresAt, setOverrideExpiresAt] = useState("");
  const [selectedRouteId, setSelectedRouteId] = useState("");
  const [admissionShape, setAdmissionShape] = useState("");
  const [admissionCapabilities, setAdmissionCapabilities] = useState("");
  const [admissionRequirementsJson, setAdmissionRequirementsJson] = useState("");
  const [admissionMode, setAdmissionMode] = useState<"shadow" | "enforce">("shadow");
  const [admissionReason, setAdmissionReason] = useState("");
  const [lowTrafficException, setLowTrafficException] = useState(false);

  const profilesQuery = useQuery({
    queryKey: ["target-capabilities"],
    queryFn: () => capabilitiesApi.listAll(),
  });
  const detailQuery = useQuery({
    queryKey: ["target-capability", selectedKey],
    queryFn: () => capabilitiesApi.get(selectedKey ?? ""),
    enabled: selectedKey !== null,
    refetchInterval: selectedKey !== null ? 3000 : false,
  });
  const probeRunsQuery = useQuery({
    queryKey: ["target-capability-probe-runs", selectedKey],
    queryFn: () => capabilitiesApi.probeRuns(selectedKey ?? "", { limit: 20 }),
    enabled: selectedKey !== null,
    refetchInterval: selectedKey !== null ? 5000 : false,
  });
  const registryQuery = useQuery({
    queryKey: ["capability-registry"],
    queryFn: () => capabilitiesApi.registryAll(),
  });
  const routesQuery = useQuery({
    queryKey: ["routes", "capability-admissions"],
    queryFn: () => routesApi.listAll(),
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
    },
    onError: (error: Error) => toast.error(t("capabilities.probeFailed"), error.message),
  });

  const overrideMutation = useMutation({
    mutationFn: () => {
      if (!selectedKey) throw new Error(t("capabilities.selectTarget"));
      return capabilitiesApi.override(selectedKey, {
        capability_id: overrideCapability.trim(),
        state: overrideState,
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
  const routes = routesQuery.data?.entries ?? [];
  const registryEntries = registryQuery.data?.entries ?? registryQuery.data?.items ?? [];
  const admissions = currentAdmissions;
  const metrics = metricsQuery.data?.entries ?? [];
  const probeWorkerEnabled =
    settingsQuery.data?.settings["gateway.capabilities.probe_enabled"] !== "false";
  const detail = detailQuery.data;
  const probeRuns = probeRunsQuery.data?.entries ?? probeRunsQuery.data?.items ?? [];
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
        <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_minmax(20rem,32rem)]">
          <Card>
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
              <Table tableClassName="min-w-max">
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
                  {profiles.map((profile) => (
                    <Tr
                      key={profile.target_key}
                      className={selectedKey === profile.target_key ? "bg-primary-soft/40" : undefined}
                    >
                      <Td>
                        <button
                          type="button"
                          className="max-w-48 truncate font-mono text-xs text-primary hover:underline"
                          onClick={() => setSelectedKey(profile.target_key)}
                          title={profile.target_key}
                        >
                          {profile.target_key}
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
                  ))}
                </tbody>
              </Table>
            )}
          </Card>

          <Card>
            <CardHeader
              title={t("capabilities.details")}
              description={selectedKey ?? t("capabilities.selectTarget")}
            />
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
                  <div><span className="text-text-subtle">{t("capabilities.provider")}</span><div className="font-medium">{detail.profile.provider_id}</div></div>
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
                {probeRunsQuery.error ? (
                  <div className="text-xs text-danger">{(probeRunsQuery.error as Error).message}</div>
                ) : probeRuns.length > 0 ? (
                  <div className="rounded border border-border px-2.5 py-2 text-xs text-text-muted">
                    <div className="font-medium text-text">{t("capabilities.probeRuns")}</div>
                    <div className="mt-1 space-y-1">
                      {probeRuns.slice(0, 20).map((run) => (
                        <div key={run.run_id} className="flex items-center justify-between gap-2">
                          <span className="truncate font-mono">{run.probe_id}</span>
                          <span>{run.outcome} · {run.budget_weight}u · {Math.round(run.duration_micros / 1000)}ms</span>
                        </div>
                      ))}
                    </div>
                  </div>
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
                  <Field label={t("capabilities.capabilityId")}>
                    <Input value={overrideCapability} onChange={(event) => setOverrideCapability(event.target.value)} placeholder="tools.function" />
                  </Field>
                  <Field label={t("capabilities.state")}>
                    <Select value={overrideState} onValueChange={(value) => setOverrideState(value as CapabilityState)} options={stateOptions} ariaLabel={t("capabilities.state")} />
                  </Field>
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
          </Card>
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
            description={`${registryEntries.length} capability descriptors`}
          />
          <CardBody className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
            {registryEntries.map((descriptor) => (
              <div key={descriptor.id} className="rounded border border-border px-2.5 py-2 text-xs">
                <div className="flex items-center justify-between gap-2">
                  <span className="truncate font-mono" title={descriptor.id}>{descriptor.id}</span>
                  <Badge tone={descriptor.routing_eligibility === "enforce_eligible" ? "success" : "neutral"}>
                    {descriptor.routing_eligibility}
                  </Badge>
                </div>
                <div className="mt-1 text-text-subtle">
                  {descriptor.value_kind} · {descriptor.matcher} · {descriptor.implementation_status}
                </div>
              </div>
            ))}
          </CardBody>
        </Card>
      ) : null}
    </div>
  );
}
