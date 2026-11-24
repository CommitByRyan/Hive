use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

const STATE_CARDINALITY: usize = 12;
const REASON_CLASS_CARDINALITY: usize = 16;
const BUDGET_DIMENSION_CARDINALITY: usize = 5;
const LEASE_TRANSITION_CARDINALITY: usize = 2;
const SSE_CONNECTION_CLASS_CARDINALITY: usize = 8;
const RECOVERY_PHASE_CARDINALITY: usize = 6;
const MISSION_OUTCOME_CARDINALITY: usize = 6;
const TOOL_CARDINALITY: usize = 64;

pub const HISTOGRAM_BUCKETS_MS: [u64; 10] = [1, 2, 5, 10, 20, 50, 100, 250, 500, 1_000];

pub const METRIC_NODES_BY_STATE: &str = "hive_nodes_by_state";
pub const METRIC_NODES_QUEUED: &str = "hive_nodes_queued";
pub const METRIC_NODES_RUNNING: &str = "hive_nodes_running";
pub const METRIC_NODES_WAITING_APPROVAL: &str = "hive_nodes_waiting_approval";
pub const METRIC_RETRIES_TOTAL: &str = "hive_retries_total";
pub const METRIC_BUDGET_EXCEEDED_TOTAL: &str = "hive_budget_exceeded_total";
pub const METRIC_LEASE_RECLAIMS_TOTAL: &str = "hive_lease_reclaims_total";
pub const METRIC_SSE_LAG: &str = "hive_sse_lag";
pub const METRIC_SSE_DROPPED_TOTAL: &str = "hive_sse_dropped_total";
pub const METRIC_EVENT_WRITE_LATENCY_MS: &str = "hive_event_write_latency_ms";
pub const METRIC_RECOVERY_SECONDS: &str = "hive_recovery_seconds";
pub const METRIC_MISSION_DURATION_SECONDS: &str = "hive_mission_duration_seconds";
pub const METRIC_TOOL_CALLS_TOTAL: &str = "hive_tool_calls_total";
pub const METRIC_APPROVAL_PENDING: &str = "hive_approval_pending";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NodeStateMetric {
    Pending,
    Ready,
    Leased,
    Running,
    WaitingApproval,
    NeedsRecovery,
    Succeeded,
    Failed,
    RetryReady,
    FailedFinal,
    Cancelled,
    Skipped,
}

impl NodeStateMetric {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::NeedsRecovery => "needs_recovery",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RetryReady => "retry_ready",
            Self::FailedFinal => "failed_final",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BudgetDimensionMetric {
    Tokens,
    Cost,
    Requests,
    ToolCalls,
    WallMs,
}

impl BudgetDimensionMetric {
    pub const fn from_store(value: hive_store::BudgetDimension) -> Self {
        match value {
            hive_store::BudgetDimension::Tokens => Self::Tokens,
            hive_store::BudgetDimension::Cost => Self::Cost,
            hive_store::BudgetDimension::Requests => Self::Requests,
            hive_store::BudgetDimension::ToolCalls => Self::ToolCalls,
            hive_store::BudgetDimension::WallMs => Self::WallMs,
        }
    }

    pub fn parse_store(value: &str) -> Option<Self> {
        hive_store::BudgetDimension::parse(value).map(Self::from_store)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Cost => "cost",
            Self::Requests => "requests",
            Self::ToolCalls => "tool_calls",
            Self::WallMs => "wall_ms",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LeaseTransitionMetric {
    Ready,
    NeedsRecovery,
}

impl LeaseTransitionMetric {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NeedsRecovery => "needs_recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RecoveryPhaseMetric {
    RevokeLeases,
    ExpireApprovals,
    ClassifySideEffects,
    OrphanApprovals,
    NormalizeRetryReady,
    RebuildBudgets,
}

impl RecoveryPhaseMetric {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RevokeLeases => "revoke_leases",
            Self::ExpireApprovals => "expire_approvals",
            Self::ClassifySideEffects => "classify_side_effects",
            Self::OrphanApprovals => "orphan_approvals",
            Self::NormalizeRetryReady => "normalize_retry_ready",
            Self::RebuildBudgets => "rebuild_budgets",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MissionOutcomeMetric {
    Done,
    Failed,
    Cancelled,
    FailedFinal,
    Skipped,
    Other,
}

impl MissionOutcomeMetric {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::FailedFinal => "failed_final",
            Self::Skipped => "skipped",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    inner: Arc<Mutex<BTreeMap<String, MetricEntry>>>,
    label_registry: Arc<Mutex<BTreeMap<String, BTreeSet<&'static str>>>>,
}

#[derive(Debug, Default)]
struct MetricEntry {
    value: AtomicU64,
    labels: BTreeMap<&'static str, &'static str>,
    histogram: Option<BTreeMap<u64, AtomicU64>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_gauge(&self, name: &str, value: u64) {
        self.upsert(name, value, BTreeMap::new(), false);
    }

    pub fn set_nodes_by_state(&self, state: NodeStateMetric, value: u64) {
        self.set_state_gauge(state, value);
    }

    pub fn set_state_gauge(&self, state: NodeStateMetric, value: u64) {
        self.set_labeled_gauge(
            METRIC_NODES_BY_STATE,
            state.as_str(),
            value,
            STATE_CARDINALITY,
        );
    }

    pub fn set_sse_lag(&self, connection_class: &str, value: u64) {
        self.set_labeled_gauge(
            METRIC_SSE_LAG,
            connection_class,
            value,
            SSE_CONNECTION_CLASS_CARDINALITY,
        );
    }

    pub fn set_sse_dropped(&self, connection_class: &str, value: u64) {
        self.set_labeled_gauge(
            METRIC_SSE_DROPPED_TOTAL,
            connection_class,
            value,
            SSE_CONNECTION_CLASS_CARDINALITY,
        );
    }

    pub fn set_approval_pending(&self, value: u64) {
        self.set_gauge(METRIC_APPROVAL_PENDING, value);
    }

    pub fn set_nodes_queued(&self, value: u64) {
        self.set_gauge(METRIC_NODES_QUEUED, value);
    }

    pub fn set_nodes_running(&self, value: u64) {
        self.set_gauge(METRIC_NODES_RUNNING, value);
    }

    pub fn set_nodes_waiting_approval(&self, value: u64) {
        self.set_gauge(METRIC_NODES_WAITING_APPROVAL, value);
    }

    pub fn inc_counter(&self, name: &str) {
        self.upsert(name, 1, BTreeMap::new(), false);
    }

    pub fn inc_retries(&self, reason_class: &str) {
        self.inc_labeled_counter(METRIC_RETRIES_TOTAL, reason_class, REASON_CLASS_CARDINALITY);
    }

    pub fn inc_budget_exceeded(&self, dimension: BudgetDimensionMetric) {
        self.inc_labeled_counter(
            METRIC_BUDGET_EXCEEDED_TOTAL,
            dimension.as_str(),
            BUDGET_DIMENSION_CARDINALITY,
        );
    }

    pub fn inc_lease_reclaim(&self, transition: LeaseTransitionMetric) {
        self.inc_labeled_counter(
            METRIC_LEASE_RECLAIMS_TOTAL,
            transition.as_str(),
            LEASE_TRANSITION_CARDINALITY,
        );
    }

    pub fn inc_tool_calls(&self, tool: &str) {
        self.inc_labeled_counter(METRIC_TOOL_CALLS_TOTAL, tool, TOOL_CARDINALITY);
    }

    pub fn inc_sse_dropped(&self, connection_class: &str) {
        self.inc_labeled_counter(
            METRIC_SSE_DROPPED_TOTAL,
            connection_class,
            SSE_CONNECTION_CLASS_CARDINALITY,
        );
    }

    pub fn inc_labeled_counter(&self, name: &str, label: &str, limit: usize) {
        let normalized = ("label", self.normalize_label(name, label, limit));
        self.upsert_labeled_counter(name, normalized);
    }

    pub fn observe(&self, name: &str, value_ms: u64) {
        self.observe_labeled(name, value_ms, BTreeMap::new());
    }

    pub fn observe_event_write_latency(&self, latency_ms: u64) {
        self.observe(METRIC_EVENT_WRITE_LATENCY_MS, latency_ms);
    }

    pub fn observe_recovery_seconds(&self, phase: RecoveryPhaseMetric, seconds: u64) {
        self.observe_with_label(
            METRIC_RECOVERY_SECONDS,
            phase.as_str(),
            RECOVERY_PHASE_CARDINALITY,
            seconds,
        );
    }

    pub fn observe_mission_duration(&self, outcome: MissionOutcomeMetric, seconds: u64) {
        self.observe_with_label(
            METRIC_MISSION_DURATION_SECONDS,
            outcome.as_str(),
            MISSION_OUTCOME_CARDINALITY,
            seconds,
        );
    }

    pub fn observe_with_label(&self, name: &str, label: &str, limit: usize, value_ms: u64) {
        let normalized = ("label", self.normalize_label(name, label, limit));
        let mut labels = BTreeMap::new();
        labels.insert(normalized.0, normalized.1);
        self.observe_labeled(name, value_ms, labels);
    }

    pub fn snapshot(&self) -> Value {
        let metrics = self.inner.lock().expect("metrics lock poisoned");
        let mut out = serde_json::Map::new();
        for (name, entry) in metrics.iter() {
            let value = entry.value.load(Ordering::Relaxed);
            if let Some(histogram) = &entry.histogram {
                let mut buckets = serde_json::Map::new();
                for (bound, count) in histogram {
                    let count = count.load(Ordering::Relaxed);
                    if count > 0 {
                        buckets.insert(bound.to_string(), Value::from(count));
                    }
                }
                out.insert(
                    name.clone(),
                    json!({ "value": value, "labels": entry.labels, "buckets": buckets }),
                );
            } else {
                out.insert(
                    name.clone(),
                    json!({ "value": value, "labels": entry.labels }),
                );
            }
        }
        Value::Object(out)
    }

    fn set_labeled_gauge(&self, name: &str, label: &str, value: u64, limit: usize) {
        let normalized = ("label", self.normalize_label(name, label, limit));
        let mut labels = BTreeMap::new();
        labels.insert(normalized.0, normalized.1);
        self.set_gauge_labeled(name, value, labels);
    }

    /// Bound the number of distinct values a single metric may expose. The
    /// first `limit` values are registered; anything beyond collapses into
    /// `other` so a noisy tool name cannot leak into the exported snapshot.
    fn normalize_label(&self, name: &str, value: &str, limit: usize) -> &'static str {
        let mut registry = self
            .label_registry
            .lock()
            .expect("metrics label registry poisoned");
        let seen = registry.entry(name.to_owned()).or_default();
        if let Some(existing) = seen.get(value) {
            return existing;
        }
        if seen.len() >= limit {
            return "other";
        }
        let leaked = leak_static(value);
        seen.insert(leaked);
        leaked
    }

    fn set_gauge_labeled(
        &self,
        name: &str,
        value: u64,
        labels: BTreeMap<&'static str, &'static str>,
    ) {
        let mut metrics = self.inner.lock().expect("metrics lock poisoned");
        let key = metric_key(name, &labels);
        let entry = metrics.entry(key).or_insert_with(|| MetricEntry {
            labels,
            ..Default::default()
        });
        entry.value.store(value, Ordering::Relaxed);
    }

    fn upsert_labeled_counter(&self, name: &str, normalized: (&'static str, &'static str)) {
        let mut labels = BTreeMap::new();
        labels.insert(normalized.0, normalized.1);
        self.upsert(name, 1, labels, false);
    }

    fn upsert(
        &self,
        name: &str,
        delta: u64,
        labels: BTreeMap<&'static str, &'static str>,
        histogram: bool,
    ) {
        let mut metrics = self.inner.lock().expect("metrics lock poisoned");
        let key = metric_key(name, &labels);
        let entry = metrics.entry(key).or_insert_with(|| MetricEntry {
            labels,
            histogram: histogram.then(BTreeMap::new),
            ..Default::default()
        });
        entry.value.fetch_add(delta, Ordering::Relaxed);
    }

    fn observe_labeled(
        &self,
        name: &str,
        value_ms: u64,
        labels: BTreeMap<&'static str, &'static str>,
    ) {
        let mut metrics = self.inner.lock().expect("metrics lock poisoned");
        let key = metric_key(name, &labels);
        let entry = metrics.entry(key).or_insert_with(|| {
            let mut histogram = BTreeMap::new();
            for bucket in HISTOGRAM_BUCKETS_MS {
                histogram.insert(bucket, AtomicU64::new(0));
            }
            MetricEntry {
                labels,
                histogram: Some(histogram),
                ..Default::default()
            }
        });
        entry.value.fetch_add(1, Ordering::Relaxed);
        if let Some(histogram) = &entry.histogram {
            for (bound, count) in histogram {
                if value_ms <= *bound {
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

pub fn global_metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::new)
}

fn leak_static(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}

fn metric_key(name: &str, labels: &BTreeMap<&'static str, &'static str>) -> String {
    let mut key = name.to_owned();
    for (label, value) in labels {
        key.push('|');
        key.push_str(label);
        key.push('=');
        key.push_str(value);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labeled_metrics_limit_cardinality_and_bucket_histograms() {
        let metrics = Metrics::new();
        metrics.inc_retries("transient");
        metrics.inc_retries("network");
        metrics.inc_retries("b");
        metrics.inc_retries("c");
        metrics.inc_retries("d");
        metrics.inc_retries("e");
        metrics.inc_retries("f");
        metrics.inc_retries("g");
        metrics.inc_retries("h");
        metrics.inc_retries("i");
        metrics.inc_retries("j");
        metrics.inc_retries("k");
        metrics.inc_retries("l");
        metrics.inc_retries("m");
        metrics.inc_retries("n");
        metrics.inc_retries("o");
        metrics.inc_retries("too-long-to-fit");

        metrics.observe_event_write_latency(7);
        metrics.set_approval_pending(3);

        let snapshot = metrics.snapshot();
        let text = snapshot.to_string();
        assert!(text.contains("hive_retries_total"));
        assert!(text.contains("other"));
        assert!(!text.contains("too-long-to-fit"));
        assert!(text.contains("hive_event_write_latency_ms"));
        assert!(text.contains("hive_approval_pending"));
        assert!(text.contains("\"10\":1"));
    }
}
