use hive_store::StoreError;
use hive_types::NodeId;
use thiserror::Error;

use crate::plan::PlanError;

/// Errors raised by the deterministic scheduler and plan installation.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("max_parallel must be at least 1")]
    ZeroParallelism,
    #[error("node {0} is not in a startable state")]
    NodeNotStartable(NodeId),
    #[error("node {0} is not currently in flight")]
    NodeNotInFlight(NodeId),
    #[error("node {0} has already settled")]
    NodeAlreadyTerminal(NodeId),
    #[error("node {0} is already failed but the reported outcome is not a failure")]
    NodeOutcomeMismatch(NodeId),
    #[error("threaded executor error: {0}")]
    Executor(String),
}
