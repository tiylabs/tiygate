import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  Copy,
  GripVertical,
  Pencil,
  Plus,
  Trash2,
  X,
} from "lucide-react";
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
  Alert,
  RowActions,
  Select,
  Switch,
  Table,
  TableSkeleton,
  Thead,
  Td,
  Th,
  Tooltip,
  Tr,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";

interface FormState {
  id?: string;
  virtual_model: string;
  targets: RouteTarget[];
  routing_strategy: RoutingStrategyName | "";
  enabled: boolean;
}

// Strategies that consume a per-target numeric value (`weight`). For
// `priority` the backend reuses `weight` (sorted descending), so the
// weight is sent for every strategy (the order of rows in the form maps
// to a descending weight in the request). `cooldown` and `latency`
// ignore the weight on the runtime side; the value is still persisted
// to preserve order when the operator switches strategies.
const STRATEGY_OPTIONS: RoutingStrategyName[] = [
  "weighted",
  "priority",
  "cooldown",
  "latency",
];

function isTargetEnabled(tg: RouteTarget): boolean {
  return tg.enabled ?? true;
}

function emptyForm(): FormState {
  return {
    virtual_model: "",
    targets: [{ provider_id: "", model_id: "", enabled: true }],
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
  // Index of the row currently being dragged, or `null` when no drag is
  // active. Lives in component state so any drag-related UI (cursor,
  // highlight) can react synchronously.
  const [dragIndex, setDragIndex] = useState<number | null>(null);

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
          enabled: tg.enabled ?? true,
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
        ? r.targets.map((tg) => ({
            provider_id: tg.provider_id,
            model_id: tg.model_id,
            enabled: tg.enabled ?? true,
          }))
        : [{ provider_id: "", model_id: "", enabled: true }],
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

  function moveTarget(from: number, to: number) {
    if (from === to) return;
    setForm((f) => {
      const next = f.targets.slice();
      const [moved] = next.splice(from, 1);
      next.splice(to, 0, moved);
      return { ...f, targets: next };
    });
  }

  function submit() {
    setFormError(null);
    // Filter to rows that have at least a provider and a model id, then
    // assign a strictly decreasing `weight` based on row order so the
    // first row carries the highest weight. The filtered array is what
    // gets persisted; row indices in the filtered list map to weights.
    const valid = form.targets
      .map((tg, idx) => ({ tg, idx }))
      .filter(({ tg }) => tg.provider_id && tg.model_id);
    const targets = valid.map(({ tg }, i) => ({
      provider_id: tg.provider_id,
      model_id: tg.model_id,
      enabled: tg.enabled ?? true,
      weight: valid.length - i,
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
            <Table
              maxHeight={["max-h-[calc(100vh-9.5rem)]", "lg:max-h-[calc(100vh-5.5rem)]"]}
            >
              <colgroup>
                <col style={{ width: "20rem" }} />
                <col />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "9rem" }} />
                <col style={{ width: "3.5rem" }} />
              </colgroup>
              <Thead>
                <tr>
                  <Th>{t("routes.virtualModel")}</Th>
                  <Th>{t("routes.targets")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.updatedAt")}</Th>
                  <Th className="text-right">{t("common.actions")}</Th>
                </tr>
              </Thead>
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
                      <TargetBadges
                        targets={r.targets}
                        resolveProvider={resolveProvider}
                      />
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

          <div className="space-y-1.5">
            <div className="flex items-center justify-between">
              <div>
                <Label>{t("routes.targets")}</Label>
                <p className="mt-0.5 text-xs text-text-subtle">
                  {t("routes.orderHint")}
                </p>
              </div>
              <Button
                variant="ghost"
                size="sm"
                icon={<Plus size={14} />}
                onClick={() =>
                  setForm((f) => ({
                    ...f,
                    targets: [
                      ...f.targets,
                      { provider_id: "", model_id: "", enabled: true },
                    ],
                  }))
                }
              >
                {t("routes.addTarget")}
              </Button>
            </div>
            <div className="overflow-hidden rounded-md border border-border">
              <div
                className="hidden border-b border-border bg-surface-muted/50 px-3 py-1.5 text-[11px] font-medium uppercase tracking-[0.04em] text-text-subtle sm:grid sm:grid-cols-[18px_minmax(0,1.2fr)_minmax(0,1fr)_36px_28px] sm:gap-2"
                aria-hidden="true"
              >
                <span />
                <span>{t("routes.provider")}</span>
                <span>{t("routes.model")}</span>
                <span className="text-center">
                  {t("routes.targetEnabledHeader")}
                </span>
                <span />
              </div>
              {form.targets.map((tg, idx) => {
                const enabled = isTargetEnabled(tg);
                return (
                  <div
                    key={idx}
                    draggable
                    onDragStart={(e) => {
                      setDragIndex(idx);
                      // Some browsers expect a dataTransfer payload to
                      // initiate a drag — any string is fine.
                      e.dataTransfer.effectAllowed = "move";
                      e.dataTransfer.setData("text/plain", String(idx));
                    }}
                    onDragOver={(e) => {
                      // Prevent default to mark this row as a valid drop
                      // target; otherwise the browser cancels the drop.
                      e.preventDefault();
                      e.dataTransfer.dropEffect = "move";
                    }}
                    onDrop={(e) => {
                      e.preventDefault();
                      if (dragIndex === null) return;
                      moveTarget(dragIndex, idx);
                      setDragIndex(null);
                    }}
                    onDragEnd={() => setDragIndex(null)}
                    className={
                      "grid grid-cols-1 gap-2 px-3 py-2 sm:items-center sm:grid-cols-[18px_minmax(0,1.2fr)_minmax(0,1fr)_36px_28px] sm:gap-2" +
                      (idx > 0 ? " border-t border-border" : "") +
                      (dragIndex === idx
                        ? " opacity-60"
                        : !enabled
                          ? " opacity-50"
                          : "")
                    }
                  >
                    <span
                      className="hidden cursor-grab items-center justify-center text-text-subtle transition-colors hover:text-text sm:flex"
                      role="button"
                      aria-label={t("routes.dragToReorder")}
                      title={t("routes.dragToReorder")}
                      onMouseDown={(e) => e.stopPropagation()}
                    >
                      <GripVertical size={14} />
                    </span>
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
                    <Input
                      value={tg.model_id}
                      placeholder={t("routes.model")}
                      onChange={(e) => updateTarget(idx, { model_id: e.target.value })}
                    />
                    <div className="flex items-center justify-center">
                      <Switch
                        checked={enabled}
                        onCheckedChange={(v) => updateTarget(idx, { enabled: v })}
                        aria-label={t("routes.targetEnabled", {
                          index: idx + 1,
                        })}
                      />
                    </div>
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
                );
              })}
              {form.targets.length === 0 ? (
                <div className="px-3 py-3 text-center text-xs text-text-subtle">
                  {t("routes.empty")}
                </div>
              ) : null}
            </div>
          </div>

          <Switch
            checked={form.enabled}
            onCheckedChange={(v) => setForm({ ...form, enabled: v })}
            label={t("common.enabled")}
          />

          <Alert tone="info" className="text-xs leading-5">
            <div className="font-medium">{t("routes.fallbackRuleTitle")}</div>
            <ul className="mt-1 list-disc space-y-0.5 pl-4">
              <li>{t("routes.fallbackRuleMax")}</li>
              <li>{t("routes.fallbackRuleCount")}</li>
              <li>{t("routes.fallbackRuleCooldown")}</li>
            </ul>
          </Alert>
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

/**
 * Renders route targets as primary badges, clamped to at most two rows.
 * Targets overflowing the two-row limit are summarized by a trailing
 * "+N" badge. The visible count is measured from real DOM layout (the
 * badges' `offsetTop` relative to the container) so it adapts to the
 * column width and wrapping, then the list is re-clamped reserving room
 * for the overflow badge itself.
 */
function TargetBadges({
  targets,
  resolveProvider,
}: {
  targets: RouteTarget[];
  resolveProvider: (id: string) => string | undefined;
}) {
  const { t } = useTranslation();
  const containerRef = useRef<HTMLDivElement>(null);
  // Number of targets to render directly; the rest collapse into "+N".
  const [visibleCount, setVisibleCount] = useState(targets.length);
  // When true, render every target so layout can be measured; the
  // layout effect then clamps to two rows and turns measuring off.
  const [measuring, setMeasuring] = useState(true);

  // Re-enter the measuring phase whenever the target set changes.
  useLayoutEffect(() => {
    setVisibleCount(targets.length);
    setMeasuring(true);
  }, [targets]);

  // Clamp to two rows after a full render, before paint (no flicker).
  useLayoutEffect(() => {
    if (!measuring) return;
    const container = containerRef.current;
    if (!container) return;
    const children = Array.from(container.children) as HTMLElement[];
    if (children.length === 0) {
      setMeasuring(false);
      return;
    }
    // Distinct row offsets (top positions) in document order.
    const rowTops: number[] = [];
    for (const child of children) {
      const top = child.offsetTop;
      if (rowTops.length === 0 || top > rowTops[rowTops.length - 1] + 1) {
        rowTops.push(top);
      }
    }
    if (rowTops.length <= 2) {
      // Everything already fits within two rows.
      setVisibleCount(targets.length);
      setMeasuring(false);
      return;
    }
    // Keep badges whose top is within the first two rows, then drop one
    // more so the trailing "+N" badge fits without spilling to a 3rd row.
    const thirdRowTop = rowTops[2];
    let kept = 0;
    for (const child of children) {
      if (child.offsetTop >= thirdRowTop - 1) break;
      kept += 1;
    }
    if (kept >= 1 && kept < targets.length) {
      kept -= 1;
    }
    setVisibleCount(Math.max(kept, 1));
    setMeasuring(false);
  }, [measuring, targets.length]);

  // Re-measure when the column width changes.
  useEffect(() => {
    const container = containerRef.current;
    if (!container || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setMeasuring(true));
    observer.observe(container);
    return () => observer.disconnect();
  }, []);

  const hiddenCount = measuring ? 0 : targets.length - visibleCount;
  const shown = measuring ? targets : targets.slice(0, visibleCount);

  return (
    <div
      ref={containerRef}
      className="flex max-h-[3.25rem] flex-wrap gap-1 overflow-hidden"
    >
      {shown.map((tg, i) => (
        <Badge
          key={i}
          tone="primary"
          title={`${resolveProvider(tg.provider_id) ?? tg.provider_id} → ${tg.model_id}`}
        >
          <span className="truncate font-medium">
            {resolveProvider(tg.provider_id) ?? tg.provider_id}
          </span>
          <span aria-hidden="true" className="text-text-subtle">
            {" "}
            →
          </span>
          <span className="truncate font-mono text-[11px]" title={tg.model_id}>
            {tg.model_id}
          </span>
        </Badge>
      ))}
      {hiddenCount > 0 && (
        <Badge
          tone="neutral"
          title={`${t("routes.moreTargets", { count: hiddenCount })}\n${targets
            .slice(visibleCount)
            .map(
              (tg) =>
                `${resolveProvider(tg.provider_id) ?? tg.provider_id} → ${tg.model_id}`,
            )
            .join("\n")}`}
        >
          +{hiddenCount}
        </Badge>
      )}
    </div>
  );
}
