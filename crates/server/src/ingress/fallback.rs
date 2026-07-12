//! Unified fallback / circuit-breaker / retry loop.

use std::collections::HashSet;
use std::pin::Pin;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::Response;
use futures::Future;

use tiygate_core::telemetry::RequestErrorClass;
use tiygate_core::{
    classify_error, DefaultFallbackPolicy, ErrorClass, FallbackDecision, FallbackPolicy,
    RetryPolicy,
};

use super::{build_strategy, AppError, AppState};

async fn emit_hop_decision(
    state: &AppState,
    request_id: &str,
    target: &str,
    hop: usize,
    decision: &str,
) {
    use chrono::Utc;
    use tiygate_core::telemetry::{EventPayload, PipelineEvent};

    state
        .telemetry
        .send(PipelineEvent {
            request_id: request_id.to_string(),
            timestamp: Utc::now(),
            stage: "execute".to_string(),
            payload: EventPayload::HopDecision {
                target: target.to_string(),
                hop,
                decision: decision.to_string(),
            },
        })
        .await;
}

// ---------------------------------------------------------------------------
// Unified fallback / circuit-breaker / retry loop
// ---------------------------------------------------------------------------

/// The outcome of `execute_with_fallback`. The caller (the handler) is
/// responsible for calling `scope.emit_ok` / `scope.emit_error` because
/// those methods consume the scope by value.
#[allow(dead_code)]
pub(super) enum FallbackOutcome {
    /// A target returned a successful response.
    Success {
        response: Response,
        ttfb_ms: Option<u64>,
    },
    /// All targets / attempts were exhausted without a success.
    Exhausted { error: AppError },
    /// The fallback policy decided the error is terminal mid-loop
    /// (e.g. `FallbackDecision::Fail`).
    Failed {
        error: AppError,
        error_class: RequestErrorClass,
    },
}

/// Generic multi-target retry/fallback executor shared by every ingress
/// handler. The `execute_one` callback performs a single upstream call
/// for the given routing target and returns `(Response, Option<ttfb_ms>)`
/// on success or an `AppError` on failure.
///
/// Inside the loop this function:
/// - orders targets via the routing strategy
/// - skips circuit-broken targets (`HealthRegistry::is_healthy`)
/// - applies backoff on retries (`RetryPolicy`)
/// - records `record_success` / `record_failure` / `record_latency_ms`
/// - applies cooling for rate-limit (429) and auth (401/403) errors
/// - classifies errors and decides `TryNext` / `Retry` / `Fail`
/// - emits `RouteResolved`, `HopStart`, `HopSuccess`, `HopFailure` telemetry
///
/// It does **not** call `scope.emit_ok` / `scope.emit_error` (those consume
/// the scope). The caller should inspect `FallbackOutcome` and emit the
/// terminal telemetry event.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_with_fallback<'a, F>(
    state: &'a AppState,
    scope: &mut crate::ingress::observability::RequestScope<'_>,
    targets: &'a [tiygate_core::RoutingTarget],
    virtual_model: &'a str,
    request_id: &'a str,
    execute_one: F,
) -> FallbackOutcome
where
    F: Fn(
        &'a tiygate_core::RoutingTarget,
    )
        -> Pin<Box<dyn Future<Output = Result<(Response, Option<u64>), AppError>> + Send + 'a>>,
{
    use chrono::Utc;
    use tiygate_core::telemetry::{EventPayload, PipelineEvent};

    // Fallback + retry policies (parity with the previous
    // handle_chat_completions gold-standard loop).
    let fallback = DefaultFallbackPolicy::with_defaults();
    let retry = RetryPolicy::with_defaults();
    let max_attempts = fallback.max_total_attempts;
    let deadline = Instant::now() + fallback.deadline;

    let mut attempt = 0usize;
    let mut hop = 0usize;
    let mut target_index = 0usize;
    let mut last_error: Option<AppError> = None;
    let bytes_emitted: u64 = 0;
    let mut oauth_recovered = HashSet::new();

    // Strategy ordering — per-route override takes precedence over the
    // gateway-wide default.
    let effective_strategy = state
        .current_config()
        .routing_table
        .resolve_strategy(virtual_model)
        .unwrap_or(state.tunables().routing_strategy);
    let (strategy, strategy_label) = build_strategy(effective_strategy, state.health.clone());
    let ordered_targets: Vec<&tiygate_core::RoutingTarget> = strategy.order(targets);

    // Telemetry: RouteResolved
    state
        .telemetry
        .send(PipelineEvent {
            request_id: request_id.to_string(),
            timestamp: Utc::now(),
            stage: "ingress".to_string(),
            payload: EventPayload::RouteResolved {
                targets: ordered_targets.iter().map(|t| t.health_key()).collect(),
                strategy: strategy_label.to_string(),
            },
        })
        .await;

    while target_index < ordered_targets.len() && attempt < max_attempts {
        if Instant::now() > deadline {
            let app_err = AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "request deadline exceeded".to_string(),
            )
            .with_class(tiygate_core::ErrorClass::DeadlineExceeded);
            return FallbackOutcome::Failed {
                error: app_err,
                error_class: RequestErrorClass::DeadlineExceeded,
            };
        }

        let target = ordered_targets[target_index];

        // Check health — skip circuit-broken targets
        let health_key = target.health_key();
        if !state.health.is_healthy(&health_key) {
            hop += 1;
            let current_hop = hop;
            state
                .telemetry
                .send(PipelineEvent {
                    request_id: request_id.to_string(),
                    timestamp: Utc::now(),
                    stage: "routing".to_string(),
                    payload: EventPayload::HopFailure {
                        target: health_key.clone(),
                        hop: current_hop,
                        error: "circuit-broken".to_string(),
                        error_class: "circuit_breaker".to_string(),
                        latency_ms: 0,
                    },
                })
                .await;
            emit_hop_decision(state, request_id, &health_key, current_hop, "try_next").await;
            target_index += 1;
            continue;
        }

        // Per-retry backoff (only when retrying the same target)
        if attempt > 0 && attempt > target_index {
            let delay = retry.delay_for(attempt);
            tokio::time::sleep(delay).await;
        }

        attempt += 1;
        hop += 1;
        let current_hop = hop;

        // Telemetry: HopStart
        let hop_started = Utc::now();
        state
            .telemetry
            .send(PipelineEvent {
                request_id: request_id.to_string(),
                timestamp: hop_started,
                stage: "execute".to_string(),
                payload: EventPayload::HopStart {
                    target: health_key.clone(),
                    provider: target.provider_id.clone(),
                    model: target.model_id.clone(),
                    egress_protocol: format!(
                        "{:?}/{}",
                        target.api_protocol.suite, target.api_protocol.name
                    ),
                    hop: current_hop,
                },
            })
            .await;

        // Bind the resolved target on the scope so the terminal
        // RequestEvent attributes the row to the right upstream.
        scope.set_egress(target.api_protocol.clone());
        scope.set_resolved(target.provider_id.clone(), target.model_id.clone());

        let (mut execution, rejected_access_token) =
            crate::oauth_manager::capture_applied_access_token(execute_one(target)).await;
        if execution
            .as_ref()
            .is_err_and(|error| error.http_status() == StatusCode::UNAUTHORIZED)
            && !oauth_recovered.contains(&health_key)
        {
            if let Some(rejected_access_token) = rejected_access_token {
                oauth_recovered.insert(health_key.clone());
                if state
                    .oauth_manager
                    .refresh_after_unauthorized(target, &rejected_access_token)
                    .await
                    .unwrap_or(false)
                {
                    execution =
                        crate::oauth_manager::capture_applied_access_token(execute_one(target))
                            .await
                            .0;
                }
            }
        }

        match execution {
            Ok((response, ttfb_ms)) => {
                let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0) as u64;
                state.health.record_success(&health_key);
                state.health.record_latency_ms(&health_key, hop_elapsed_ms);
                // Telemetry: HopSuccess
                state
                    .telemetry
                    .send(PipelineEvent {
                        request_id: request_id.to_string(),
                        timestamp: Utc::now(),
                        stage: "execute".to_string(),
                        payload: EventPayload::HopSuccess {
                            target: health_key.clone(),
                            hop: current_hop,
                            latency_ms: hop_elapsed_ms,
                            usage: None,
                        },
                    })
                    .await;
                emit_hop_decision(state, request_id, &health_key, current_hop, "success").await;
                scope.set_ttfb_ms(ttfb_ms);
                return FallbackOutcome::Success { response, ttfb_ms };
            }
            Err(app_err) => {
                let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0) as u64;
                state.health.record_failure(&health_key);
                state.health.record_latency_ms(&health_key, hop_elapsed_ms);

                // Classify the error — prefer structured fields when
                // available (HTTP status + error code from upstream),
                // fall back to substring matching on the message.
                let classification = if app_err.upstream_status.is_some()
                    || app_err.upstream_error_code().is_some()
                {
                    tiygate_core::classify_structured(
                        app_err.upstream_status,
                        app_err.upstream_error_code(),
                    )
                } else {
                    let core_err = tiygate_core::Error::Routing(app_err.message.clone());
                    classify_error(&core_err)
                };

                // Telemetry: HopFailure
                state
                    .telemetry
                    .send(PipelineEvent {
                        request_id: request_id.to_string(),
                        timestamp: Utc::now(),
                        stage: "execute".to_string(),
                        payload: EventPayload::HopFailure {
                            target: health_key.clone(),
                            hop: current_hop,
                            error: app_err.message.clone(),
                            error_class: classification.class.as_str().to_string(),
                            latency_ms: hop_elapsed_ms,
                        },
                    })
                    .await;

                // Rate-limit cooling (parse Retry-After header)
                if classification.fallback_class == ErrorClass::RateLimited {
                    if let Some(rh) = &app_err.retry_after_header {
                        if let Ok(secs) = rh.parse::<u64>() {
                            state.health.apply_cooling(
                                &health_key,
                                Duration::from_secs(secs),
                                "rate_limited",
                            );
                        } else {
                            state.health.apply_cooling(
                                &health_key,
                                Duration::from_secs(30),
                                "rate_limited",
                            );
                        }
                    }
                }

                // Decide next action
                let core_err = tiygate_core::Error::Routing(app_err.message.clone());
                let decision =
                    fallback.classify(&core_err, target, attempt, max_attempts, bytes_emitted);

                match decision {
                    FallbackDecision::TryNext => {
                        emit_hop_decision(state, request_id, &health_key, current_hop, "try_next")
                            .await;
                        // Auth 401/403: extended cooling + skip same account
                        if classification.fallback_class == ErrorClass::Auth {
                            state.health.apply_cooling(
                                &health_key,
                                Duration::from_secs(300),
                                "auth_broken",
                            );
                            let skip_label = target.account_label.clone();
                            last_error = Some(app_err);
                            target_index += 1;
                            while let Some(next) = ordered_targets.get(target_index) {
                                if skip_label.is_some() && next.account_label == skip_label {
                                    target_index += 1;
                                } else {
                                    break;
                                }
                            }
                            continue;
                        }
                        last_error = Some(app_err);
                        target_index += 1;
                        continue;
                    }
                    FallbackDecision::Retry => {
                        emit_hop_decision(state, request_id, &health_key, current_hop, "retry")
                            .await;
                        last_error = Some(app_err);
                        continue;
                    }
                    FallbackDecision::Fail => {
                        emit_hop_decision(state, request_id, &health_key, current_hop, "fail")
                            .await;
                        return FallbackOutcome::Failed {
                            error: app_err,
                            error_class: classification.class,
                        };
                    }
                }
            }
        }
    }

    // All targets / attempts exhausted
    let final_err = last_error.unwrap_or_else(|| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "all upstream targets exhausted".to_string(),
        )
        .with_class(tiygate_core::ErrorClass::UpstreamExhausted)
    });
    FallbackOutcome::Exhausted { error: final_err }
}
