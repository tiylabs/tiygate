//! Restart-safe capability probe worker.
//!
//! The worker intentionally uses the same target credentials and HTTP client
//! as the data plane, but it never records health/EWMA or business request
//! telemetry. Probe conclusions are conservative: model non-compliance is
//! inconclusive, while explicit capability errors can produce a negative
//! observation.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::{net::IpAddr, str::FromStr};

use chrono::Utc;
use futures::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tiygate_core::{
    CapabilityId, CapabilityObservation, CapabilityState, CapabilityValue, EvidenceSource,
};
use tiygate_store::capabilities::{
    wire_profile_for_target, ProbeJob, ProfileStatus, TargetCapabilityProfile,
    CAPABILITY_BASELINE_VERSION, CAPABILITY_REGISTRY_VERSION, CAPABILITY_SCHEMA_VERSION,
    PROBE_JUDGE_VERSION, PROBE_SUITE_VERSION,
};
use tiygate_store::config_store::DbConfigStore;
use tokio::sync::watch;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::AppState;

const PROBE_WORKER_ID_ENV: &str = "TIYGATE_PROBE_WORKER_ID";
// A CRL A/B probe can perform a control call plus two carrier calls, each
// bounded by the 30-second transport deadline. Keep the lease longer than the
// worst-case bundle while still allowing a crashed worker to be reclaimed.
const PROBE_LEASE_SECS: u64 = 180;
const PROBE_POLL_SECS: u64 = 2;
const FRESH_SECS: i64 = 24 * 60 * 60;
const STALE_SECS: i64 = 7 * 24 * 60 * 60;
const MAX_ERROR_BYTES: usize = 512;
const NONCE_PREFIX: &str = "tiygate-probe";
const GLOBAL_PROBE_CONCURRENCY: usize = 4;
const PROVIDER_PROBE_CONCURRENCY: usize = 2;
const ACCOUNT_PROBE_CONCURRENCY: usize = 1;
const DEFAULT_TARGET_DAILY_PROBE_BUDGET: u64 = 64;
const DEFAULT_GLOBAL_DAILY_PROBE_BUDGET: u64 = 512;

static GLOBAL_PROBE_GATE: OnceLock<Arc<DynamicGate>> = OnceLock::new();
static PROVIDER_PROBE_GATES: OnceLock<dashmap::DashMap<String, Arc<DynamicGate>>> = OnceLock::new();
static ACCOUNT_PROBE_GATES: OnceLock<dashmap::DashMap<String, Arc<DynamicGate>>> = OnceLock::new();

struct DynamicGate {
    active: std::sync::atomic::AtomicUsize,
    notify: Notify,
}

struct DynamicPermit {
    gate: Arc<DynamicGate>,
}

impl Drop for DynamicPermit {
    fn drop(&mut self) {
        self.gate
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.gate.notify.notify_one();
    }
}

/// Return true only for probe IDs declared by the compiled capability
/// registry. Jobs are persisted data and must not be able to select an
/// arbitrary request shape or command.
pub fn is_allowed_probe_id(probe_id: &str) -> bool {
    tiygate_protocols::capabilities::registry()
        .iter()
        .any(|descriptor| descriptor.probe_id.as_deref() == Some(probe_id))
}

async fn acquire_probe_permits(
    store: &DbConfigStore,
    target: &tiygate_core::RoutingTarget,
    target_key: &tiygate_core::TargetKey,
    budget_weight: u64,
) -> Result<Vec<DynamicPermit>, String> {
    let global = GLOBAL_PROBE_GATE
        .get_or_init(|| {
            Arc::new(DynamicGate {
                active: std::sync::atomic::AtomicUsize::new(0),
                notify: Notify::new(),
            })
        })
        .clone();
    let provider_map = PROVIDER_PROBE_GATES.get_or_init(dashmap::DashMap::new);
    let provider = provider_map
        .entry(target.provider_id.clone())
        .or_insert_with(|| {
            Arc::new(DynamicGate {
                active: std::sync::atomic::AtomicUsize::new(0),
                notify: Notify::new(),
            })
        })
        .clone();
    let account_scope = target
        .oauth
        .as_ref()
        .and_then(|oauth| oauth.account_id.clone())
        .or_else(|| target.account_label.clone())
        .unwrap_or_else(|| "anonymous".to_string());
    let account_key = format!("{}:{account_scope}", target.provider_id);
    let account_map = ACCOUNT_PROBE_GATES.get_or_init(dashmap::DashMap::new);
    let account = account_map
        .entry(account_key)
        .or_insert_with(|| {
            Arc::new(DynamicGate {
                active: std::sync::atomic::AtomicUsize::new(0),
                notify: Notify::new(),
            })
        })
        .clone();

    let global_limit = tiygate_store::settings_keys::get_usize(
        store,
        tiygate_store::settings_keys::CAPABILITY_PROBE_GLOBAL_CONCURRENCY,
        GLOBAL_PROBE_CONCURRENCY,
    )
    .await
    .clamp(1, 64);
    let provider_limit = tiygate_store::settings_keys::get_usize(
        store,
        tiygate_store::settings_keys::CAPABILITY_PROBE_PROVIDER_CONCURRENCY,
        PROVIDER_PROBE_CONCURRENCY,
    )
    .await
    .clamp(1, 64);
    let account_limit = tiygate_store::settings_keys::get_usize(
        store,
        tiygate_store::settings_keys::CAPABILITY_PROBE_ACCOUNT_CONCURRENCY,
        ACCOUNT_PROBE_CONCURRENCY,
    )
    .await
    .clamp(1, 64);
    let global_permit = match acquire_gate(global, global_limit).await {
        Ok(permit) => permit,
        Err(error) => {
            return Err(error);
        }
    };
    let provider_permit = match acquire_gate(provider, provider_limit).await {
        Ok(permit) => permit,
        Err(error) => {
            return Err(error);
        }
    };
    let account_permit = match acquire_gate(account, account_limit).await {
        Ok(permit) => permit,
        Err(error) => {
            return Err(error);
        }
    };
    let target_budget = tiygate_store::settings_keys::get_u64(
        store,
        tiygate_store::settings_keys::CAPABILITY_PROBE_DAILY_BUDGET,
        DEFAULT_TARGET_DAILY_PROBE_BUDGET,
    )
    .await
    .min(10_000);
    let global_budget = tiygate_store::settings_keys::get_u64(
        store,
        tiygate_store::settings_keys::CAPABILITY_PROBE_GLOBAL_BUDGET,
        DEFAULT_GLOBAL_DAILY_PROBE_BUDGET,
    )
    .await
    .min(100_000);
    if !store
        .try_consume_probe_budget_with_cost(
            target_key,
            target_budget,
            global_budget,
            budget_weight.max(1),
        )
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("daily capability probe budget exhausted".to_string());
    }
    Ok(vec![global_permit, provider_permit, account_permit])
}

async fn acquire_gate(gate: Arc<DynamicGate>, limit: usize) -> Result<DynamicPermit, String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = gate.active.load(std::sync::atomic::Ordering::Acquire);
            if current >= limit {
                gate.notify.notified().await;
                continue;
            }
            if gate
                .active
                .compare_exchange(
                    current,
                    current.saturating_add(1),
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(DynamicPermit { gate });
            }
        }
    })
    .await
    .map_err(|_| "probe concurrency permit timeout".to_string())?
}

#[derive(Debug, Clone)]
enum ProbeOutcome {
    Positive(CapabilityObservation),
    ExplicitNegative(CapabilityObservation),
    Inconclusive {
        capability_id: CapabilityId,
        reason: String,
    },
    Error {
        class: String,
        detail: String,
    },
}

#[derive(Debug, Clone)]
enum ProbeRequestError {
    Auth(String),
    Transient(String),
}

impl ProbeRequestError {
    fn into_parts(self) -> (&'static str, String) {
        match self {
            Self::Auth(detail) => ("auth", detail),
            Self::Transient(detail) => ("transient", detail),
        }
    }
}

#[derive(Debug, Clone)]
struct ProbeApplyResult {
    partial: bool,
    retryable_error: Option<(String, String)>,
    terminal_error: Option<(String, String)>,
}

/// Owned handle for the durable probe worker. The worker stops claiming new
/// jobs as soon as the signal is sent and is bounded by a short join timeout
/// during application shutdown.
pub struct ProbeWorkerHandle {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl ProbeWorkerHandle {
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let mut join = self.join;
        if tokio::time::timeout(Duration::from_secs(5), &mut join)
            .await
            .is_err()
        {
            join.abort();
            let _ = join.await;
        }
    }
}

/// Spawn the durable probe worker when a DB-backed control plane is present.
pub fn spawn_worker(state: AppState) -> Option<ProbeWorkerHandle> {
    let store = state.db_store.clone()?;
    let worker_id = std::env::var(PROBE_WORKER_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("probe-{}", uuid::Uuid::now_v7()));
    let (stop, stop_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        run_worker(state, store, worker_id, stop_rx).await;
    });
    Some(ProbeWorkerHandle { stop, join })
}

async fn run_worker(
    state: AppState,
    store: Arc<DbConfigStore>,
    worker_id: String,
    mut stop_rx: watch::Receiver<bool>,
) {
    // Backfill targets that predate capability discovery or were restored from
    // an older config export.  This runs once in the background; each store
    // operation is idempotent and the request path is never involved.
    if let Err(error) = bootstrap_missing_targets(&state, &store).await {
        tracing::warn!(error = %error, "capability probe bootstrap failed");
    }
    loop {
        if *stop_rx.borrow() {
            break;
        }
        let enabled = tiygate_store::settings_keys::get_bool(
            store.as_ref(),
            tiygate_store::settings_keys::CAPABILITY_PROBE_ENABLED,
            true,
        )
        .await;
        if !enabled {
            if wait_or_stop(&mut stop_rx, Duration::from_secs(PROBE_POLL_SECS)).await {
                break;
            }
            continue;
        }
        let job = match store.claim_probe_job(&worker_id, PROBE_LEASE_SECS).await {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(error = %error, "probe worker claim failed");
                if wait_or_stop(&mut stop_rx, Duration::from_secs(PROBE_POLL_SECS)).await {
                    break;
                }
                continue;
            }
        };
        let Some(job) = job else {
            if wait_or_stop(&mut stop_rx, Duration::from_secs(PROBE_POLL_SECS)).await {
                break;
            }
            continue;
        };
        if let Err(error) = execute_job(&state, &store, &worker_id, &job, &mut stop_rx).await {
            tracing::warn!(job = %job.id, error = %error, "probe worker job failed");
            let retry_at = Utc::now() + chrono::Duration::seconds(30);
            let _ = store
                .fail_probe_job(
                    &job.id,
                    &worker_id,
                    "worker",
                    &redact_error(&error),
                    retry_at,
                )
                .await;
        }
    }
}

async fn bootstrap_missing_targets(state: &AppState, store: &DbConfigStore) -> Result<(), String> {
    let targets = state
        .current_config()
        .routing_table
        .routes
        .values()
        .flat_map(|entry| entry.targets.iter().cloned())
        .collect::<Vec<_>>();
    for target in targets {
        let probe_set = tiygate_store::capabilities::default_probe_set_for_target(&target);
        store
            .ensure_target_capability(&target, &probe_set)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn wait_or_stop(stop_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => *stop_rx.borrow(),
        changed = stop_rx.changed() => {
            if changed.is_ok() {
                *stop_rx.borrow()
            } else {
                tokio::time::sleep(duration).await;
                false
            }
        },
    }
}

async fn execute_job(
    state: &AppState,
    store: &DbConfigStore,
    worker_id: &str,
    job: &ProbeJob,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    // Reconcile the worker's configuration snapshot with the durable epoch
    // before reading credentials. This closes the small window where a route
    // update could otherwise leave a queued job using an old API key/base.
    let durable_epoch = store
        .current_epoch()
        .await
        .map_err(|error| error.to_string())?;
    let snapshot_epoch = state
        .current_config()
        .snapshot()
        .map(|snapshot| snapshot.epoch);
    if snapshot_epoch != Some(durable_epoch) {
        store.refresh().await.map_err(|error| error.to_string())?;
    }
    let target = find_target(state, store, &job.target_key)?;
    let Some(target) = target else {
        store
            .complete_probe_job(&job.id, worker_id, "cancelled")
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    };
    let (current_key, _) = store
        .target_key_for(&target)
        .map_err(|error| error.to_string())?;
    if current_key != job.target_key {
        // The route/provider identity changed while this job was waiting or
        // probing. Never commit observations under a stale TargetKey.
        store
            .complete_probe_job(&job.id, worker_id, "cancelled")
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let mut outcomes = Vec::new();
    let probe_judge_version = job
        .probe_set
        .iter()
        .filter_map(|probe_id| {
            tiygate_protocols::capabilities::probe_manifest()
                .iter()
                .find(|probe| probe.id == probe_id)
                .map(|probe| probe.judge_version)
        })
        .max()
        .unwrap_or(PROBE_JUDGE_VERSION);
    for (probe_index, probe_id) in job
        .probe_set
        .iter()
        .enumerate()
        .skip(usize::try_from(job.next_probe_index.max(0)).unwrap_or(usize::MAX))
    {
        if !is_allowed_probe_id(probe_id) {
            return Err(format!(
                "probe id is not in the audited registry: {probe_id}"
            ));
        }
        if *stop_rx.borrow() {
            // Persist outcomes already completed in this bundle before
            // releasing the lease.  The cursor lets the next worker resume
            // at the first unexecuted probe instead of paying for duplicates.
            if !outcomes.is_empty() {
                apply_outcomes(
                    store,
                    &target,
                    &job.target_key,
                    std::mem::take(&mut outcomes),
                    probe_judge_version,
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            store
                .complete_probe_job_partial_with_progress(&job.id, worker_id, probe_index)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        if !store
            .renew_probe_lease(&job.id, worker_id, PROBE_LEASE_SECS)
            .await
            .map_err(|error| error.to_string())?
        {
            return Err("probe job lease was lost".to_string());
        }
        let budget_weight = tiygate_protocols::capabilities::probe_manifest()
            .iter()
            .find(|probe| probe.id == probe_id)
            .map(|probe| u64::from(probe.budget_weight))
            .unwrap_or(1);
        let _probe_permits =
            match acquire_probe_permits(store, &target, &job.target_key, budget_weight).await {
                Ok(permits) => permits,
                Err(error) if error.contains("daily capability probe budget exhausted") => {
                    if !outcomes.is_empty() {
                        apply_outcomes(
                            store,
                            &target,
                            &job.target_key,
                            std::mem::take(&mut outcomes),
                            probe_judge_version,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                    store
                        .defer_probe_job(
                            &job.id,
                            worker_id,
                            Utc::now() + chrono::Duration::hours(24),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
        let probe_started = std::time::Instant::now();
        let outcome = run_probe(state, &target, probe_id).await;
        let (outcome_name, error_class) = probe_outcome_summary(&outcome);
        state
            .telemetry
            .send(tiygate_core::PipelineEvent {
                request_id: job.id.clone(),
                timestamp: Utc::now(),
                stage: "capability_probe".to_string(),
                payload: tiygate_core::telemetry::EventPayload::CapabilityProbe {
                    run_id: probe_run_id(job, probe_id),
                    target: job.target_key.as_str().to_string(),
                    probe_id: probe_id.clone(),
                    outcome: outcome_name.to_string(),
                    duration_micros: probe_started.elapsed().as_micros() as u64,
                    budget_weight: u32::try_from(budget_weight).unwrap_or(u32::MAX),
                    error_class: error_class.map(str::to_string),
                },
            })
            .await;
        outcomes.push(outcome);
    }
    if !store
        .renew_probe_lease(&job.id, worker_id, PROBE_LEASE_SECS)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("probe job lease was lost before result commit".to_string());
    }
    let Some(current_after) = find_target(state, store, &job.target_key)? else {
        store
            .complete_probe_job(&job.id, worker_id, "cancelled")
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    };
    let (current_after_key, _) = store
        .target_key_for(&current_after)
        .map_err(|error| error.to_string())?;
    if current_after_key != job.target_key {
        store
            .complete_probe_job(&job.id, worker_id, "cancelled")
            .await
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let apply_result = apply_outcomes(
        store,
        &target,
        &job.target_key,
        outcomes,
        probe_judge_version,
    )
    .await
    .map_err(|error| error.to_string())?;
    if let Some((class, detail)) = apply_result.retryable_error {
        // A transient transport failure gets at most one retry. Auth and
        // rate-limit errors are terminal for this execution and require a
        // credential/config change or a later explicit probe; they must not
        // burn the whole job retry budget in a tight loop.
        if class == "transient" && job.attempt_count < 2 {
            let retry_at = Utc::now() + chrono::Duration::seconds(30);
            store
                .fail_probe_job(&job.id, worker_id, &class, &detail, retry_at)
                .await
                .map_err(|error| error.to_string())?;
        } else {
            store
                .complete_probe_job(&job.id, worker_id, "failed")
                .await
                .map_err(|error| error.to_string())?;
        }
    } else if let Some((class, detail)) = apply_result.terminal_error {
        store
            .complete_probe_job_with_error(&job.id, worker_id, "failed", &class, &detail)
            .await
            .map_err(|error| error.to_string())?;
    } else {
        if apply_result.partial {
            // An accepted but semantically inconclusive bundle is retried
            // after a delay. Keeping `partial` runnable immediately would
            // spin the worker and exhaust the probe budget while the target
            // is returning ordinary text instead of a deterministic call.
            store
                .defer_partial_probe_job(
                    &job.id,
                    worker_id,
                    Utc::now() + chrono::Duration::hours(1),
                )
                .await
                .map_err(|error| error.to_string())?;
        } else {
            store
                .complete_probe_job(&job.id, worker_id, "complete")
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn probe_outcome_summary(outcome: &ProbeOutcome) -> (&'static str, Option<&str>) {
    match outcome {
        ProbeOutcome::Positive(_) => ("success", None),
        ProbeOutcome::ExplicitNegative(_) => ("capability_rejection", None),
        ProbeOutcome::Inconclusive { .. } => ("inconclusive", None),
        ProbeOutcome::Error { class, .. } => ("error", Some(class.as_str())),
    }
}

fn probe_run_id(job: &ProbeJob, probe_id: &str) -> String {
    let material = format!(
        "probe-run/v1\0{}\0{}\0{}\0{}",
        job.target_key.as_str(),
        job.probe_set_hash,
        probe_id,
        job.attempt_count
    );
    format!("probe-{}", hex::encode(Sha256::digest(material.as_bytes())))
}

fn find_target(
    state: &AppState,
    store: &DbConfigStore,
    target_key: &tiygate_core::TargetKey,
) -> Result<Option<tiygate_core::RoutingTarget>, String> {
    let config = state.current_config();
    for entry in config.routing_table.routes.values() {
        for target in &entry.targets {
            let Ok((key, _)) = store.target_key_for(target) else {
                continue;
            };
            if &key == target_key {
                return Ok(Some(target.clone()));
            }
        }
    }
    Ok(None)
}

async fn run_probe(
    state: &AppState,
    target: &tiygate_core::RoutingTarget,
    probe_id: &str,
) -> ProbeOutcome {
    if !is_allowed_probe_id(probe_id) {
        return ProbeOutcome::Error {
            class: "invalid_probe".to_string(),
            detail: "probe id is not in the audited registry".to_string(),
        };
    }
    if !probe_is_applicable(target, probe_id) {
        return ProbeOutcome::Error {
            class: "invalid_probe".to_string(),
            detail: "probe is not applicable to the target endpoint or suite".to_string(),
        };
    }
    if let Err(error) = validate_probe_target(target).await {
        return ProbeOutcome::Error {
            class: "target_address".to_string(),
            detail: truncate(&error, MAX_ERROR_BYTES),
        };
    }
    let nonce = format!("{NONCE_PREFIX}-{}", uuid::Uuid::now_v7());
    if probe_id == "tools.function.continuation" {
        return run_continuation_probe(state, target, &nonce).await;
    }
    if probe_id == "tools.crl.additional_tools" {
        return run_crl_probe(state, target, &nonce).await;
    }
    let (body, capability_id, stream) = probe_body(target, probe_id, &nonce);
    let response = send_probe(state, target, body, stream, probe_timeout(probe_id)).await;
    match response {
        Ok((status, content_type, body_text)) => {
            if !(200..300).contains(&status) {
                return classify_http_error(status, &body_text, &capability_id);
            }
            let positive = if stream {
                is_valid_sse_lifecycle(content_type.as_deref(), &body_text)
            } else if probe_id == "http.basic" {
                is_valid_basic_response(target, &body_text)
            } else if probe_id == "tools.namespace" {
                has_expected_namespace_tool_call(&body_text, &nonce, "__tiygate_probe_ns")
            } else {
                has_expected_tool_call(&body_text, &nonce, probe_id)
            };
            if positive {
                if probe_id == "tools.namespace" {
                    ProbeOutcome::Positive(probe_observation_with_value(
                        &capability_id,
                        CapabilityState::Constrained,
                        Some("namespace_probe"),
                        Some(CapabilityValue::EnumSet(
                            ["__tiygate_probe_ns".to_string()].into_iter().collect(),
                        )),
                    ))
                } else {
                    ProbeOutcome::Positive(probe_observation(
                        &capability_id,
                        CapabilityState::Supported,
                        None,
                    ))
                }
            } else {
                ProbeOutcome::Inconclusive {
                    capability_id,
                    reason: "upstream accepted probe but did not produce the deterministic semantic result".to_string(),
                }
            }
        }
        Err(error) => {
            let (class, detail) = error.into_parts();
            ProbeOutcome::Error {
                class: class.to_string(),
                detail: redact_target_error(target, &detail),
            }
        }
    }
}

fn probe_is_applicable(target: &tiygate_core::RoutingTarget, probe_id: &str) -> bool {
    let Some(manifest) = tiygate_protocols::capabilities::probe_manifest()
        .iter()
        .find(|probe| probe.id == probe_id)
    else {
        return false;
    };
    let profile = tiygate_store::capabilities::wire_profile_for_target(target);
    let profile_alias = if profile.dialect == "auto" {
        match target.api_protocol.suite {
            tiygate_core::ProtocolSuite::OpenAiResponses => "openai-responses-standard",
            tiygate_core::ProtocolSuite::OpenAiCompatible
                if target.api_protocol.name.eq_ignore_ascii_case("embeddings") =>
            {
                "openai-embeddings-standard"
            }
            tiygate_core::ProtocolSuite::OpenAiCompatible => "openai-chat-standard",
            tiygate_core::ProtocolSuite::AnthropicMessages => "anthropic-messages-standard",
            tiygate_core::ProtocolSuite::GoogleGemini => "gemini-generate-content-standard",
        }
    } else {
        profile.dialect.as_str()
    };
    if !manifest.wire_profiles.iter().any(|allowed| {
        *allowed == "*"
            || *allowed == profile_alias
            || (*allowed == "*generation"
                && !target.api_protocol.name.eq_ignore_ascii_case("embeddings"))
            || (*allowed == "*embeddings"
                && target.api_protocol.name.eq_ignore_ascii_case("embeddings"))
    }) {
        return false;
    }
    if probe_id == "http.basic" {
        return true;
    }
    let generation_endpoint = matches!(
        target.api_protocol.name.as_str(),
        "chat-completions" | "responses" | "messages" | "generateContent"
    );
    if !generation_endpoint {
        return false;
    }
    match probe_id {
        "transport.sse" | "tools.function" | "tools.choice.required" | "tools.choice.specific" => {
            true
        }
        // The executor currently has a continuation fixture only for
        // Responses/Chat-compatible wires. Other generation protocols are
        // still applicable to the bundle and must report Inconclusive from
        // the protocol-specific runner rather than an invalid-probe error.
        "tools.function.continuation" => true,
        "tools.namespace" | "tools.custom" | "tools.crl.additional_tools" => {
            target.api_protocol.suite == tiygate_core::ProtocolSuite::OpenAiResponses
        }
        _ => false,
    }
}

fn is_valid_basic_response(target: &tiygate_core::RoutingTarget, body: &str) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        if value.get("error").is_some() {
            return false;
        }
        if !value.is_object() {
            return false;
        }
        return match target.api_protocol.name.as_str() {
            "embeddings" => value.get("data").is_some(),
            "messages" => value.get("id").is_some() && value.get("content").is_some(),
            "generateContent" => value.get("candidates").is_some(),
            "responses" => {
                value.get("id").is_some()
                    && (value.get("output").is_some() || value.get("status").is_some())
            }
            _ => value.get("id").is_some() || value.get("choices").is_some(),
        };
    }
    false
}

fn is_valid_sse_lifecycle(content_type: Option<&str>, body: &str) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    if !content_type
        .to_ascii_lowercase()
        .contains("text/event-stream")
    {
        return false;
    }
    let has_content = body.contains("response.output")
        || body.contains("response.output_text")
        || body.contains("choices")
        || body.contains("message.delta")
        || body.contains("content_block_delta")
        || body.contains("candidates");
    let has_terminal = body.contains("response.completed")
        || body.contains("response.done")
        || body.contains("message_stop")
        || body.contains("[DONE]")
        || body.contains("finishReason")
        || body.contains("finish_reason");
    has_content && has_terminal
}

async fn validate_probe_target(target: &tiygate_core::RoutingTarget) -> Result<(), String> {
    let url = url::Url::parse(target.effective_api_base())
        .map_err(|_| "probe target API base is not an absolute URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("probe target must use http or https".to_string());
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err("probe target URL has an invalid host or userinfo".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("probe target URL must not contain query or fragment".to_string());
    }
    let allow_private = std::env::var("TIYGATE_PROBE_ALLOW_PRIVATE_TARGETS")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes"));
    if !allow_private {
        let host = url
            .host_str()
            .ok_or_else(|| "probe target URL has no host".to_string())?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "probe target URL has no known default port".to_string())?;
        let addresses = if let Ok(address) = IpAddr::from_str(host) {
            vec![address]
        } else {
            tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::lookup_host((host, port)),
            )
            .await
            .map_err(|_| "probe target hostname resolution timed out".to_string())?
            .map_err(|_| "probe target hostname could not be resolved".to_string())?
            .map(|address| address.ip())
            .collect::<Vec<_>>()
        };
        if addresses.is_empty() {
            return Err("probe target hostname has no resolved address".to_string());
        }
        if addresses.iter().any(is_private_or_local) {
            return Err(
                "probe target resolves to a private or local address; set TIYGATE_PROBE_ALLOW_PRIVATE_TARGETS=true only for trusted local targets"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn is_private_or_local(address: &IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => is_private_ipv4(value),
        IpAddr::V6(value) => {
            value
                .to_ipv4_mapped()
                .is_some_and(|mapped| is_private_ipv4(&mapped))
                || value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || (value.segments()[0] & 0xfe00) == 0xfc00
                || (value.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn is_private_ipv4(value: &std::net::Ipv4Addr) -> bool {
    let octets = value.octets();
    value.is_private()
        || value.is_loopback()
        || value.is_link_local()
        || value.is_unspecified()
        || value.is_multicast()
        || value.is_broadcast()
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
}

async fn run_crl_probe(
    state: &AppState,
    target: &tiygate_core::RoutingTarget,
    nonce: &str,
) -> ProbeOutcome {
    let (control_body, capability_id, _) = probe_body(target, "tools.function", nonce);
    let control = match send_probe(
        state,
        target,
        control_body,
        false,
        probe_timeout("tools.crl.additional_tools"),
    )
    .await
    {
        Ok((status, _, body)) if (200..300).contains(&status) => body,
        Ok((status, _, body)) => return classify_http_error(status, &body, &capability_id),
        Err(error) => {
            let (class, detail) = error.into_parts();
            return ProbeOutcome::Error {
                class: class.to_string(),
                detail: redact_target_error(target, &detail),
            };
        }
    };
    if !has_expected_tool_call(&control, nonce, "tools.function") {
        return ProbeOutcome::Inconclusive {
            capability_id: CapabilityId::from("tools.crl.additional_tools"),
            reason: "top-level function control did not produce the expected call".to_string(),
        };
    }
    let (carrier_body, crl_id, _) = probe_body(target, "tools.crl.additional_tools", nonce);
    let carrier = match send_probe(
        state,
        target,
        carrier_body,
        false,
        probe_timeout("tools.crl.additional_tools"),
    )
    .await
    {
        Ok((status, _, body)) if (200..300).contains(&status) => body,
        Ok((status, _, body)) => return classify_http_error(status, &body, &crl_id),
        Err(error) => {
            let (class, detail) = error.into_parts();
            return ProbeOutcome::Error {
                class: class.to_string(),
                detail: redact_target_error(target, &detail),
            };
        }
    };
    if has_expected_tool_call(&carrier, nonce, "tools.crl.additional_tools") {
        return ProbeOutcome::Positive(probe_observation(
            &crl_id,
            CapabilityState::Supported,
            None,
        ));
    }

    // A second controlled run with a fresh nonce distinguishes a stable
    // carrier omission from one stochastic model response.  A pair of
    // accepted-but-ignored experiments is still recorded as a negative only
    // because the successful top-level control isolates the carrier variable.
    let second_nonce = format!("{NONCE_PREFIX}-{}", uuid::Uuid::now_v7());
    let (second_body, _, _) = probe_body(target, "tools.crl.additional_tools", &second_nonce);
    let second = match send_probe(
        state,
        target,
        second_body,
        false,
        probe_timeout("tools.crl.additional_tools"),
    )
    .await
    {
        Ok((status, _, body)) if (200..300).contains(&status) => body,
        Ok((status, _, body)) => return classify_http_error(status, &body, &crl_id),
        Err(error) => {
            let (class, detail) = error.into_parts();
            return ProbeOutcome::Error {
                class: class.to_string(),
                detail: redact_target_error(target, &detail),
            };
        }
    };
    if !has_expected_tool_call(&second, &second_nonce, "tools.crl.additional_tools") {
        ProbeOutcome::ExplicitNegative(probe_observation(
            &crl_id,
            CapabilityState::Unsupported,
            Some("controlled_carrier_ignored"),
        ))
    } else {
        ProbeOutcome::Inconclusive {
            capability_id: crl_id,
            reason: "CRL carrier result was not stable across controlled runs".to_string(),
        }
    }
}

async fn run_continuation_probe(
    state: &AppState,
    target: &tiygate_core::RoutingTarget,
    nonce: &str,
) -> ProbeOutcome {
    let (first_body, capability_id, _) = probe_body(target, "tools.function", nonce);
    let first = match send_probe(
        state,
        target,
        first_body,
        false,
        probe_timeout("tools.function.continuation"),
    )
    .await
    {
        Ok((status, _, body)) if (200..300).contains(&status) => body,
        Ok((status, _, body)) => return classify_http_error(status, &body, &capability_id),
        Err(error) => {
            let (class, detail) = error.into_parts();
            return ProbeOutcome::Error {
                class: class.to_string(),
                detail: redact_target_error(target, &detail),
            };
        }
    };
    let Ok(first_json) = serde_json::from_str::<Value>(&first) else {
        return ProbeOutcome::Inconclusive {
            capability_id,
            reason: "function probe response was not valid JSON".to_string(),
        };
    };
    if !has_expected_tool_call(&first, nonce, "tools.function") {
        return ProbeOutcome::Inconclusive {
            capability_id,
            reason: "function control did not return the expected tool name and nonce".to_string(),
        };
    }
    let Some(call_id) = find_call_id(&first_json) else {
        return ProbeOutcome::Inconclusive {
            capability_id,
            reason: "function probe did not return a call id".to_string(),
        };
    };
    let mut continuation_body = match target.api_protocol.suite {
        tiygate_core::ProtocolSuite::OpenAiResponses => json!({
            "model": target.model_id,
            "input": [
                {"type": "function_call", "call_id": call_id, "name": "__tiygate_probe", "arguments": format!("{{\"nonce\":\"{nonce}\"}}")},
                {"type": "function_call_output", "call_id": call_id, "output": "probe-ok"}
            ],
            "stream": false,
            "max_output_tokens": 32
        }),
        tiygate_core::ProtocolSuite::OpenAiCompatible => json!({
            "model": target.model_id,
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": call_id, "type": "function", "function": {"name": "__tiygate_probe", "arguments": format!("{{\"nonce\":\"{nonce}\"}}")}}]},
                {"role": "tool", "tool_call_id": call_id, "content": "probe-ok"}
            ],
            "stream": false,
            "max_tokens": 32
        }),
        _ => {
            return ProbeOutcome::Inconclusive {
                capability_id,
                reason: "continuation probe is not implemented for this wire suite".to_string(),
            }
        }
    };
    if target.api_protocol.suite == tiygate_core::ProtocolSuite::OpenAiResponses {
        if let Some(response_id) = first_json.get("id").and_then(Value::as_str) {
            continuation_body["previous_response_id"] = Value::String(response_id.to_string());
        }
    }
    match send_probe(
        state,
        target,
        continuation_body,
        false,
        probe_timeout("tools.function.continuation"),
    )
    .await
    {
        Ok((status, _, body)) if (200..300).contains(&status) => {
            if has_final_message(&body) {
                ProbeOutcome::Positive(probe_observation(
                    &capability_id,
                    CapabilityState::Supported,
                    None,
                ))
            } else {
                ProbeOutcome::Inconclusive {
                    capability_id,
                    reason: "continuation accepted but no final message was observed".to_string(),
                }
            }
        }
        Ok((status, _, body)) => classify_http_error(status, &body, &capability_id),
        Err(error) => {
            let (class, detail) = error.into_parts();
            ProbeOutcome::Error {
                class: class.to_string(),
                detail: redact_target_error(target, &detail),
            }
        }
    }
}

fn has_final_message(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    fn visit(value: &Value) -> bool {
        match value {
            Value::Array(items) => items.iter().any(visit),
            Value::Object(object) => {
                if object.get("error").is_some() {
                    return false;
                }
                if object.get("type").and_then(Value::as_str) == Some("message")
                    || object.get("choices").and_then(Value::as_array).is_some()
                {
                    return true;
                }
                if let Some(output) = object.get("output").and_then(Value::as_array) {
                    if output.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("message")
                            || item.get("content").and_then(Value::as_array).is_some_and(
                                |content| {
                                    content.iter().any(|part| {
                                        matches!(
                                            part.get("type").and_then(Value::as_str),
                                            Some("output_text") | Some("text")
                                        )
                                    })
                                },
                            )
                    }) {
                        return true;
                    }
                }
                object.values().any(visit)
            }
            _ => false,
        }
    }
    visit(&value)
}

fn find_call_id(value: &Value) -> Option<String> {
    match value {
        Value::Array(items) => items.iter().find_map(find_call_id),
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("function_call") | Some("custom_tool_call")
            ) {
                if let Some(call_id) = object.get("call_id").and_then(Value::as_str) {
                    return Some(call_id.to_string());
                }
            }
            if let Some(items) = object.get("tool_calls").and_then(Value::as_array) {
                if let Some(call_id) = items
                    .first()
                    .and_then(|item| item.get("id"))
                    .and_then(Value::as_str)
                {
                    return Some(call_id.to_string());
                }
            }
            object.values().find_map(find_call_id)
        }
        _ => None,
    }
}

fn probe_body(
    target: &tiygate_core::RoutingTarget,
    probe_id: &str,
    nonce: &str,
) -> (Value, CapabilityId, bool) {
    let capability_id = match probe_id {
        "http.basic" => "transport.http",
        "transport.sse" => "transport.sse",
        "tools.function" => "tools.function",
        "tools.function.continuation" => "tools.function.continuation",
        "tools.choice.required" => "tools.choice.required",
        "tools.choice.specific" => "tools.choice.specific",
        "tools.namespace" => "tools.namespace",
        "tools.custom" => "tools.custom",
        "tools.crl.additional_tools" => "tools.crl.additional_tools",
        _ => "transport.http",
    };
    let stream = probe_id == "transport.sse";
    let prompt = format!("Capability probe {nonce}. Respond only with the requested result.");
    let function = json!({
        "type": "function",
        "name": "__tiygate_probe",
        "description": format!("Call this function and include nonce {nonce} in the nonce argument."),
        "parameters": {
            "type": "object",
            "properties": {"nonce": {"type": "string"}},
            "required": ["nonce"],
            "additionalProperties": false
        }
    });
    let mut body = match target.api_protocol.suite {
        tiygate_core::ProtocolSuite::OpenAiResponses => json!({
            "model": target.model_id,
            "input": prompt,
            "stream": stream,
            "max_output_tokens": 32
        }),
        tiygate_core::ProtocolSuite::OpenAiCompatible
            if target.api_protocol.name == "embeddings" =>
        {
            json!({
                "model": target.model_id,
                "input": "capability probe"
            })
        }
        tiygate_core::ProtocolSuite::OpenAiCompatible => json!({
            "model": target.model_id,
            "messages": [{"role": "user", "content": prompt}],
            "stream": stream,
            "max_tokens": 32
        }),
        tiygate_core::ProtocolSuite::AnthropicMessages => json!({
            "model": target.model_id,
            "max_tokens": 32,
            "stream": stream,
            "messages": [{"role": "user", "content": prompt}]
        }),
        tiygate_core::ProtocolSuite::GoogleGemini => json!({
            "contents": [{"role": "user", "parts": [{"text": prompt}]}]
        }),
    };

    if probe_id == "http.basic" || probe_id == "transport.sse" {
        return (body, CapabilityId::from(capability_id), stream);
    }

    match target.api_protocol.suite {
        tiygate_core::ProtocolSuite::OpenAiResponses => {
            let tools = if probe_id == "tools.namespace" {
                json!([{
                    "type": "namespace",
                    "name": "__tiygate_probe_ns",
                    "tools": [function]
                }])
            } else if probe_id == "tools.custom" {
                json!([{
                    "type": "custom",
                    "name": "__tiygate_probe_custom",
                    "description": format!("Return nonce {nonce} as free text.")
                }])
            } else {
                json!([function])
            };
            if probe_id == "tools.crl.additional_tools" {
                body["input"] = json!([
                    {"role": "user", "content": prompt},
                    {"type": "additional_tools", "role": "developer", "tools": tools}
                ]);
            } else {
                body["tools"] = tools;
            }
            if probe_id == "tools.function" || probe_id == "tools.function.continuation" {
                body["tool_choice"] = json!({"type": "function", "name": "__tiygate_probe"});
            } else if probe_id == "tools.choice.required" {
                body["tool_choice"] = json!("required");
            } else if probe_id == "tools.choice.specific" {
                body["tool_choice"] = json!({"type": "function", "name": "__tiygate_probe"});
            } else if matches!(
                probe_id,
                "tools.namespace" | "tools.custom" | "tools.crl.additional_tools"
            ) {
                body["tool_choice"] = json!("required");
            } else {
                body["tool_choice"] = json!("auto");
            }
        }
        tiygate_core::ProtocolSuite::OpenAiCompatible => {
            body["tools"] = json!([function]);
            body["tool_choice"] =
                if probe_id == "tools.function" || probe_id == "tools.function.continuation" {
                    json!({"type": "function", "function": {"name": "__tiygate_probe"}})
                } else if probe_id == "tools.choice.required" {
                    json!("required")
                } else if probe_id == "tools.choice.specific" {
                    json!({"type": "function", "function": {"name": "__tiygate_probe"}})
                } else {
                    json!("auto")
                };
        }
        tiygate_core::ProtocolSuite::AnthropicMessages => {
            body["tools"] = json!([{
                "name": "__tiygate_probe",
                "description": format!("Call this tool and include nonce {nonce}."),
                "input_schema": function["parameters"]
            }]);
            body["tool_choice"] = if probe_id == "tools.choice.required" {
                json!({"type": "any"})
            } else {
                json!({"type": "tool", "name": "__tiygate_probe"})
            };
        }
        tiygate_core::ProtocolSuite::GoogleGemini => {
            body["tools"] = json!([{
                "functionDeclarations": [{
                    "name": "__tiygate_probe",
                    "description": format!("Call this function and include nonce {nonce}."),
                    "parameters": function["parameters"]
                }]
            }]);
            body["toolConfig"] = json!({"functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": ["__tiygate_probe"]
            }});
        }
    }
    (body, CapabilityId::from(capability_id), false)
}

async fn send_probe(
    state: &AppState,
    target: &tiygate_core::RoutingTarget,
    body: Value,
    stream: bool,
    timeout: Duration,
) -> Result<(u16, Option<String>, String), ProbeRequestError> {
    let suite = target.api_protocol.suite;
    let url = match suite {
        tiygate_core::ProtocolSuite::GoogleGemini => format!(
            "{}/v1beta/models/{}:{}",
            target.effective_api_base().trim_end_matches('/'),
            target.model_id,
            if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            }
        ),
        _ => format!(
            "{}{}",
            target.effective_api_base().trim_end_matches('/'),
            probe_path_suffix(target, suite)
        ),
    };
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    super::apply_provider_auth(target, &mut headers, &state.oauth_manager)
        .await
        .map_err(|error| {
            let detail = format!("provider auth failed with HTTP {}", error.http_status());
            if error.http_status() == http::StatusCode::UNAUTHORIZED
                || error.http_status() == http::StatusCode::FORBIDDEN
            {
                ProbeRequestError::Auth(detail)
            } else {
                ProbeRequestError::Transient(detail)
            }
        })?;
    let client = if target.api_protocol.suite == tiygate_core::ProtocolSuite::AnthropicMessages
        && target.oauth.is_some()
    {
        state.tunables().anthropic_oauth_http_client.clone()
    } else {
        state.tunables().http_client.clone()
    };
    let mut request = client.post(url).headers(headers);
    if stream {
        request = request.header(http::header::ACCEPT, "text/event-stream");
    }
    let request = request.json(&body);
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| ProbeRequestError::Transient("probe request timed out".to_string()))?
        .map_err(|error| ProbeRequestError::Transient(error.to_string()))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let text = tokio::time::timeout(timeout, read_limited_body(response))
        .await
        .map_err(|_| ProbeRequestError::Transient("probe response body timed out".to_string()))?
        .map_err(ProbeRequestError::Transient)?;
    let redacted = redact_upstream_text(state, &text);
    // JSON redaction handles structured credential fields; the target-aware
    // pass also removes API keys, API-base strings and URL-like tokens from
    // plain-text upstream errors before they reach profile diagnostics.
    let redacted = redact_target_error(target, &redacted);
    Ok((status, content_type, truncate(&redacted, MAX_ERROR_BYTES)))
}

fn probe_timeout(probe_id: &str) -> Duration {
    tiygate_protocols::capabilities::probe_manifest()
        .iter()
        .find(|probe| probe.id == probe_id)
        .map(|probe| Duration::from_secs(probe.timeout_secs))
        .unwrap_or(Duration::from_secs(30))
}

async fn read_limited_body(response: reqwest::Response) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(MAX_ERROR_BYTES + 1);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        let remaining = MAX_ERROR_BYTES
            .saturating_add(1)
            .saturating_sub(bytes.len());
        if remaining == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() > MAX_ERROR_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn probe_path_suffix(
    target: &tiygate_core::RoutingTarget,
    suite: tiygate_core::ProtocolSuite,
) -> &'static str {
    if suite == tiygate_core::ProtocolSuite::OpenAiCompatible {
        match target.api_protocol.name.as_str() {
            "embeddings" => "/embeddings",
            "images-generations" | "images/generations" => "/images/generations",
            "images-edits" | "images/edits" => "/images/edits",
            _ => "/chat/completions",
        }
    } else {
        suite.upstream_path_suffix().unwrap_or("/responses")
    }
}

fn redact_upstream_text(state: &AppState, text: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<Value>(text) {
        state.redactor.redact_value(&mut value);
        value.to_string()
    } else {
        text.to_string()
    }
}

fn has_expected_tool_call(body: &str, nonce: &str, probe_id: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let expected_name = if probe_id == "tools.namespace" {
        "__tiygate_probe"
    } else if probe_id == "tools.custom" {
        "__tiygate_probe_custom"
    } else {
        "__tiygate_probe"
    };
    fn visit(value: &Value, expected_name: &str, nonce: &str) -> bool {
        match value {
            Value::Array(items) => items.iter().any(|item| visit(item, expected_name, nonce)),
            Value::Object(object) => {
                let type_name = object.get("type").and_then(Value::as_str);
                let is_call = matches!(
                    type_name,
                    Some("function_call")
                        | Some("custom_tool_call")
                        | Some("tool_use")
                        | Some("functionCall")
                        | Some("function")
                ) || object.get("tool_calls").is_some()
                    || object.get("functionCall").is_some();
                if is_call {
                    let name = object
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            object
                                .get("function")
                                .and_then(|value| value.get("name"))
                                .and_then(Value::as_str)
                        })
                        .or_else(|| {
                            object
                                .get("functionCall")
                                .and_then(|value| value.get("name"))
                                .and_then(Value::as_str)
                        });
                    if name == Some(expected_name)
                        && (contains_nonce_in_field(object.get("arguments"), nonce)
                            || contains_nonce_in_field(object.get("input"), nonce)
                            || contains_nonce_in_field(object.get("args"), nonce)
                            || contains_nonce_in_field(object.get("functionCall"), nonce)
                            || contains_nonce_in_field(object.get("function"), nonce)
                            || object
                                .get("tool_calls")
                                .and_then(Value::as_array)
                                .is_some_and(|calls| {
                                    calls.iter().any(|call| visit(call, expected_name, nonce))
                                }))
                    {
                        return true;
                    }
                }
                object
                    .values()
                    .any(|child| visit(child, expected_name, nonce))
            }
            _ => false,
        }
    }
    visit(&value, expected_name, nonce)
}

fn has_expected_namespace_tool_call(body: &str, nonce: &str, namespace: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    fn visit(value: &Value, nonce: &str, namespace: &str) -> bool {
        match value {
            Value::Array(items) => items.iter().any(|item| visit(item, nonce, namespace)),
            Value::Object(object) => {
                let is_function_call = matches!(
                    object.get("type").and_then(Value::as_str),
                    Some("function_call") | Some("functionCall") | Some("function")
                ) || object.get("tool_calls").is_some();
                if is_function_call {
                    let name = object.get("name").and_then(Value::as_str).or_else(|| {
                        object
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                    });
                    let namespace_value = object
                        .get("namespace")
                        .or_else(|| object.get("namespace_path"))
                        .or_else(|| object.get("tool_namespace"))
                        .or_else(|| object.get("function").and_then(|f| f.get("namespace")));
                    let namespace_matches = namespace_value.is_some_and(|candidate| {
                        candidate.as_str() == Some(namespace)
                            || candidate.as_array().is_some_and(|items| {
                                items.iter().any(|item| item.as_str() == Some(namespace))
                            })
                            || candidate.get("name").and_then(Value::as_str) == Some(namespace)
                    });
                    if name == Some("__tiygate_probe")
                        && namespace_matches
                        && (contains_nonce_in_field(object.get("arguments"), nonce)
                            || contains_nonce_in_field(object.get("input"), nonce)
                            || contains_nonce_in_field(object.get("args"), nonce)
                            || object
                                .get("tool_calls")
                                .and_then(Value::as_array)
                                .is_some_and(|calls| {
                                    calls.iter().any(|call| visit(call, nonce, namespace))
                                }))
                    {
                        return true;
                    }
                }
                object.values().any(|child| visit(child, nonce, namespace))
            }
            _ => false,
        }
    }
    visit(&value, nonce, namespace)
}

fn contains_nonce_in_field(field: Option<&Value>, nonce: &str) -> bool {
    let Some(field) = field else {
        return false;
    };
    if field.as_str().is_some_and(|text| text.contains(nonce)) {
        return true;
    }
    match field {
        Value::Object(object) => object
            .values()
            .any(|child| contains_nonce_in_field(Some(child), nonce)),
        Value::Array(items) => items
            .iter()
            .any(|child| contains_nonce_in_field(Some(child), nonce)),
        _ => false,
    }
}

fn classify_http_error(status: u16, body: &str, capability_id: &CapabilityId) -> ProbeOutcome {
    if status == 401 || status == 403 {
        return ProbeOutcome::Error {
            class: "auth".to_string(),
            detail: truncate(body, MAX_ERROR_BYTES),
        };
    }
    if status == 429 {
        return ProbeOutcome::Error {
            class: "rate_limited".to_string(),
            detail: truncate(body, MAX_ERROR_BYTES),
        };
    }
    if status >= 500 {
        return ProbeOutcome::Error {
            class: "transient".to_string(),
            detail: truncate(body, MAX_ERROR_BYTES),
        };
    }
    if (400..500).contains(&status) && is_explicit_capability_rejection(body, capability_id) {
        return ProbeOutcome::ExplicitNegative(probe_observation(
            capability_id,
            CapabilityState::Unsupported,
            Some("explicit_capability_rejection"),
        ));
    }
    ProbeOutcome::Inconclusive {
        capability_id: capability_id.clone(),
        reason: format!("upstream returned HTTP {status} without a capability-specific error"),
    }
}

fn is_explicit_capability_rejection(body: &str, capability_id: &CapabilityId) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let error = value.get("error").unwrap_or(&value);
    let Some(object) = error.as_object() else {
        return false;
    };
    let code = object
        .get("code")
        .or_else(|| object.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let param = object
        .get("param")
        .or_else(|| object.get("field"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let unsupported_message = message.contains("unsupported")
        || message.contains("not supported")
        || message.contains("unknown field")
        || message.contains("unrecognized")
        || message.contains("not allowed")
        || message.contains("does not support");
    let code_signal = code.contains("unsupported")
        || code.contains("unknown")
        || code.contains("unrecognized")
        || code.contains("invalid_tool")
        || code.contains("unsupported_parameter")
        || (code == "invalid_request_error" && unsupported_message);
    if !code_signal {
        return false;
    }
    let expected = capability_id.as_str().replace('.', " ");
    let capability_signal = match capability_id.as_str() {
        id if id.contains("crl") || id.contains("additional_tools") => {
            ["additional_tools", "crl", "tool"]
                .iter()
                .any(|needle| param.contains(needle) || message.contains(needle))
        }
        id if id.contains("namespace") => ["namespace", "tool"]
            .iter()
            .any(|needle| param.contains(needle) || message.contains(needle)),
        id if id.contains("choice") => ["tool_choice", "required", "specific"]
            .iter()
            .any(|needle| param.contains(needle) || message.contains(needle)),
        _ => ["tool", "function"]
            .iter()
            .any(|needle| param.contains(needle) || message.contains(needle)),
    };
    capability_signal || message.contains(&expected)
}

fn probe_observation(
    capability_id: &CapabilityId,
    state: CapabilityState,
    reason: Option<&str>,
) -> CapabilityObservation {
    probe_observation_with_value(capability_id, state, reason, None)
}

fn probe_observation_with_value(
    capability_id: &CapabilityId,
    state: CapabilityState,
    reason: Option<&str>,
    value: Option<CapabilityValue>,
) -> CapabilityObservation {
    let mut observation = CapabilityObservation::now(
        capability_id.clone(),
        state,
        EvidenceSource::SemanticProbe,
        PROBE_SUITE_VERSION,
    );
    observation.value = value;
    let ttl = if state == CapabilityState::Unsupported {
        chrono::Duration::hours(6)
    } else {
        chrono::Duration::seconds(FRESH_SECS)
    };
    observation.expires_at = Some(Utc::now() + ttl);
    observation.reason_code = reason.map(str::to_string);
    observation.probe_suite_version = Some(PROBE_SUITE_VERSION);
    observation
}

async fn apply_outcomes(
    store: &DbConfigStore,
    target: &tiygate_core::RoutingTarget,
    target_key: &tiygate_core::TargetKey,
    outcomes: Vec<ProbeOutcome>,
    probe_judge_version: u32,
) -> Result<ProbeApplyResult, tiygate_store::config_store::StoreError> {
    let identity = store.target_identity(target)?;
    let existing = store.get_capability_profile(target_key).await?;
    let mut profile =
        existing.unwrap_or_else(|| TargetCapabilityProfile::pending(&identity, target_key.clone()));
    let now = Utc::now();
    let mut updated_observation = false;
    let mut partial = false;
    let mut retryable_error: Option<(String, String)> = None;
    let mut terminal_error: Option<(String, String)> = None;
    for outcome in outcomes {
        match outcome {
            ProbeOutcome::Positive(observation) | ProbeOutcome::ExplicitNegative(observation) => {
                if let Some(descriptor) =
                    tiygate_protocols::capabilities::descriptor_for(&observation.capability_id)
                {
                    if let Err(error) =
                        tiygate_core::validate_capability_observation(descriptor, &observation)
                    {
                        partial = true;
                        profile.last_probe_error_class = Some("invalid_observation".to_string());
                        profile.last_probe_error_redacted = Some(truncate(&error, MAX_ERROR_BYTES));
                        continue;
                    }
                }
                profile.observations.retain(|old| {
                    !(old.capability_id == observation.capability_id
                        && old.source == EvidenceSource::SemanticProbe)
                });
                if observation.state == CapabilityState::Supported {
                    profile.last_successful_probe_at = Some(now);
                }
                profile.observations.push(observation);
                updated_observation = true;
            }
            ProbeOutcome::Inconclusive {
                capability_id,
                reason,
            } => {
                partial = true;
                // Keep an explicit, non-routing diagnostic observation. The
                // resolver ignores Unknown observations, so this preserves the
                // distinction between "not proven" and Unsupported while
                // allowing Admin/UI to explain why the bundle is partial.
                profile.observations.retain(|old| {
                    !(old.capability_id == capability_id
                        && old.source == EvidenceSource::SemanticProbe
                        && old.state == CapabilityState::Unknown)
                });
                let mut diagnostic = CapabilityObservation::now(
                    capability_id.clone(),
                    CapabilityState::Unknown,
                    EvidenceSource::SemanticProbe,
                    PROBE_SUITE_VERSION,
                );
                diagnostic.reason_code = Some("inconclusive".to_string());
                diagnostic.redacted_detail = Some(truncate(&reason, MAX_ERROR_BYTES));
                diagnostic.expires_at = Some(now + chrono::Duration::hours(1));
                profile.observations.push(diagnostic);
                tracing::debug!(
                    capability = %capability_id,
                    reason = %reason,
                    target_key = %target_key.as_str(),
                    "capability probe inconclusive"
                );
            }
            ProbeOutcome::Error { class, detail } => {
                partial = true;
                let is_transient = class == "transient";
                profile.last_probe_error_class = Some(class.clone());
                profile.last_probe_error_redacted = Some(detail.clone());
                if is_transient && retryable_error.is_none() {
                    retryable_error = Some((
                        profile
                            .last_probe_error_class
                            .clone()
                            .unwrap_or_else(|| "transient".to_string()),
                        detail,
                    ));
                } else if !is_transient && terminal_error.is_none() {
                    terminal_error = Some((class, detail));
                }
            }
        }
    }
    let wire_profile = wire_profile_for_target(target);
    let baseline = tiygate_protocols::capabilities::baseline_for(&wire_profile);
    profile.resolved_capabilities = tiygate_core::resolve_capabilities_with_matchers(
        &baseline,
        &tiygate_protocols::capabilities::matcher_map(),
        profile.observations.clone(),
        now,
    );
    profile.schema_version = CAPABILITY_SCHEMA_VERSION;
    profile.registry_version = CAPABILITY_REGISTRY_VERSION;
    profile.baseline_version = CAPABILITY_BASELINE_VERSION;
    profile.last_probe_suite_version = Some(PROBE_SUITE_VERSION);
    profile.last_probe_judge_version = Some(probe_judge_version.max(1));
    if !partial {
        profile.last_probe_error_class = None;
        profile.last_probe_error_redacted = None;
    }
    profile.profile_status = if profile.observations.is_empty() {
        if partial {
            ProfileStatus::Partial
        } else {
            ProfileStatus::Pending
        }
    } else if partial {
        ProfileStatus::Partial
    } else {
        ProfileStatus::Ready
    };
    if updated_observation {
        let fresh_until = profile
            .observations
            .iter()
            .filter(|observation| observation.state != CapabilityState::Unknown)
            .filter_map(|observation| observation.expires_at)
            .min()
            .unwrap_or_else(|| now + chrono::Duration::seconds(FRESH_SECS));
        let stale_until = profile
            .observations
            .iter()
            .filter(|observation| observation.state != CapabilityState::Unknown)
            .filter_map(|observation| {
                observation.expires_at.map(|expires_at| {
                    expires_at
                        + if observation.state == CapabilityState::Unsupported {
                            chrono::Duration::hours(24)
                        } else {
                            chrono::Duration::seconds(STALE_SECS)
                        }
                })
            })
            .min()
            .unwrap_or_else(|| now + chrono::Duration::seconds(STALE_SECS));
        profile.fresh_until = Some(fresh_until);
        profile.stale_until = Some(stale_until);
    }
    profile.updated_at = now;
    store.upsert_capability_profile(&profile).await?;
    Ok(ProbeApplyResult {
        partial,
        retryable_error,
        terminal_error,
    })
}

fn redact_error(error: &str) -> String {
    truncate(error, MAX_ERROR_BYTES)
}

fn redact_target_error(target: &tiygate_core::RoutingTarget, error: &str) -> String {
    let mut sanitized = error.to_string();
    if !target.effective_api_base().is_empty() {
        sanitized = sanitized.replace(target.effective_api_base(), "[TARGET]");
    }
    if !target.effective_api_key().is_empty() {
        sanitized = sanitized.replace(target.effective_api_key(), "[CREDENTIAL]");
    }
    let sanitized = sanitized
        .split_whitespace()
        .map(|token| {
            if token.starts_with("http://") || token.starts_with("https://") {
                "[URL]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    redact_error(&sanitized)
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        value.to_string()
    } else {
        let mut end = max_bytes;
        while !value.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        format!("{}…", &value[..end])
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn error_classifier_does_not_turn_generic_400_into_unsupported() {
        let result = classify_http_error(
            400,
            "bad request: missing model",
            &CapabilityId::from("tools.function"),
        );
        assert!(matches!(result, ProbeOutcome::Inconclusive { .. }));
    }

    #[test]
    fn error_classifier_requires_structured_capability_field() {
        let result = classify_http_error(
            400,
            r#"{"error":{"type":"invalid_request_error","param":"tools","message":"tools are unsupported"}}"#,
            &CapabilityId::from("tools.function"),
        );
        assert!(matches!(result, ProbeOutcome::ExplicitNegative(_)));
        let result = classify_http_error(
            400,
            r#"{"error":{"type":"invalid_request_error","param":"model","message":"model is unsupported"}}"#,
            &CapabilityId::from("tools.function"),
        );
        assert!(matches!(result, ProbeOutcome::Inconclusive { .. }));
        let result = classify_http_error(
            400,
            r#"{"error":{"type":"invalid_request_error","param":"tools","message":"function schema is invalid"}}"#,
            &CapabilityId::from("tools.function"),
        );
        assert!(matches!(result, ProbeOutcome::Inconclusive { .. }));
    }

    #[test]
    fn only_registry_probe_ids_are_allowed() {
        assert!(is_allowed_probe_id("tools.function"));
        assert!(!is_allowed_probe_id("shell.exec"));
    }

    #[test]
    fn truncate_preserves_utf8() {
        assert_eq!(truncate("你好世界", 4), "你…");
    }

    #[test]
    fn basic_probe_rejects_embedded_error_response() {
        let target = test_responses_target();
        assert!(!is_valid_basic_response(
            &target,
            r#"{"error":{"type":"server_error"}}"#
        ));
        assert!(is_valid_basic_response(
            &target,
            r#"{"id":"resp_1","output":[]}"#
        ));
    }

    #[test]
    fn sse_probe_requires_content_and_terminal_frame() {
        assert!(!is_valid_sse_lifecycle(
            Some("text/event-stream"),
            "data: {\"type\":\"response.output_text.delta\"}\n\n"
        ));
        assert!(is_valid_sse_lifecycle(
            Some("text/event-stream"),
            "data: {\"type\":\"response.output_text.delta\"}\n\ndata: {\"type\":\"response.completed\"}\n\n"
        ));
    }

    #[test]
    fn continuation_probe_requires_a_structured_final_response() {
        assert!(has_final_message(r#"{"type":"message","content":[]}"#));
        assert!(has_final_message(r#"{"choices":[]}"#));
        assert!(!has_final_message(
            r#"{"error":{"type":"invalid_request_error"}}"#
        ));
        assert!(!has_final_message(
            r#"{"output":[{"type":"function_call","call_id":"call-1"}]}"#
        ));
        assert!(has_final_message(
            r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]}"#
        ));
        assert!(!has_expected_tool_call(
            r#"{"type":"message","content":[{"type":"output_text","text":"__tiygate_probe tiygate-probe-1"}]}"#,
            "tiygate-probe-1",
            "tools.function"
        ));
        assert!(has_expected_tool_call(
            r#"{"type":"function_call","name":"__tiygate_probe","arguments":"{\"nonce\":\"tiygate-probe-1\"}"}"#,
            "tiygate-probe-1",
            "tools.function"
        ));
        assert!(has_expected_tool_call(
            r#"{"choices":[{"message":{"tool_calls":[{"type":"function","function":{"name":"__tiygate_probe","arguments":"{\"nonce\":\"tiygate-probe-1\"}"}}]}}]}"#,
            "tiygate-probe-1",
            "tools.function"
        ));
        assert!(has_expected_tool_call(
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"__tiygate_probe","args":{"nonce":"tiygate-probe-1"}}}]}}]}"#,
            "tiygate-probe-1",
            "tools.function"
        ));
    }

    #[tokio::test]
    async fn private_probe_targets_are_rejected_without_override() {
        let target = tiygate_core::RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "model".to_string(),
            api_base: "http://127.0.0.1:12345/v1".to_string(),
            api_key: String::new(),
            api_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiResponses,
                "responses",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            egress_dialect_id: None,
            weight: 1.0,
            oauth: None,
        };
        let error = validate_probe_target(&target)
            .await
            .expect_err("private target must be rejected");
        assert!(error.contains("private"));
    }

    fn test_responses_target() -> tiygate_core::RoutingTarget {
        tiygate_core::RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "model".to_string(),
            api_base: "https://example.com/v1".to_string(),
            api_key: String::new(),
            api_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiResponses,
                "responses",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            egress_dialect_id: None,
            weight: 1.0,
            oauth: None,
        }
    }

    #[test]
    fn namespace_probe_requires_namespace_identity() {
        let flat = serde_json::json!({
            "output": [{"type": "function_call", "name": "__tiygate_probe", "arguments": "{\"nonce\":\"n\"}"}]
        });
        assert!(!has_expected_namespace_tool_call(
            &flat.to_string(),
            "n",
            "__tiygate_probe_ns"
        ));
        let namespaced = serde_json::json!({
            "output": [{"type": "function_call", "name": "__tiygate_probe", "namespace": "__tiygate_probe_ns", "arguments": "{\"nonce\":\"n\"}"}]
        });
        assert!(has_expected_namespace_tool_call(
            &namespaced.to_string(),
            "n",
            "__tiygate_probe_ns"
        ));
    }
}
