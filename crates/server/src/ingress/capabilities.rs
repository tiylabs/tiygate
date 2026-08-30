//! Runtime capability snapshot and reload loop.
//!
//! Profiles are persisted by `tiygate-store`, but the request path only reads
//! this atomically replaced in-memory snapshot. Reload failures leave the
//! last known snapshot intact.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashSet;
use tiygate_core::TargetKey;
use tiygate_core::{CapabilityObservation, EvidenceSource, WireProfileId};
use tiygate_store::capabilities::CapabilityRouteAdmission;
use tiygate_store::capabilities::{ProfileStatus, TargetCapabilityProfile};
use tiygate_store::config_store::DbConfigStore;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::AppState;

/// Gate policy version understood by the first enforce implementation. A
/// persisted admission created under an older/newer policy is never allowed
/// to silently authorize the request path.
pub const CURRENT_GATE_POLICY_VERSION: u32 = 1;

/// Immutable capability view consumed by request planning.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySnapshot {
    pub epoch: i64,
    /// True only after a coherent database-backed snapshot has been loaded.
    /// The default empty snapshot is intentionally not evidence and cannot
    /// enable Shadow/Enforce behavior during startup or a failed reload.
    pub loaded: bool,
    pub profiles: HashMap<TargetKey, TargetCapabilityProfile>,
    pub admissions: HashMap<(String, String), CapabilityRouteAdmission>,
}

pub type CapabilitySnapshotStore = ArcSwap<CapabilitySnapshot>;

/// Work submitted from the request path when an upstream response proves
/// that a target's capability profile is stale.  The queue is unbounded on
/// purpose: dropping this invalidation would leave a known-bad profile in
/// the resolver and could make the next request repeat the same rejection.
#[derive(Debug)]
pub enum CapabilityBackgroundCommand {
    MarkProfileStale {
        target: tiygate_core::RoutingTarget,
        error_class: String,
    },
}

/// Non-blocking handle used by request handlers to enqueue capability
/// invalidation work.  The actual store I/O is owned by
/// [`CapabilityFeedbackHandle`] and is joined during shutdown.
#[derive(Clone)]
pub struct CapabilityFeedbackDispatcher {
    tx: mpsc::UnboundedSender<CapabilityBackgroundCommand>,
    pending: Arc<DashSet<String>>,
}

impl CapabilityFeedbackDispatcher {
    pub fn enqueue(
        &self,
        target: tiygate_core::RoutingTarget,
        error_class: impl Into<String>,
    ) -> bool {
        let key = target.health_key();
        if !self.pending.insert(key.clone()) {
            return true;
        }
        if self
            .tx
            .send(CapabilityBackgroundCommand::MarkProfileStale {
                target,
                error_class: error_class.into(),
            })
            .is_ok()
        {
            true
        } else {
            self.pending.remove(&key);
            false
        }
    }
}

/// Owned lifecycle for stale-profile/reprobe feedback.  Keeping the sender,
/// stop signal and join handle together prevents a detached task from
/// surviving a graceful server shutdown.
pub struct CapabilityFeedbackHandle {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
    pub dispatcher: Arc<CapabilityFeedbackDispatcher>,
}

impl CapabilityFeedbackHandle {
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

/// Spawn the durable invalidation/reprobe worker and return its dispatcher.
pub fn spawn_feedback_worker(store: Arc<DbConfigStore>) -> CapabilityFeedbackHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<CapabilityBackgroundCommand>();
    let pending = Arc::new(DashSet::new());
    let dispatcher = Arc::new(CapabilityFeedbackDispatcher {
        tx,
        pending: pending.clone(),
    });
    let (stop, mut stop_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = stop_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    if *stop_rx.borrow() {
                        // Drain work already accepted by the dispatcher
                        // before exiting.  Each command persists the stale
                        // marker and re-probe job; a shutdown must not turn a
                        // confirmed capability rejection into silent loss.
                        while let Ok(command) = rx.try_recv() {
                            process_feedback_command(&store, &pending, command).await;
                        }
                        break;
                    }
                }
                command = rx.recv() => {
                    let Some(command) = command else { break };
                    process_feedback_command(&store, &pending, command).await;
                }
            }
        }
    });
    CapabilityFeedbackHandle {
        stop,
        join,
        dispatcher,
    }
}

async fn process_feedback_command(
    store: &DbConfigStore,
    pending: &DashSet<String>,
    command: CapabilityBackgroundCommand,
) {
    match command {
        CapabilityBackgroundCommand::MarkProfileStale {
            target,
            error_class,
        } => {
            let key = target.health_key();
            match store.target_is_referenced(&target) {
                Ok(true) => {
                    if let Err(error) = store
                        .mark_capability_profile_stale(&target, &error_class)
                        .await
                    {
                        tracing::warn!(
                            target = %target.health_key(),
                            error = %error,
                            "failed to mark capability profile stale"
                        );
                    }
                }
                Ok(false) => {
                    tracing::debug!(target = %key, "discarding stale capability feedback for deleted target");
                }
                Err(error) => {
                    tracing::warn!(target = %key, error = %error, "failed to verify capability feedback target");
                }
            }
            pending.remove(&key);
        }
    }
}

impl CapabilitySnapshot {
    pub fn profile(&self, key: &TargetKey) -> Option<&TargetCapabilityProfile> {
        self.profiles.get(key)
    }

    pub fn admission(&self, route_id: &str, shape_hash: &str) -> Option<&CapabilityRouteAdmission> {
        self.admissions
            .get(&(route_id.to_string(), shape_hash.to_string()))
    }

    pub fn shape_is_enforced(&self, route_id: &str, shape_hash: &str) -> bool {
        self.admission(route_id, shape_hash)
            .is_some_and(|admission| {
                let requirements = if admission.required_requirements.is_empty() {
                    admission
                        .required_capabilities
                        .iter()
                        .cloned()
                        .map(tiygate_core::CapabilityRequirement::required)
                        .collect::<Vec<_>>()
                } else {
                    admission.required_requirements.clone()
                };
                let shape_valid =
                    tiygate_core::capability_shape_hash_from_requirements(&requirements)
                        == shape_hash
                        && requirements.iter().all(|requirement| {
                            tiygate_protocols::capabilities::enforce_eligible_ids()
                                .contains(&requirement.id.as_str())
                        });
                let legacy_shape = admission.required_requirements.is_empty();
                let report_versions_valid = admission
                    .report
                    .get("registry_version")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(legacy_shape, |version| {
                        version
                            == u64::from(tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION)
                    })
                    && admission
                        .report
                        .get("baseline_version")
                        .and_then(serde_json::Value::as_u64)
                        .map_or(legacy_shape, |version| {
                            version
                                == u64::from(
                                    tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION,
                                )
                        })
                    && admission
                        .report
                        .get("shape_hash_version")
                        .and_then(serde_json::Value::as_str)
                        .map_or(legacy_shape, |version| {
                            version == tiygate_core::CAPABILITY_SHAPE_HASH_VERSION
                        });
                admission.mode == tiygate_core::CapabilityRoutingMode::Enforce
                    && shape_valid
                    && report_versions_valid
                    && admission.gate_policy_version == CURRENT_GATE_POLICY_VERSION
                    && admission
                        .expires_at
                        .is_none_or(|expires_at| expires_at > chrono::Utc::now())
                    && (admission
                        .report
                        .get("gate_passed")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                        || admission
                            .report
                            .get("gate_passed_by_exception")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false))
            })
    }
}

/// Build a snapshot from the durable profile table.
pub async fn load_snapshot(store: &DbConfigStore) -> Result<CapabilitySnapshot, String> {
    // Read the epoch on both sides of the paginated load. A write that lands
    // while profiles/admissions are being read must not be published under the
    // new epoch; retrying gives the watcher a coherent point-in-time view and
    // prevents a missed reload when multiple replicas update the DB.
    for _attempt in 0..3 {
        let epoch_before = store
            .current_capability_epoch()
            .await
            .map_err(|error| error.to_string())?;
        let (profiles, admissions) = load_snapshot_data(store).await?;
        let epoch_after = store
            .current_capability_epoch()
            .await
            .map_err(|error| error.to_string())?;
        if epoch_before == epoch_after {
            return Ok(CapabilitySnapshot {
                epoch: epoch_after,
                loaded: true,
                profiles,
                admissions,
            });
        }
    }
    Err("capability snapshot changed while loading".to_string())
}

async fn load_snapshot_data(
    store: &DbConfigStore,
) -> Result<
    (
        HashMap<TargetKey, TargetCapabilityProfile>,
        HashMap<(String, String), CapabilityRouteAdmission>,
    ),
    String,
> {
    let mut profiles = Vec::new();
    let mut offset = 0u32;
    loop {
        let page = store
            .list_capability_profiles(500, offset)
            .await
            .map_err(|error| error.to_string())?;
        let page_len = page.len();
        profiles.extend(page);
        if page_len < 500 {
            break;
        }
        offset = offset.saturating_add(page_len as u32);
    }
    let mut resolved_profiles = HashMap::with_capacity(profiles.len());
    for mut profile in profiles {
        let profile_schema_compatible = profile.schema_version
            == tiygate_store::capabilities::CAPABILITY_SCHEMA_VERSION
            && profile.identity_version == 1
            && profile.registry_version == tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION
            && profile.baseline_version == tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION;
        let now = chrono::Utc::now();
        let stale_grace_valid = profile
            .fresh_until
            .is_some_and(|fresh_until| fresh_until <= now)
            && profile
                .stale_until
                .is_some_and(|stale_until| stale_until > now);
        let overrides = store
            .list_capability_overrides(&profile.target_key)
            .await
            .map_err(|error| error.to_string())?;
        let mut observations = if profile_schema_compatible {
            profile.observations.clone()
        } else {
            // Keep future-version rows visible for diagnostics, but never
            // reinterpret their observations with an older resolver. Manual
            // overrides are applied below because they are independently
            // validated against the current descriptor/baseline.
            profile.profile_status = ProfileStatus::Error;
            Vec::new()
        };
        if stale_grace_valid {
            // stale-while-revalidate keeps the last verified conclusions
            // usable during the grace window. The resolver normally ignores
            // expired observations, so clear only their per-observation TTL
            // in this local snapshot copy; the durable evidence remains
            // unchanged and is still marked stale in the profile status.
            for observation in &mut observations {
                if observation
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= now)
                {
                    observation.expires_at = None;
                }
            }
        }
        for override_record in overrides {
            if override_record
                .expires_at
                .is_some_and(|expires_at| expires_at <= now)
            {
                continue;
            }
            let mut observation = CapabilityObservation::now(
                override_record.capability_id,
                override_record.state,
                EvidenceSource::ExplicitOverride,
                1,
            );
            observation.value = override_record.value;
            observation.expires_at = override_record.expires_at;
            observation.reason_code = Some("admin_override".to_string());
            observations.push(observation);
        }
        let baseline = tiygate_protocols::capabilities::baseline_for(&WireProfileId::new(
            profile.protocol_suite.clone(),
            profile.endpoint_name.clone(),
            profile.endpoint_version.clone(),
            profile.dialect_id.clone(),
        ));
        profile.resolved_capabilities = tiygate_core::resolve_capabilities_with_matchers(
            &baseline,
            &tiygate_protocols::capabilities::matcher_map(),
            observations,
            now,
        );
        if profile
            .fresh_until
            .is_some_and(|fresh_until| fresh_until <= now)
            && profile
                .stale_until
                .is_none_or(|stale_until| stale_until > now)
        {
            profile.profile_status = ProfileStatus::Stale;
        }
        resolved_profiles.insert(profile.target_key.clone(), profile);
    }
    let profiles = resolved_profiles;
    let admissions = store
        .list_all_capability_route_admissions()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|admission| {
            (
                (
                    admission.route_id.clone(),
                    admission.capability_shape_hash.clone(),
                ),
                admission,
            )
        })
        .collect();
    Ok((profiles, admissions))
}

/// Owned handle for the capability epoch watcher. Keeping the stop signal and
/// join handle together prevents the production worker from becoming a
/// detached task when the server enters graceful shutdown.
pub struct CapabilityReloaderHandle {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl CapabilityReloaderHandle {
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

/// Spawn the capability epoch watcher. It is independent from the route and
/// tunable watcher so probe results do not rebuild provider configuration.
pub fn spawn_reloader(
    store: Arc<DbConfigStore>,
    snapshot: Arc<CapabilitySnapshotStore>,
) -> CapabilityReloaderHandle {
    let (stop, mut stop_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        let mut last_epoch = -1i64;
        let mut stop_watch_closed = false;
        loop {
            if stop_watch_closed {
                tokio::time::sleep(Duration::from_secs(2)).await;
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    changed = stop_rx.changed() => {
                        match changed {
                            Ok(()) if *stop_rx.borrow() => break,
                            Ok(()) => continue,
                            Err(_) => stop_watch_closed = true,
                        }
                    }
                }
            }
            let epoch = match store.current_capability_epoch().await {
                Ok(epoch) => epoch,
                Err(error) => {
                    tracing::warn!(error = %error, "capability snapshot epoch read failed");
                    continue;
                }
            };
            if epoch == last_epoch {
                continue;
            }
            match load_snapshot(store.as_ref()).await {
                Ok(next) => {
                    last_epoch = next.epoch;
                    snapshot.store(Arc::new(next));
                }
                Err(error) => {
                    tracing::warn!(error = %error, "capability snapshot reload failed");
                }
            }
        }
    });
    CapabilityReloaderHandle { stop, join }
}

/// Install the watcher on an AppState.
pub fn install_reloader(
    store: Option<Arc<DbConfigStore>>,
    state: &AppState,
) -> Option<CapabilityReloaderHandle> {
    store.map(|store| spawn_reloader(store, state.capabilities.clone()))
}

/// Owned handle for the admission guard that automatically demotes only the
/// affected Route × shape when the persisted Shadow gate degrades.
pub struct CapabilityAdmissionGuardHandle {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl CapabilityAdmissionGuardHandle {
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

/// Poll durable Shadow metrics and conservatively demote stale enforce
/// admissions. The task is off the request path and uses optimistic revisions,
/// so a concurrent Admin update wins without being overwritten.
pub fn spawn_admission_guard(store: Arc<DbConfigStore>) -> CapabilityAdmissionGuardHandle {
    let (stop, mut stop_rx) = watch::channel(false);
    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                changed = stop_rx.changed() => {
                    if changed.is_ok() && *stop_rx.borrow() {
                        break;
                    }
                    if changed.is_err() {
                        break;
                    }
                    continue;
                }
            }
            let admissions = match store.list_all_capability_route_admissions().await {
                Ok(admissions) => admissions,
                Err(error) => {
                    tracing::warn!(error = %error, "capability admission guard list failed");
                    continue;
                }
            };
            for admission in admissions {
                if admission.mode != tiygate_core::CapabilityRoutingMode::Enforce {
                    continue;
                }
                if admission
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= chrono::Utc::now())
                {
                    let _ = store
                        .demote_capability_route_admission(
                            &admission.route_id,
                            &admission.capability_shape_hash,
                            admission.revision,
                            "admission_expired",
                        )
                        .await;
                    continue;
                }
                if admission.gate_policy_version != CURRENT_GATE_POLICY_VERSION {
                    continue;
                }
                let metrics = match tiygate_store::log_sink::oltp::list_capability_shadow_metrics(
                    store.pool(),
                    Some(&admission.route_id),
                    Some(&admission.capability_shape_hash),
                    None,
                    None,
                )
                .await
                {
                    Ok(metrics) => metrics,
                    Err(error) => {
                        tracing::warn!(route = %admission.route_id, shape = %admission.capability_shape_hash, error = %error, "capability admission guard metrics failed");
                        let _ = store
                            .demote_capability_route_admission(
                                &admission.route_id,
                                &admission.capability_shape_hash,
                                admission.revision,
                                "shadow_metrics_unavailable",
                            )
                            .await;
                        continue;
                    }
                };
                let Some(metric) = metrics.first() else {
                    // An enforce admission is valid only while its
                    // observation window can be proven.  Missing telemetry
                    // is fail-closed rather than an implicit healthy result.
                    let _ = store
                        .demote_capability_route_admission(
                            &admission.route_id,
                            &admission.capability_shape_hash,
                            admission.revision,
                            "shadow_metrics_missing",
                        )
                        .await;
                    continue;
                };
                let low_traffic_exception = admission
                    .report
                    .get("low_traffic_exception")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let low_traffic_eligible = admission
                    .report
                    .get("low_traffic_eligible")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let telemetry_gap = admission
                    .report
                    .get("telemetry_gap")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    || metric.telemetry_gap;
                let probe_terminal_error_rate = metric.probe_terminal_error_rate.max(
                    admission
                        .report
                        .get("probe_terminal_error_rate")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0),
                );
                let probe_auth_errors = metric.probe_auth_errors.max(
                    admission
                        .report
                        .get("probe_auth_error_count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                );
                let sample_gate = (metric.observation_window_complete
                    && metric.relevant_requests >= 100)
                    || (low_traffic_exception && low_traffic_eligible);
                let healthy = metric.profile_resolution_coverage >= 1.0
                    && metric.compatible_shape_coverage >= 1.0
                    && metric.planner_unknown_rate <= f64::EPSILON
                    && metric.planner_internal_error_rate <= f64::EPSILON
                    && metric.verified_success_disagreements == 0
                    && metric.verified_success_disagreement_rate <= f64::EPSILON
                    && metric.planning_latency_p95_micros <= 1_000
                    && !metric.truncated
                    && !telemetry_gap
                    && probe_terminal_error_rate <= 0.05
                    && probe_auth_errors == 0
                    && sample_gate;
                if !healthy {
                    match store
                        .demote_capability_route_admission(
                            &admission.route_id,
                            &admission.capability_shape_hash,
                            admission.revision,
                            "shadow_gate_degraded",
                        )
                        .await
                    {
                        Ok(true) => {
                            tracing::warn!(route = %admission.route_id, shape = %admission.capability_shape_hash, "capability admission automatically downgraded to shadow")
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(route = %admission.route_id, shape = %admission.capability_shape_hash, error = %error, "capability admission guard demotion failed")
                        }
                    }
                }
            }
        }
    });
    CapabilityAdmissionGuardHandle { stop, join }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_grace_keeps_last_verified_observation_in_snapshot() {
        let pool = tiygate_store::db::open_pool("sqlite::memory:")
            .await
            .expect("pool");
        tiygate_store::db::run_migrations(&pool)
            .await
            .expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let now = chrono::Utc::now();
        let mut profile = TargetCapabilityProfile::pending(
            &tiygate_core::CanonicalTargetIdentity {
                identity_version: 1,
                provider_id: "p".to_string(),
                credential_scope_fingerprint: "s".to_string(),
                canonical_api_base: "https://example.com".to_string(),
                egress_protocol_suite: "openai_responses".to_string(),
                egress_endpoint_name: "responses".to_string(),
                egress_endpoint_version: "v1".to_string(),
                egress_dialect_id: "openai-responses-standard".to_string(),
                exact_model_id: "m".to_string(),
            },
            tiygate_core::TargetKey("stale-target".to_string()),
        );
        let mut observation = CapabilityObservation::now(
            "tools.function",
            tiygate_core::CapabilityState::Supported,
            EvidenceSource::SemanticProbe,
            1,
        );
        observation.expires_at = Some(now - chrono::Duration::hours(1));
        profile.observations = vec![observation];
        profile.fresh_until = Some(now - chrono::Duration::minutes(1));
        profile.stale_until = Some(now + chrono::Duration::hours(1));
        profile.profile_status = ProfileStatus::Ready;
        store
            .upsert_capability_profile(&profile)
            .await
            .expect("profile");
        let snapshot = load_snapshot(&store).await.expect("snapshot");
        let resolved = snapshot
            .profile(&tiygate_core::TargetKey("stale-target".to_string()))
            .expect("profile")
            .resolved_capabilities
            .get(&tiygate_core::CapabilityId::from("tools.function"));
        assert_eq!(resolved.state, tiygate_core::CapabilityState::Supported);
        assert_eq!(
            snapshot
                .profile(&tiygate_core::TargetKey("stale-target".to_string()))
                .expect("profile")
                .profile_status,
            ProfileStatus::Stale
        );
    }
}
