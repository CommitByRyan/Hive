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
mod plan;
mod recovery;
mod scheduler;
mod threaded;

pub use clock::{Clock, SystemClock, TestClock};
pub use error::RuntimeError;
pub use executor::{ExecOutcome, NodeExecutor};
pub use plan::{validate_plan, NodeSpec, Plan, PlanError, PlanNode, Topology};
pub use recovery::{run_startup_recovery, RecoveryReport};
pub use scheduler::{install_plan, Action, Scheduler, StoreLike};
pub use threaded::{RunSummary, ThreadedExecutor};
