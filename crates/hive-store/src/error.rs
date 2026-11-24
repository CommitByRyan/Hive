use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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
    #[error("approval replayed: {0}")]
    ApprovalReplayed(String),
    #[error("event sequence exhausted")]
    SequenceExhausted,
    #[error("stale execution: lease or attempt no longer matches")]
    StaleExecution,
    #[error("lease token mismatch")]
    LeaseTokenMismatch,
    #[error("budget exceeded for {dimension}")]
    BudgetExceeded { dimension: String },
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),
    #[error("routine limit reached")]
    RoutineLimit,
    #[error("archive error: {0}")]
    Archive(String),
    #[error("retention error: {0}")]
    Retention(String),
}

impl StoreError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ApprovalReplayed(_) => "approval.replayed",
            Self::ApprovalConflict(_) => "approval.conflict",
            Self::StaleExecution => "tool.stale_execution",
            Self::LeaseTokenMismatch => "tool.stale_execution",
            Self::BudgetExceeded { .. } => "budget.exceeded",
            Self::InvalidSchedule(_) => "invalid_schedule",
            Self::InvalidTimezone(_) => "invalid_timezone",
            Self::RoutineLimit => "routine_limit",
            Self::Archive(_) => "archive_error",
            Self::Retention(_) => "retention_error",
            Self::NotFound(_) => "not_found",
            Self::DuplicateEvent(_) => "duplicate_event",
            Self::SequenceExhausted => "sequence_exhausted",
            Self::Conflict(_) => "conflict",
            Self::Database(_) => "database_error",
            Self::Serialization(_) => "serialization_error",
            Self::Io(_) => "io_error",
            Self::State(_) => "state_error",
        }
    }
}

pub type StoreResult<T> = Result<T, StoreError>;
