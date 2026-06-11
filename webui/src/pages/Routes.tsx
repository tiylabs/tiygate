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
  ErrorBox,
  Field,
  Input,
  Modal,
  Select,
  Spinner,
  Table,
  Td,
  Th,
} from "@/components/ui";
import { PageHeader, fmtTime, shortId } from "@/components/PageHeader";

interface FormState {
  id?: string;
  virtual_model: string;
  targets: RouteTarget[];
  enabled: boolean;
  tenant_scope: string;
}

function emptyForm(): FormState {
  return {
    virtual_model: "",
    targets: [{ provider_id: "", model: "" }],
    enabled: true,
    tenant_scope: "",
  };
}

export default function RoutesPage() {
  const { t } = useTranslation();
  const qc = useQueryClient();

  const { data, isLoading, error } = useQuery({
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

  const invalidate = () => qc.invalidateQueries({ queryKey: ["routes"] });

  const saveMutation = useMutation({
    mutationFn: (input: { id?: string; body: RouteInput }) =>
      input.id
        ? routesApi.update(input.id, input.body)
        : routesApi.create(input.body),
    onSuccess: () => {
      setModalOpen(false);
      void invalidate();
    },
    onError: (e: Error) => setFormError(e.message),
  });

  const deleteMutation = useMutation({
    mutationFn: routesApi.remove,
    onSuccess: () => void invalidate(),
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
      tenant_scope: r.tenant_scope ?? "",
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
      setFormError("virtual_model and at least one valid target are required");
      return;
    }
    const body: RouteInput = {
      virtual_model: form.virtual_model,
      targets,
      enabled: form.enabled,
      tenant_scope: form.tenant_scope || null,
    };
    saveMutation.mutate({ id: editing?.id, body });
  }

  return (
    <div>
      <PageHeader
        title={t("routes.title")}
        action={
          <Button variant="primary" onClick={openCreate}>
            <Plus size={16} />
            {t("routes.add")}
          </Button>
        }
      />
      {error ? <ErrorBox message={(error as Error).message} /> : null}
      <Card>
        {isLoading ? (
          <Spinner />
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
                <tr key={r.id}>
                  <Td>
                    <div className="font-medium text-slate-800">
                      {r.virtual_model}
                    </div>
                    <div className="text-xs text-slate-400">{shortId(r.id)}</div>
                  </Td>
                  <Td>
                    <div className="flex flex-wrap gap-1">
                      {r.targets.map((tg, i) => (
                        <Badge key={i}>
                          {tg.provider_id} → {tg.model}
                        </Badge>
                      ))}
                    </div>
                  </Td>
                  <Td>
                    {r.enabled ? (
                      <Badge tone="green">{t("common.enabled")}</Badge>
                    ) : (
                      <Badge tone="slate">{t("common.disabled")}</Badge>
                    )}
                  </Td>
                  <Td className="text-xs text-slate-500">
                    {fmtTime(r.updated_at)}
                  </Td>
                  <Td className="text-right">
                    <div className="flex justify-end gap-1">
                      <Button variant="ghost" onClick={() => openEdit(r)}>
                        <Pencil size={14} />
                      </Button>
                      <Button
                        variant="ghost"
                        onClick={() => {
                          if (
                            window.confirm(
                              t("routes.deleteConfirm", {
                                name: r.virtual_model,
                              }),
                            )
                          ) {
                            deleteMutation.mutate(r.id);
                          }
                        }}
                      >
                        <Trash2 size={14} className="text-red-500" />
                      </Button>
                    </div>
                  </Td>
                </tr>
              ))}
              {(data ?? []).length === 0 && !isLoading ? (
                <tr>
                  <Td className="text-slate-400">{t("common.empty")}</Td>
                </tr>
              ) : null}
            </tbody>
          </Table>
        )}
      </Card>

      <Modal
        open={modalOpen}
        onClose={() => setModalOpen(false)}
        title={editing ? t("routes.editTitle") : t("routes.addTitle")}
        footer={
          <>
            <Button onClick={() => setModalOpen(false)}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="primary"
              onClick={submit}
              disabled={saveMutation.isPending}
            >
              {t("common.save")}
            </Button>
          </>
        }
      >
        <div className="space-y-4">
          {formError ? <ErrorBox message={formError} /> : null}
          <Field label={t("routes.virtualModel")}>
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
              <span className="text-sm font-medium text-slate-700">
                {t("routes.targets")}
              </span>
              <Button
                variant="ghost"
                onClick={() =>
                  setForm((f) => ({
                    ...f,
                    targets: [...f.targets, { provider_id: "", model: "" }],
                  }))
                }
              >
                <Plus size={14} />
                {t("routes.addTarget")}
              </Button>
            </div>
            {form.targets.map((tg, idx) => (
              <div
                key={idx}
                className="grid grid-cols-[1fr_1fr_70px_70px_auto] items-end gap-2 rounded-md border border-slate-200 p-2"
              >
                <div>
                  <label className="text-xs text-slate-500">
                    {t("routes.provider")}
                  </label>
                  <Select
                    value={tg.provider_id}
                    onChange={(e) =>
                      updateTarget(idx, { provider_id: e.target.value })
                    }
                  >
                    <option value="">—</option>
                    {(providers ?? []).map((p) => (
                      <option key={p.id} value={p.id}>
                        {p.name} ({p.id})
                      </option>
                    ))}
                  </Select>
                </div>
                <div>
                  <label className="text-xs text-slate-500">
                    {t("routes.model")}
                  </label>
                  <Input
                    value={tg.model}
                    onChange={(e) => updateTarget(idx, { model: e.target.value })}
                  />
                </div>
                <div>
                  <label className="text-xs text-slate-500">
                    {t("routes.weight")}
                  </label>
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
                <div>
                  <label className="text-xs text-slate-500">
                    {t("routes.priority")}
                  </label>
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
                  onClick={() =>
                    setForm((f) => ({
                      ...f,
                      targets: f.targets.filter((_, i) => i !== idx),
                    }))
                  }
                >
                  <X size={14} className="text-red-500" />
                </Button>
              </div>
            ))}
          </div>

          <Field label={t("providers.tenantScope")}>
            <Input
              value={form.tenant_scope}
              onChange={(e) =>
                setForm({ ...form, tenant_scope: e.target.value })
              }
            />
          </Field>
          <label className="flex items-center gap-2 text-sm text-slate-600">
            <input
              type="checkbox"
              checked={form.enabled}
              onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
            />
            {t("common.enabled")}
          </label>
        </div>
      </Modal>
    </div>
  );
}
