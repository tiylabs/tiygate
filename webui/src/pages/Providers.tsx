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
  ConfirmDialog,
  Dialog,
  EmptyState,
  ErrorBox,
  Field,
  Input,
  PasswordInput,
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
  const toast = useToast();
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });

  const [modalOpen, setModalOpen] = useState(false);
  const [form, setForm] = useState<FormState>(emptyForm());
  const [editing, setEditing] = useState<Provider | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<Provider | null>(null);

  const invalidate = () => qc.invalidateQueries({ queryKey: ["providers"] });

  const saveMutation = useMutation({
    mutationFn: (input: { id?: string; body: ProviderInput }) =>
      input.id
        ? providersApi.update(input.id, input.body)
        : providersApi.create(input.body),
    onSuccess: () => {
      setModalOpen(false);
      toast.success(t("providers.saved"));
      void invalidate();
    },
    onError: (e: Error) => setFormError(e.message),
  });

  const deleteMutation = useMutation({
    mutationFn: providersApi.remove,
    onSuccess: () => {
      setPendingDelete(null);
      toast.success(t("providers.deleted"));
      void invalidate();
    },
    onError: (e: Error) => {
      setPendingDelete(null);
      toast.error(t("providers.deleteFailed"), e.message);
    },
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
          <Button variant="primary" icon={<Plus size={16} />} onClick={openCreate}>
            {t("providers.add")}
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
              description={t("providers.empty")}
              action={
                <Button
                  variant="primary"
                  icon={<Plus size={16} />}
                  onClick={openCreate}
                >
                  {t("providers.add")}
                </Button>
              }
            />
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
                  <Tr key={p.id}>
                    <Td>
                      <div className="font-medium text-text">{p.name}</div>
                      <div className="font-mono text-xs text-text-subtle">
                        {shortId(p.id)}
                      </div>
                    </Td>
                    <Td>{p.vendor}</Td>
                    <Td className="max-w-[220px] truncate font-mono text-xs" title={p.api_base}>
                      {p.api_base}
                    </Td>
                    <Td>{p.auth_mode}</Td>
                    <Td>
                      {p.enabled ? (
                        <Badge tone="success">{t("common.enabled")}</Badge>
                      ) : (
                        <Badge tone="neutral">{t("common.disabled")}</Badge>
                      )}
                    </Td>
                    <Td className="text-xs text-text-muted">
                      {fmtTime(p.updated_at)}
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
                              onSelect: () => openEdit(p),
                            },
                            {
                              key: "delete",
                              label: t("common.delete"),
                              icon: <Trash2 size={14} />,
                              destructive: true,
                              onSelect: () => setPendingDelete(p),
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
        title={editing ? t("providers.editTitle") : t("providers.addTitle")}
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
          <Field label={t("common.name")} required>
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
              onValueChange={(v) => setForm({ ...form, auth_mode: v })}
              ariaLabel={t("providers.authMode")}
              options={AUTH_MODES.map((m) => ({ value: m, label: m }))}
            />
          </Field>
          <Field
            label={t("providers.apiKey")}
            hint={editing ? t("providers.apiKeyHint") : t("providers.redacted")}
          >
            <PasswordInput
              value={form.api_key}
              onChange={(e) => setForm({ ...form, api_key: e.target.value })}
              placeholder={editing ? "••••••••" : "sk-…"}
              toggleLabel={t("providers.apiKey")}
              autoComplete="off"
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
        title={t("providers.deleteTitle")}
        description={t("providers.deleteConfirm", {
          name: pendingDelete?.name ?? "",
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
