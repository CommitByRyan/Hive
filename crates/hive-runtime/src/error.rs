use hive_store::StoreError;
use hive_types::NodeId;
use thiserror::Error;

/// Errors raised by the deterministic scheduler and plan installation.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("max_parallel must be at least 1")]
    ZeroParallelism,
    #[error("node {0} is not in a startable state")]
    NodeNotStartable(NodeId),
    #[error("node {0} is not currently in flight")]
    NodeNotInFlight(NodeId),
    #[error("node {0} has already settled")]
    NodeAlreadyTerminal(NodeId),
}
