import { useEffect, useState, type FormEvent } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { useAuth } from "@/auth/AuthContext";
import {
  tauriSetAdminToken,
  tauriEnablePasswordless,
  tauriGetMasterKey,
  tauriApplyMasterKey,
  tauriListInstances,
  tauriAddInstance,
  tauriUpdateInstance,
  tauriRemoveInstance,
  tauriSwitchInstance,
  tauriGetLastInstanceId,
  tauriCheckInstanceHealth,
  tauriGetServerPort,
  checkIsFirstRun,
  type InstanceEntry,
  type HealthStatus,
} from "@/auth/setup";
import { resetApiBase } from "@/api/client";
import {
  Alert,
  Button,
  Card,
  ErrorBox,
  Field,
  Input,
  PasswordInput,
  Spinner,
  Switch,
} from "@/components/ui";
import { LanguageSwitcher } from "@/components/LanguageSwitcher";
import { BootScreen } from "@/components/BootScreen";
import { cn } from "@/lib/cn";

type Mode =
  | "instance-select"
  | "choose"
  | "set-token"
  | "busy"
  | "show-master-key";

/** A small colored dot that reflects instance health. */
function HealthDot({ status }: { status: HealthStatus | null }) {
  const color =
    status === "ok"
      ? "bg-success"
      : status === "warning"
        ? "bg-warning"
        : status === "error"
          ? "bg-danger"
          : "bg-text-subtle";
  return (
    <span
      className={cn("inline-block h-2 w-2 shrink-0 rounded-full", color)}
      title={status ?? "unknown"}
    />
  );
}

export default function Setup() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { login, setPasswordless } = useAuth();
  const [mode, setMode] = useState<Mode>("instance-select");
  const [token, setToken] = useState("");
  const [confirmToken, setConfirmToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [masterKey, setMasterKey] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  // Tracks the auto-generated token from passwordless mode so we can
  // auto-login after the master-key step without a page reload.
  const [passwordlessToken, setPasswordlessToken] = useState<string | null>(
    null,
  );

  // ---- Instance-select state ----
  const [instances, setInstances] = useState<InstanceEntry[]>([]);
  const [instancesLoading, setInstancesLoading] = useState(true);
  const [selectedId, setSelectedId] = useState<string | null>(null); // null = local
  const [healthMap, setHealthMap] = useState<Record<string, HealthStatus>>({});
  const [localHealth, setLocalHealth] = useState<HealthStatus | null>(null);
  const [localUrl, setLocalUrl] = useState<string | null>(null);
  // Add / edit form
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [formLabel, setFormLabel] = useState("");
  const [formUrl, setFormUrl] = useState("");
  const [formSkipTls, setFormSkipTls] = useState(false);
  const [formSaving, setFormSaving] = useState(false);

  // Redirect to home if not in Tauri environment — the setup wizard is
  // only meaningful for the desktop client.
  useEffect(() => {
    const tauriInternals = "__TAURI_INTERNALS__" in window;
    if (!tauriInternals) {
      navigate("/", { replace: true });
    }
  }, [navigate]);

  // Load instances + last-selected on mount.
  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let cancelled = false;
    (async () => {
      try {
        const [list, lastId] = await Promise.all([
          tauriListInstances(),
          tauriGetLastInstanceId(),
        ]);
        if (cancelled) return;
        setInstances(list);
        setSelectedId(lastId);
        setInstancesLoading(false);
        // Probe health for the local sidecar.
        const port = await tauriGetServerPort();
        if (port && !cancelled) {
          const url = `http://127.0.0.1:${port}`;
          setLocalUrl(url);
          tauriCheckInstanceHealth(url, false).then(
            (status) => {
              if (!cancelled) setLocalHealth(status);
            },
          );
        }
        // Probe health for all remote instances.
        for (const inst of list) {
          tauriCheckInstanceHealth(inst.url, inst.skip_tls_verify).then(
            (status) => {
              if (cancelled) return;
              setHealthMap((prev) => ({ ...prev, [inst.id]: status }));
            },
          );
        }
      } catch {
        if (!cancelled) setInstancesLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  async function refreshInstances() {
    const list = await tauriListInstances();
    setInstances(list);
    // Re-probe health for instances we haven't checked yet.
    for (const inst of list) {
      if (!healthMap[inst.id]) {
        tauriCheckInstanceHealth(inst.url, inst.skip_tls_verify).then(
          (status) => {
            setHealthMap((prev) => ({ ...prev, [inst.id]: status }));
          },
        );
      }
    }
  }

  function resetForm() {
    setFormLabel("");
    setFormUrl("");
    setFormSkipTls(false);
    setEditingId(null);
    setShowForm(false);
  }

  async function handleSaveInstance(e: FormEvent) {
    e.preventDefault();
    if (!formLabel.trim() || !formUrl.trim()) return;
    setFormSaving(true);
    setError(null);
    try {
      if (editingId) {
        await tauriUpdateInstance(
          editingId,
          formLabel.trim(),
          formUrl.trim(),
          formSkipTls,
        );
      } else {
        await tauriAddInstance(formLabel.trim(), formUrl.trim(), formSkipTls);
      }
      await refreshInstances();
      resetForm();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setFormSaving(false);
    }
  }

  async function handleEditInstance(inst: InstanceEntry) {
    setEditingId(inst.id);
    setFormLabel(inst.label);
    setFormUrl(inst.url);
    setFormSkipTls(inst.skip_tls_verify);
    setShowForm(true);
  }

  async function handleDeleteInstance(inst: InstanceEntry) {
    try {
      await tauriRemoveInstance(inst.id);
      await refreshInstances();
      if (selectedId === inst.id) setSelectedId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleSelectInstance() {
    setMode("busy");
    setError(null);
    try {
      if (selectedId) {
        // Remote instance: switch active, set API base + instance key,
        // clear React Query cache, go to login.
        const inst = instances.find((i) => i.id === selectedId);
        if (!inst) {
          setError(t("setup.instanceNotFound"));
          setMode("instance-select");
          return;
        }
        await tauriSwitchInstance(inst.id);
        queryClient.clear();
        // Reload so AuthContext re-initializes with the new instance
        // key. If a remembered token exists in per-instance storage,
        // the app will auto-login; otherwise it shows the login page.
        window.location.replace("/");
      } else {
        // Local instance: switch active and either enter first-run
        // setup or reload into the existing local admin session.
        await tauriSwitchInstance(null);
        resetApiBase();
        queryClient.clear();
        const firstRun = await checkIsFirstRun();
        if (firstRun) {
          setMode("choose");
        } else {
          window.location.replace("/");
        }
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setMode("instance-select");
    }
  }

  async function handleSetToken(e: FormEvent) {
    e.preventDefault();
    if (!token.trim()) return;
    if (token !== confirmToken) {
      setError(t("setup.tokenMismatch"));
      return;
    }
    setMode("busy");
    setError(null);
    try {
      await tauriSetAdminToken(token.trim());
      await waitForSidecar(token.trim());
      // Fetch master key to show in the next step.
      const key = await tauriGetMasterKey();
      setMasterKey(key);
      setMode("show-master-key");
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(t("setup.restartFailed", { message: msg }));
      setMode("set-token");
    }
  }

  async function handlePasswordless() {
    setMode("busy");
    setError(null);
    try {
      const autoToken = await tauriEnablePasswordless();
      setPasswordlessToken(autoToken);
      // No sidecar restart needed — the token was already injected at
      // startup. Skip waitForSidecar and go straight to master key.
      const key = await tauriGetMasterKey();
      setMasterKey(key);
      setMode("show-master-key");
    } catch (err) {
      setError(t("setup.restartFailed", { message: String(err) }));
      setMode("choose");
    }
  }

  async function regenerateKey() {
    // Generate 32 random bytes on the frontend using Web Crypto API.
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    const hex = Array.from(buf)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    setMasterKey(hex);
    setCopied(false);
  }

  async function handleMasterKeyDone() {
    if (!masterKey) return;
    setMode("busy");
    setError(null);
    try {
      // Persist the master key and restart the sidecar so the new
      // TIYGATE_MASTER_KEY takes effect.
      await tauriApplyMasterKey(masterKey);
      if (passwordlessToken) {
        // Passwordless flow: wait for sidecar, then auto-login.
        await waitForSidecar(passwordlessToken);
        setPasswordless(true);
        login(passwordlessToken, true);
        // Full reload so AuthContext re-initializes with the local
        // instance key and passwordless token.
        window.location.replace("/");
      } else {
        // Set-token flow: wait for sidecar, then go to login.
        await waitForSidecar(token.trim());
        navigate("/login", { replace: true });
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(t("setup.restartFailed", { message: msg }));
      setMode("show-master-key");
    }
  }

  async function copyMasterKey() {
    if (!masterKey) return;
    try {
      await navigator.clipboard.writeText(masterKey);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard not available — user can manually select & copy.
    }
  }

  if (mode === "busy") {
    return <BootScreen />;
  }

  return (
    <div className="flex min-h-full items-center justify-center bg-bg px-4 py-10 sm:py-16">
      <div className="w-full max-w-[460px] animate-content-in">
        {/* Brand */}
        <div className="mb-7 flex flex-col items-center text-center">
          <span className="flex h-12 w-12 items-center justify-center overflow-hidden rounded-lg">
            <img src="./icon.svg" alt="" aria-hidden className="h-12 w-12" />
          </span>
          <h1 className="mt-3 text-lg font-semibold tracking-[-0.01em] text-text">
            {t("setup.title")}
          </h1>
          <p className="mt-1 text-sm text-text-muted">{t("setup.subtitle")}</p>
        </div>

        <Card className="p-6">
          <div className="mb-5 flex items-start justify-between gap-3">
            <div>
              <h2 className="text-base font-semibold text-text">
                {mode === "show-master-key"
                  ? t("setup.masterKeyTitle")
                  : mode === "instance-select"
                    ? t("setup.instanceSelectTitle")
                    : t("setup.heading")}
              </h2>
              <p className="mt-1 text-sm text-text-muted">
                {mode === "show-master-key"
                  ? t("setup.masterKeySubtitle")
                  : mode === "instance-select"
                    ? t("setup.instanceSelectDesc")
                    : t("setup.description")}
              </p>
            </div>
            <LanguageSwitcher className="shrink-0" />
          </div>

          {mode === "instance-select" && (
            <div className="space-y-3">
              {instancesLoading ? (
                <div className="flex justify-center py-4">
                  <Spinner />
                </div>
              ) : (
                <>
                  {/* Local instance card */}
                  <button
                    type="button"
                    onClick={() => setSelectedId(null)}
                    className={cn(
                      "flex w-full items-center gap-3 rounded-md border p-4 text-left transition-colors",
                      selectedId === null
                        ? "border-primary bg-primary-soft"
                        : "border-border hover:border-border-strong hover:bg-surface-muted",
                      "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                    )}
                  >
                    <HealthDot status={localHealth} />
                    <div className="flex-1 min-w-0">
                      <p className="font-medium text-text">
                        {t("setup.localInstance")}
                      </p>
                      {localUrl && (
                        <p className="mt-0.5 truncate text-xs text-text-muted">
                          {localUrl}
                        </p>
                      )}
                    </div>
                    {selectedId === null && (
                      <span className="text-xs font-medium text-primary">
                        ✓
                      </span>
                    )}
                  </button>

                  {/* Remote instances */}
                  {instances.map((inst) => (
                    <div
                      key={inst.id}
                      className={cn(
                        "flex w-full items-center gap-3 rounded-md border p-4 transition-colors",
                        selectedId === inst.id
                          ? "border-primary bg-primary-soft"
                          : "border-border hover:border-border-strong hover:bg-surface-muted",
                      )}
                    >
                      <button
                        type="button"
                        onClick={() => setSelectedId(inst.id)}
                        className="flex flex-1 items-center gap-3 text-left focus-visible:outline-none"
                      >
                        <HealthDot status={healthMap[inst.id] ?? null} />
                        <div className="flex-1 min-w-0">
                          <p className="font-medium text-text">{inst.label}</p>
                          <p className="mt-0.5 truncate text-xs text-text-muted">
                            {inst.url}
                          </p>
                        </div>
                        {selectedId === inst.id && (
                          <span className="text-xs font-medium text-primary">
                            ✓
                          </span>
                        )}
                      </button>
                      <div className="flex shrink-0 gap-1">
                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          onClick={() => handleEditInstance(inst)}
                        >
                          {t("common.edit")}
                        </Button>
                        <Button
                          type="button"
                          variant="ghost"
                          size="sm"
                          onClick={() => handleDeleteInstance(inst)}
                        >
                          {t("common.delete")}
                        </Button>
                      </div>
                    </div>
                  ))}

                  {/* Add / edit form */}
                  {showForm ? (
                    <form
                      className="space-y-3 rounded-md border border-border p-4"
                      onSubmit={handleSaveInstance}
                    >
                      <Field label={t("setup.instanceLabel")}>
                        <Input
                          value={formLabel}
                          onChange={(e) => setFormLabel(e.target.value)}
                          placeholder={t("setup.instanceLabelPlaceholder")}
                        />
                      </Field>
                      <Field label={t("setup.instanceUrl")}>
                        <Input
                          value={formUrl}
                          onChange={(e) => setFormUrl(e.target.value)}
                          placeholder="https://gateway.example.com"
                        />
                      </Field>
                      <Switch
                        checked={formSkipTls}
                        onCheckedChange={setFormSkipTls}
                        label={t("setup.skipTlsVerify")}
                      />
                      {error ? <ErrorBox message={error} /> : null}
                      <div className="flex gap-2">
                        <Button
                          type="button"
                          variant="secondary"
                          onClick={resetForm}
                        >
                          {t("common.cancel")}
                        </Button>
                        <Button
                          type="submit"
                          variant="primary"
                          className="flex-1"
                          loading={formSaving}
                          disabled={!formLabel.trim() || !formUrl.trim()}
                        >
                          {editingId ? t("common.save") : t("setup.addRemote")}
                        </Button>
                      </div>
                    </form>
                  ) : (
                    <button
                      type="button"
                      onClick={() => {
                        setEditingId(null);
                        setFormLabel("");
                        setFormUrl("");
                        setFormSkipTls(false);
                        setShowForm(true);
                        setError(null);
                      }}
                      className={cn(
                        "flex w-full items-center justify-center gap-2 rounded-md border border-dashed border-border p-3 text-sm text-text-muted",
                        "transition-colors hover:border-border-strong hover:bg-surface-muted hover:text-text",
                      )}
                    >
                      + {t("setup.addRemote")}
                    </button>
                  )}

                  {error && !showForm ? <ErrorBox message={error} /> : null}

                  {/* Connect button */}
                  <Button
                    type="button"
                    variant="primary"
                    className="w-full"
                    onClick={handleSelectInstance}
                  >
                    {t("setup.connect")}
                  </Button>
                </>
              )}
            </div>
          )}

          {mode === "choose" && (
            <div className="space-y-3">
              <button
                type="button"
                onClick={() => {
                  setMode("instance-select");
                  setError(null);
                }}
                className="mb-1 text-sm text-primary hover:underline"
              >
                ← {t("setup.backToInstanceSelect")}
              </button>
              <button
                type="button"
                onClick={() => {
                  setMode("set-token");
                  setError(null);
                }}
                className={cn(
                  "flex w-full items-start gap-3 rounded-md border border-border p-4 text-left",
                  "transition-colors hover:border-border-strong hover:bg-surface-muted",
                  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                )}
              >
                <div className="flex-1">
                  <p className="font-medium text-text">
                    {t("setup.setTokenTitle")}
                  </p>
                  <p className="mt-1 text-sm text-text-muted">
                    {t("setup.setTokenDesc")}
                  </p>
                </div>
              </button>
              <button
                type="button"
                onClick={handlePasswordless}
                className={cn(
                  "flex w-full items-start gap-3 rounded-md border border-border p-4 text-left",
                  "transition-colors hover:border-border-strong hover:bg-surface-muted",
                  "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                )}
              >
                <div className="flex-1">
                  <p className="font-medium text-text">
                    {t("setup.passwordlessTitle")}
                  </p>
                  <p className="mt-1 text-sm text-text-muted">
                    {t("setup.passwordlessDesc")}
                  </p>
                </div>
              </button>
              {error ? <ErrorBox message={error} /> : null}
            </div>
          )}

          {mode === "set-token" && (
            <form className="space-y-4" onSubmit={handleSetToken}>
              <Field label={t("setup.tokenLabel")}>
                <PasswordInput
                  autoFocus
                  autoComplete="new-password"
                  value={token}
                  onChange={(e) => setToken(e.target.value)}
                  placeholder={t("setup.tokenPlaceholder")}
                  toggleLabel={t("setup.tokenLabel")}
                />
              </Field>
              <Field label={t("setup.confirmTokenLabel")}>
                <PasswordInput
                  autoComplete="new-password"
                  value={confirmToken}
                  onChange={(e) => setConfirmToken(e.target.value)}
                  placeholder={t("setup.confirmTokenPlaceholder")}
                  toggleLabel={t("setup.confirmTokenLabel")}
                />
              </Field>
              {error ? <ErrorBox message={error} /> : null}
              <div className="flex gap-2">
                <Button
                  type="button"
                  variant="secondary"
                  onClick={() => {
                    setMode("choose");
                    setError(null);
                  }}
                >
                  {t("common.cancel")}
                </Button>
                <Button
                  type="submit"
                  variant="primary"
                  className="flex-1"
                  disabled={!token.trim() || !confirmToken.trim()}
                >
                  {t("setup.confirm")}
                </Button>
              </div>
            </form>
          )}

          {mode === "show-master-key" && (
            <div className="space-y-4">
              <Alert tone="warning">{t("setup.masterKeyWarning")}</Alert>
              <div>
                <label className="mb-1 block text-sm font-medium text-text">
                  {t("setup.masterKeyLabel")}
                </label>
                <div className="flex items-center gap-2">
                  <code className="flex-1 overflow-hidden overflow-ellipsis whitespace-nowrap rounded-md border border-border bg-surface-muted px-3 py-2 text-xs text-text">
                    {masterKey}
                  </code>
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    onClick={regenerateKey}
                  >
                    {t("setup.masterKeyRegenerate")}
                  </Button>
                  <Button
                    type="button"
                    variant="secondary"
                    size="sm"
                    onClick={copyMasterKey}
                  >
                    {copied ? t("common.copied") : t("common.copy")}
                  </Button>
                </div>
              </div>
              <Button
                type="button"
                variant="primary"
                className="w-full"
                onClick={handleMasterKeyDone}
              >
                {t("setup.masterKeyConfirm")}
              </Button>
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}

/**
 * Wait for the sidecar to become reachable after a restart. Polls the
 * `/healthz` endpoint directly (bypassing the API client) to avoid
 * depending on CORS or token resolution. The port is fetched from the
 * Tauri backend.
 */
async function waitForSidecar(token: string): Promise<void> {
  // Fetch the sidecar port via Tauri command.
  let port: number | null = null;
  try {
    const mod = await import("@tauri-apps/api/core");
    port = await mod.invoke<number>("get_server_port");
  } catch {
    // Not in Tauri — can't wait.
    return;
  }
  if (!port || port === 0) return;

  const healthUrl = `http://127.0.0.1:${port}/healthz`;
  const probeUrl = `http://127.0.0.1:${port}/admin/v1/audit?limit=1`;
  const maxAttempts = 50;
  const delayMs = 300;
  for (let i = 0; i < maxAttempts; i++) {
    // First check liveness (no auth, no CORS preflight on GET /healthz
    // in most WKWebView versions). Then probe with the token.
    try {
      const healthRes = await fetch(healthUrl);
      if (healthRes.ok) {
        // Sidecar is up — now verify the token works.
        try {
          const probeRes = await fetch(probeUrl, {
            headers: { Authorization: `Bearer ${token}` },
          });
          if (probeRes.ok) return;
        } catch {
          // Token probe failed (CORS?) but healthz passed — the
          // sidecar is running, so consider it ready.
          return;
        }
      }
    } catch {
      // Sidecar not yet listening — retry.
    }
    await new Promise((r) => setTimeout(r, delayMs));
  }
  throw new Error("sidecar did not become reachable");
}
