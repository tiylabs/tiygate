//! In-process Shadow/Enforce capability metrics.
//!
//! Counters are deliberately independent from request health and quota state.
//! They are cheap atomic updates on the request path; detailed decisions are
//! persisted asynchronously through `EventPayload::CapabilityPlan`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tiygate_core::CapabilityId;

use super::capability_planner::TargetPlanDiagnostic;

#[derive(Default)]
struct Counters {
    relevant_requests: AtomicU64,
    target_pairs: AtomicU64,
    resolved_pairs: AtomicU64,
    compatible_shapes: AtomicU64,
    shapes: AtomicU64,
    unknown_requests: AtomicU64,
    planning_samples: AtomicU64,
    planning_micros: AtomicU64,
}

static REGISTRY: OnceLock<DashMap<String, Arc<Counters>>> = OnceLock::new();

fn registry() -> &'static DashMap<String, Arc<Counters>> {
    REGISTRY.get_or_init(DashMap::new)
}

pub fn record(
    scope: &str,
    shape_hash: &str,
    requirements: &[CapabilityId],
    diagnostics: &[TargetPlanDiagnostic],
    planning_micros: u64,
) {
    if requirements.is_empty() || diagnostics.is_empty() {
        return;
    }
    let counters = registry()
        .entry(format!("{scope}|{shape_hash}"))
        .or_insert_with(|| Arc::new(Counters::default()))
        .clone();
    counters.relevant_requests.fetch_add(1, Ordering::Relaxed);
    counters
        .target_pairs
        .fetch_add(diagnostics.len() as u64, Ordering::Relaxed);
    let resolved = diagnostics
        .iter()
        .filter(|item| item.status != "planner_error" && item.unknown.is_empty())
        .count() as u64;
    counters
        .resolved_pairs
        .fetch_add(resolved, Ordering::Relaxed);
    counters.shapes.fetch_add(1, Ordering::Relaxed);
    if diagnostics.iter().any(|item| {
        item.status == "compatible" && item.missing.is_empty() && item.unknown.is_empty()
    }) {
        counters.compatible_shapes.fetch_add(1, Ordering::Relaxed);
    }
    if diagnostics.iter().any(|item| !item.unknown.is_empty()) {
        counters.unknown_requests.fetch_add(1, Ordering::Relaxed);
    }
    counters.planning_samples.fetch_add(1, Ordering::Relaxed);
    counters
        .planning_micros
        .fetch_add(planning_micros, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_capability_bearing_shapes() {
        record("test-shadow", "shape/v1:test", &[], &[], 1);
        assert!(registry().get("test-shadow|shape/v1:test").is_none());
    }
}
