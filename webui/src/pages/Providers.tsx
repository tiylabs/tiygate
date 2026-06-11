import { useState } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Pencil, Trash2 } from "lucide-react";
import { providersApi } from "@/api/resources";
import type { Provider, ProviderInput } from "@/api/types";
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

const AUTH_MODES = ["api_key", "oauth"];

interface FormState {
  id?: string;
  name: string;
  vendor: string;
  api_base: string;
  api_key: string;
  auth_mode: string;
  tenant_scope: string;
  enabled: boolean;
}

function emptyForm(): FormState {
  return {
    name: "",
    vendor: "openai",
    api_base: "",
    api_key: "",
    auth_mode: "api_key",
    tenant_scope: "",
    enabled: true,
  };
}

export default function Providers() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const { data, isLoading, error } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });

  const [modalOpen, setModalOpen] = useState(false);
  const [form, setForm] = useState<FormState>(emptyForm());
  const [editing, setEditing] = useState<Provider | null>(null);
  const [formError, setFormError] = useState<string | null>(null);

  const invalidate = () =>
    qc.invalidateQueries({ queryKey: ["providers"] });

  const saveMutation = useMutation({
    mutationFn: (input: { id?: string; body: ProviderInput }) =>
      input.id
        ? providersApi.update(input.id, input.body)
        : providersApi.create(input.body),
    onSuccess: () => {
      setModalOpen(false);
      void invalidate();
    },
    onError: (e: Error) => setFormError(e.message),
  });

  const deleteMutation = useMutation({
    mutationFn: providersApi.remove,
    onSuccess: () => void invalidate(),
  });

  function openCreate() {
    setEditing(null);
    setForm(emptyForm());
    setFormError(null);
    setModalOpen(true);
  }

  function openEdit(p: Provider) {
    setEditing(p);
    setForm({
      id: p.id,
      name: p.name,
      vendor: p.vendor,
      api_base: p.api_base,
      api_key: "",
      auth_mode: p.auth_mode,
      tenant_scope: p.tenant_scope ?? "",
      enabled: p.enabled,
    });
    setFormError(null);
    setModalOpen(true);
  }

  function submit() {
    setFormError(null);
    const body: ProviderInput = {
      name: form.name,
      vendor: form.vendor,
      api_base: form.api_base,
      auth_mode: form.auth_mode,
      tenant_scope: form.tenant_scope || null,
      enabled: form.enabled,
    };
    // Only send api_key when the operator typed one — blank keeps the
    // existing encrypted secret untouched.
    if (form.api_key.trim()) body.api_key = form.api_key.trim();
    saveMutation.mutate({ id: editing?.id, body });
  }

  return (
    <div>
      <PageHeader
        title={t("providers.title")}
        action={
          <Button variant="primary" onClick={openCreate}>
            <Plus size={16} />
            {t("providers.add")}
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
                <Th>{t("common.name")}</Th>
                <Th>{t("providers.vendor")}</Th>
                <Th>{t("providers.apiBase")}</Th>
                <Th>{t("providers.authMode")}</Th>
                <Th>{t("common.status")}</Th>
                <Th>{t("common.updatedAt")}</Th>
                <Th className="text-right">{t("common.actions")}</Th>
              </tr>
            </thead>
            <tbody>
              {(data ?? []).map((p) => (
                <tr key={p.id}>
                  <Td>
                    <div className="font-medium text-slate-800">{p.name}</div>
                    <div className="text-xs text-slate-400">{shortId(p.id)}</div>
                  </Td>
                  <Td>{p.vendor}</Td>
                  <Td className="max-w-[220px] truncate">{p.api_base}</Td>
                  <Td>{p.auth_mode}</Td>
                  <Td>
                    {p.enabled ? (
                      <Badge tone="green">{t("common.enabled")}</Badge>
                    ) : (
                      <Badge tone="slate">{t("common.disabled")}</Badge>
                    )}
                  </Td>
                  <Td className="text-xs text-slate-500">{fmtTime(p.updated_at)}</Td>
                  <Td className="text-right">
                    <div className="flex justify-end gap-1">
                      <Button variant="ghost" onClick={() => openEdit(p)}>
                        <Pencil size={14} />
                      </Button>
                      <Button
                        variant="ghost"
                        onClick={() => {
                          if (
                            window.confirm(
                              t("providers.deleteConfirm", { name: p.name }),
                            )
                          ) {
                            deleteMutation.mutate(p.id);
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
                  <Td className="text-slate-400" >
                    {t("common.empty")}
                  </Td>
                </tr>
              ) : null}
            </tbody>
          </Table>
        )}
      </Card>

      <Modal
        open={modalOpen}
        onClose={() => setModalOpen(false)}
        title={editing ? t("providers.editTitle") : t("providers.addTitle")}
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
          <Field label={t("common.name")}>
            <Input
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
            />
          </Field>
          <Field label={t("providers.vendor")}>
            <Input
              value={form.vendor}
              onChange={(e) => setForm({ ...form, vendor: e.target.value })}
            />
          </Field>
          <Field label={t("providers.apiBase")}>
            <Input
              value={form.api_base}
              onChange={(e) => setForm({ ...form, api_base: e.target.value })}
              placeholder="https://api.openai.com/v1"
            />
          </Field>
          <Field label={t("providers.authMode")}>
            <Select
              value={form.auth_mode}
              onChange={(e) => setForm({ ...form, auth_mode: e.target.value })}
            >
              {AUTH_MODES.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </Select>
          </Field>
          <Field
            label={t("providers.apiKey")}
            hint={editing ? t("providers.apiKeyHint") : t("providers.redacted")}
          >
            <Input
              type="password"
              value={form.api_key}
              onChange={(e) => setForm({ ...form, api_key: e.target.value })}
              placeholder={editing ? "••••••••" : "sk-…"}
            />
          </Field>
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
