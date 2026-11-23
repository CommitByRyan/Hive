use std::fmt::{self, Display, Formatter};

use hive_policy::{ErrorCode, PolicyError};

#[derive(Debug)]
pub struct ToolError {
    code: ErrorCode,
    message: String,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn error_code(&self) -> ErrorCode {
        self.code
    }

    pub const fn code(&self) -> &'static str {
        self.code.code()
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for ToolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.code(), self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<PolicyError> for ToolError {
    fn from(error: PolicyError) -> Self {
        Self {
            code: error.error_code(),
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        Self::new(ErrorCode::ToolExecutionFailed, error.to_string())
    }
}

impl From<hive_store::StoreError> for ToolError {
    fn from(error: hive_store::StoreError) -> Self {
        let code = match &error {
            hive_store::StoreError::ApprovalReplayed(_) => ErrorCode::ApprovalReplayed,
            hive_store::StoreError::ApprovalConflict(_) => ErrorCode::ApprovalRequired,
            hive_store::StoreError::StaleExecution => ErrorCode::ToolStaleExecution,
            _ => ErrorCode::ToolExecutionFailed,
        };
        Self::new(code, error.to_string())
    }
}

pub type ToolResult<T> = Result<T, ToolError>;
