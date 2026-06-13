import { useCallback, useMemo, useState, type MouseEvent } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, Copy, Pencil, Plus, Trash2, X } from "lucide-react";
import { providersApi, routesApi } from "@/api/resources";
import type {
  Route,
  RouteInput,
  RouteTarget,
  RoutingStrategyName,
} from "@/api/types";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Dialog,
  EmptyState,
  ErrorBox,
  Field,
  Input,
  Label,
  RowActions,
  Select,
  Switch,
  Table,
  TableSkeleton,
  Td,
  Th,
  Tooltip,
  Tr,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime, shortId } from "@/components/PageHeader";

interface FormState {
  id?: string;
  virtual_model: string;
  targets: RouteTarget[];
  routing_strategy: RoutingStrategyName | "";
  enabled: boolean;
}

// Strategies that consume a per-target numeric value (`weight`). For
// `priority` the backend reuses `weight` (sorted descending), so we relabel
// the same column rather than introducing a separate field. `cooldown` and
// `latency` select targets from runtime health/latency data and need no
// per-target input, so the column is hidden for them.
const STRATEGY_OPTIONS: RoutingStrategyName[] = [
  "weighted",
  "priority",
  "cooldown",
  "latency",
];

function strategyUsesValue(s: RoutingStrategyName | ""): boolean {
  return s === "" || s === "weighted" || s === "priority";
}

function emptyForm(): FormState {
  return {
    virtual_model: "",
    targets: [{ provider_id: "", model_id: "" }],
    routing_strategy: "",
    enabled: true,
  };
}

export default function RoutesPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();

  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["routes"],
    queryFn: routesApi.list,
  });
  const { data: providers } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });
  const providerNameById = useMemo(() => {
    const m = new Map<string, string>();
    (providers ?? []).forEach((p) => m.set(p.id, p.name));
    return m;
  }, [providers]);
  const resolveProvider = useCallback(
    (id: string) => providerNameById.get(id),
    [providerNameById],
  );

  const [modalOpen, setModalOpen] = useState(false);
  const [editing, setEditing] = useState<Route | null>(null);
  const [form, setForm] = useState<FormState>(emptyForm());
  const [formError, setFormError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Route | null>(null);

  const invalidate = () => qc.invalidateQueries({ queryKey: ["routes"] });

  const saveMutation = useMutation({
    mutationFn: (input: { id?: string; body: RouteInput }) =>
      input.id
        ? routesApi.update(input.id, input.body)
        : routesApi.create(input.body),
    onSuccess: () => {
      setModalOpen(false);
      toast.success(t("routes.saved"));
      void invalidate();
    },
    onError: (e: Error) => setFormError(e.message),
  });

  const deleteMutation = useMutation({
    mutationFn: routesApi.remove,
    onSuccess: () => {
      setPendingDelete(null);
      toast.success(t("routes.deleted"));
      void invalidate();
    },
    onError: (e: Error) => {
      setPendingDelete(null);
      toast.error(t("routes.deleteFailed"), e.message);
    },
  });

  const copyMutation = useMutation({
    mutationFn: (r: Route) => {
      const suffix = Math.random().toString(36).slice(2, 7);
      const body: RouteInput = {
        virtual_model: `${r.virtual_model}-${suffix}`,
        targets: r.targets.map((tg) => ({
          provider_id: tg.provider_id,
          model_id: tg.model_id,
          weight: tg.weight ?? undefined,
        })),
        routing_strategy: r.routing_strategy ?? undefined,
        enabled: false,
      };
      return routesApi.create(body);
    },
    onSuccess: () => {
      toast.success(t("routes.copied"));
      void invalidate();
    },
    onError: (e: Error) => toast.error(t("routes.copyFailed"), e.message),
  });

  function openCreate() {
    setEditing(null);
    setForm(emptyForm());
    setFormError(null);
    setModalOpen(true);
  }

  function openEdit(r: Route) {
    setEditing(r);
    setForm({
      id: r.id,
      virtual_model: r.virtual_model,
      targets: r.targets.length
        ? r.targets.map((tg) => ({ ...tg }))
        : [{ provider_id: "", model_id: "" }],
      routing_strategy: r.routing_strategy ?? "",
      enabled: r.enabled,
    });
    setFormError(null);
    setModalOpen(true);
  }

  function updateTarget(idx: number, patch: Partial<RouteTarget>) {
    setForm((f) => ({
      ...f,
      targets: f.targets.map((tg, i) => (i === idx ? { ...tg, ...patch } : tg)),
    }));
  }

  function submit() {
    setFormError(null);
    const usesValue = strategyUsesValue(form.routing_strategy);
    const targets = form.targets
      .filter((tg) => tg.provider_id && tg.model_id)
      .map((tg) => ({
        provider_id: tg.provider_id,
        model_id: tg.model_id,
        // Only send the numeric value for strategies that consume it.
        weight: usesValue ? (tg.weight ?? undefined) : undefined,
      }));
    if (!form.virtual_model || targets.length === 0) {
      setFormError(t("routes.validationError"));
      return;
    }
    const body: RouteInput = {
      virtual_model: form.virtual_model,
      targets,
      routing_strategy: form.routing_strategy || undefined,
      enabled: form.enabled,
    };
    saveMutation.mutate({ id: editing?.id, body });
  }

  const providerOptions = [
    { value: "", label: "—" },
    ...(providers ?? []).map((p) => ({
      value: p.id,
      // Show only the provider name in the picker — the id is a technical
      // detail exposed via the trigger's title tooltip.
      label: p.name,
    })),
  ];

  // Map for tooltip on the selected value (hover shows id alongside name).
  const providerLabelById = useMemo(() => {
    const m = new Map<string, string>();
    (providers ?? []).forEach((p) => m.set(p.id, `${p.name} (${p.id})`));
    return m;
  }, [providers]);

  // Strategy picker: empty value → inherit gateway default.
  const strategyOptions = useMemo(
    () => [
      { value: "", label: t("routes.strategyDefault") },
      ...STRATEGY_OPTIONS.map((s) => ({
        value: s,
        label: t(`routes.strategyOptions.${s}`),
      })),
    ],
    [t],
  );
  const showValueColumn = strategyUsesValue(form.routing_strategy);
  // For the `priority` strategy the same `weight` value is relabeled.
  const valueLabel =
    form.routing_strategy === "priority"
      ? t("routes.priority")
      : t("routes.weight");

  return (
    <div>
      <PageHeader
        title={t("routes.title")}
        action={
          <Button variant="primary" icon={<Plus size={16} />} onClick={openCreate}>
            {t("routes.add")}
          </Button>
        }
      />
      {error ? (
        <ErrorBox
          message={(error as Error).message}
          onRetry={() => refetch()}
          retryLabel={t("common.retry")}
        />
      ) : (
        <Card>
          {isLoading ? (
            <TableSkeleton />
          ) : (data ?? []).length === 0 ? (
            <EmptyState
              title={t("common.emptyTitle")}
              description={t("routes.empty")}
              action={
                <Button
                  variant="primary"
                  icon={<Plus size={16} />}
                  onClick={openCreate}
                >
                  {t("routes.add")}
                </Button>
              }
            />
          ) : (
            <Table>
              <colgroup>
                <col style={{ width: "20rem" }} />
                <col />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "9rem" }} />
                <col style={{ width: "3.5rem" }} />
              </colgroup>
              <thead>
                <tr>
                  <Th>{t("routes.virtualModel")}</Th>
                  <Th>{t("routes.targets")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.updatedAt")}</Th>
                  <Th className="text-right">{t("common.actions")}</Th>
                </tr>
              </thead>
              <tbody>
                {(data ?? []).map((r) => (
                  <Tr key={r.id}>
                    <Td className="align-middle">
                      <div className="flex items-center gap-1.5">
                        <span
                          className="truncate font-medium text-text"
                          title={r.virtual_model}
                        >
                          {r.virtual_model}
                        </span>
                        <Tooltip content={t("routes.copyVirtualModel")} side="top">
                          <CopyValueButton value={r.virtual_model} />
                        </Tooltip>
                      </div>
                      <div
                        className="break-all font-mono text-xs text-text-subtle"
                        title={r.id}
                      >
                        {r.id}
                      </div>
                    </Td>
                    <Td className="align-middle">
                      <div className="flex flex-wrap gap-1">
                        {r.targets.map((tg, i) => (
                          <Badge key={i} tone="primary" title={`${resolveProvider(tg.provider_id) ?? tg.provider_id} → ${tg.model_id}`}>
                            <span className="truncate font-medium">
                              {resolveProvider(tg.provider_id) ?? tg.provider_id}
                            </span>
                            <span aria-hidden="true" className="text-text-subtle">
                              {" "}
                              →
                            </span>
                            <span
                              className="truncate font-mono text-[11px]"
                              title={tg.model_id}
                            >
                              {tg.model_id}
                            </span>
                          </Badge>
                        ))}
                      </div>
                    </Td>
                    <Td>
                      {r.enabled ? (
                        <Badge tone="success">{t("common.enabled")}</Badge>
                      ) : (
                        <Badge tone="neutral">{t("common.disabled")}</Badge>
                      )}
                    </Td>
                    <Td className="text-xs text-text-muted">
                      {fmtTime(r.updated_at)}
                    </Td>
                    <Td className="text-right">
                      <div className="flex justify-end">
                        <RowActions
                          label={t("common.rowActions")}
                          items={[
                            {
                              key: "edit",
                              label: t("common.edit"),
                              icon: <Pencil size={14} />,
                              onSelect: () => openEdit(r),
                            },
                            {
                              key: "copy",
                              label: t("common.copy"),
                              icon: <Copy size={14} />,
                              onSelect: () => copyMutation.mutate(r),
                            },
                            {
                              key: "delete",
                              label: t("common.delete"),
                              icon: <Trash2 size={14} />,
                              destructive: true,
                              onSelect: () => setPendingDelete(r),
                            },
                          ]}
                        />
                      </div>
                    </Td>
                  </Tr>
                ))}
              </tbody>
            </Table>
          )}
        </Card>
      )}

      <Dialog
        open={modalOpen}
        onOpenChange={setModalOpen}
        size="lg"
        title={editing ? t("routes.editTitle") : t("routes.addTitle")}
        closeLabel={t("common.close")}
        footer={
          <>
            <Button variant="secondary" onClick={() => setModalOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              onClick={submit}
              loading={saveMutation.isPending}
            >
              {t("common.save")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {formError ? <ErrorBox message={formError} /> : null}
          <Field label={t("routes.virtualModel")} required>
            <Input
              value={form.virtual_model}
              onChange={(e) =>
                setForm({ ...form, virtual_model: e.target.value })
              }
              placeholder="gpt-4o"
            />
          </Field>

          <Field
            label={t("routes.strategy")}
            hint={t("routes.strategyHint")}
          >
            <Select
              value={form.routing_strategy}
              onValueChange={(v) =>
                setForm((f) => ({
                  ...f,
                  routing_strategy: v as RoutingStrategyName | "",
                }))
              }
              ariaLabel={t("routes.strategy")}
              options={strategyOptions}
            />
          </Field>

          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-sm font-medium text-text">
                {t("routes.targets")}
              </span>
              <Button
                variant="ghost"
                size="sm"
                icon={<Plus size={14} />}
                onClick={() =>
                  setForm((f) => ({
                    ...f,
                    targets: [...f.targets, { provider_id: "", model_id: "" }],
                  }))
                }
              >
                {t("routes.addTarget")}
              </Button>
            </div>
            {form.targets.map((tg, idx) => (
              <div
                key={idx}
                className={
                  showValueColumn
                    ? "grid grid-cols-1 gap-2 rounded-md border border-border p-3 sm:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)_72px_auto] sm:items-end"
                    : "grid grid-cols-1 gap-2 rounded-md border border-border p-3 sm:grid-cols-[minmax(0,1.2fr)_minmax(0,1fr)_auto] sm:items-end"
                }
              >
                <div className="space-y-1">
                  <Label>{t("routes.provider")}</Label>
                  <Select
                    value={tg.provider_id}
                    onValueChange={(v) => updateTarget(idx, { provider_id: v })}
                    ariaLabel={t("routes.provider")}
                    options={providerOptions}
                    triggerTitle={
                      tg.provider_id
                        ? (providerLabelById.get(tg.provider_id) ?? tg.provider_id)
                        : undefined
                    }
                  />
                </div>
                <div className="space-y-1">
                  <Label>{t("routes.model")}</Label>
                  <Input
                    value={tg.model_id}
                    onChange={(e) => updateTarget(idx, { model_id: e.target.value })}
                  />
                </div>
                {showValueColumn ? (
                  <div className="space-y-1">
                    <Label>{valueLabel}</Label>
                    <Input
                      type="number"
                      value={tg.weight ?? ""}
                      onChange={(e) =>
                        updateTarget(idx, {
                          weight: e.target.value ? Number(e.target.value) : null,
                        })
                      }
                    />
                  </div>
                ) : null}
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={t("routes.removeTarget")}
                  onClick={() =>
                    setForm((f) => ({
                      ...f,
                      targets: f.targets.filter((_, i) => i !== idx),
                    }))
                  }
                >
                  <X size={14} className="text-danger" />
                </Button>
              </div>
            ))}
          </div>

          <Switch
            checked={form.enabled}
            onCheckedChange={(v) => setForm({ ...form, enabled: v })}
            label={t("common.enabled")}
          />
        </div>
      </Dialog>

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(o) => !o && setPendingDelete(null)}
        title={t("routes.deleteTitle")}
        description={t("routes.deleteConfirm", {
          name: pendingDelete?.virtual_model ?? "",
        })}
        confirmLabel={t("common.delete")}
        cancelLabel={t("common.cancel")}
        destructive
        loading={deleteMutation.isPending}
        onConfirm={() =>
          pendingDelete && deleteMutation.mutate(pendingDelete.id)
        }
      />
    </div>
  );
}

function CopyValueButton({ value }: { value: string }) {
  const { t } = useTranslation();
  const toast = useToast();
  const [done, setDone] = useState(false);
  async function handle(e: MouseEvent) {
    e.stopPropagation();
    e.preventDefault();
    try {
      await navigator.clipboard.writeText(value);
      toast.success(t("routes.virtualModelCopied"));
    } catch {
      toast.error(t("routes.virtualModelCopyFailed"));
    }
    setDone(true);
    window.setTimeout(() => setDone(false), 1200);
  }
  return (
    <button
      type="button"
      onClick={handle}
      aria-label={t("routes.copyVirtualModel")}
      className="inline-flex h-5 w-5 shrink-0 items-center justify-center rounded text-text-subtle transition-colors hover:bg-surface-muted hover:text-text focus:outline-none focus-visible:ring-2 focus-visible:ring-primary"
    >
      {done ? <Check size={12} /> : <Copy size={12} />}
    </button>
  );
}
