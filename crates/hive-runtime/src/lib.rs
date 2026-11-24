//! Deterministic DAG scheduling core for Hive.
//!
//! This crate provides the run-level state machine ([`Scheduler`]) that turns
//! a validated [`Plan`] into a dependency-ordered stream of node starts,
//! retries, skips and cancellations. It is single-threaded and network-free by
//! design: callers drive it with `next_actions` / `on_started` / `on_finished`
//! so the concurrency semantics can be exercised deterministically in tests.
//! The threaded executor is H05b.

mod clock;
mod error;
mod executor;
mod lifecycle;
mod logging;
mod metrics;
mod plan;
mod recovery;
mod routine;
mod scheduler;
mod threaded;

pub use clock::{Clock, SystemClock, TestClock};
pub use error::RuntimeError;
pub use executor::{ExecOutcome, NodeExecutor};
pub use lifecycle::{run_startup, tick_once, TickReport};
pub use logging::{
    emit, emit_error, emit_info, new_span_id, start_capture, LogCapture, LogContext, LogLevel,
};
pub use metrics::{
    global_metrics, BudgetDimensionMetric, LeaseTransitionMetric, Metrics, MissionOutcomeMetric,
    NodeStateMetric, RecoveryPhaseMetric, HISTOGRAM_BUCKETS_MS, METRIC_BUDGET_EXCEEDED_TOTAL,
};
pub use plan::{validate_plan, NodeSpec, Plan, PlanError, PlanNode, Topology};
pub use recovery::{run_startup_recovery, RecoveryReport};
pub use routine::{tick_routines, RoutineTickReport};
pub use scheduler::{install_plan, Action, Scheduler, StoreLike};
pub use threaded::{RunSummary, ThreadedExecutor};
