//! Persistent target-capability profiles, overrides and probe jobs.
//!
//! The data plane never queries these tables directly.  The server publishes
//! the records into an in-memory snapshot and uses the durable job table for
//! restart-safe, multi-replica probing.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use tiygate_core::{
    canonicalize_api_base, target_instance_id, target_key, BaselineSupport,
    CanonicalTargetIdentity, CapabilityId, CapabilityObservation, CapabilityRequirement,
    CapabilityState, RequirementStrength, ResolvedTargetCapabilities, TargetInstanceId, TargetKey,
    WireProfileId,
};

use crate::config_store::{DbConfigStore, StoreError};

pub const CAPABILITY_SCHEMA_VERSION: u32 = 1;
pub const CAPABILITY_REGISTRY_VERSION: u32 = 1;
pub const CAPABILITY_BASELINE_VERSION: u32 = 1;
pub const PROBE_SUITE_VERSION: u32 = 1;
pub const PROBE_JUDGE_VERSION: u32 = 1;
const SUCCESSFUL_TRAFFIC_FRESH_SECS: i64 = 24 * 60 * 60;
const SUCCESSFUL_TRAFFIC_STALE_SECS: i64 = 7 * 24 * 60 * 60;

/// Profile lifecycle persisted independently from target health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    Pending,
    Partial,
    Ready,
    Stale,
    Error,
}

impl ProfileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Partial => "partial",
            Self::Ready => "ready",
            Self::Stale => "stale",
            Self::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "partial" => Some(Self::Partial),
            "ready" => Some(Self::Ready),
            "stale" => Some(Self::Stale),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// A persisted capability profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetCapabilityProfile {
    pub target_key: TargetKey,
    pub identity_version: u32,
    pub provider_id: String,
    pub credential_scope_fingerprint: String,
    pub canonical_api_base: String,
    pub protocol_suite: String,
    pub endpoint_name: String,
    pub endpoint_version: String,
    pub dialect_id: String,
    pub model_id: String,
    pub schema_version: u32,
    pub registry_version: u32,
    pub baseline_version: u32,
    pub profile_status: ProfileStatus,
    pub resolved_capabilities: ResolvedTargetCapabilities,
    pub observations: Vec<CapabilityObservation>,
    pub last_probe_suite_version: Option<u32>,
    pub last_probe_judge_version: Option<u32>,
    pub last_successful_probe_at: Option<DateTime<Utc>>,
    pub last_probe_error_class: Option<String>,
    pub last_probe_error_redacted: Option<String>,
    pub fresh_until: Option<DateTime<Utc>>,
    pub stale_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TargetCapabilityProfile {
    /// Create a pending profile shell for a target identity.
    pub fn pending(identity: &CanonicalTargetIdentity, key: TargetKey) -> Self {
        let now = Utc::now();
        Self {
            target_key: key,
            identity_version: identity.identity_version,
            provider_id: identity.provider_id.clone(),
            credential_scope_fingerprint: identity.credential_scope_fingerprint.clone(),
            canonical_api_base: identity.canonical_api_base.clone(),
            protocol_suite: identity.egress_protocol_suite.clone(),
            endpoint_name: identity.egress_endpoint_name.clone(),
            endpoint_version: identity.egress_endpoint_version.clone(),
            dialect_id: identity.egress_dialect_id.clone(),
            model_id: identity.exact_model_id.clone(),
            schema_version: CAPABILITY_SCHEMA_VERSION,
            registry_version: CAPABILITY_REGISTRY_VERSION,
            baseline_version: CAPABILITY_BASELINE_VERSION,
            profile_status: ProfileStatus::Pending,
            resolved_capabilities: ResolvedTargetCapabilities::default(),
            observations: Vec::new(),
            last_probe_suite_version: None,
            last_probe_judge_version: None,
            last_successful_probe_at: None,
            last_probe_error_class: None,
            last_probe_error_redacted: None,
            fresh_until: None,
            stale_until: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// A manually configured capability conclusion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetCapabilityOverride {
    pub target_key: TargetKey,
    pub capability_id: CapabilityId,
    pub state: CapabilityState,
    pub value: Option<tiygate_core::CapabilityValue>,
    pub reason: String,
    pub actor: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Durable probe work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeJob {
    pub id: String,
    pub target_key: TargetKey,
    pub probe_set: Vec<String>,
    pub probe_set_hash: String,
    pub status: String,
    pub priority: i32,
    pub attempt_count: i32,
    pub max_attempts: i32,
    pub next_probe_index: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub lease_owner: Option<String>,
    pub lease_until: Option<DateTime<Utc>>,
    pub last_error_class: Option<String>,
    pub last_error_redacted: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Persisted rollout gate for one Route and one required capability shape.
/// Route-level mode remains the upper bound; an enforce record is required
/// before a shape can alter target selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRouteAdmission {
    pub route_id: String,
    pub capability_shape_hash: String,
    pub required_capabilities: Vec<CapabilityId>,
    /// Normalized required leaves, including typed constraints.  Older rows
    /// may omit this column and are reconstructed from `required_capabilities`
    /// as unconstrained boolean requirements.
    #[serde(default)]
    pub required_requirements: Vec<CapabilityRequirement>,
    pub mode: tiygate_core::CapabilityRoutingMode,
    pub gate_policy_version: u32,
    pub report: serde_json::Value,
    pub approved_by: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Result of reserving an Admin capability mutation idempotency key.
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityMutationIdempotency {
    /// No prior request exists; the caller owns the reservation and must
    /// complete or release it.
    New { request_hash: String },
    /// A completed request with the same key/payload can be replayed.
    Replay {
        status: u16,
        response: serde_json::Value,
    },
    /// The key was reused with another payload or a still-running request.
    Conflict(String),
}

/// Public, redacted profile summary for Admin/API consumers.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityProfileSummary {
    pub target_key: TargetKey,
    pub profile_status: ProfileStatus,
    pub dialect_id: String,
    pub supported: usize,
    pub unsupported: usize,
    pub constrained: usize,
    pub unknown: usize,
    pub fresh_until: Option<DateTime<Utc>>,
    pub stale_until: Option<DateTime<Utc>>,
}

/// Counts returned by the capability-state retention pass.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CapabilityCleanupReport {
    pub profiles_deleted: u64,
    pub overrides_deleted: u64,
    pub jobs_deleted: u64,
}

impl From<&TargetCapabilityProfile> for CapabilityProfileSummary {
    fn from(profile: &TargetCapabilityProfile) -> Self {
        let now = Utc::now();
        let profile_status = if profile
            .fresh_until
            .is_some_and(|fresh_until| fresh_until <= now)
        {
            ProfileStatus::Stale
        } else {
            profile.profile_status
        };
        let mut summary = Self {
            target_key: profile.target_key.clone(),
            profile_status,
            dialect_id: profile.dialect_id.clone(),
            supported: 0,
            unsupported: 0,
            constrained: 0,
            unknown: 0,
            fresh_until: profile.fresh_until,
            stale_until: profile.stale_until,
        };
        for capability in profile.resolved_capabilities.capabilities.values() {
            match capability.state {
                CapabilityState::Supported => summary.supported += 1,
                CapabilityState::Unsupported => summary.unsupported += 1,
                CapabilityState::Constrained => summary.constrained += 1,
                CapabilityState::Unknown => summary.unknown += 1,
            }
        }
        summary
    }
}

impl DbConfigStore {
    /// Check whether a runtime target identity is still referenced by the
    /// current configuration snapshot. Background stale/reprobe feedback
    /// must not recreate a profile after the originating route was deleted.
    pub fn target_is_referenced(
        &self,
        target: &tiygate_core::RoutingTarget,
    ) -> Result<bool, StoreError> {
        let (key, _) = self.target_key_for(target)?;
        Ok(self
            .config_store()
            .routing_table
            .routes
            .values()
            .flat_map(|entry| entry.targets.iter())
            .filter_map(|candidate| self.target_key_for(candidate).ok().map(|(key, _)| key))
            .any(|candidate_key| candidate_key == key))
    }

    /// Reserve an idempotency key for a capability-control mutation. The
    /// reservation is durable across replicas and expires after ten minutes
    /// so a crashed Admin request cannot permanently block retries.
    pub async fn begin_capability_mutation(
        &self,
        operation: &str,
        idempotency_key: &str,
        payload: &serde_json::Value,
    ) -> Result<CapabilityMutationIdempotency, StoreError> {
        let key = idempotency_key.trim();
        if key.is_empty() {
            return Ok(CapabilityMutationIdempotency::New {
                request_hash: String::new(),
            });
        }
        if key.len() > 256 {
            return Err(StoreError::Invalid(
                "idempotency key exceeds 256 bytes".to_string(),
            ));
        }
        let request_hash = hex::encode(Sha256::digest(serde_json::to_vec(payload)?));
        let now = Utc::now();
        let expires_at = now + chrono::Duration::minutes(10);
        let mut tx = self.pool.any().begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO capability_mutation_idempotency
             (operation, idempotency_key, request_hash, response_status, response_json, created_at, expires_at)
             VALUES ($1,$2,$3,NULL,NULL,$4,$5)
             ON CONFLICT(operation, idempotency_key) DO NOTHING",
        )
        .bind(operation)
        .bind(key)
        .bind(&request_hash)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() > 0 {
            tx.commit().await?;
            return Ok(CapabilityMutationIdempotency::New { request_hash });
        }
        let row = sqlx::query(
            "SELECT request_hash, response_status, response_json, expires_at
             FROM capability_mutation_idempotency
             WHERE operation=$1 AND idempotency_key=$2",
        )
        .bind(operation)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(row) = row else {
            return Ok(CapabilityMutationIdempotency::New { request_hash });
        };
        let existing_hash: String = row.get(0);
        let existing_expires: String = row.get(3);
        let expired = DateTime::parse_from_rfc3339(&existing_expires)
            .map(|value| value.with_timezone(&Utc) <= now)
            .unwrap_or(true);
        if expired {
            let result = sqlx::query(
                "UPDATE capability_mutation_idempotency
                 SET request_hash=$1, response_status=NULL, response_json=NULL,
                     created_at=$2, expires_at=$3
                 WHERE operation=$4 AND idempotency_key=$5 AND expires_at <= $2",
            )
            .bind(&request_hash)
            .bind(now.to_rfc3339())
            .bind(expires_at.to_rfc3339())
            .bind(operation)
            .bind(key)
            .execute(self.pool.any())
            .await?;
            if result.rows_affected() == 1 {
                return Ok(CapabilityMutationIdempotency::New { request_hash });
            }
        }
        if existing_hash != request_hash {
            return Ok(CapabilityMutationIdempotency::Conflict(
                "idempotency key was already used with a different payload".to_string(),
            ));
        }
        let status: Option<i64> = row.get(1);
        let response_json: Option<String> = row.get(2);
        match (status, response_json) {
            (Some(status), Some(response_json)) => Ok(CapabilityMutationIdempotency::Replay {
                status: status.clamp(100, 599) as u16,
                response: serde_json::from_str(&response_json)?,
            }),
            _ => Ok(CapabilityMutationIdempotency::Conflict(
                "idempotent mutation is still in progress".to_string(),
            )),
        }
    }

    /// Complete a previously reserved idempotency key with a bounded response.
    pub async fn complete_capability_mutation(
        &self,
        operation: &str,
        idempotency_key: &str,
        request_hash: &str,
        status: u16,
        response: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let encoded = serde_json::to_string(response)?;
        if encoded.len() > 64 * 1024 {
            return Err(StoreError::Invalid(
                "idempotent response exceeds 64 KiB".to_string(),
            ));
        }
        let result = sqlx::query(
            "UPDATE capability_mutation_idempotency
             SET response_status=$1, response_json=$2
             WHERE operation=$3 AND idempotency_key=$4 AND request_hash=$5
               AND response_status IS NULL",
        )
        .bind(i64::from(status))
        .bind(encoded)
        .bind(operation)
        .bind(idempotency_key.trim())
        .bind(request_hash)
        .execute(self.pool.any())
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::Invalid(
                "idempotency mutation reservation disappeared".to_string(),
            ));
        }
        Ok(())
    }

    /// Release a reservation when the mutation failed before producing a
    /// response, allowing the caller to retry with the same key.
    pub async fn release_capability_mutation(
        &self,
        operation: &str,
        idempotency_key: &str,
        request_hash: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM capability_mutation_idempotency
             WHERE operation=$1 AND idempotency_key=$2 AND request_hash=$3
               AND response_status IS NULL",
        )
        .bind(operation)
        .bind(idempotency_key.trim())
        .bind(request_hash)
        .execute(self.pool.any())
        .await?;
        Ok(())
    }

    /// Build the canonical identity and public TargetKey for one runtime
    /// target.  The credential material is never included in the identity;
    /// only its keyed scope fingerprint is.
    pub fn target_identity(
        &self,
        target: &tiygate_core::RoutingTarget,
    ) -> Result<CanonicalTargetIdentity, StoreError> {
        let canonical_api_base = canonicalize_api_base(target.effective_api_base())
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let scope_material = if let Some(oauth) = &target.oauth {
            let mut scopes = oauth.scopes.clone();
            scopes.sort();
            let account = oauth
                .account_id
                .as_deref()
                .or(target.account_label.as_deref())
                .unwrap_or("__provider__");
            format!(
                "oauth\0{}\0{}\0{}",
                target.provider_id,
                account,
                scopes.join(" ")
            )
        } else if !target.effective_api_key().is_empty() {
            format!("api_key\0{}", target.effective_api_key())
        } else if let Some(account) = target
            .account_label
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            // IAM/role-based targets do not carry a static key. The account
            // label is the stable principal scope configured by the route;
            // temporary session credentials are intentionally excluded.
            format!("iam\0{}\0{}", target.provider_id, account)
        } else {
            "anonymous".to_string()
        };
        let fingerprint_secret = self.fingerprint_secret.load();
        let credential_scope_fingerprint = tiygate_core::credential_scope_fingerprint(
            fingerprint_secret.as_ref().as_ref(),
            &scope_material,
        );
        let suite = format!("{:?}", target.api_protocol.suite).to_lowercase();
        Ok(CanonicalTargetIdentity {
            identity_version: 1,
            provider_id: target.provider_id.clone(),
            credential_scope_fingerprint,
            canonical_api_base,
            egress_protocol_suite: suite,
            egress_endpoint_name: target.api_protocol.name.clone(),
            egress_endpoint_version: target.api_protocol.version.clone(),
            egress_dialect_id: target.effective_egress_dialect_id().to_string(),
            exact_model_id: target.model_id.clone(),
        })
    }

    /// Compute the profile key for a runtime target.
    pub fn target_key_for(
        &self,
        target: &tiygate_core::RoutingTarget,
    ) -> Result<(TargetKey, TargetInstanceId), StoreError> {
        let identity = self.target_identity(target)?;
        Ok((target_key(&identity), target_instance_id(&identity)))
    }

    /// Ensure a profile exists for a configured target and enqueue the
    /// selected probe bundle when the profile is missing, stale, or from an
    /// older probe suite. This method is idempotent.
    pub async fn ensure_target_capability(
        &self,
        target: &tiygate_core::RoutingTarget,
        probe_set: &[String],
    ) -> Result<(TargetKey, ProbeJob), StoreError> {
        self.ensure_fingerprint_secret().await?;
        let identity = self.target_identity(target)?;
        let key = target_key(&identity);
        // Profile creation and probe-job enqueue share one transaction.  A
        // route/provider update can therefore never commit a pending profile
        // while losing its durable job to an in-memory delivery failure.
        let mut tx = self.pool.any().begin().await?;
        let capability_changed = self
            .ensure_target_capability_tx(&mut tx, target, probe_set)
            .await?;
        if capability_changed {
            self.bump_capability_epoch_tx(&mut tx).await?;
        }
        tx.commit().await?;
        let job = self
            .get_probe_job_by_target_and_hash(&key, &probe_set_hash(probe_set))
            .await?
            .ok_or_else(|| {
                StoreError::NotFound(format!(
                    "probe job missing after ensuring target {}",
                    key.as_str()
                ))
            })?;
        Ok((key, job))
    }

    /// Mark a profile stale after a target-specific capability rejection while
    /// retaining its last observations for diagnostics, then enqueue the
    /// target's normal probe bundle.
    pub async fn mark_capability_profile_stale(
        &self,
        target: &tiygate_core::RoutingTarget,
        error_class: &str,
    ) -> Result<(), StoreError> {
        self.ensure_fingerprint_secret().await?;
        let key = self.target_key_for(target)?.0;
        let now = Utc::now();
        let probes = default_probe_set_for_target(target);
        let mut tx = self.pool.any().begin().await?;
        sqlx::query(
            "UPDATE target_capability_profiles SET profile_status='stale', fresh_until=$1,
             last_probe_error_class=$2, updated_at=$3 WHERE target_key=$4",
        )
        .bind((now - chrono::Duration::seconds(1)).to_rfc3339())
        .bind(error_class)
        .bind(now.to_rfc3339())
        .bind(key.as_str())
        .execute(&mut *tx)
        .await?;
        self.ensure_target_capability_tx(&mut tx, target, &probes)
            .await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ensure a profile and probe job while the caller owns the route-write
    /// transaction. No epoch bump is performed here; the caller bumps it once
    /// before commit after all targets have been processed.
    pub async fn ensure_target_capability_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        target: &tiygate_core::RoutingTarget,
        probe_set: &[String],
    ) -> Result<bool, StoreError> {
        let identity = self.target_identity(target)?;
        let key = target_key(&identity);
        let now = Utc::now();
        let existing = sqlx::query(
            "SELECT profile_status, fresh_until, schema_version, identity_version,
                    registry_version, baseline_version, last_probe_suite_version,
                    last_probe_judge_version
             FROM target_capability_profiles WHERE target_key = $1",
        )
        .bind(key.as_str())
        .fetch_optional(&mut **tx)
        .await?;
        let needs_profile = existing.as_ref().is_none_or(|row| {
            let status: String = row.get(0);
            let fresh_until: Option<String> = row.get(1);
            let schema_version = row.get::<i64, _>(2) as u32;
            let identity_version = row.get::<i64, _>(3) as u32;
            let registry_version = row.get::<i64, _>(4) as u32;
            let baseline_version = row.get::<i64, _>(5) as u32;
            let probe_suite_version = row.get::<Option<i64>, _>(6).map(|value| value as u32);
            let probe_judge_version = row.get::<Option<i64>, _>(7).map(|value| value as u32);
            status != ProfileStatus::Ready.as_str()
                || schema_version != CAPABILITY_SCHEMA_VERSION
                || identity_version != 1
                || registry_version != CAPABILITY_REGISTRY_VERSION
                || baseline_version != CAPABILITY_BASELINE_VERSION
                || probe_suite_version != Some(PROBE_SUITE_VERSION)
                || probe_judge_version != Some(PROBE_JUDGE_VERSION)
                || fresh_until
                    .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
                    .is_none_or(|until| until.with_timezone(&Utc) <= now)
        });
        if needs_profile {
            let profile = TargetCapabilityProfile::pending(&identity, key.clone());
            let resolved = serde_json::to_string(&profile.resolved_capabilities)?;
            let observations = serde_json::to_string(&profile.observations)?;
            sqlx::query(
                "INSERT INTO target_capability_profiles
                 (target_key, identity_version, provider_id, credential_scope_fingerprint, canonical_api_base,
                 protocol_suite, endpoint_name, endpoint_version, dialect_id, model_id, schema_version,
                  registry_version, baseline_version, profile_status, resolved_capabilities_json, observations_json,
                  last_probe_suite_version, last_probe_judge_version,
                  last_successful_probe_at, last_probe_error_class, last_probe_error_redacted, fresh_until, stale_until,
                  created_at, updated_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25)
                 ON CONFLICT(target_key) DO UPDATE SET profile_status='pending', updated_at=excluded.updated_at",
            )
            .bind(key.as_str())
            .bind(i64::from(profile.identity_version))
            .bind(&profile.provider_id)
            .bind(&profile.credential_scope_fingerprint)
            .bind(&profile.canonical_api_base)
            .bind(&profile.protocol_suite)
            .bind(&profile.endpoint_name)
            .bind(&profile.endpoint_version)
            .bind(&profile.dialect_id)
            .bind(&profile.model_id)
            .bind(i64::from(profile.schema_version))
            .bind(i64::from(profile.registry_version))
            .bind(i64::from(profile.baseline_version))
            .bind(ProfileStatus::Pending.as_str())
            .bind(resolved)
            .bind(observations)
            .bind(Option::<i64>::None)
            .bind(Option::<i64>::None)
            .bind(Option::<String>::None)
            .bind(Option::<String>::None)
            .bind(Option::<String>::None)
            .bind(Option::<String>::None)
            .bind(Option::<String>::None)
            .bind(profile.created_at.to_rfc3339())
            .bind(now.to_rfc3339())
            .execute(&mut **tx)
            .await?;
        }
        let canonical = canonical_probe_set(probe_set);
        let probe_set_json = serde_json::to_string(&canonical)?;
        let probe_set_hash = probe_set_hash(probe_set);
        let id = Uuid::now_v7().to_string();
        let job_sql = if needs_profile {
            "INSERT INTO target_probe_jobs
             (id, target_key, probe_set_json, probe_set_hash, status, priority, attempt_count, max_attempts,
              next_attempt_at, lease_owner, lease_until, last_error_class, last_error_redacted, created_at, updated_at)
             VALUES ($1,$2,$3,$4,'pending',0,0,3,$5,NULL,NULL,NULL,NULL,$5,$5)
             ON CONFLICT(target_key, probe_set_hash) DO UPDATE SET
              status=CASE WHEN target_probe_jobs.status IN ('complete','partial','failed','cancelled') THEN 'pending' ELSE target_probe_jobs.status END,
              next_attempt_at=CASE WHEN target_probe_jobs.status IN ('complete','partial','failed','cancelled') THEN excluded.next_attempt_at ELSE target_probe_jobs.next_attempt_at END,
              next_probe_index=CASE WHEN target_probe_jobs.status IN ('complete','partial','failed','cancelled') THEN 0 ELSE target_probe_jobs.next_probe_index END,
              updated_at=excluded.updated_at"
        } else {
            "INSERT INTO target_probe_jobs
             (id, target_key, probe_set_json, probe_set_hash, status, priority, attempt_count, max_attempts,
              next_attempt_at, lease_owner, lease_until, last_error_class, last_error_redacted, created_at, updated_at)
             VALUES ($1,$2,$3,$4,'pending',0,0,3,$5,NULL,NULL,NULL,NULL,$5,$5)
             ON CONFLICT(target_key, probe_set_hash) DO NOTHING"
        };
        sqlx::query(job_sql)
            .bind(id)
            .bind(key.as_str())
            .bind(probe_set_json)
            .bind(probe_set_hash)
            .bind(now.to_rfc3339())
            .execute(&mut **tx)
            .await?;
        Ok(needs_profile)
    }

    /// Upsert one complete profile atomically from the caller's perspective.
    pub async fn upsert_capability_profile(
        &self,
        profile: &TargetCapabilityProfile,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.any().begin().await?;
        self.upsert_capability_profile_tx(&mut tx, profile).await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Upsert a profile inside a caller-owned transaction.  This is used by
    /// concurrent business-feedback writers so two capabilities cannot lose
    /// each other's observations between read and write.
    pub async fn upsert_capability_profile_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        profile: &TargetCapabilityProfile,
    ) -> Result<(), StoreError> {
        let resolved = serde_json::to_string(&profile.resolved_capabilities)?;
        let observations = serde_json::to_string(&profile.observations)?;
        sqlx::query(
            "INSERT INTO target_capability_profiles \
             (target_key, identity_version, provider_id, credential_scope_fingerprint, canonical_api_base, \
             protocol_suite, endpoint_name, endpoint_version, dialect_id, model_id, schema_version, \
              registry_version, baseline_version, profile_status, resolved_capabilities_json, observations_json, \
              last_probe_suite_version, last_probe_judge_version, \
              last_successful_probe_at, last_probe_error_class, last_probe_error_redacted, fresh_until, stale_until, \
              created_at, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25) \
             ON CONFLICT(target_key) DO UPDATE SET \
              identity_version=excluded.identity_version, provider_id=excluded.provider_id, \
              credential_scope_fingerprint=excluded.credential_scope_fingerprint, canonical_api_base=excluded.canonical_api_base, \
              protocol_suite=excluded.protocol_suite, endpoint_name=excluded.endpoint_name, endpoint_version=excluded.endpoint_version, \
              dialect_id=excluded.dialect_id, model_id=excluded.model_id, schema_version=excluded.schema_version, \
              registry_version=excluded.registry_version, baseline_version=excluded.baseline_version, \
              profile_status=excluded.profile_status, resolved_capabilities_json=excluded.resolved_capabilities_json, \
              observations_json=excluded.observations_json, last_probe_suite_version=excluded.last_probe_suite_version, \
              last_probe_judge_version=excluded.last_probe_judge_version, \
              last_successful_probe_at=excluded.last_successful_probe_at, last_probe_error_class=excluded.last_probe_error_class, \
              last_probe_error_redacted=excluded.last_probe_error_redacted, fresh_until=excluded.fresh_until, \
              stale_until=excluded.stale_until, updated_at=excluded.updated_at",
        )
        .bind(profile.target_key.as_str())
        .bind(i64::from(profile.identity_version))
        .bind(&profile.provider_id)
        .bind(&profile.credential_scope_fingerprint)
        .bind(&profile.canonical_api_base)
        .bind(&profile.protocol_suite)
        .bind(&profile.endpoint_name)
        .bind(&profile.endpoint_version)
        .bind(&profile.dialect_id)
        .bind(&profile.model_id)
        .bind(i64::from(profile.schema_version))
        .bind(i64::from(profile.registry_version))
        .bind(i64::from(profile.baseline_version))
        .bind(profile.profile_status.as_str())
        .bind(resolved)
        .bind(observations)
        .bind(profile.last_probe_suite_version.map(i64::from))
        .bind(profile.last_probe_judge_version.map(i64::from))
        .bind(profile.last_successful_probe_at.map(|value| value.to_rfc3339()))
        .bind(&profile.last_probe_error_class)
        .bind(&profile.last_probe_error_redacted)
        .bind(profile.fresh_until.map(|value| value.to_rfc3339()))
        .bind(profile.stale_until.map(|value| value.to_rfc3339()))
        .bind(profile.created_at.to_rfc3339())
        .bind(profile.updated_at.to_rfc3339())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Fetch one profile by TargetKey.
    pub async fn get_capability_profile(
        &self,
        target_key: &TargetKey,
    ) -> Result<Option<TargetCapabilityProfile>, StoreError> {
        let row = sqlx::query(
            "SELECT target_key, identity_version, provider_id, credential_scope_fingerprint, canonical_api_base,
            protocol_suite, endpoint_name, endpoint_version, dialect_id, model_id, schema_version,
             registry_version, baseline_version, profile_status, resolved_capabilities_json, observations_json,
             last_probe_suite_version, last_probe_judge_version,
             last_successful_probe_at, last_probe_error_class, last_probe_error_redacted, fresh_until, stale_until,
             created_at, updated_at FROM target_capability_profiles WHERE target_key = $1",
        )
        .bind(target_key.as_str())
        .fetch_optional(self.pool.any())
        .await?;
        row.map(parse_profile).transpose()
    }

    /// Fetch a bounded page of profiles for the Admin API and snapshot load.
    pub async fn list_capability_profiles(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<TargetCapabilityProfile>, StoreError> {
        let rows = sqlx::query(
            "SELECT target_key, identity_version, provider_id, credential_scope_fingerprint, canonical_api_base,
            protocol_suite, endpoint_name, endpoint_version, dialect_id, model_id, schema_version,
             registry_version, baseline_version, profile_status, resolved_capabilities_json, observations_json,
             last_probe_suite_version, last_probe_judge_version,
             last_successful_probe_at, last_probe_error_class, last_probe_error_redacted, fresh_until, stale_until,
             created_at, updated_at FROM target_capability_profiles ORDER BY updated_at DESC LIMIT $1 OFFSET $2",
        )
        .bind(i64::from(limit.clamp(1, 500)))
        .bind(i64::from(offset))
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(parse_profile).collect()
    }

    pub async fn count_capability_profiles(&self) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM target_capability_profiles")
            .fetch_one(self.pool.any())
            .await?;
        Ok(count.max(0) as u64)
    }

    /// Upsert a manual override and advance the capability epoch.
    pub async fn upsert_capability_override(
        &self,
        override_record: &TargetCapabilityOverride,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.any().begin().await?;
        self.upsert_capability_override_tx(&mut tx, override_record)
            .await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically persist an override, advance the capability epoch and append
    /// its audit record.  The Admin layer uses this for capability-control
    /// writes so an audit failure cannot leave a successful mutation without
    /// history.
    pub async fn upsert_capability_override_with_audit(
        &self,
        override_record: &TargetCapabilityOverride,
        audit_target_id: &str,
        audit_details: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.any().begin().await?;
        self.upsert_capability_override_tx(&mut tx, override_record)
            .await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        crate::audit::record_tx(
            &mut tx,
            &override_record.actor,
            "upsert",
            "target_capability_override",
            audit_target_id,
            audit_details,
        )
        .await
        .map_err(|error| StoreError::Audit(error.to_string()))?;
        tx.commit().await?;
        Ok(())
    }

    /// Upsert a capability override using a caller-owned transaction. The
    /// caller is responsible for bumping the capability epoch exactly once
    /// after all related mutations have been applied.
    pub async fn upsert_capability_override_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        override_record: &TargetCapabilityOverride,
    ) -> Result<(), StoreError> {
        let value = override_record
            .value
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        sqlx::query(
            "INSERT INTO target_capability_overrides
             (target_key, capability_id, state, value_json, reason, actor, expires_at, created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
             ON CONFLICT(target_key, capability_id) DO UPDATE SET state=excluded.state, value_json=excluded.value_json,
             reason=excluded.reason, actor=excluded.actor, expires_at=excluded.expires_at, updated_at=excluded.updated_at",
        )
        .bind(override_record.target_key.as_str())
        .bind(override_record.capability_id.as_str())
        .bind(serde_json::to_string(&override_record.state)?)
        .bind(value)
        .bind(&override_record.reason)
        .bind(&override_record.actor)
        .bind(override_record.expires_at.map(|value| value.to_rfc3339()))
        .bind(override_record.created_at.to_rfc3339())
        .bind(override_record.updated_at.to_rfc3339())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Delete one manual override.
    pub async fn delete_capability_override(
        &self,
        target_key: &TargetKey,
        capability_id: &CapabilityId,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "DELETE FROM target_capability_overrides WHERE target_key = $1 AND capability_id = $2",
        )
        .bind(target_key.as_str())
        .bind(capability_id.as_str())
        .execute(self.pool.any())
        .await?;
        if result.rows_affected() > 0 {
            self.bump_capability_epoch().await?;
        }
        Ok(result.rows_affected() > 0)
    }

    /// Atomically delete an override, advance the capability epoch and append
    /// an audit record.  A missing row rolls back without writing an audit.
    pub async fn delete_capability_override_with_audit(
        &self,
        target_key: &TargetKey,
        capability_id: &CapabilityId,
        actor: &str,
        audit_details: &serde_json::Value,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let result = sqlx::query(
            "DELETE FROM target_capability_overrides WHERE target_key = $1 AND capability_id = $2",
        )
        .bind(target_key.as_str())
        .bind(capability_id.as_str())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        self.bump_capability_epoch_tx(&mut tx).await?;
        crate::audit::record_tx(
            &mut tx,
            actor,
            "delete",
            "target_capability_override",
            capability_id.as_str(),
            audit_details,
        )
        .await
        .map_err(|error| StoreError::Audit(error.to_string()))?;
        tx.commit().await?;
        Ok(true)
    }

    /// Load non-expired overrides for a TargetKey.
    pub async fn list_capability_overrides(
        &self,
        target_key: &TargetKey,
    ) -> Result<Vec<TargetCapabilityOverride>, StoreError> {
        let rows = sqlx::query(
            "SELECT target_key, capability_id, state, value_json, reason, actor, expires_at, created_at, updated_at
             FROM target_capability_overrides WHERE target_key = $1 ORDER BY capability_id",
        )
        .bind(target_key.as_str())
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(parse_override).collect()
    }

    /// Load all capability overrides for configuration export. This method is
    /// intentionally separate from the target-detail query so callers can
    /// build a portable, non-secret target selector without exposing the
    /// rows directly to the Admin response.
    pub async fn list_all_capability_overrides(
        &self,
    ) -> Result<Vec<TargetCapabilityOverride>, StoreError> {
        let rows = sqlx::query(
            "SELECT target_key, capability_id, state, value_json, reason, actor, expires_at, created_at, updated_at
             FROM target_capability_overrides ORDER BY target_key, capability_id",
        )
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(parse_override).collect()
    }

    /// Upsert a durable probe job.  The hash gives one active row per
    /// TargetKey/probe set while retaining the last execution status.
    pub async fn enqueue_probe_job(
        &self,
        target_key: &TargetKey,
        probe_set: &[String],
        priority: i32,
        max_attempts: i32,
    ) -> Result<ProbeJob, StoreError> {
        self.enqueue_probe_job_inner(target_key, probe_set, priority, max_attempts, None)
            .await
    }

    /// Enqueue a probe job, advance the capability epoch and append an audit
    /// record atomically. Admin-triggered probes use this path so a successful
    /// 202 response cannot lose either the job or its audit trail.
    pub async fn enqueue_probe_job_with_audit(
        &self,
        target_key: &TargetKey,
        probe_set: &[String],
        priority: i32,
        max_attempts: i32,
        actor: &str,
        audit_details: &serde_json::Value,
    ) -> Result<ProbeJob, StoreError> {
        self.enqueue_probe_job_inner(
            target_key,
            probe_set,
            priority,
            max_attempts,
            Some((actor, audit_details)),
        )
        .await
    }

    async fn enqueue_probe_job_inner(
        &self,
        target_key: &TargetKey,
        probe_set: &[String],
        priority: i32,
        max_attempts: i32,
        audit: Option<(&str, &serde_json::Value)>,
    ) -> Result<ProbeJob, StoreError> {
        let canonical = canonical_probe_set(probe_set);
        let now = Utc::now();
        let probe_set_json = serde_json::to_string(&canonical)?;
        let hash = probe_set_hash(probe_set);
        let mut tx = self.pool.any().begin().await?;
        let existing = sqlx::query(
            "SELECT id FROM target_probe_jobs WHERE target_key = $1 AND probe_set_hash = $2",
        )
        .bind(target_key.as_str())
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?;
        let id = existing
            .map(|row| row.get::<String, _>(0))
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        sqlx::query(
            "INSERT INTO target_probe_jobs
             (id, target_key, probe_set_json, probe_set_hash, status, priority, attempt_count, max_attempts,
              next_attempt_at, lease_owner, lease_until, last_error_class, last_error_redacted, created_at, updated_at)
             VALUES ($1,$2,$3,$4,'pending',$5,0,$6,$7,NULL,NULL,NULL,NULL,$8,$8)
             ON CONFLICT(target_key, probe_set_hash) DO UPDATE SET
              probe_set_json=excluded.probe_set_json, priority=excluded.priority, max_attempts=excluded.max_attempts,
              status=CASE WHEN target_probe_jobs.status IN ('complete','partial','failed','cancelled') THEN 'pending' ELSE target_probe_jobs.status END,
              next_attempt_at=CASE WHEN target_probe_jobs.status IN ('complete','partial','failed','cancelled') THEN excluded.next_attempt_at ELSE target_probe_jobs.next_attempt_at END,
              next_probe_index=CASE WHEN target_probe_jobs.status IN ('complete','partial','failed','cancelled') THEN 0 ELSE target_probe_jobs.next_probe_index END,
              updated_at=excluded.updated_at",
        )
        .bind(&id)
        .bind(target_key.as_str())
        .bind(probe_set_json)
        .bind(&hash)
        .bind(priority)
        .bind(max_attempts.max(1))
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        if let Some((actor, details)) = audit {
            crate::audit::record_tx(
                &mut tx,
                actor,
                "enqueue",
                "target_capability_probe",
                &format!("{}:{hash}", target_key.as_str()),
                details,
            )
            .await
            .map_err(|error| StoreError::Audit(error.to_string()))?;
        }
        tx.commit().await?;
        if let Some(job) = self.get_probe_job(&id).await? {
            return Ok(job);
        }
        // A concurrent enqueue may have won the unique
        // (target_key, probe_set_hash) race with a different generated UUID.
        // Return the durable winner instead of reporting a false NotFound.
        self.get_probe_job_by_target_and_hash(target_key, &hash)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("probe job {id}")))
    }

    pub async fn get_probe_job(&self, id: &str) -> Result<Option<ProbeJob>, StoreError> {
        let row = sqlx::query(
            "SELECT id, target_key, probe_set_json, probe_set_hash, status, priority, attempt_count,
             max_attempts, next_probe_index, next_attempt_at, lease_owner, lease_until, last_error_class,
             last_error_redacted, created_at, updated_at FROM target_probe_jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool.any())
        .await?;
        row.map(parse_job).transpose()
    }

    /// Return the most recently updated probe job for a target. This is used by
    /// the Admin detail view so operators can see pending/running/failed state
    /// without knowing an internal job id or querying the database directly.
    pub async fn latest_probe_job_for_target(
        &self,
        target_key: &TargetKey,
    ) -> Result<Option<ProbeJob>, StoreError> {
        let row = sqlx::query(
            "SELECT id, target_key, probe_set_json, probe_set_hash, status, priority, attempt_count,
             max_attempts, next_probe_index, next_attempt_at, lease_owner, lease_until, last_error_class,
             last_error_redacted, created_at, updated_at FROM target_probe_jobs
             WHERE target_key = $1 ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(target_key.as_str())
        .fetch_optional(self.pool.any())
        .await?;
        row.map(parse_job).transpose()
    }

    async fn get_probe_job_by_target_and_hash(
        &self,
        target_key: &TargetKey,
        probe_set_hash: &str,
    ) -> Result<Option<ProbeJob>, StoreError> {
        let row = sqlx::query(
            "SELECT id, target_key, probe_set_json, probe_set_hash, status, priority, attempt_count,
             max_attempts, next_probe_index, next_attempt_at, lease_owner, lease_until, last_error_class,
             last_error_redacted, created_at, updated_at FROM target_probe_jobs
             WHERE target_key = $1 AND probe_set_hash = $2",
        )
        .bind(target_key.as_str())
        .bind(probe_set_hash)
        .fetch_optional(self.pool.any())
        .await?;
        row.map(parse_job).transpose()
    }

    /// Atomically claim one runnable job.  The conditional UPDATE is the
    /// cross-database coordination primitive; a losing worker observes zero
    /// rows and retries on the next poll.
    pub async fn claim_probe_job(
        &self,
        worker: &str,
        lease_seconds: u64,
    ) -> Result<Option<ProbeJob>, StoreError> {
        let now = Utc::now();
        let now_text = now.to_rfc3339();
        // Semantic-inconclusive and transient retries share the same durable
        // attempt budget. Mark exhausted runnable jobs terminal before the
        // claim so a target that keeps returning ordinary text cannot hot-loop
        // forever while consuming the daily probe quota.
        sqlx::query(
            "UPDATE target_probe_jobs SET status='failed', lease_owner=NULL, lease_until=NULL,
             last_error_class=COALESCE(last_error_class, 'attempts_exhausted'),
             last_error_redacted=COALESCE(last_error_redacted, 'probe attempt budget exhausted'),
             updated_at=$1, next_probe_index=0
             WHERE status IN ('pending','partial') AND attempt_count >= max_attempts
               AND next_attempt_at <= $1",
        )
        .bind(&now_text)
        .execute(self.pool.any())
        .await?;
        let row = sqlx::query(
            "SELECT id FROM target_probe_jobs
             WHERE ((status IN ('pending', 'partial')) AND next_attempt_at <= $1
                    AND attempt_count < max_attempts)
                OR (status = 'running' AND lease_until IS NOT NULL AND lease_until <= $1)
             ORDER BY priority DESC, next_attempt_at ASC LIMIT 1",
        )
        .bind(&now_text)
        .fetch_optional(self.pool.any())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let id: String = row.get(0);
        let lease_until = now + chrono::Duration::seconds(lease_seconds.max(1) as i64);
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status='running', lease_owner=$1, lease_until=$2,
             attempt_count=attempt_count+1, updated_at=$3
             WHERE id=$4 AND (((status IN ('pending', 'partial')) AND next_attempt_at <= $3)
                OR (status='running' AND lease_until IS NOT NULL AND lease_until <= $3))",
        )
        .bind(worker)
        .bind(lease_until.to_rfc3339())
        .bind(&now_text)
        .bind(&id)
        .execute(self.pool.any())
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_probe_job(&id).await
    }

    pub async fn renew_probe_lease(
        &self,
        id: &str,
        worker: &str,
        lease_seconds: u64,
    ) -> Result<bool, StoreError> {
        let now = Utc::now();
        let lease_until = now + chrono::Duration::seconds(lease_seconds.max(1) as i64);
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET lease_until=$1, updated_at=$2
             WHERE id=$3 AND status='running' AND lease_owner=$4",
        )
        .bind(lease_until.to_rfc3339())
        .bind(now.to_rfc3339())
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn complete_probe_job(
        &self,
        id: &str,
        worker: &str,
        status: &str,
    ) -> Result<bool, StoreError> {
        if !matches!(status, "complete" | "partial" | "failed" | "cancelled") {
            return Err(StoreError::Invalid(format!(
                "invalid probe job status: {status}"
            )));
        }
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status=$1, lease_owner=NULL, lease_until=NULL, updated_at=$2
             ,last_error_class=CASE WHEN $1 IN ('complete','partial') THEN NULL ELSE last_error_class END
             ,last_error_redacted=CASE WHEN $1 IN ('complete','partial') THEN NULL ELSE last_error_redacted END
             ,next_probe_index=CASE WHEN $1 IN ('complete','failed','cancelled') THEN 0 ELSE next_probe_index END
             WHERE id=$3 AND status='running' AND lease_owner=$4",
        )
        .bind(status)
        .bind(now)
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Persist progress for an interrupted bundle and release its lease.  The
    /// job remains `partial` and is claimed later starting at this cursor.
    pub async fn complete_probe_job_partial_with_progress(
        &self,
        id: &str,
        worker: &str,
        next_probe_index: usize,
    ) -> Result<bool, StoreError> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status='partial', next_probe_index=$1,
             lease_owner=NULL, lease_until=NULL, last_error_class=NULL,
             last_error_redacted=NULL, updated_at=$2
             WHERE id=$3 AND status='running' AND lease_owner=$4",
        )
        .bind(i64::try_from(next_probe_index).unwrap_or(i64::MAX))
        .bind(now)
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Finish a bundle whose probes were accepted but inconclusive and retry
    /// it after a bounded delay. This is distinct from an interrupted bundle:
    /// the latter resumes from a cursor immediately, while an inconclusive
    /// semantic result must not hot-loop and consume the daily budget.
    pub async fn defer_partial_probe_job(
        &self,
        id: &str,
        worker: &str,
        retry_at: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status='partial', next_probe_index=0,
             next_attempt_at=$1, lease_owner=NULL, lease_until=NULL,
             last_error_class=NULL, last_error_redacted=NULL, updated_at=$2
             WHERE id=$3 AND status='running' AND lease_owner=$4",
        )
        .bind(retry_at.to_rfc3339())
        .bind(now)
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Complete a terminal probe job while retaining the structured terminal
    /// error used by probe-error-rate aggregation.  This is separate from
    /// [`complete_probe_job`] so successful/partial completion clears stale
    /// retry diagnostics but an auth/rate-limit failure remains attributable.
    pub async fn complete_probe_job_with_error(
        &self,
        id: &str,
        worker: &str,
        status: &str,
        error_class: &str,
        redacted: &str,
    ) -> Result<bool, StoreError> {
        if !matches!(status, "failed" | "cancelled") {
            return Err(StoreError::Invalid(format!(
                "invalid terminal probe job status: {status}"
            )));
        }
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status=$1, lease_owner=NULL, lease_until=NULL,
             last_error_class=$2, last_error_redacted=$3, updated_at=$4
             ,next_probe_index=0
             WHERE id=$5 AND status='running' AND lease_owner=$6",
        )
        .bind(status)
        .bind(error_class)
        .bind(redacted)
        .bind(now)
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn fail_probe_job(
        &self,
        id: &str,
        worker: &str,
        class: &str,
        redacted: &str,
        retry_at: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let now = Utc::now().to_rfc3339();
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status=CASE WHEN attempt_count >= max_attempts THEN 'failed' ELSE 'pending' END,
             next_attempt_at=$1, lease_owner=NULL, lease_until=NULL, last_error_class=$2,
             last_error_redacted=$3, updated_at=$4
             WHERE id=$5 AND status='running' AND lease_owner=$6",
        )
        .bind(retry_at.to_rfc3339())
        .bind(class)
        .bind(redacted)
        .bind(now)
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch one Route × capability-shape admission record.
    pub async fn get_capability_route_admission(
        &self,
        route_id: &str,
        capability_shape_hash: &str,
    ) -> Result<Option<CapabilityRouteAdmission>, StoreError> {
        let row = sqlx::query(
            "SELECT route_id, capability_shape_hash, required_capabilities_json, mode,
             gate_policy_version, report_json, approved_by, approved_at, expires_at, revision,
             created_at, updated_at, required_requirements_json FROM capability_route_admissions
             WHERE route_id = $1 AND capability_shape_hash = $2",
        )
        .bind(route_id)
        .bind(capability_shape_hash)
        .fetch_optional(self.pool.any())
        .await?;
        row.map(parse_admission).transpose()
    }

    /// List shape admissions for one route. Results are bounded for Admin and
    /// snapshot callers and sorted by latest update.
    pub async fn list_capability_route_admissions(
        &self,
        route_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<CapabilityRouteAdmission>, StoreError> {
        let rows = sqlx::query(
            "SELECT route_id, capability_shape_hash, required_capabilities_json, mode,
             gate_policy_version, report_json, approved_by, approved_at, expires_at, revision,
             created_at, updated_at, required_requirements_json FROM capability_route_admissions
             WHERE route_id = $1 ORDER BY updated_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(route_id)
        .bind(i64::from(limit.clamp(1, 500)))
        .bind(i64::from(offset))
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(parse_admission).collect()
    }

    pub async fn count_capability_route_admissions(
        &self,
        route_id: &str,
    ) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM capability_route_admissions WHERE route_id = $1",
        )
        .bind(route_id)
        .fetch_one(self.pool.any())
        .await?;
        Ok(count.max(0) as u64)
    }

    /// Load all shape admissions for building the immutable data-plane
    /// snapshot. The table is bounded by Admin limits at write time, and the
    /// caller may page or reject an unexpectedly large installation before
    /// publishing if needed.
    pub async fn list_all_capability_route_admissions(
        &self,
    ) -> Result<Vec<CapabilityRouteAdmission>, StoreError> {
        let rows = sqlx::query(
            "SELECT route_id, capability_shape_hash, required_capabilities_json, mode,
             gate_policy_version, report_json, approved_by, approved_at, expires_at, revision,
             created_at, updated_at, required_requirements_json FROM capability_route_admissions ORDER BY updated_at DESC",
        )
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(parse_admission).collect()
    }

    /// Upsert a shape admission with an optional optimistic-concurrency
    /// revision. `expected_revision=None` creates or replaces only when the
    /// row does not exist; callers updating an existing row must provide its
    /// current revision.
    pub async fn upsert_capability_route_admission(
        &self,
        admission: &CapabilityRouteAdmission,
        expected_revision: Option<i64>,
    ) -> Result<CapabilityRouteAdmission, StoreError> {
        validate_capability_route_admission(admission)?;
        let now = Utc::now();
        let mut tx = self.pool.any().begin().await?;
        upsert_capability_route_admission_tx(&mut tx, admission, expected_revision, now).await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        tx.commit().await?;
        self.get_capability_route_admission(&admission.route_id, &admission.capability_shape_hash)
            .await?
            .ok_or_else(|| {
                StoreError::NotFound("shape admission disappeared after upsert".to_string())
            })
    }

    /// Atomically persist an admission, advance capability epoch and append an
    /// audit row in the same database transaction.
    pub async fn upsert_capability_route_admission_with_audit(
        &self,
        admission: &CapabilityRouteAdmission,
        expected_revision: Option<i64>,
        audit_actor: &str,
        audit_action: &str,
        audit_target_id: &str,
        audit_details: &serde_json::Value,
    ) -> Result<CapabilityRouteAdmission, StoreError> {
        validate_capability_route_admission(admission)?;
        let now = Utc::now();
        let mut tx = self.pool.any().begin().await?;
        upsert_capability_route_admission_tx(&mut tx, admission, expected_revision, now).await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        crate::audit::record_tx(
            &mut tx,
            audit_actor,
            audit_action,
            "capability_route_admission",
            audit_target_id,
            audit_details,
        )
        .await
        .map_err(|error| StoreError::Audit(error.to_string()))?;
        tx.commit().await?;
        self.get_capability_route_admission(&admission.route_id, &admission.capability_shape_hash)
            .await?
            .ok_or_else(|| {
                StoreError::NotFound("shape admission disappeared after upsert".to_string())
            })
    }

    /// Remove one shape admission and advance the capability epoch.
    pub async fn delete_capability_route_admission(
        &self,
        route_id: &str,
        capability_shape_hash: &str,
        expected_revision: Option<i64>,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let result = if let Some(expected) = expected_revision {
            sqlx::query(
                "DELETE FROM capability_route_admissions
                 WHERE route_id = $1 AND capability_shape_hash = $2 AND revision = $3",
            )
            .bind(route_id)
            .bind(capability_shape_hash)
            .bind(expected)
            .execute(&mut *tx)
            .await?
        } else {
            sqlx::query(
                "DELETE FROM capability_route_admissions
                 WHERE route_id = $1 AND capability_shape_hash = $2",
            )
            .bind(route_id)
            .bind(capability_shape_hash)
            .execute(&mut *tx)
            .await?
        };
        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        self.bump_capability_epoch_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Atomically delete an admission, advance capability epoch and append an
    /// audit row.  A missing row rolls back without an audit event.
    pub async fn delete_capability_route_admission_with_audit(
        &self,
        route_id: &str,
        capability_shape_hash: &str,
        expected_revision: Option<i64>,
        actor: &str,
        audit_details: &serde_json::Value,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let result = if let Some(expected) = expected_revision {
            sqlx::query(
                "DELETE FROM capability_route_admissions
                 WHERE route_id = $1 AND capability_shape_hash = $2 AND revision = $3",
            )
            .bind(route_id)
            .bind(capability_shape_hash)
            .bind(expected)
            .execute(&mut *tx)
            .await?
        } else {
            sqlx::query(
                "DELETE FROM capability_route_admissions
                 WHERE route_id = $1 AND capability_shape_hash = $2",
            )
            .bind(route_id)
            .bind(capability_shape_hash)
            .execute(&mut *tx)
            .await?
        };
        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        self.bump_capability_epoch_tx(&mut tx).await?;
        crate::audit::record_tx(
            &mut tx,
            actor,
            "delete",
            "capability_route_admission",
            &format!("{route_id}:{capability_shape_hash}"),
            audit_details,
        )
        .await
        .map_err(|error| StoreError::Audit(error.to_string()))?;
        tx.commit().await?;
        Ok(true)
    }

    /// Atomically move an enforce admission back to shadow after a runtime
    /// gate violation. This is used by the server-side admission guard and
    /// never deletes the historical report or audit correlation fields.
    pub async fn demote_capability_route_admission(
        &self,
        route_id: &str,
        capability_shape_hash: &str,
        expected_revision: i64,
        reason: &str,
    ) -> Result<bool, StoreError> {
        let Some(mut admission) = self
            .get_capability_route_admission(route_id, capability_shape_hash)
            .await?
        else {
            return Ok(false);
        };
        if admission.revision != expected_revision
            || admission.mode != tiygate_core::CapabilityRoutingMode::Enforce
        {
            return Ok(false);
        }
        admission.mode = tiygate_core::CapabilityRoutingMode::Shadow;
        if let Some(object) = admission.report.as_object_mut() {
            object.insert("auto_downgraded".to_string(), serde_json::Value::Bool(true));
            object.insert(
                "auto_downgrade_reason".to_string(),
                serde_json::Value::String(reason.to_string()),
            );
        }
        self.upsert_capability_route_admission(&admission, Some(expected_revision))
            .await
            .map(|_| true)
    }

    /// Mark all admissions for a removed route as shadow-only while retaining
    /// their reports for audit. Route deletion is intentionally not allowed to
    /// erase rollout history.
    ///
    /// Configuration writers should prefer [`Self::mark_route_admissions_stale_tx`]
    /// so the route/provider mutation and its capability gate invalidation are
    /// committed atomically.
    pub async fn mark_route_admissions_stale(&self, route_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let changed = self
            .mark_route_admissions_stale_tx(&mut tx, route_id, "route_or_target_changed")
            .await?;
        if changed {
            self.bump_capability_epoch_tx(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Invalidate every shape admission belonging to a route while the caller
    /// owns an existing configuration transaction. Enforce rows are demoted to
    /// shadow and all rows receive a bounded stale marker in their historical
    /// report. The helper never opens a second pooled connection and returns
    /// whether any row changed so the caller can bump capability epoch once.
    pub async fn mark_route_admissions_stale_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        route_id: &str,
        reason: &str,
    ) -> Result<bool, StoreError> {
        let rows = sqlx::query(
            "SELECT route_id, capability_shape_hash, required_capabilities_json, mode,
             gate_policy_version, report_json, approved_by, approved_at, expires_at, revision,
             created_at, updated_at, required_requirements_json FROM capability_route_admissions
             WHERE route_id = $1",
        )
        .bind(route_id)
        .fetch_all(&mut **tx)
        .await?;
        let now = Utc::now().to_rfc3339();
        let mut changed = false;
        for row in rows {
            let admission = parse_admission(row)?;
            let mut report = admission.report;
            if let Some(object) = report.as_object_mut() {
                object.insert("stale".to_string(), serde_json::Value::Bool(true));
                object.insert(
                    "stale_reason".to_string(),
                    serde_json::Value::String(reason.to_string()),
                );
            } else {
                report = serde_json::json!({
                    "stale": true,
                    "stale_reason": reason,
                });
            }
            let mode = if admission.mode == tiygate_core::CapabilityRoutingMode::Enforce {
                tiygate_core::CapabilityRoutingMode::Shadow.as_str()
            } else {
                admission.mode.as_str()
            };
            let report_json = serde_json::to_string(&report)?;
            let result = sqlx::query(
                "UPDATE capability_route_admissions
                 SET mode=$1, report_json=$2, revision=revision+1, updated_at=$3
                 WHERE route_id=$4 AND capability_shape_hash=$5 AND revision=$6",
            )
            .bind(mode)
            .bind(report_json)
            .bind(&now)
            .bind(route_id)
            .bind(&admission.capability_shape_hash)
            .bind(admission.revision)
            .execute(&mut **tx)
            .await?;
            changed |= result.rows_affected() > 0;
        }
        Ok(changed)
    }

    /// Reconcile capability identities after a provider-side OAuth metadata
    /// update (for example an account/tenant claim learned during token
    /// exchange). The stable scope fingerprint may change, so target jobs are
    /// ensured for the new identity and affected route admissions are forced
    /// back to shadow.
    pub async fn ensure_provider_target_capabilities(
        &self,
        provider_id: &str,
    ) -> Result<(), StoreError> {
        let routes = self
            .config_store()
            .snapshot()
            .map_or_else(Vec::new, |snapshot| {
                snapshot
                    .routes
                    .values()
                    .filter(|route| {
                        route
                            .targets
                            .iter()
                            .any(|target| target.provider_id == provider_id)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            });
        for route in routes {
            let runtime_targets = self
                .config_store()
                .routing_table
                .resolve(&route.virtual_model)
                .unwrap_or_default();
            let mut identity_changed = false;
            for target in runtime_targets
                .into_iter()
                .filter(|target| target.provider_id == provider_id)
            {
                let key = self.target_key_for(&target)?.0;
                identity_changed |= self.get_capability_profile(&key).await?.is_none();
                let probe_set = default_probe_set_for_target(&target);
                self.ensure_target_capability(&target, &probe_set).await?;
            }
            if identity_changed {
                self.mark_route_admissions_stale(&route.id).await?;
            }
        }
        Ok(())
    }

    /// Atomically consume one target and one global daily probe budget. A
    /// false result leaves both counters unchanged so the caller can defer
    /// the durable job until the next UTC day.
    pub async fn try_consume_probe_budget(
        &self,
        target_key: &TargetKey,
        target_limit: u64,
        global_limit: u64,
    ) -> Result<bool, StoreError> {
        self.try_consume_probe_budget_with_cost(target_key, target_limit, global_limit, 1)
            .await
    }

    /// Atomically consume a weighted amount of target and global daily probe
    /// budget. A weighted probe (for example the CRL A/B bundle) consumes more
    /// than one unit while preserving the all-or-nothing transaction.
    pub async fn try_consume_probe_budget_with_cost(
        &self,
        target_key: &TargetKey,
        target_limit: u64,
        global_limit: u64,
        cost: u64,
    ) -> Result<bool, StoreError> {
        if target_limit == 0 || global_limit == 0 || cost == 0 {
            return Ok(false);
        }
        let cost_i64 = i64::try_from(cost).unwrap_or(i64::MAX);
        let target_limit_i64 = i64::try_from(target_limit).unwrap_or(i64::MAX);
        let global_limit_i64 = i64::try_from(global_limit).unwrap_or(i64::MAX);
        let day = Utc::now().format("%Y-%m-%d").to_string();
        let now = Utc::now().to_rfc3339();
        let mut tx = self.pool.any().begin().await?;
        for scope in [target_key.as_str(), "__global__"] {
            sqlx::query(
                "INSERT INTO capability_probe_budgets (scope, day, used, updated_at)
                 VALUES ($1,$2,0,$3) ON CONFLICT(scope, day) DO NOTHING",
            )
            .bind(scope)
            .bind(&day)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        let target_result = sqlx::query(
            "UPDATE capability_probe_budgets SET used=used+$1, updated_at=$2
             WHERE scope=$3 AND day=$4 AND used <= $5-$1",
        )
        .bind(cost_i64)
        .bind(&now)
        .bind(target_key.as_str())
        .bind(&day)
        .bind(target_limit_i64)
        .execute(&mut *tx)
        .await?;
        if target_result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        let global_result = sqlx::query(
            "UPDATE capability_probe_budgets SET used=used+$1, updated_at=$2
             WHERE scope='__global__' AND day=$3 AND used <= $4-$1",
        )
        .bind(cost_i64)
        .bind(&now)
        .bind(&day)
        .bind(global_limit_i64)
        .execute(&mut *tx)
        .await?;
        if global_result.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Record a positive capability observation from a semantically verified
    /// business response. This is intentionally positive-only: a request that
    /// did not call a tool never reaches this method and cannot create a
    /// false Unsupported conclusion.
    pub async fn record_successful_capability(
        &self,
        target_key: &TargetKey,
        capability_id: &CapabilityId,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let profile_query = if self.pool.kind() == crate::db::DbKind::Postgres {
            "SELECT target_key, identity_version, provider_id, credential_scope_fingerprint, canonical_api_base,
             protocol_suite, endpoint_name, endpoint_version, dialect_id, model_id, schema_version,
             registry_version, baseline_version, profile_status, resolved_capabilities_json, observations_json,
             last_probe_suite_version, last_probe_judge_version,
             last_successful_probe_at, last_probe_error_class, last_probe_error_redacted, fresh_until, stale_until,
             created_at, updated_at FROM target_capability_profiles WHERE target_key = $1 FOR UPDATE"
        } else {
            "SELECT target_key, identity_version, provider_id, credential_scope_fingerprint, canonical_api_base,
             protocol_suite, endpoint_name, endpoint_version, dialect_id, model_id, schema_version,
             registry_version, baseline_version, profile_status, resolved_capabilities_json, observations_json,
             last_probe_suite_version, last_probe_judge_version,
             last_successful_probe_at, last_probe_error_class, last_probe_error_redacted, fresh_until, stale_until,
             created_at, updated_at FROM target_capability_profiles WHERE target_key = $1"
        };
        let Some(row) = sqlx::query(profile_query)
            .bind(target_key.as_str())
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Ok(false);
        };
        let mut profile = parse_profile(row)?;
        let now = Utc::now();
        if profile.observations.iter().any(|observation| {
            observation.capability_id == *capability_id
                && observation.source == tiygate_core::EvidenceSource::SuccessfulTraffic
                && observation.is_fresh_at(now)
        }) {
            tx.rollback().await?;
            return Ok(false);
        }
        profile.observations.retain(|observation| {
            !(observation.capability_id == *capability_id
                && observation.source == tiygate_core::EvidenceSource::SuccessfulTraffic)
        });
        let mut observation = CapabilityObservation::now(
            capability_id.clone(),
            CapabilityState::Supported,
            tiygate_core::EvidenceSource::SuccessfulTraffic,
            1,
        );
        observation.expires_at =
            Some(now + chrono::Duration::seconds(SUCCESSFUL_TRAFFIC_FRESH_SECS));
        observation.reason_code = Some("verified_business_response".to_string());
        profile.observations.push(observation);
        // The store crate must not depend on concrete protocol codecs. Keep a
        // hard baseline rejection when it has no observation, and otherwise
        // publish the verified positive observation directly. The server
        // snapshot re-applies the current protocol baseline on reload.
        let existing = profile
            .resolved_capabilities
            .capabilities
            .get(capability_id)
            .cloned();
        if existing.as_ref().is_some_and(|capability| {
            capability.state == CapabilityState::Unsupported && capability.observation.is_none()
        }) {
            tx.rollback().await?;
            return Ok(false);
        }
        let successful = profile
            .observations
            .iter()
            .find(|candidate| {
                candidate.capability_id == *capability_id
                    && candidate.source == tiygate_core::EvidenceSource::SuccessfulTraffic
            })
            .cloned();
        profile.resolved_capabilities.capabilities.insert(
            capability_id.clone(),
            tiygate_core::ResolvedCapability {
                state: CapabilityState::Supported,
                value: None,
                observation: successful,
                matcher: None,
            },
        );
        profile.fresh_until = Some(now + chrono::Duration::seconds(SUCCESSFUL_TRAFFIC_FRESH_SECS));
        profile.stale_until = Some(now + chrono::Duration::seconds(SUCCESSFUL_TRAFFIC_STALE_SECS));
        profile.schema_version = CAPABILITY_SCHEMA_VERSION;
        profile.registry_version = CAPABILITY_REGISTRY_VERSION;
        profile.baseline_version = CAPABILITY_BASELINE_VERSION;
        profile.profile_status = ProfileStatus::Ready;
        profile.last_probe_error_class = None;
        profile.last_probe_error_redacted = None;
        profile.updated_at = now;
        self.upsert_capability_profile_tx(&mut tx, &profile).await?;
        self.bump_capability_epoch_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Defer a claimed job without consuming an attempt. This is used when a
    /// daily probe budget is exhausted, allowing it to run after the next UTC
    /// rollover instead of being marked permanently failed.
    pub async fn defer_probe_job(
        &self,
        id: &str,
        worker: &str,
        retry_at: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE target_probe_jobs SET status='pending', next_attempt_at=$1,
             lease_owner=NULL, lease_until=NULL, updated_at=$2
             WHERE id=$3 AND status='running' AND lease_owner=$4",
        )
        .bind(retry_at.to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .bind(worker)
        .execute(self.pool.any())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove terminal probe state and orphaned profiles after the diagnostic
    /// retention window. A profile that is still referenced by any current
    /// route target is never removed, even if its observations are stale.
    /// The operation is transactional and deliberately leaves route-admission
    /// history intact for audit purposes.
    pub async fn cleanup_orphaned_capability_state(
        &self,
        retention_days: u32,
    ) -> Result<CapabilityCleanupReport, StoreError> {
        if retention_days == 0 {
            return Ok(CapabilityCleanupReport::default());
        }
        let runtime = self.config_store();
        let mut active_keys = std::collections::HashSet::new();
        for entry in runtime.routing_table.routes.values() {
            for target in &entry.targets {
                if let Ok((key, _)) = self.target_key_for(target) {
                    active_keys.insert(key.0);
                }
            }
        }
        let cutoff = (Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
        let mut tx = self.pool.any().begin().await?;
        let profile_rows = sqlx::query(
            "SELECT target_key, updated_at FROM target_capability_profiles WHERE updated_at < $1",
        )
        .bind(&cutoff)
        .fetch_all(&mut *tx)
        .await?;
        let mut report = CapabilityCleanupReport::default();
        for row in profile_rows {
            let target_key: String = row.get("target_key");
            if active_keys.contains(&target_key) {
                continue;
            }
            let overrides = sqlx::query(
                "DELETE FROM target_capability_overrides
                 WHERE target_key = $1 AND updated_at < $2",
            )
            .bind(&target_key)
            .bind(&cutoff)
            .execute(&mut *tx)
            .await?;
            let profile = sqlx::query(
                "DELETE FROM target_capability_profiles WHERE target_key = $1 AND updated_at < $2",
            )
            .bind(&target_key)
            .bind(&cutoff)
            .execute(&mut *tx)
            .await?;
            report.overrides_deleted = report
                .overrides_deleted
                .saturating_add(overrides.rows_affected());
            report.profiles_deleted = report
                .profiles_deleted
                .saturating_add(profile.rows_affected());
        }
        let jobs = sqlx::query(
            "DELETE FROM target_probe_jobs
             WHERE status IN ('complete','failed','cancelled') AND updated_at < $1",
        )
        .bind(&cutoff)
        .execute(&mut *tx)
        .await?;
        report.jobs_deleted = jobs.rows_affected();
        if report.profiles_deleted > 0 || report.overrides_deleted > 0 || report.jobs_deleted > 0 {
            self.bump_capability_epoch_tx(&mut tx).await?;
        }
        tx.commit().await?;
        Ok(report)
    }

    pub async fn current_capability_epoch(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT epoch FROM capability_epoch WHERE id=1")
            .fetch_optional(self.pool.any())
            .await?;
        Ok(row.map(|row| row.get::<i64, _>(0)).unwrap_or(0))
    }

    pub async fn bump_capability_epoch(&self) -> Result<i64, StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO capability_epoch (id, epoch, updated_at) VALUES (1,1,$1)
             ON CONFLICT(id) DO UPDATE SET epoch=capability_epoch.epoch+1, updated_at=excluded.updated_at",
        )
        .bind(now)
        .execute(self.pool.any())
        .await?;
        self.current_capability_epoch().await
    }

    pub async fn bump_capability_epoch_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    ) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO capability_epoch (id, epoch, updated_at) VALUES (1,1,$1)
             ON CONFLICT(id) DO UPDATE SET epoch=capability_epoch.epoch+1, updated_at=excluded.updated_at",
        )
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

fn validate_capability_route_admission(
    admission: &CapabilityRouteAdmission,
) -> Result<(Vec<CapabilityId>, Vec<CapabilityRequirement>), StoreError> {
    if !matches!(
        admission.mode,
        tiygate_core::CapabilityRoutingMode::Shadow | tiygate_core::CapabilityRoutingMode::Enforce
    ) {
        return Err(StoreError::Invalid(
            "shape admission mode must be shadow or enforce".to_string(),
        ));
    }
    if !admission.capability_shape_hash.starts_with("shape/v1:") {
        return Err(StoreError::Invalid(
            "capability shape hash must use the shape/v1 format".to_string(),
        ));
    }
    if admission.required_capabilities.len() > 64 || admission.required_requirements.len() > 64 {
        return Err(StoreError::Invalid(
            "shape admission required capability count is outside the allowed range".to_string(),
        ));
    }
    let mut requirements = if admission.required_requirements.is_empty() {
        admission
            .required_capabilities
            .iter()
            .cloned()
            .map(CapabilityRequirement::required)
            .collect::<Vec<_>>()
    } else {
        admission.required_requirements.clone()
    };
    if requirements.is_empty() {
        return Err(StoreError::Invalid(
            "shape admission required capability count is outside the allowed range".to_string(),
        ));
    }
    if requirements
        .iter()
        .any(|requirement| requirement.strength != RequirementStrength::Required)
    {
        return Err(StoreError::Invalid(
            "shape admission requirements must all be required".to_string(),
        ));
    }
    requirements.sort_by(|left, right| {
        let left_key = serde_json::to_string(left).unwrap_or_default();
        let right_key = serde_json::to_string(right).unwrap_or_default();
        left_key.cmp(&right_key)
    });
    requirements.dedup();
    let mut canonical_ids = requirements
        .iter()
        .map(|requirement| requirement.id.clone())
        .collect::<Vec<_>>();
    canonical_ids.sort();
    canonical_ids.dedup();
    if !admission.required_capabilities.is_empty() {
        let mut supplied_ids = admission.required_capabilities.clone();
        supplied_ids.sort();
        supplied_ids.dedup();
        if supplied_ids != canonical_ids {
            return Err(StoreError::Invalid(
                "shape admission required capabilities do not match requirements".to_string(),
            ));
        }
    }
    if tiygate_core::capability_shape_hash_from_requirements(&requirements)
        != admission.capability_shape_hash
    {
        return Err(StoreError::Invalid(
            "shape admission hash does not match required requirements".to_string(),
        ));
    }
    let required = serde_json::to_string(&canonical_ids)?;
    let report = serde_json::to_string(&admission.report)?;
    let required_requirements = serde_json::to_string(&requirements)?;
    if required.len() > 16 * 1024
        || required_requirements.len() > 32 * 1024
        || report.len() > 64 * 1024
    {
        return Err(StoreError::Invalid(
            "shape admission payload exceeds the size limit".to_string(),
        ));
    }
    if admission.mode == tiygate_core::CapabilityRoutingMode::Enforce {
        let gate_passed = admission
            .report
            .get("gate_passed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || admission
                .report
                .get("gate_passed_by_exception")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        if !gate_passed
            || admission
                .approved_by
                .as_deref()
                .is_none_or(|actor| actor.trim().is_empty())
            || admission.approved_at.is_none()
        {
            return Err(StoreError::Invalid(
                "enforce admission requires a passed gate report and approval metadata".to_string(),
            ));
        }
    }
    Ok((canonical_ids, requirements))
}

async fn upsert_capability_route_admission_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    admission: &CapabilityRouteAdmission,
    expected_revision: Option<i64>,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let (canonical_ids, canonical_requirements) = validate_capability_route_admission(admission)?;
    let required = serde_json::to_string(&canonical_ids)?;
    let required_requirements = serde_json::to_string(&canonical_requirements)?;
    let report = serde_json::to_string(&admission.report)?;
    let current = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM capability_route_admissions
         WHERE route_id = $1 AND capability_shape_hash = $2",
    )
    .bind(&admission.route_id)
    .bind(&admission.capability_shape_hash)
    .fetch_optional(&mut **tx)
    .await?;
    match (current, expected_revision) {
        (Some(current), Some(expected)) if current != expected => {
            return Err(StoreError::Invalid(format!(
                "shape admission revision conflict (expected {expected}, current {current})"
            )));
        }
        (Some(_), None) => {
            return Err(StoreError::Invalid(
                "shape admission revision is required for update".to_string(),
            ));
        }
        (None, Some(expected)) if expected != 0 => {
            return Err(StoreError::Invalid(
                "shape admission does not exist for the requested revision".to_string(),
            ));
        }
        _ => {}
    }
    let revision = current.unwrap_or(0).saturating_add(1);
    let write_result = sqlx::query(
        "INSERT INTO capability_route_admissions
         (route_id, capability_shape_hash, required_capabilities_json, mode,
          gate_policy_version, report_json, approved_by, approved_at, expires_at, revision,
          created_at, updated_at, required_requirements_json)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
         ON CONFLICT(route_id, capability_shape_hash) DO UPDATE SET
          required_capabilities_json=excluded.required_capabilities_json,
          mode=excluded.mode, gate_policy_version=excluded.gate_policy_version,
          report_json=excluded.report_json, approved_by=excluded.approved_by,
          approved_at=excluded.approved_at, expires_at=excluded.expires_at,
          revision=excluded.revision, updated_at=excluded.updated_at,
          required_requirements_json=excluded.required_requirements_json
         WHERE capability_route_admissions.revision = $14",
    )
    .bind(&admission.route_id)
    .bind(&admission.capability_shape_hash)
    .bind(required)
    .bind(admission.mode.as_str())
    .bind(i64::from(admission.gate_policy_version))
    .bind(report)
    .bind(&admission.approved_by)
    .bind(admission.approved_at.map(|value| value.to_rfc3339()))
    .bind(admission.expires_at.map(|value| value.to_rfc3339()))
    .bind(revision)
    .bind(admission.created_at.to_rfc3339())
    .bind(now.to_rfc3339())
    .bind(required_requirements)
    .bind(current.unwrap_or(0))
    .execute(&mut **tx)
    .await?;
    if write_result.rows_affected() == 0 {
        return Err(StoreError::Invalid(
            "shape admission update lost its revision race".to_string(),
        ));
    }
    if let Some(expected) = expected_revision {
        let changed = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM capability_route_admissions
             WHERE route_id = $1 AND capability_shape_hash = $2",
        )
        .bind(&admission.route_id)
        .bind(&admission.capability_shape_hash)
        .fetch_optional(&mut **tx)
        .await?;
        if changed != Some(expected.saturating_add(1)) {
            return Err(StoreError::Invalid(
                "shape admission update lost its revision race".to_string(),
            ));
        }
    }
    Ok(())
}

/// Compute a canonical probe-set representation for dedupe and hashing.
pub fn canonical_probe_set(probe_set: &[String]) -> Vec<String> {
    let mut result = probe_set.to_vec();
    result.sort();
    result.dedup();
    result
}

/// Hash a probe set together with the suite version. Changing a probe's
/// request shape or judge semantics must create a new durable job instead of
/// reusing an observation produced by an older suite.
pub fn probe_set_hash(probe_set: &[String]) -> String {
    probe_set_hash_for_versions(probe_set, PROBE_SUITE_VERSION, PROBE_JUDGE_VERSION)
}

/// Hash helper exposed for migration/property tests so a future suite version
/// can be proven to invalidate the previous durable job identity.
pub fn probe_set_hash_for_version(probe_set: &[String], suite_version: u32) -> String {
    probe_set_hash_for_versions(probe_set, suite_version, PROBE_JUDGE_VERSION)
}

pub fn probe_set_hash_for_versions(
    probe_set: &[String],
    suite_version: u32,
    judge_version: u32,
) -> String {
    let payload = serde_json::json!({
        "probe_suite_version": suite_version,
        "probe_judge_version": judge_version,
        "probe_ids": canonical_probe_set(probe_set),
    });
    let encoded = serde_json::to_vec(&payload).unwrap_or_default();
    hex::encode(Sha256::digest(encoded))
}

/// Build a wire profile from a runtime Target and its selected dialect.
pub fn wire_profile_for_target(target: &tiygate_core::RoutingTarget) -> WireProfileId {
    WireProfileId::new(
        format!("{:?}", target.api_protocol.suite).to_lowercase(),
        target.api_protocol.name.clone(),
        target.api_protocol.version.clone(),
        target.effective_egress_dialect_id(),
    )
}

/// Select the first-phase probe bundle for a runtime target.
pub fn default_probe_set_for_target(target: &tiygate_core::RoutingTarget) -> Vec<String> {
    let generation_endpoint = matches!(
        target.api_protocol.name.as_str(),
        "chat-completions" | "responses" | "messages" | "generateContent"
    );
    let mut probes = if generation_endpoint {
        vec!["http.basic".to_string(), "transport.sse".to_string()]
    } else {
        vec!["http.basic".to_string()]
    };
    // Tool probes are opt-in because they invoke the model and may incur
    // provider cost. A route write only needs transport/auth evidence; the
    // Admin manual-probe endpoint or an explicit private Responses dialect can
    // request the tool bundle. This keeps the default discovery cheap and
    // avoids claiming capabilities for models that were never asked to use a
    // tool.
    if generation_endpoint && target.effective_egress_dialect_id() == "openai-responses-codex-lite"
    {
        probes.extend(
            [
                "tools.function",
                "tools.function.continuation",
                "tools.choice.required",
                "tools.choice.specific",
                "tools.namespace",
                "tools.custom",
                "tools.crl.additional_tools",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    probes
}

/// Build the operator-requested probe bundle from a persisted profile when no
/// live routing target is available (Admin detail endpoint). The manual bundle
/// is broader than the cheap route-write default, but never sends tool probes
/// to an embeddings endpoint.
pub fn manual_probe_set_for_profile(profile: &TargetCapabilityProfile) -> Vec<String> {
    if profile.endpoint_name.eq_ignore_ascii_case("embeddings")
        || profile
            .protocol_suite
            .eq_ignore_ascii_case("openai_embeddings")
    {
        return vec!["http.basic".to_string()];
    }
    let mut probes = vec!["http.basic".to_string(), "transport.sse".to_string()];
    match profile.protocol_suite.as_str() {
        "openai_responses" | "openairesponses" | "openai-responses" => probes.extend(
            [
                "tools.function",
                "tools.function.continuation",
                "tools.choice.required",
                "tools.choice.specific",
                "tools.namespace",
                "tools.custom",
                "tools.crl.additional_tools",
            ]
            .into_iter()
            .map(str::to_string),
        ),
        "openai_compatible" | "openaicompatible" | "openai-compatible"
            if profile.endpoint_name != "images-generations"
                && profile.endpoint_name != "images-edits" =>
        {
            probes.extend(
                [
                    "tools.function",
                    "tools.function.continuation",
                    "tools.choice.required",
                    "tools.choice.specific",
                ]
                .into_iter()
                .map(str::to_string),
            );
        }
        "anthropic_messages" | "anthropicmessages" | "anthropic-messages" | "google_gemini"
        | "googlegemini" | "google-gemini" => probes.extend(
            [
                "tools.function",
                "tools.choice.required",
                "tools.choice.specific",
            ]
            .into_iter()
            .map(str::to_string),
        ),
        _ => {}
    }
    probes
}

fn parse_profile(row: sqlx::any::AnyRow) -> Result<TargetCapabilityProfile, StoreError> {
    let status_text: String = row.get(13);
    let schema_version = row.get::<i64, _>(10) as u32;
    let identity_version = row.get::<i64, _>(1) as u32;
    let registry_version = row.get::<i64, _>(11) as u32;
    let baseline_version = row.get::<i64, _>(12) as u32;
    let status = ProfileStatus::parse(&status_text).unwrap_or(ProfileStatus::Error);
    let incompatible_schema = schema_version != CAPABILITY_SCHEMA_VERSION
        || identity_version != 1
        || registry_version != CAPABILITY_REGISTRY_VERSION
        || baseline_version != CAPABILITY_BASELINE_VERSION;
    let resolved_text: String = row.get(14);
    let observations_text: String = row.get(15);
    let resolved_result = serde_json::from_str(&resolved_text);
    let observations_result = serde_json::from_str(&observations_text);
    let parse_error = resolved_result.is_err() || observations_result.is_err();
    let resolved_raw = resolved_result.unwrap_or_default();
    let observations_raw = observations_result.unwrap_or_default();
    let (profile_status, resolved_capabilities, observations, fresh_until, stale_until) =
        if incompatible_schema || status == ProfileStatus::Error || parse_error {
            // Preserve the row for diagnostics but do not let an unknown
            // status/schema participate in routing until a new probe rebuilds
            // it. This keeps one future-version row from breaking the whole
            // snapshot load.
            (
                ProfileStatus::Error,
                ResolvedTargetCapabilities::default(),
                observations_raw,
                None,
                None,
            )
        } else {
            (
                status,
                resolved_raw,
                observations_raw,
                parse_optional_dt(row.get(21))?,
                parse_optional_dt(row.get(22))?,
            )
        };
    Ok(TargetCapabilityProfile {
        target_key: TargetKey(row.get(0)),
        identity_version,
        provider_id: row.get(2),
        credential_scope_fingerprint: row.get(3),
        canonical_api_base: row.get(4),
        protocol_suite: row.get(5),
        endpoint_name: row.get(6),
        endpoint_version: row.get(7),
        dialect_id: row.get(8),
        model_id: row.get(9),
        schema_version,
        registry_version,
        baseline_version,
        profile_status,
        resolved_capabilities,
        observations,
        last_probe_suite_version: row.get::<Option<i64>, _>(16).map(|v| v as u32),
        last_probe_judge_version: row.get::<Option<i64>, _>(17).map(|v| v as u32),
        last_successful_probe_at: parse_optional_dt(row.get(18))?,
        last_probe_error_class: row.get(19),
        last_probe_error_redacted: row.get(20),
        fresh_until,
        stale_until,
        created_at: parse_dt(row.get(23))?,
        updated_at: parse_dt(row.get(24))?,
    })
}

fn parse_override(row: sqlx::any::AnyRow) -> Result<TargetCapabilityOverride, StoreError> {
    let state: CapabilityState = serde_json::from_str(&row.get::<String, _>(2))?;
    let value = row
        .get::<Option<String>, _>(3)
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?;
    Ok(TargetCapabilityOverride {
        target_key: TargetKey(row.get(0)),
        capability_id: CapabilityId::from(row.get::<String, _>(1)),
        state,
        value,
        reason: row.get(4),
        actor: row.get(5),
        expires_at: parse_optional_dt(row.get(6))?,
        created_at: parse_dt(row.get(7))?,
        updated_at: parse_dt(row.get(8))?,
    })
}

fn parse_job(row: sqlx::any::AnyRow) -> Result<ProbeJob, StoreError> {
    let probe_set: Vec<String> = serde_json::from_str(&row.get::<String, _>(2))?;
    Ok(ProbeJob {
        id: row.get(0),
        target_key: TargetKey(row.get(1)),
        probe_set,
        probe_set_hash: row.get(3),
        status: row.get(4),
        priority: row.get(5),
        attempt_count: row.get(6),
        max_attempts: row.get(7),
        next_probe_index: row.get(8),
        next_attempt_at: parse_dt(row.get(9))?,
        lease_owner: row.get(10),
        lease_until: parse_optional_dt(row.get(11))?,
        last_error_class: row.get(12),
        last_error_redacted: row.get(13),
        created_at: parse_dt(row.get(14))?,
        updated_at: parse_dt(row.get(15))?,
    })
}

fn parse_admission(row: sqlx::any::AnyRow) -> Result<CapabilityRouteAdmission, StoreError> {
    let mode_text: String = row.get(3);
    let mode = tiygate_core::CapabilityRoutingMode::parse(&mode_text)
        .unwrap_or(tiygate_core::CapabilityRoutingMode::Shadow);
    if mode == tiygate_core::CapabilityRoutingMode::Off {
        return Err(StoreError::Invalid(
            "shape admission cannot use off mode".to_string(),
        ));
    }
    let required_capabilities: Vec<CapabilityId> = serde_json::from_str(&row.get::<String, _>(2))?;
    let stored_requirements: Vec<CapabilityRequirement> =
        serde_json::from_str(&row.get::<String, _>(12))?;
    let required_requirements = if stored_requirements.is_empty() {
        required_capabilities
            .iter()
            .cloned()
            .map(CapabilityRequirement::required)
            .collect()
    } else {
        stored_requirements
    };
    Ok(CapabilityRouteAdmission {
        route_id: row.get(0),
        capability_shape_hash: row.get(1),
        required_capabilities,
        required_requirements,
        mode,
        gate_policy_version: row.get::<i64, _>(4) as u32,
        report: serde_json::from_str(&row.get::<String, _>(5))?,
        approved_by: row.get(6),
        approved_at: parse_optional_dt(row.get(7))?,
        expires_at: parse_optional_dt(row.get(8))?,
        revision: row.get(9),
        created_at: parse_dt(row.get(10))?,
        updated_at: parse_dt(row.get(11))?,
    })
}

fn parse_dt(value: String) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(&value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|error| StoreError::Invalid(format!("invalid timestamp: {error}")))
}

fn parse_optional_dt(value: Option<String>) -> Result<Option<DateTime<Utc>>, StoreError> {
    value.map(parse_dt).transpose()
}

/// Keep these imports anchored while the baseline adapters are added by the
/// protocol crate; they also document the intended store/core boundary.
#[allow(dead_code)]
fn _baseline_type_anchor(_: BTreeMap<CapabilityId, BaselineSupport>) {}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn probe_set_hash_is_order_independent() {
        let a = canonical_probe_set(&["b".to_string(), "a".to_string(), "a".to_string()]);
        let b = canonical_probe_set(&["a".to_string(), "b".to_string()]);
        assert_eq!(a, b);
        assert_eq!(
            probe_set_hash(&["b".to_string(), "a".to_string()]),
            probe_set_hash(&["a".to_string(), "b".to_string()])
        );
        assert_ne!(
            probe_set_hash_for_version(&["a".to_string()], 1),
            probe_set_hash_for_version(&["a".to_string()], 2)
        );
    }

    #[test]
    fn embeddings_probe_bundle_does_not_include_stream_or_tools() {
        let target = tiygate_core::RoutingTarget {
            provider_id: "p".to_string(),
            model_id: "embedding-model".to_string(),
            api_base: "https://example.com/v1".to_string(),
            api_key: String::new(),
            api_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiCompatible,
                "embeddings",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            egress_dialect_id: None,
            weight: 1.0,
            oauth: None,
        };
        assert_eq!(default_probe_set_for_target(&target), vec!["http.basic"]);
    }

    #[test]
    fn profile_summary_counts_states() {
        let mut profile = TargetCapabilityProfile::pending(
            &CanonicalTargetIdentity {
                identity_version: 1,
                provider_id: "p".to_string(),
                credential_scope_fingerprint: "s".to_string(),
                canonical_api_base: "https://example.com".to_string(),
                egress_protocol_suite: "openai_responses".to_string(),
                egress_endpoint_name: "responses".to_string(),
                egress_endpoint_version: "v1".to_string(),
                egress_dialect_id: "auto".to_string(),
                exact_model_id: "m".to_string(),
            },
            TargetKey("key".to_string()),
        );
        let mut capabilities = BTreeMap::new();
        capabilities.insert(
            CapabilityId::from("a"),
            tiygate_core::ResolvedCapability {
                state: CapabilityState::Supported,
                value: None,
                observation: None,
                matcher: None,
            },
        );
        profile.resolved_capabilities = ResolvedTargetCapabilities { capabilities };
        let summary = CapabilityProfileSummary::from(&profile);
        assert_eq!(summary.supported, 1);
        assert_eq!(summary.unknown, 0);
    }

    #[tokio::test]
    async fn profile_and_probe_job_round_trip_with_lease() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let target = tiygate_core::RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "model".to_string(),
            api_base: "https://example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            api_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiResponses,
                "responses",
                "v1",
            ),
            account_label: Some("account-a".to_string()),
            api_key_override: None,
            api_base_override: None,
            egress_dialect_id: None,
            weight: 1.0,
            oauth: None,
        };
        let (key, job) = store
            .ensure_target_capability(
                &target,
                &["http.basic".to_string(), "transport.sse".to_string()],
            )
            .await
            .expect("ensure");
        assert_eq!(job.status, "pending");
        let claimed = store
            .claim_probe_job("worker-a", 60)
            .await
            .expect("claim")
            .expect("job");
        assert_eq!(claimed.target_key, key);
        assert_eq!(claimed.status, "running");
        assert!(store
            .complete_probe_job(&claimed.id, "worker-a", "complete")
            .await
            .expect("complete"));
        let profile = store
            .get_capability_profile(&key)
            .await
            .expect("profile")
            .expect("profile row");
        assert_eq!(profile.profile_status, ProfileStatus::Pending);
    }

    #[tokio::test]
    async fn profile_version_mismatch_is_diagnostic_only() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let identity = CanonicalTargetIdentity {
            identity_version: 1,
            provider_id: "future".to_string(),
            credential_scope_fingerprint: "scope".to_string(),
            canonical_api_base: "https://example.com/v1".to_string(),
            egress_protocol_suite: "openai_responses".to_string(),
            egress_endpoint_name: "responses".to_string(),
            egress_endpoint_version: "v1".to_string(),
            egress_dialect_id: "openai-responses-standard".to_string(),
            exact_model_id: "future-model".to_string(),
        };
        let key = TargetKey("future-profile".to_string());
        let mut profile = TargetCapabilityProfile::pending(&identity, key.clone());
        profile.registry_version = CAPABILITY_REGISTRY_VERSION + 1;
        profile.resolved_capabilities.capabilities.insert(
            CapabilityId::from("tools.function"),
            tiygate_core::ResolvedCapability {
                state: CapabilityState::Supported,
                value: None,
                observation: None,
                matcher: None,
            },
        );
        store
            .upsert_capability_profile(&profile)
            .await
            .expect("profile upsert");
        let loaded = store
            .get_capability_profile(&key)
            .await
            .expect("profile read")
            .expect("profile exists");
        assert_eq!(loaded.profile_status, ProfileStatus::Error);
        assert!(loaded.resolved_capabilities.capabilities.is_empty());
    }

    #[tokio::test]
    async fn partial_probe_job_is_resumable_after_worker_stop() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let key = TargetKey("partial-target".to_string());
        let job = store
            .enqueue_probe_job(&key, &["http.basic".to_string()], 0, 3)
            .await
            .expect("enqueue");
        let claimed = store
            .claim_probe_job("worker-a", 60)
            .await
            .expect("claim")
            .expect("claimed");
        assert_eq!(claimed.id, job.id);
        assert!(store
            .complete_probe_job(&job.id, "worker-a", "partial")
            .await
            .expect("partial"));
        let resumed = store
            .claim_probe_job("worker-b", 60)
            .await
            .expect("resume claim")
            .expect("resumed");
        assert_eq!(resumed.id, job.id);
        assert_eq!(resumed.lease_owner.as_deref(), Some("worker-b"));

        assert!(store
            .complete_probe_job_partial_with_progress(&job.id, "worker-b", 1)
            .await
            .expect("partial progress"));
        let resumed_again = store
            .claim_probe_job("worker-c", 60)
            .await
            .expect("resume with cursor")
            .expect("resumed cursor job");
        assert_eq!(resumed_again.next_probe_index, 1);
    }

    #[tokio::test]
    async fn concurrent_probe_claim_has_one_winner() {
        let pool = crate::db::open_pool_with_max_connections("sqlite::memory:", 4)
            .await
            .expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = std::sync::Arc::new(DbConfigStore::new(pool, None));
        store
            .enqueue_probe_job(
                &TargetKey("concurrent-target".to_string()),
                &["http.basic".to_string()],
                0,
                3,
            )
            .await
            .expect("enqueue");
        let (left, right) = tokio::join!(
            store.claim_probe_job("worker-left", 60),
            store.claim_probe_job("worker-right", 60)
        );
        let winners = [left.expect("left claim"), right.expect("right claim")]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn capability_mutation_idempotency_replays_and_rejects_payload_reuse() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let payload = serde_json::json!({"enabled": true});
        let first = store
            .begin_capability_mutation("probe_worker", "idem-1", &payload)
            .await
            .expect("reserve");
        let request_hash = match first {
            CapabilityMutationIdempotency::New { request_hash } => request_hash,
            other => panic!("unexpected reservation: {other:?}"),
        };
        store
            .complete_capability_mutation(
                "probe_worker",
                "idem-1",
                &request_hash,
                200,
                &serde_json::json!({"enabled": true}),
            )
            .await
            .expect("complete");
        assert_eq!(
            store
                .begin_capability_mutation("probe_worker", "idem-1", &payload)
                .await
                .expect("replay"),
            CapabilityMutationIdempotency::Replay {
                status: 200,
                response: serde_json::json!({"enabled": true})
            }
        );
        assert!(matches!(
            store
                .begin_capability_mutation(
                    "probe_worker",
                    "idem-1",
                    &serde_json::json!({"enabled": false})
                )
                .await
                .expect("conflict"),
            CapabilityMutationIdempotency::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn capability_override_audit_failure_rolls_back_state() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool.clone(), None);
        let key = TargetKey("audit-rollback-target".to_string());
        let record = TargetCapabilityOverride {
            target_key: key.clone(),
            capability_id: CapabilityId::from("tools.function"),
            state: CapabilityState::Supported,
            value: None,
            reason: "audit rollback".to_string(),
            actor: "test".to_string(),
            expires_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        sqlx::query("DROP TABLE audit_log")
            .execute(pool.any())
            .await
            .expect("drop audit table");
        assert!(store
            .upsert_capability_override_with_audit(
                &record,
                "tools.function",
                &serde_json::json!({})
            )
            .await
            .is_err());
        assert!(store
            .list_capability_overrides(&key)
            .await
            .expect("override list")
            .is_empty());
    }

    #[tokio::test]
    async fn fingerprint_secret_is_persisted_and_reused_across_store_instances() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let encryption =
            std::sync::Arc::new(crate::encryption::KeyEncryption::from_bytes([11_u8; 32]));
        let target = tiygate_core::RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "model".to_string(),
            api_base: "https://example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
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
        let first = DbConfigStore::new(pool.clone(), Some(encryption.clone()));
        first.ensure_fingerprint_secret().await.expect("secret");
        let first_key = first.target_key_for(&target).expect("target key").0;
        let stored = sqlx::query_scalar::<_, String>(
            "SELECT encrypted_value FROM installation_secrets WHERE name = 'target-key-hmac/v1'",
        )
        .fetch_one(pool.any())
        .await
        .expect("stored secret");
        assert!(!stored.is_empty());
        assert!(!stored.contains("sk-test"));

        let second = DbConfigStore::new(pool, Some(encryption));
        second.ensure_fingerprint_secret().await.expect("secret");
        let second_key = second.target_key_for(&target).expect("target key").0;
        assert_eq!(first_key, second_key);
    }

    #[tokio::test]
    async fn fingerprint_secret_stays_stable_in_explicit_cleartext_legacy_mode() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let target = tiygate_core::RoutingTarget {
            provider_id: "legacy".to_string(),
            model_id: "model".to_string(),
            api_base: "https://example.com/v1".to_string(),
            api_key: "sk-legacy".to_string(),
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
        let first = DbConfigStore::new(pool.clone(), None);
        first.ensure_fingerprint_secret().await.expect("secret");
        let first_key = first.target_key_for(&target).expect("key").0;
        let second = DbConfigStore::new(pool, None);
        second.ensure_fingerprint_secret().await.expect("secret");
        let second_key = second.target_key_for(&target).expect("key").0;
        assert_eq!(first_key, second_key);
    }

    #[tokio::test]
    async fn shape_admission_is_revisioned_and_epoch_backed() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let now = Utc::now();
        let shape_hash =
            tiygate_core::capability_shape_hash_from_ids(&[CapabilityId::from("tools.function")]);
        let mut admission = CapabilityRouteAdmission {
            route_id: "route-a".to_string(),
            capability_shape_hash: shape_hash,
            required_capabilities: vec![CapabilityId::from("tools.function")],
            required_requirements: Vec::new(),
            mode: tiygate_core::CapabilityRoutingMode::Shadow,
            gate_policy_version: 1,
            report: serde_json::json!({"gate_passed": false}),
            approved_by: None,
            approved_at: None,
            expires_at: None,
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        let saved = store
            .upsert_capability_route_admission(&admission, None)
            .await
            .expect("create admission");
        assert_eq!(saved.revision, 1);
        admission.mode = tiygate_core::CapabilityRoutingMode::Enforce;
        admission.report = serde_json::json!({"gate_passed": true});
        admission.approved_by = Some("test".to_string());
        admission.approved_at = Some(now);
        let updated = store
            .upsert_capability_route_admission(&admission, Some(saved.revision))
            .await
            .expect("update admission");
        assert_eq!(updated.revision, 2);
        let conflict = store
            .upsert_capability_route_admission(&admission, Some(saved.revision))
            .await;
        assert!(
            matches!(conflict, Err(StoreError::Invalid(message)) if message.contains("revision"))
        );
        assert!(store
            .delete_capability_route_admission(
                &updated.route_id,
                &updated.capability_shape_hash,
                Some(updated.revision),
            )
            .await
            .expect("delete admission"));
        assert!(store
            .get_capability_route_admission(&updated.route_id, &updated.capability_shape_hash)
            .await
            .expect("read admission")
            .is_none());
    }

    #[tokio::test]
    async fn route_admission_invalidation_is_transactional_and_keeps_history() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let now = Utc::now();
        let shape_hash =
            tiygate_core::capability_shape_hash_from_ids(&[CapabilityId::from("tools.function")]);
        let admission = CapabilityRouteAdmission {
            route_id: "route-atomic".to_string(),
            capability_shape_hash: shape_hash.clone(),
            required_capabilities: vec![CapabilityId::from("tools.function")],
            required_requirements: Vec::new(),
            mode: tiygate_core::CapabilityRoutingMode::Enforce,
            gate_policy_version: 1,
            report: serde_json::json!({"gate_passed": true}),
            approved_by: Some("test".to_string()),
            approved_at: Some(now),
            expires_at: Some(now + chrono::Duration::hours(1)),
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        store
            .upsert_capability_route_admission(&admission, None)
            .await
            .expect("create admission");
        let mut tx = store.pool.any().begin().await.expect("transaction");
        assert!(store
            .mark_route_admissions_stale_tx(&mut tx, "route-atomic", "route_updated")
            .await
            .expect("invalidate"));
        store
            .bump_capability_epoch_tx(&mut tx)
            .await
            .expect("epoch");
        tx.commit().await.expect("commit");
        let updated = store
            .get_capability_route_admission("route-atomic", &shape_hash)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(updated.mode, tiygate_core::CapabilityRoutingMode::Shadow);
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.report["stale"], true);
        assert_eq!(updated.report["stale_reason"], "route_updated");
    }

    #[tokio::test]
    async fn constrained_shape_admission_round_trips_typed_requirements() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let requirements = vec![CapabilityRequirement::with_value(
            "tools.namespace",
            RequirementStrength::Required,
            tiygate_core::CapabilityValue::EnumSet(["functions".to_string()].into_iter().collect()),
        )];
        let now = Utc::now();
        let admission = CapabilityRouteAdmission {
            route_id: "route-constrained".to_string(),
            capability_shape_hash: tiygate_core::capability_shape_hash_from_requirements(
                &requirements,
            ),
            required_capabilities: vec![CapabilityId::from("tools.namespace")],
            required_requirements: requirements.clone(),
            mode: tiygate_core::CapabilityRoutingMode::Shadow,
            gate_policy_version: 1,
            report: serde_json::json!({"gate_passed": false}),
            approved_by: None,
            approved_at: None,
            expires_at: None,
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        let saved = store
            .upsert_capability_route_admission(&admission, None)
            .await
            .expect("create constrained admission");
        assert_eq!(saved.required_requirements, requirements);
        let loaded = store
            .get_capability_route_admission(&saved.route_id, &saved.capability_shape_hash)
            .await
            .expect("read constrained admission")
            .expect("constrained admission row");
        assert_eq!(loaded.required_requirements, requirements);
        assert_ne!(
            loaded.capability_shape_hash,
            tiygate_core::capability_shape_hash_from_ids(&[CapabilityId::from("tools.namespace")])
        );
    }

    #[tokio::test]
    async fn probe_budget_is_atomic_for_target_and_global_scopes() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let key = TargetKey("budget-target".to_string());
        assert!(store
            .try_consume_probe_budget(&key, 1, 10)
            .await
            .expect("first consume"));
        assert!(!store
            .try_consume_probe_budget(&key, 1, 10)
            .await
            .expect("target limit"));
        let other = TargetKey("budget-other".to_string());
        assert!(store
            .try_consume_probe_budget(&other, 10, 10)
            .await
            .expect("global second consume"));
        assert!(!store
            .try_consume_probe_budget(&TargetKey("budget-third".to_string()), 10, 1)
            .await
            .expect("global limit"));
        assert!(!store
            .try_consume_probe_budget_with_cost(&TargetKey("budget-weighted".to_string()), 1, 10, 2)
            .await
            .expect("weighted target limit"));
        assert!(store
            .try_consume_probe_budget_with_cost(
                &TargetKey("budget-weighted-ok".to_string()),
                2,
                10,
                2
            )
            .await
            .expect("weighted consume"));
        assert!(!store
            .try_consume_probe_budget_with_cost(
                &TargetKey("budget-weighted-ok".to_string()),
                2,
                10,
                1
            )
            .await
            .expect("weighted target exhausted"));
    }

    #[tokio::test]
    async fn successful_capability_feedback_is_positive_only_and_refreshes_profile() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let profile = TargetCapabilityProfile::pending(
            &CanonicalTargetIdentity {
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
            TargetKey("feedback-target".to_string()),
        );
        store
            .upsert_capability_profile(&profile)
            .await
            .expect("profile");
        assert!(store
            .record_successful_capability(
                &TargetKey("feedback-target".to_string()),
                &CapabilityId::from("tools.function"),
            )
            .await
            .expect("feedback"));
        let updated = store
            .get_capability_profile(&TargetKey("feedback-target".to_string()))
            .await
            .expect("read profile")
            .expect("profile");
        assert!(updated.observations.iter().any(|observation| {
            observation.capability_id == CapabilityId::from("tools.function")
                && observation.source == tiygate_core::EvidenceSource::SuccessfulTraffic
                && observation.state == CapabilityState::Supported
        }));
    }

    #[tokio::test]
    async fn cleanup_removes_orphaned_profiles_and_terminal_jobs() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrations");
        let store = DbConfigStore::new(pool, None);
        let identity = CanonicalTargetIdentity {
            identity_version: 1,
            provider_id: "orphan-provider".to_string(),
            credential_scope_fingerprint: "scope".to_string(),
            canonical_api_base: "https://orphan.example".to_string(),
            egress_protocol_suite: "openai_responses".to_string(),
            egress_endpoint_name: "responses".to_string(),
            egress_endpoint_version: "v1".to_string(),
            egress_dialect_id: "openai-responses-standard".to_string(),
            exact_model_id: "orphan-model".to_string(),
        };
        let key = TargetKey("orphan-profile".to_string());
        let mut profile = TargetCapabilityProfile::pending(&identity, key.clone());
        let old = Utc::now() - chrono::Duration::days(60);
        profile.created_at = old;
        profile.updated_at = old;
        profile.fresh_until = Some(old);
        profile.stale_until = Some(old);
        store
            .upsert_capability_profile(&profile)
            .await
            .expect("profile");
        let job = store
            .enqueue_probe_job(&key, &["http.basic".to_string()], 0, 1)
            .await
            .expect("job");
        sqlx::query("UPDATE target_probe_jobs SET status='complete', updated_at=$1 WHERE id=$2")
            .bind(old.to_rfc3339())
            .bind(&job.id)
            .execute(store.pool.any())
            .await
            .expect("age job");

        let report = store
            .cleanup_orphaned_capability_state(30)
            .await
            .expect("cleanup");
        assert_eq!(report.profiles_deleted, 1);
        assert_eq!(report.jobs_deleted, 1);
        assert!(store
            .get_capability_profile(&key)
            .await
            .expect("profile lookup")
            .is_none());
        assert!(store
            .get_probe_job(&job.id)
            .await
            .expect("job lookup")
            .is_none());
    }
}
