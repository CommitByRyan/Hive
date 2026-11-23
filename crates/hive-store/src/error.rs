use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("state error: {0}")]
    State(#[from] hive_types::HiveError),
    #[error("duplicate event id `{0}`")]
    DuplicateEvent(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("approval conflict: {0}")]
    ApprovalConflict(String),
    #[error("event sequence exhausted")]
    SequenceExhausted,
    #[error("stale execution: lease or attempt no longer matches")]
    StaleExecution,
    #[error("lease token mismatch")]
    LeaseTokenMismatch,
}

pub type StoreResult<T> = Result<T, StoreError>;
