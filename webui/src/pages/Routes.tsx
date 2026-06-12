import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Pencil, Trash2, X } from "lucide-react";
import { providersApi, routesApi } from "@/api/resources";
import type { Route, RouteInput, RouteTarget } from "@/api/types";
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
  Tr,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime, shortId } from "@/components/PageHeader";

interface FormState {
  id?: string;
  virtual_model: string;
  targets: RouteTarget[];
  enabled: boolean;
}

function emptyForm(): FormState {
  return {
    virtual_model: "",
    targets: [{ provider_id: "", model: "" }],
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
        : [{ provider_id: "", model: "" }],
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
    const targets = form.targets
      .filter((tg) => tg.provider_id && tg.model)
      .map((tg) => ({
        provider_id: tg.provider_id,
        model: tg.model,
        weight: tg.weight ?? undefined,
        priority: tg.priority ?? undefined,
      }));
    if (!form.virtual_model || targets.length === 0) {
      setFormError(t("routes.validationError"));
      return;
    }
    const body: RouteInput = {
      virtual_model: form.virtual_model,
      targets,
      enabled: form.enabled,
    };
    saveMutation.mutate({ id: editing?.id, body });
  }

  const providerOptions = [
    { value: "", label: "—" },
    ...(providers ?? []).map((p) => ({
      value: p.id,
      label: `${p.name} (${shortId(p.id)})`,
    })),
  ];

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
              <thead>
                <tr>
                  <Th>{t("routes.virtualModel")}</Th>
                  <Th>{t("routes.targets")}</Th>
                  <Th>{t("common.status")}</Th>
                  <Th>{t("common.updatedAt")}</Th>
                  <Th className="text-right">{t("common.actions")}</Th>
                </tr>
              </thead>
              <tbody>
                {(data ?? []).map((r) => (
                  <Tr key={r.id}>
                    <Td>
                      <div className="font-medium text-text">
                        {r.virtual_model}
                      </div>
                      <div className="font-mono text-xs text-text-subtle">
                        {shortId(r.id)}
                      </div>
                    </Td>
                    <Td>
                      <div className="flex flex-wrap gap-1">
                        {r.targets.map((tg, i) => (
                          <Badge key={i} tone="primary">
                            {tg.provider_id} → {tg.model}
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
                    targets: [...f.targets, { provider_id: "", model: "" }],
                  }))
                }
              >
                {t("routes.addTarget")}
              </Button>
            </div>
            {form.targets.map((tg, idx) => (
              <div
                key={idx}
                className="grid grid-cols-1 gap-2 rounded-md border border-border p-3 sm:grid-cols-[1fr_1fr_70px_70px_auto] sm:items-end"
              >
                <div className="space-y-1">
                  <Label>{t("routes.provider")}</Label>
                  <Select
                    value={tg.provider_id}
                    onValueChange={(v) => updateTarget(idx, { provider_id: v })}
                    ariaLabel={t("routes.provider")}
                    options={providerOptions}
                  />
                </div>
                <div className="space-y-1">
                  <Label>{t("routes.model")}</Label>
                  <Input
                    value={tg.model}
                    onChange={(e) => updateTarget(idx, { model: e.target.value })}
                  />
                </div>
                <div className="space-y-1">
                  <Label>{t("routes.weight")}</Label>
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
                <div className="space-y-1">
                  <Label>{t("routes.priority")}</Label>
                  <Input
                    type="number"
                    value={tg.priority ?? ""}
                    onChange={(e) =>
                      updateTarget(idx, {
                        priority: e.target.value
                          ? Number(e.target.value)
                          : null,
                      })
                    }
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
