import { useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, Pencil, Trash2 } from "lucide-react";
import { providersApi, providerCatalogApi } from "@/api/resources";
import type { Provider, ProviderDeleteImpact, ProviderInput } from "@/api/types";
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
  Thead,
  Td,
  Th,
  Tr,
  useToast,
} from "@/components/ui";
import { PageHeader, fmtTime } from "@/components/PageHeader";
import { VendorIcon } from "@/lib/vendors";

const AUTH_MODES = ["api_key", "oauth"];
const SUPPORTED_AUTH_MODE = "api_key";

function authModeLabelKey(mode: string): string {
  return mode === "oauth" ? "providers.authModes.oauth" : "providers.authModes.staticKey";
}

interface FormState {
  id?: string;
  name: string;
  vendor: string;
  api_base: string;
  api_key: string;
  auth_mode: string;
  enabled: boolean;
}

interface PendingDeleteState {
  provider: Provider;
  impact?: ProviderDeleteImpact;
  loading: boolean;
  error?: string;
}

function emptyForm(): FormState {
  return {
    name: "",
    vendor: "openai",
    api_base: "",
    api_key: "",
    auth_mode: SUPPORTED_AUTH_MODE,
    enabled: true,
  };
}

export default function Providers() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const toast = useToast();
  const deleteImpactRequestRef = useRef(0);
  const { data, isLoading, error, refetch } = useQuery({
    queryKey: ["providers"],
    queryFn: providersApi.list,
  });
  const {
    data: catalog,
    isLoading: catalogLoading,
    isError: catalogError,
  } = useQuery({
    queryKey: ["provider-catalog"],
    queryFn: providerCatalogApi.list,
  });

  // Map catalog id → display name for the table's vendor column.
  const catalogLabel = useMemo(() => {
    const m = new Map<string, string>();
    for (const e of catalog ?? []) m.set(e.id, e.display_name);
    return m;
  }, [catalog]);
  const [modalOpen, setModalOpen] = useState(false);
  const [form, setForm] = useState<FormState>(emptyForm());
  const [editing, setEditing] = useState<Provider | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<PendingDeleteState | null>(null);

  // Options for the vendor dropdown, sourced from the server catalog. When
  // editing a provider whose vendor is no longer in the catalog (server
  // narrowed the set), we inject the current value so its value is never
  // silently dropped.
  const vendorOptions = useMemo(() => {
    const entries = catalog ?? [];
    const opts = entries.map((e) => ({
      value: e.id,
      label: (
        <span className="flex items-center gap-2">
          <VendorIcon vendor={e.id} className="h-4 w-4" />
          <span>{e.display_name}</span>
        </span>
      ),
    }));
    if (form.vendor && !entries.some((e) => e.id === form.vendor)) {
      opts.push({
        value: form.vendor,
        label: (
          <span className="flex items-center gap-2">
            <VendorIcon vendor={form.vendor} className="h-4 w-4" />
            <span>{form.vendor}</span>
          </span>
        ),
      });
    }
    return opts;
  }, [catalog, form.vendor]);

  const invalidateProviders = () => qc.invalidateQueries({ queryKey: ["providers"] });
  const invalidateProviderDelete = () => {
    void qc.invalidateQueries({ queryKey: ["providers"] });
    void qc.invalidateQueries({ queryKey: ["routes"] });
  };

  const saveMutation = useMutation({
    mutationFn: (input: { id?: string; body: ProviderInput }) =>
      input.id
        ? providersApi.update(input.id, input.body)
        : providersApi.create(input.body),
    onSuccess: () => {
      setModalOpen(false);
      toast.success(t("providers.saved"));
      void invalidateProviders();
    },
    onError: (e: Error) => setFormError(e.message),
  });

  const deleteMutation = useMutation({
    mutationFn: providersApi.remove,
    onSuccess: () => {
      setPendingDelete(null);
      toast.success(t("providers.deleted"));
      invalidateProviderDelete();
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
      auth_mode: SUPPORTED_AUTH_MODE,
      enabled: p.enabled,
    });
    setFormError(null);
    setModalOpen(true);
  }

  async function openDelete(p: Provider) {
    const requestId = deleteImpactRequestRef.current + 1;
    deleteImpactRequestRef.current = requestId;
    setPendingDelete({ provider: p, loading: true });
    try {
      const impact = await providersApi.deleteImpact(p.id);
      setPendingDelete((current) =>
        deleteImpactRequestRef.current === requestId && current?.provider.id === p.id
          ? { provider: p, impact, loading: false }
          : current,
      );
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (deleteImpactRequestRef.current === requestId) {
        toast.error(t("providers.deleteImpactLoadFailed"), message);
      }
      setPendingDelete((current) =>
        deleteImpactRequestRef.current === requestId && current?.provider.id === p.id
          ? { provider: p, loading: false, error: message }
          : current,
      );
    }
  }

  function submit() {
    setFormError(null);
    const body: ProviderInput = {
      name: form.name,
      vendor: form.vendor,
      api_base: form.api_base,
      auth_mode: SUPPORTED_AUTH_MODE,
      enabled: form.enabled,
    };
    // Only send api_key when the operator typed one — blank keeps the
    // existing encrypted secret untouched.
    if (form.api_key.trim()) body.api_key = form.api_key.trim();
    saveMutation.mutate({ id: editing?.id, body });
  }

  function renderDeleteDescription(): ReactNode {
    if (!pendingDelete) return null;
    const { provider, impact, loading, error } = pendingDelete;
    return (
      <div className="space-y-2">
        <p>{t("providers.deleteConfirm", { name: provider.name })}</p>
        {loading ? <p>{t("providers.deleteImpactLoading")}</p> : null}
        {error ? <p>{t("providers.deleteImpactLoadFailed")}</p> : null}
        {impact && impact.route_count > 0 ? (
          <>
            <p>
              {t("providers.deleteImpactRoutes", {
                count: impact.route_count,
                targets: impact.target_count,
              })}
            </p>
            {impact.delete_route_count > 0 ? (
              <p>
                {t("providers.deleteImpactEmptyRoutes", {
                  count: impact.delete_route_count,
                })}
              </p>
            ) : null}
          </>
        ) : null}
      </div>
    );
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
            <Table
              maxHeight={["max-h-[calc(100vh-9.5rem)]", "lg:max-h-[calc(100vh-5.5rem)]"]}
            >
              <colgroup>
                <col style={{ width: "20rem" }} />
                <col style={{ width: "16%" }} />
                <col />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "6rem" }} />
                <col style={{ width: "9rem" }} />
                <col style={{ width: "3.5rem" }} />
              </colgroup>
              <Thead>
                <tr>
                  <Th>{t("common.name")}</Th>
                  <Th>{t("providers.vendor")}</Th>
                  <Th>{t("providers.apiBase")}</Th>
                  <Th>{t("providers.authMode")}</Th>
                  <Th className="text-center">{t("common.status")}</Th>
                  <Th>{t("common.updatedAt")}</Th>
                  <Th className="text-right">{t("common.actions")}</Th>
                </tr>
              </Thead>
              <tbody>
                {(data ?? []).map((p) => (
                  <Tr key={p.id}>
                    <Td className="align-middle">
                      <div
                        className="truncate font-medium text-text"
                        title={p.name}
                      >
                        {p.name}
                      </div>
                      <div
                        className="break-all font-mono text-xs text-text-subtle"
                        title={p.id}
                      >
                        {p.id}
                      </div>
                    </Td>
                    <Td className="align-middle">
                      <div className="flex items-center gap-2">
                        <span className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-primary-soft text-primary">
                          <VendorIcon vendor={p.vendor} />
                        </span>
                        <span className="truncate">
                          {catalogLabel.get(p.vendor) ?? p.vendor}
                        </span>
                      </div>
                    </Td>
                    <Td
                      className="truncate font-mono text-xs"
                      title={p.api_base}
                    >
                      {p.api_base}
                    </Td>
                    <Td className="whitespace-nowrap text-xs">
                      {t(authModeLabelKey(p.auth_mode))}
                    </Td>
                    <Td className="text-center whitespace-nowrap">
                      {p.enabled ? (
                        <Badge tone="success">{t("common.enabled")}</Badge>
                      ) : (
                        <Badge tone="neutral">{t("common.disabled")}</Badge>
                      )}
                    </Td>
                    <Td className="text-xs text-text-muted whitespace-nowrap">
                      {fmtTime(p.updated_at)}
                    </Td>
                    <Td className="text-right">
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
                            onSelect: () => void openDelete(p),
                          },
                        ]}
                      />
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
            <Select
              value={form.vendor}
              onValueChange={(v) => {
                // On a fresh create, prefill api_base from the selected
                // catalog entry only when still empty (don't clobber a typed URL).
                // Auth mode is fixed to Static Key because OAuth is not supported yet.
                const entry = catalog?.find((e) => e.id === v);
                setForm((prev) => ({
                  ...prev,
                  vendor: v,
                  api_base:
                    !editing && !prev.api_base && entry
                      ? entry.default_base_url
                      : prev.api_base,
                  auth_mode: SUPPORTED_AUTH_MODE,
                }));
              }}
              ariaLabel={t("providers.vendor")}
              disabled={catalogLoading || catalogError || vendorOptions.length === 0}
              placeholder={
                catalogLoading
                  ? t("providers.vendorLoading")
                  : catalogError
                    ? t("providers.vendorLoadError")
                    : undefined
              }
              options={vendorOptions}
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
              onValueChange={(v) => {
                if (v === SUPPORTED_AUTH_MODE) {
                  setForm({ ...form, auth_mode: v });
                }
              }}
              ariaLabel={t("providers.authMode")}
              options={AUTH_MODES.map((m) => ({
                value: m,
                label:
                  m === "oauth"
                    ? `${t(authModeLabelKey(m))}（${t("providers.unsupported")}）`
                    : t(authModeLabelKey(m)),
                disabled: m !== SUPPORTED_AUTH_MODE,
              }))}
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
        description={renderDeleteDescription()}
        confirmLabel={t("common.delete")}
        cancelLabel={t("common.cancel")}
        destructive
        loading={deleteMutation.isPending || (pendingDelete?.loading ?? false)}
        onConfirm={() =>
          pendingDelete && deleteMutation.mutate(pendingDelete.provider.id)
        }
      />
    </div>
  );
}
