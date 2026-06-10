#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ir::*;
    use crate::protocol::*;
    use crate::routing::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn test_routing_table_resolve() {
        let mut table = RoutingTable::new();
        let targets = vec![RoutingTarget {
            provider_id: "openai".to_string(),
            model_id: "gpt-4o".to_string(),
            api_base: "https://api.openai.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
        }];

        table.insert("gpt-4o".to_string(), targets.clone());
        let resolved = table.resolve("gpt-4o").unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].provider_id, "openai");
        assert!(table.resolve("nonexistent").is_none());
    }

    #[test]
    fn test_routing_target_effective_key() {
        let target = RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "test-model".to_string(),
            api_base: "https://test.api".to_string(),
            api_key: "original-key".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: None,
            api_key_override: Some("override-key".to_string()),
            api_base_override: None,
            weight: 1.0,
        };
        assert_eq!(target.effective_api_key(), "override-key");
    }

    #[test]
    fn test_health_registry_circuit_breaker() {
        let registry = HealthRegistry::new(3, Duration::from_secs(30));
        let key = "test:model";

        // Initially healthy
        assert!(registry.is_healthy(key));

        // 2 failures — still healthy
        registry.record_failure(key);
        registry.record_failure(key);
        assert!(registry.is_healthy(key));

        // 3rd failure — circuit broken
        registry.record_failure(key);
        assert!(!registry.is_healthy(key));

        // Success should restore health
        registry.record_success(key);
        assert!(registry.is_healthy(key));
    }

    #[test]
    fn test_health_registry_cooling() {
        let registry = HealthRegistry::new(3, Duration::from_secs(30));
        let key = "test:model";

        // Apply cooling
        registry.apply_cooling(key, Duration::from_secs(60), "rate_limited");
        assert!(!registry.is_healthy(key));

        // Status should be Cooling
        let status = registry.health_status(key);
        assert!(matches!(status, RoutingTargetHealth::Cooling { .. }));
    }

    #[test]
    fn test_health_registry_reset() {
        let registry = HealthRegistry::new(3, Duration::from_secs(30));
        let key = "test:model";

        registry.record_failure(key);
        registry.record_failure(key);
        registry.record_failure(key);
        assert!(!registry.is_healthy(key));

        registry.reset();
        assert!(registry.is_healthy(key));
    }

    #[test]
    fn test_health_registry_half_open_recovery() {
        let registry = HealthRegistry::new(3, Duration::from_millis(10));
        let key = "test:halfopen";

        // Trigger circuit break
        registry.record_failure(key);
        registry.record_failure(key);
        registry.record_failure(key);
        assert!(!registry.is_healthy(key));

        // Wait for recovery period
        std::thread::sleep(Duration::from_millis(20));

        // After recovery, should be healthy again (half-open)
        assert!(registry.is_healthy(key));

        // Record success to confirm recovery
        registry.record_success(key);
        assert!(registry.is_healthy(key));
    }

    #[test]
    fn test_error_classification() {
        // Rate limited
        let err = crate::Error::Routing("429 rate limit exceeded".to_string());
        let class = classify_error(&err);
        assert_eq!(class.class, ErrorClass::RateLimited);

        // Auth error
        let err = crate::Error::Routing("401 unauthorized".to_string());
        let class = classify_error(&err);
        assert_eq!(class.class, ErrorClass::Auth);

        // Bad request
        let err = crate::Error::Routing("400 bad request".to_string());
        let class = classify_error(&err);
        assert_eq!(class.class, ErrorClass::BadRequest);

        // Transient (default)
        let err = crate::Error::Routing("500 internal server error".to_string());
        let class = classify_error(&err);
        assert_eq!(class.class, ErrorClass::Transient);
    }

    #[test]
    fn test_fallback_policy_bytes_emitted() {
        let policy = DefaultFallbackPolicy::with_defaults();
        let target = RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "m".to_string(),
            api_base: "https://test".to_string(),
            api_key: "k".to_string(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
        };
        let err = crate::Error::Routing("500 error".to_string());

        // No bytes emitted — should TryNext
        let decision = policy.classify(&err, &target, 0, 4, 0);
        assert_eq!(decision, FallbackDecision::TryNext);

        // Bytes emitted — should Fail (idempotency gate)
        let decision = policy.classify(&err, &target, 0, 4, 1);
        assert_eq!(decision, FallbackDecision::Fail);
    }

    #[test]
    fn test_fallback_policy_budget() {
        let policy = DefaultFallbackPolicy::with_defaults();
        let target = RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "m".to_string(),
            api_base: "https://test".to_string(),
            api_key: "k".to_string(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
        };
        let err = crate::Error::Routing("500 error".to_string());

        // Exceed max attempts
        let decision = policy.classify(&err, &target, 4, 4, 0);
        assert_eq!(decision, FallbackDecision::Fail);
    }

    #[test]
    fn test_protocol_endpoint_identity() {
        let ep = ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1");
        assert_eq!(ep.suite, ProtocolSuite::OpenAiCompatible);
        assert!(ep.full_id().contains("chat-completions"));
        assert!(ep.full_id().contains("v1"));
    }

    #[test]
    fn test_usage_default() {
        let usage = Usage::default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
        assert!(usage.reasoning_tokens.is_none());
    }

    #[test]
    fn test_content_serde() {
        let text = Content::Text {
            text: "hello".to_string(),
        };
        let json = serde_json::to_value(&text).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");

        let tc = Content::ToolCall {
            id: "tc1".to_string(),
            name: "get_weather".to_string(),
            arguments: serde_json::json!({"city": "London"}),
        };
        let json = serde_json::to_value(&tc).unwrap();
        assert_eq!(json["type"], "tool_call");
    }

    #[test]
    fn test_retry_policy_delay() {
        let policy = RetryPolicy::with_defaults();
        let delay = policy.delay_for(0);
        assert!(delay >= Duration::from_millis(750));
        assert!(delay <= Duration::from_millis(1250));

        let delay2 = policy.delay_for(1);
        assert!(delay2 >= Duration::from_millis(1500));
    }

    #[test]
    fn test_weighted_strategy() {
        let targets = vec![
            RoutingTarget {
                provider_id: "a".to_string(),
                model_id: "m".to_string(),
                api_base: "https://a".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 10.0,
            },
            RoutingTarget {
                provider_id: "b".to_string(),
                model_id: "m".to_string(),
                api_base: "https://b".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            },
        ];

        let strategy = WeightedStrategy;
        let ordered = strategy.order(&targets);
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn test_priority_strategy() {
        let targets = vec![
            RoutingTarget {
                provider_id: "low".to_string(),
                model_id: "m".to_string(),
                api_base: "https://low".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            },
            RoutingTarget {
                provider_id: "high".to_string(),
                model_id: "m".to_string(),
                api_base: "https://high".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 10.0,
            },
        ];

        let strategy = PriorityStrategy;
        let ordered = strategy.order(&targets);
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].provider_id, "high");
    }

    #[test]
    fn test_cooldown_strategy_prefers_healthy() {
        let registry = Arc::new(HealthRegistry::new(3, Duration::from_secs(30)));
        // Mark "broken" as cooling to verify cooldown strategy deprioritizes it
        registry.apply_cooling("broken:m", Duration::from_secs(60), "rate_limited");

        let targets = vec![
            RoutingTarget {
                provider_id: "healthy".to_string(),
                model_id: "m".to_string(),
                api_base: "https://healthy".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            },
            RoutingTarget {
                provider_id: "broken".to_string(),
                model_id: "m".to_string(),
                api_base: "https://broken".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 10.0,
            },
        ];

        let strategy = CooldownStrategy::new(registry);
        let ordered = strategy.order(&targets);
        assert_eq!(ordered.len(), 2);
        // Healthy should come first despite lower weight
        assert_eq!(ordered[0].provider_id, "healthy");
        assert_eq!(ordered[1].provider_id, "broken");
    }

    #[test]
    fn test_fallback_policy_budget_limit() {
        // Test that max_attempts is enforced (budget check)
        let policy =
            DefaultFallbackPolicy::new(3, Duration::from_secs(120), RetryPolicy::with_defaults());
        let target = RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "m".to_string(),
            api_base: "https://test".to_string(),
            api_key: "k".to_string(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
        };
        let err = crate::Error::Routing("500 error".to_string());

        // attempt < max_attempts → TryNext
        assert_eq!(
            policy.classify(&err, &target, 2, 4, 0),
            FallbackDecision::TryNext
        );
        // attempt >= max_attempts → Fail
        assert_eq!(
            policy.classify(&err, &target, 4, 4, 0),
            FallbackDecision::Fail
        );
        // attempt >= policy.max_total_attempts even if max_attempts is larger → Fail
        assert_eq!(
            policy.classify(&err, &target, 3, 10, 0),
            FallbackDecision::Fail
        );
    }

    #[test]
    fn test_latency_strategy_prefers_low_latency() {
        let registry = Arc::new(HealthRegistry::new(3, Duration::from_secs(30)));
        // Record latencies: a=10ms, b=500ms, c=unobserved
        registry.record_latency_ms("a:model", 10);
        registry.record_latency_ms("b:model", 500);

        let targets = vec![
            RoutingTarget {
                provider_id: "a".to_string(),
                model_id: "model".to_string(),
                api_base: "https://a".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            },
            RoutingTarget {
                provider_id: "b".to_string(),
                model_id: "model".to_string(),
                api_base: "https://b".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            },
            RoutingTarget {
                provider_id: "c".to_string(),
                model_id: "model".to_string(),
                api_base: "https://c".to_string(),
                api_key: "k".to_string(),
                api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            },
        ];

        let strategy = LatencyStrategy::new(registry.clone());
        let ordered = strategy.order(&targets);
        // c (unobserved, 0) first → gather samples, then a (10ms), then b (500ms)
        assert_eq!(ordered[0].provider_id, "c");
        assert_eq!(ordered[1].provider_id, "a");
        assert_eq!(ordered[2].provider_id, "b");
    }

    #[test]
    fn test_record_latency_ewma() {
        let registry = HealthRegistry::with_defaults();
        // First sample sets EWMA directly
        registry.record_latency_ms("k", 100);
        assert_eq!(registry.ewma_latency_ms("k"), Some(100));
        assert_eq!(registry.latency_samples("k"), 1);

        // Second sample: ewma = 0.3*200 + 0.7*100 = 60 + 70 = 130
        registry.record_latency_ms("k", 200);
        let v = registry.ewma_latency_ms("k").unwrap();
        assert!((125..=135).contains(&v), "got {}", v);
    }

    #[test]
    fn test_fallback_policy_combined_budget() {
        // Combined test: max_total_attempts=2 + 1 retry (RetryPolicy)
        // Total possible attempts: 2 (TryNext) + 1 retry = 3
        // Verify policy rejects when attempt >= max_total_attempts
        let policy = DefaultFallbackPolicy::new(2, Duration::from_secs(60), RetryPolicy::with_defaults());
        let target = RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "m".to_string(),
            api_base: "https://test".to_string(),
            api_key: "k".to_string(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "c", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
        };
        let err = crate::Error::Routing("500".to_string());

        // attempt=0 (start of 2nd target) → TryNext allowed
        assert_eq!(
            policy.classify(&err, &target, 0, 4, 0),
            FallbackDecision::TryNext
        );
        // attempt=2 hits max_total_attempts=2 → Fail
        assert_eq!(
            policy.classify(&err, &target, 2, 4, 0),
            FallbackDecision::Fail
        );
    }

    /// §3.4 requires RateLimited to switch to the next target rather than
    /// retry the same one. The handler honors `Retry-After` via
    /// `HealthRegistry::apply_cooling` so the upstream isn't hammered.
    /// Regression test for the previous behavior that retried the same target.
    #[test]
    fn test_fallback_policy_rate_limited_uses_try_next() {
        let policy = DefaultFallbackPolicy::with_defaults();
        let target = RoutingTarget {
            provider_id: "openai".to_string(),
            model_id: "gpt-4o".to_string(),
            api_base: "https://api.openai.com/v1".to_string(),
            api_key: "sk".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
        };
        let err = crate::Error::Routing("429 rate limit exceeded".to_string());

        // RateLimited → TryNext, NOT Retry (the original bug)
        assert_eq!(
            policy.classify(&err, &target, 0, 4, 0),
            FallbackDecision::TryNext
        );

        // Transient 5xx → TryNext (unchanged baseline)
        let err_5xx = crate::Error::Routing("503 service unavailable".to_string());
        assert_eq!(
            policy.classify(&err_5xx, &target, 0, 4, 0),
            FallbackDecision::TryNext
        );

        // Auth (401) → TryNext, with same-account skipping handled by the handler
        let err_auth = crate::Error::Routing("401 unauthorized".to_string());
        assert_eq!(
            policy.classify(&err_auth, &target, 0, 4, 0),
            FallbackDecision::TryNext
        );

        // BadRequest → Fail immediately
        let err_400 = crate::Error::Routing("400 bad request".to_string());
        assert_eq!(
            policy.classify(&err_400, &target, 0, 4, 0),
            FallbackDecision::Fail
        );
    }
}
