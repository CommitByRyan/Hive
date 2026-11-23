use std::fmt::{self, Display, Formatter};

use hive_policy::{ErrorCode, PolicyError};

use crate::types::ToolOutput;

#[derive(Debug)]
pub struct ToolError {
    code: ErrorCode,
    message: String,
    exit_code: Option<i32>,
    output: Option<ToolOutput>,
}

impl ToolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            exit_code: None,
            output: None,
        }
    }

    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    pub fn with_output(mut self, output: ToolOutput) -> Self {
        self.output = Some(output);
        self
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

    pub const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn output(&self) -> Option<&ToolOutput> {
        self.output.as_ref()
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
        Self::new(error.error_code(), error.to_string())
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
