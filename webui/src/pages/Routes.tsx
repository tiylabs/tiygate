import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type PointerEvent,
  type KeyboardEvent,
} from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  Copy,
  GripVertical,
  IdCard,
  Pencil,
  Plus,
  Trash2,
  X,
} from "lucide-react";
import {
  modelCatalogApi,
  providersApi,
  routesApi,
  type RouteFilter,
} from "@/api/resources";
import type {
  ModelMetadata,
  Route,
  RouteInput,
  RouteTarget,
  RoutingStrategyName,
} from "@/api/types";
import {
  Badge,
  Button,
  Card,
  Combobox,
  ConfirmDialog,
  Dialog,
  EmptyState,
  ErrorBox,
  Field,
  Input,
  JsonEditor,
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
  useStickyTableScroll,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { Pagination } from "@/components/Pagination";
import { cn } from "@/lib/cn";

interface FormTarget extends RouteTarget {
  uiKey: string;
}

interface FormState {
  id?: string;
  virtual_model: string;
  targets: FormTarget[];
  routing_strategy: RoutingStrategyName | "";
  model_metadata: ModelMetadata | null;
  enabled: boolean;
}

interface MetadataJsonState {
  capabilities: string;
  modalities: string;
  pricing: string;
  metadata: string;
}

const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;

let nextTargetUiKey = 0;

function createFormTarget(target: Partial<RouteTarget> = {}): FormTarget {
  nextTargetUiKey += 1;
  return {
    uiKey: `target-${nextTargetUiKey}`,
    provider_id: target.provider_id ?? "",
    model_id: target.model_id ?? "",
    enabled: target.enabled ?? true,
    ...(target.weight !== undefined ? { weight: target.weight } : {}),
  };
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

const METADATA_JSON_KEYS = [
  "capabilities",
  "modalities",
  "pricing",
  "metadata",
] as const;
type MetadataJsonKey = (typeof METADATA_JSON_KEYS)[number];

function isTargetEnabled(tg: RouteTarget): boolean {
  return tg.enabled ?? true;
}

function emptyMetadata(id: string): ModelMetadata {
  return {
    id,
    lab_id: "self",
    display_name: id,
    capabilities: {},
    metadata: {},
  };
}

function formatJson(value: unknown, fallback: unknown): string {
  return JSON.stringify(value ?? fallback, null, 2);
}

function metadataJsonState(metadata: ModelMetadata | null): MetadataJsonState {
  return {
    capabilities: formatJson(metadata?.capabilities, {}),
    modalities: formatJson(metadata?.modalities, null),
    pricing: formatJson(metadata?.pricing, null),
    metadata: formatJson(metadata?.metadata, {}),
  };
}

function parseJsonField<T>(raw: string, fallback: T, label: string): T {
  const trimmed = raw.trim();
  if (!trimmed) return fallback;
  try {
    return JSON.parse(trimmed) as T;
  } catch {
    throw new Error(`${label} must be valid JSON.`);
  }
}

function parseOptionalNumber(raw: string): number | null {
  if (!raw.trim()) return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}

function emptyForm(): FormState {
  return {
    virtual_model: "",
    targets: [createFormTarget()],
    routing_strategy: "",
    model_metadata: null,
    enabled: true,
  };
}

export default function RoutesPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();

  const [filter, setFilter] = useState<RouteFilter>({
    limit: DEFAULT_PAGE_SIZE,
    offset: 0,
  });
  const limit = filter.limit ?? DEFAULT_PAGE_SIZE;

  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["routes", filter],
    queryFn: () => routesApi.list(filter),
  });
  const { data: providers } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });
  const { scrollRef, scrollState } = useStickyTableScroll([
    isLoading,
    data?.entries.length ?? 0,
  ]);
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
  const [metadataPanelOpen, setMetadataPanelOpen] = useState(false);
  const [metadataJson, setMetadataJson] = useState<MetadataJsonState>(
    metadataJsonState(null),
  );
  const [activeMetadataJsonKey, setActiveMetadataJsonKey] =
    useState<MetadataJsonKey>("capabilities");
  const [formError, setFormError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Route | null>(null);
  // Pointer-driven reorder preview state. The underlying target array is not
  // mutated until the pointer is released, so rows do not jump while dragging.
  const [dragPreview, setDragPreview] = useState<{
    from: number;
    insertIndex: number;
    offsetY: number;
  } | null>(null);
  const [reorderMessage, setReorderMessage] = useState("");
  const dragStateRef = useRef<{
    pointerId: number;
    from: number;
    insertIndex: number;
    startY: number;
  } | null>(null);
  const targetRowRefs = useRef<Array<HTMLDivElement | null>>([]);

  useEffect(() => {
    targetRowRefs.current.length = form.targets.length;
  }, [form.targets.length]);

  useEffect(() => {
    if (!modalOpen) resetTargetDrag();
  }, [modalOpen]);

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

  const resolveMetadataMutation = useMutation({
    mutationFn: () =>
      modelCatalogApi.resolve({
        virtual_model: form.virtual_model,
        target_model_id: form.targets.find((tg) => tg.model_id)?.model_id,
      }),
    onSuccess: (metadata) => {
      setForm((f) => ({ ...f, model_metadata: metadata }));
      setMetadataJson(metadataJsonState(metadata));
      setMetadataPanelOpen(true);
      toast.success(t("routes.metadataInitialized"));
    },
    onError: (e: Error) => {
      setMetadataPanelOpen(true);
      toast.error(t("routes.metadataInitFailed"), e.message);
    },
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
        model_metadata: r.model_metadata ?? null,
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

  function resetTargetDrag() {
    dragStateRef.current = null;
    setDragPreview(null);
  }

  function openCreate() {
    resetTargetDrag();
    setEditing(null);
    setForm(emptyForm());
    setMetadataJson(metadataJsonState(null));
    setMetadataPanelOpen(false);
    setFormError(null);
    setModalOpen(true);
  }

  function openEdit(r: Route) {
    resetTargetDrag();
    setEditing(r);
    setForm({
      id: r.id,
      virtual_model: r.virtual_model,
      targets: r.targets.length
        ? r.targets.map((tg) =>
            createFormTarget({
              provider_id: tg.provider_id,
              model_id: tg.model_id,
              enabled: tg.enabled ?? true,
            }),
          )
        : [createFormTarget()],
      routing_strategy: r.routing_strategy ?? "",
      model_metadata: r.model_metadata ?? null,
      enabled: r.enabled,
    });
    setMetadataJson(metadataJsonState(r.model_metadata ?? null));
    setMetadataPanelOpen(Boolean(r.model_metadata));
    setFormError(null);
    setModalOpen(true);
  }

  function updateTarget(idx: number, patch: Partial<RouteTarget>) {
    setForm((f) => ({
      ...f,
      targets: f.targets.map((tg, i) => (i === idx ? { ...tg, ...patch } : tg)),
    }));
  }

  function moveTarget(from: number, to: number): boolean {
    if (from === to) return false;
    setForm((f) => {
      if (
        from < 0 ||
        to < 0 ||
        from >= f.targets.length ||
        to >= f.targets.length
      ) {
        return f;
      }
      const next = f.targets.slice();
      const [moved] = next.splice(from, 1);
      if (!moved) return f;
      next.splice(to, 0, moved);
      return { ...f, targets: next };
    });
    return true;
  }

  function commitTargetInsert(
    from: number,
    insertIndex: number,
  ): number | null {
    // `insertIndex` is a position in the original array while the dragged row
    // still occupies `from`. Inserting after the source shifts the final index
    // left by one after the source row is removed.
    const targetIndex = insertIndex > from ? insertIndex - 1 : insertIndex;
    if (targetIndex === from) return null;
    return moveTarget(from, targetIndex) ? targetIndex : null;
  }

  function isNoopTargetInsert(from: number, insertIndex: number): boolean {
    return (insertIndex > from ? insertIndex - 1 : insertIndex) === from;
  }

  function getTargetInsertIndex(clientY: number, from: number): number {
    const rows = targetRowRefs.current;
    if (rows.length === 0) return from;

    for (let i = 0; i < rows.length; i += 1) {
      if (i === from) continue;
      const row = rows[i];
      if (!row) continue;
      const rect = row.getBoundingClientRect();
      if (clientY < rect.top + rect.height / 2) {
        return i;
      }
    }
    return rows.length;
  }

  function focusTargetHandle(idx: number) {
    window.requestAnimationFrame(() => {
      targetRowRefs.current[idx]
        ?.querySelector<HTMLElement>("[data-reorder-handle='true']")
        ?.focus({ preventScroll: true });
    });
  }

  function shouldShowInsertBefore(idx: number): boolean {
    return dragPreview !== null && dragPreview.insertIndex === idx;
  }

  function shouldShowInsertAfterLast(idx: number): boolean {
    return (
      dragPreview !== null &&
      dragPreview.insertIndex === form.targets.length &&
      idx === form.targets.length - 1
    );
  }

  function cancelTargetPointerDrag(e: PointerEvent<HTMLElement>) {
    const state = dragStateRef.current;
    if (state && state.pointerId !== e.pointerId) return;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    resetTargetDrag();
  }

  function commitTargetPointerDrag(e: PointerEvent<HTMLElement>) {
    const state = dragStateRef.current;
    if (!state || state.pointerId !== e.pointerId) return;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    const targetIndex = commitTargetInsert(state.from, state.insertIndex);
    if (targetIndex !== null) {
      setReorderMessage(
        t("routes.targetMoved", {
          position: targetIndex + 1,
          total: form.targets.length,
        }),
      );
      focusTargetHandle(targetIndex);
    }
    resetTargetDrag();
  }

  function handleTargetPointerDown(e: PointerEvent<HTMLElement>, idx: number) {
    if (e.button !== 0) return;
    e.stopPropagation();
    e.currentTarget.focus({ preventScroll: true });
    e.currentTarget.setPointerCapture(e.pointerId);
    dragStateRef.current = {
      pointerId: e.pointerId,
      from: idx,
      insertIndex: idx,
      startY: e.clientY,
    };
    setDragPreview({ from: idx, insertIndex: idx, offsetY: 0 });
  }

  function handleTargetPointerMove(e: PointerEvent<HTMLElement>) {
    const state = dragStateRef.current;
    if (!state || state.pointerId !== e.pointerId) return;
    e.preventDefault();
    const insertIndex = getTargetInsertIndex(e.clientY, state.from);
    dragStateRef.current = { ...state, insertIndex };
    setDragPreview({
      from: state.from,
      insertIndex,
      offsetY: e.clientY - state.startY,
    });
  }

  function handleTargetPointerKeyDown(
    e: KeyboardEvent<HTMLElement>,
    idx: number,
  ) {
    if (e.key === "Escape" && dragStateRef.current) {
      e.preventDefault();
      resetTargetDrag();
      return;
    }

    let nextIndex = idx;
    if (e.key === "ArrowUp") {
      nextIndex = idx - 1;
    } else if (e.key === "ArrowDown") {
      nextIndex = idx + 1;
    } else if (e.key === "Home") {
      nextIndex = 0;
    } else if (e.key === "End") {
      nextIndex = form.targets.length - 1;
    } else {
      return;
    }

    e.preventDefault();
    if (nextIndex < 0 || nextIndex >= form.targets.length) return;
    if (moveTarget(idx, nextIndex)) {
      setReorderMessage(
        t("routes.targetMoved", {
          position: nextIndex + 1,
          total: form.targets.length,
        }),
      );
      focusTargetHandle(nextIndex);
    }
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
    let modelMetadata = form.model_metadata;
    if (modelMetadata) {
      try {
        const metadataId =
          modelMetadata.id.trim() || form.virtual_model.trim();
        if (
          !metadataId ||
          !modelMetadata.lab_id.trim() ||
          !modelMetadata.display_name.trim()
        ) {
          setFormError(t("routes.metadataRequired"));
          return;
        }
        modelMetadata = {
          ...modelMetadata,
          id: metadataId,
          lab_id: modelMetadata.lab_id.trim(),
          display_name: modelMetadata.display_name.trim(),
          family: modelMetadata.family?.trim() || null,
          capabilities: parseJsonField<Record<string, unknown>>(
            metadataJson.capabilities,
            {},
            t("routes.metadataCapabilities"),
          ),
          modalities: parseJsonField<unknown>(
            metadataJson.modalities,
            null,
            t("routes.metadataModalities"),
          ),
          pricing: parseJsonField<ModelMetadata["pricing"]>(
            metadataJson.pricing,
            null,
            t("routes.metadataPricing"),
          ),
          metadata: parseJsonField<Record<string, unknown>>(
            metadataJson.metadata,
            {},
            t("routes.metadataExtra"),
          ),
        };
      } catch (e) {
        setFormError(e instanceof Error ? e.message : t("routes.metadataJsonInvalid"));
        return;
      }
    }
    const body: RouteInput = {
      virtual_model: form.virtual_model,
      targets,
      routing_strategy: form.routing_strategy || undefined,
      model_metadata: modelMetadata,
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

  // Session-level model cache: provider_id → model list. Persists across
  // target row edits within the same modal session but is not shared across
  // modal open/close cycles.
  const modelCacheRef = useRef<
    Map<string, { models: string[]; loaded: boolean }>
  >(new Map());
  const [loadingModelsFor, setLoadingModelsFor] = useState<string | null>(null);
  // Bump to force re-render after the cache is updated.
  const [modelCacheVersion, setModelCacheVersion] = useState(0);

  const fetchModels = useCallback(async (providerId: string) => {
    if (!providerId) return;
    const cached = modelCacheRef.current.get(providerId);
    if (cached?.loaded) return;
    setLoadingModelsFor(providerId);
    try {
      const resp = await providersApi.models(providerId);
      const models = resp.models.map((m) => m.id);
      modelCacheRef.current.set(providerId, { models, loaded: true });
    } catch {
      // Silent degradation — the Combobox still works as a plain input.
      modelCacheRef.current.set(providerId, { models: [], loaded: true });
    } finally {
      setLoadingModelsFor(null);
      setModelCacheVersion((v) => v + 1);
    }
  }, []);

  const getModelOptions = useCallback(
    (providerId: string) =>
      (modelCacheRef.current.get(providerId)?.models ?? []).map((id) => ({
        value: id,
        label: id,
      })),
    [modelCacheVersion],
  );

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
  const metadataJsonTabs = useMemo(
    () =>
      METADATA_JSON_KEYS.map((key) => ({
        key,
        label: t(
          key === "metadata"
            ? "routes.metadataExtra"
            : `routes.metadata${key[0].toUpperCase()}${key.slice(1)}`,
        ),
      })),
    [t],
  );

  const total = data?.total ?? 0;
  const offset = filter.offset ?? 0;
  const page = Math.floor(offset / limit) + 1;
  const pageCount = total === 0 ? 1 : Math.ceil(total / limit);

  function changePage(next: number) {
    const clamped = Math.max(1, Math.min(pageCount, next));
    setFilter((f) => ({ ...f, offset: (clamped - 1) * limit }));
  }

  function setPageSize(n: number) {
    setFilter((f) => ({ ...f, limit: n, offset: 0 }));
  }

  const routes = data?.entries ?? [];

  return (
    <div>
      <PageHeader
        title={t("routes.title")}
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
      {error ? (
        <ErrorBox
          message={(error as Error).message}
          onRetry={() => refetch()}
          retryLabel={t("common.retry")}
        />
      ) : (
        <Card>
          {isLoading ? (
            <TableSkeleton
              rows={12}
              rowHeight="h-[68px]"
              className="min-h-[calc(100vh-14rem)] lg:min-h-[calc(100vh-10rem)]"
            />
          ) : routes.length === 0 ? (
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
              maxHeight={[
                "max-h-[calc(100vh-14rem)]",
                "lg:max-h-[calc(100vh-10rem)]",
              ]}
              tableClassName="min-w-max border-separate border-spacing-0"
              containerRef={scrollRef}
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
                  <Th
                    className={cn(
                      "sticky left-0 z-30 w-80 bg-surface-muted",
                      scrollState !== "start" &&
                        "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("routes.virtualModel")}
                  </Th>
                  <Th>{t("routes.targets")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.createdAt")}</Th>
                  <Th
                    className={cn(
                      "sticky right-0 z-30 bg-surface-muted text-right",
                      scrollState !== "end" &&
                        "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                    )}
                  >
                    {t("common.actions")}
                  </Th>
                </tr>
              </Thead>
              <tbody>
                {routes.map((r) => (
                  <Tr key={r.id}>
                    <Td
                      className={cn(
                        "sticky left-0 z-10 w-80 bg-surface align-middle group-hover:bg-surface-muted",
                        scrollState !== "start" &&
                          "shadow-[6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
                      <div className="flex items-center gap-1.5">
                        <span
                          className="truncate font-medium text-text"
                          title={r.virtual_model}
                        >
                          {r.virtual_model}
                        </span>
                        <Tooltip
                          content={t("routes.copyVirtualModel")}
                          side="top"
                        >
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
                    <Td className="max-w-[60rem] align-middle">
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
                      {fmtTime(r.created_at)}
                    </Td>
                    <Td
                      className={cn(
                        "sticky right-0 z-10 bg-surface text-right group-hover:bg-surface-muted",
                        scrollState !== "end" &&
                          "shadow-[-6px_0_10px_-4px_rgba(0,0,0,0.25)]",
                      )}
                    >
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
          {routes.length > 0 && (
            <Pagination
              page={page}
              pageCount={pageCount}
              total={total}
              limit={limit}
              offset={offset}
              pageSizeOptions={PAGE_SIZE_OPTIONS}
              onPageChange={changePage}
              onPageSizeChange={setPageSize}
              labels={{
                pageSizeLabel: t("routes.pageSizeLabel"),
                pageSizeOption: t("routes.pageSizeOption"),
                total: t("routes.total"),
                range: t("routes.range"),
                pageOf: t("routes.pageOf"),
                first: t("routes.firstPage"),
                prev: t("routes.prevPage"),
                next: t("routes.nextPage"),
                last: t("routes.lastPage"),
                goTo: t("routes.goToPage"),
                go: t("routes.go"),
              }}
            />
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
            <div className="flex gap-2">
              <Input
                value={form.virtual_model}
                onChange={(e) =>
                  setForm({ ...form, virtual_model: e.target.value })
                }
                placeholder="gpt-4o"
              />
              <Button
                variant="secondary"
                className="h-9 w-9 px-0"
                icon={<IdCard size={14} />}
                loading={resolveMetadataMutation.isPending}
                aria-label={t("routes.metadata")}
                title={t("routes.metadata")}
                onClick={() => {
                  if (form.model_metadata) {
                    setMetadataPanelOpen((open) => !open);
                    return;
                  }
                  if (!form.virtual_model.trim()) {
                    setFormError(t("routes.validationError"));
                    return;
                  }
                  resolveMetadataMutation.mutate();
                }}
              />
            </div>
          </Field>

          {metadataPanelOpen ? (
            <div className="space-y-3 rounded-lg border border-border bg-surface-muted/30 p-3">
              <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-start">
                <div className="min-w-0 pr-2">
                  <Label>{t("routes.metadata")}</Label>
                  <p className="mt-0.5 max-w-[42rem] text-xs leading-5 text-text-subtle">
                    {t("routes.metadataHint")}
                  </p>
                </div>
                <div className="flex shrink-0 justify-end gap-2 whitespace-nowrap">
                  {form.model_metadata ? (
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() =>
                        setForm((f) => ({ ...f, model_metadata: null }))
                      }
                    >
                      {t("routes.clearMetadata")}
                    </Button>
                  ) : null}
                </div>
              </div>
              {form.model_metadata ? (
                <div className="space-y-3">
                  <div className="grid gap-3 sm:grid-cols-2">
                    <Field label={t("routes.metadataLabId")} required>
                      <Input
                        value={form.model_metadata.lab_id}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            model_metadata: f.model_metadata
                              ? { ...f.model_metadata, lab_id: e.target.value }
                              : f.model_metadata,
                          }))
                        }
                      />
                    </Field>
                    <Field label={t("routes.metadataDisplayName")} required>
                      <Input
                        value={form.model_metadata.display_name}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            model_metadata: f.model_metadata
                              ? {
                                  ...f.model_metadata,
                                  display_name: e.target.value,
                                }
                              : f.model_metadata,
                          }))
                        }
                      />
                    </Field>
                    <Field label={t("routes.metadataFamily")}>
                      <Input
                        value={form.model_metadata.family ?? ""}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            model_metadata: f.model_metadata
                              ? {
                                  ...f.model_metadata,
                                  family: e.target.value || null,
                                }
                              : f.model_metadata,
                          }))
                        }
                      />
                    </Field>
                    <Field label={t("routes.metadataContextWindow")}>
                      <Input
                        type="number"
                        value={form.model_metadata.context_window ?? ""}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            model_metadata: f.model_metadata
                              ? {
                                  ...f.model_metadata,
                                  context_window: parseOptionalNumber(
                                    e.target.value,
                                  ),
                                }
                              : f.model_metadata,
                          }))
                        }
                      />
                    </Field>
                    <Field label={t("routes.metadataMaxInputTokens")}>
                      <Input
                        type="number"
                        value={form.model_metadata.max_input_tokens ?? ""}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            model_metadata: f.model_metadata
                              ? {
                                  ...f.model_metadata,
                                  max_input_tokens: parseOptionalNumber(
                                    e.target.value,
                                  ),
                                }
                              : f.model_metadata,
                          }))
                        }
                      />
                    </Field>
                    <Field label={t("routes.metadataMaxOutputTokens")}>
                      <Input
                        type="number"
                        value={form.model_metadata.max_output_tokens ?? ""}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            model_metadata: f.model_metadata
                              ? {
                                  ...f.model_metadata,
                                  max_output_tokens: parseOptionalNumber(
                                    e.target.value,
                                  ),
                                }
                              : f.model_metadata,
                          }))
                        }
                      />
                    </Field>
                  </div>
                  <div className="space-y-2 rounded-md border border-border bg-surface p-2">
                    <div
                      className="flex flex-wrap gap-1 border-b border-border pb-2"
                      role="tablist"
                      aria-label={t("routes.metadataJsonFields")}
                    >
                      {metadataJsonTabs.map((tab) => (
                        <button
                          key={tab.key}
                          type="button"
                          role="tab"
                          aria-selected={activeMetadataJsonKey === tab.key}
                          className={cn(
                            "rounded-sm px-2.5 py-1 text-xs font-medium transition-colors",
                            activeMetadataJsonKey === tab.key
                              ? "bg-primary text-primary-foreground"
                              : "text-text-muted hover:bg-surface-muted hover:text-text",
                          )}
                          onClick={() => setActiveMetadataJsonKey(tab.key)}
                        >
                          {tab.label}
                        </button>
                      ))}
                    </div>
                    <div>
                      <JsonEditor
                        value={metadataJson[activeMetadataJsonKey]}
                        copyLabel={t("common.copy")}
                        copiedLabel={t("common.copied")}
                        ariaLabel={
                          metadataJsonTabs.find(
                            (tab) => tab.key === activeMetadataJsonKey,
                          )?.label ?? "JSON"
                        }
                        onChange={(value) =>
                          setMetadataJson((json) => ({
                            ...json,
                            [activeMetadataJsonKey]: value,
                          }))
                        }
                      />
                    </div>
                  </div>
                </div>
              ) : (
                <div className="flex justify-center gap-2 py-3">
                  <Button
                    variant="primary"
                    size="sm"
                    loading={resolveMetadataMutation.isPending}
                    onClick={() => {
                      if (!form.virtual_model.trim()) {
                        setFormError(t("routes.validationError"));
                        return;
                      }
                      resolveMetadataMutation.mutate();
                    }}
                  >
                    {t("routes.initMetadata")}
                  </Button>
                  <Button
                    variant="secondary"
                    size="sm"
                    onClick={() => {
                      const metadata = emptyMetadata(form.virtual_model.trim());
                      setForm((f) => ({ ...f, model_metadata: metadata }));
                      setMetadataJson(metadataJsonState(metadata));
                    }}
                  >
                    {t("routes.manualMetadata")}
                  </Button>
                </div>
              )}
            </div>
          ) : null}

          <Field label={t("routes.strategy")} hint={t("routes.strategyHint")}>
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
                    targets: [...f.targets, createFormTarget()],
                  }))
                }
              >
                {t("routes.addTarget")}
              </Button>
            </div>
            <div className="overflow-x-hidden rounded-md border border-border">
              <span
                className="sr-only"
                role="status"
                aria-live="polite"
                aria-atomic="true"
              >
                {reorderMessage}
              </span>
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
                const isDragging = dragPreview?.from === idx;
                const isNoopInsert = dragPreview
                  ? isNoopTargetInsert(
                      dragPreview.from,
                      dragPreview.insertIndex,
                    )
                  : false;
                const showInsertBefore =
                  !isNoopInsert && shouldShowInsertBefore(idx);
                const showInsertAfter =
                  !isNoopInsert && shouldShowInsertAfterLast(idx);
                return (
                  <div
                    key={tg.uiKey}
                    ref={(node) => {
                      targetRowRefs.current[idx] = node;
                    }}
                    style={
                      isDragging
                        ? { transform: `translateY(${dragPreview.offsetY}px)` }
                        : undefined
                    }
                    className={cn(
                      "relative grid grid-cols-[18px_minmax(0,1fr)_28px] gap-2 px-3 py-2 sm:items-center sm:grid-cols-[18px_minmax(0,1.2fr)_minmax(0,1fr)_36px_28px]",
                      idx > 0 && "border-t border-border",
                      !enabled && !isDragging && "opacity-50",
                      isDragging &&
                        "z-10 rounded-md border border-primary/40 bg-surface shadow-lg opacity-95 transition-none",
                      showInsertBefore &&
                        "before:absolute before:-top-px before:left-2 before:right-2 before:z-20 before:h-0.5 before:rounded-full before:bg-primary",
                      showInsertAfter &&
                        "after:absolute after:-bottom-px after:left-2 after:right-2 after:z-20 after:h-0.5 after:rounded-full after:bg-primary",
                    )}
                  >
                    <span
                      className={cn(
                        "flex touch-none select-none items-center justify-center rounded text-text-subtle transition-colors hover:text-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary",
                        isDragging ? "cursor-grabbing" : "cursor-grab",
                      )}
                      data-reorder-handle="true"
                      role="button"
                      tabIndex={0}
                      aria-label={t("routes.dragToReorder")}
                      title={t("routes.dragToReorder")}
                      onPointerDown={(e) => handleTargetPointerDown(e, idx)}
                      onPointerMove={handleTargetPointerMove}
                      onPointerUp={commitTargetPointerDrag}
                      onPointerCancel={cancelTargetPointerDrag}
                      onLostPointerCapture={cancelTargetPointerDrag}
                      onKeyDown={(e) => handleTargetPointerKeyDown(e, idx)}
                    >
                      <GripVertical size={14} />
                    </span>
                    <div className="col-start-2 grid min-w-0 gap-2 sm:contents">
                      <Select
                        value={tg.provider_id}
                        onValueChange={(v) =>
                          updateTarget(idx, { provider_id: v })
                        }
                        ariaLabel={t("routes.provider")}
                        options={providerOptions}
                        triggerTitle={
                          tg.provider_id
                            ? (providerLabelById.get(tg.provider_id) ??
                              tg.provider_id)
                            : undefined
                        }
                      />
                      <Combobox
                        value={tg.model_id}
                        placeholder={t("routes.model")}
                        aria-label={
                          loadingModelsFor === tg.provider_id
                            ? t("routes.modelLoading")
                            : t("routes.model")
                        }
                        options={getModelOptions(tg.provider_id)}
                        loading={loadingModelsFor === tg.provider_id}
                        onChange={(v) => updateTarget(idx, { model_id: v })}
                        onFocus={() => {
                          if (tg.provider_id) void fetchModels(tg.provider_id);
                        }}
                      />
                      <div className="flex items-center justify-center">
                        <Switch
                          checked={enabled}
                          onCheckedChange={(v) =>
                            updateTarget(idx, { enabled: v })
                          }
                          aria-label={t("routes.targetEnabled", {
                            index: idx + 1,
                          })}
                        />
                      </div>
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
      className="flex max-h-[3.25rem] max-w-full flex-wrap gap-1 overflow-hidden"
    >
      {shown.map((tg, i) => {
        const enabled = isTargetEnabled(tg);
        return (
          <Badge
            key={i}
            tone={enabled ? "primary" : "neutral"}
            className={cn(!enabled && "opacity-50")}
            title={`${resolveProvider(tg.provider_id) ?? tg.provider_id} → ${tg.model_id}`}
          >
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
        );
      })}
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
