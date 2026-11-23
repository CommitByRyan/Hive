//! Deterministic DAG scheduling core for Hive.
//!
//! This crate provides the run-level state machine ([`Scheduler`]) that turns
//! a validated [`Plan`] into a dependency-ordered stream of node starts,
//! retries, skips and cancellations. It is single-threaded and network-free by
//! design: callers drive it with `next_actions` / `on_started` / `on_finished`
//! so the concurrency semantics can be exercised deterministically in tests.
//! The threaded executor is H05b.

mod error;
mod executor;
mod plan;
mod scheduler;

pub use error::RuntimeError;
pub use executor::{ExecOutcome, NodeExecutor};
pub use plan::{validate_plan, NodeSpec, Plan, PlanError, PlanNode, Topology};
pub use scheduler::{install_plan, Action, Scheduler, StoreLike};
