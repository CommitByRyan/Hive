use thiserror::Error;

/// Stable, machine-readable policy error codes. New codes may be added, but
/// existing strings must not change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    PathOutsideWorkspace,
    PathUnsupportedForm,
    PathAdsForbidden,
    PathNotFound,
    NetworkAddressDenied,
    NetworkEndpointDenied,
    ShellDisabled,
    ShellProgramDenied,
    ToolArgumentsDenied,
    ToolNotFound,
    ToolDuplicate,
    ApprovalRequired,
    ApprovalDenied,
    ApprovalReplayed,
    ToolStaleExecution,
    ToolExecutionFailed,
    ToolAuditFailed,
}

impl ErrorCode {
    pub const fn code(self) -> &'static str {
        match self {
            Self::PathOutsideWorkspace => "path.outside_workspace",
            Self::PathUnsupportedForm => "path.unsupported_form",
            Self::PathAdsForbidden => "path.ads_forbidden",
            Self::PathNotFound => "path.not_found",
            Self::NetworkAddressDenied => "network.address_denied",
            Self::NetworkEndpointDenied => "network.endpoint_denied",
            Self::ShellDisabled => "shell.disabled",
            Self::ShellProgramDenied => "shell.program_denied",
            Self::ToolArgumentsDenied => "tool.arguments_denied",
            Self::ToolNotFound => "tool.not_found",
            Self::ToolDuplicate => "tool.duplicate",
            Self::ApprovalRequired => "approval.required",
            Self::ApprovalDenied => "approval.denied",
            Self::ApprovalReplayed => "approval.replayed",
            Self::ToolStaleExecution => "tool.stale_execution",
            Self::ToolExecutionFailed => "tool.execution_failed",
            Self::ToolAuditFailed => "tool.audit_failed",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("path is outside the workspace: {0}")]
    PathOutsideWorkspace(String),
    #[error("path form is unsupported: {0}")]
    PathUnsupportedForm(String),
    #[error("alternate data streams are forbidden: {0}")]
    PathAdsForbidden(String),
    #[error("path does not exist or cannot be resolved: {0}")]
    PathNotFound(String),
    #[error("network address is denied: {0}")]
    NetworkAddressDenied(String),
    #[error("network endpoint is denied: {0}")]
    NetworkEndpointDenied(String),
    #[error("shell execution is disabled")]
    ShellDisabled,
    #[error("shell program is denied: {0}")]
    ShellProgramDenied(String),
    #[error("tool arguments are denied: {0}")]
    ToolArgumentsDenied(String),
    #[error("tool is not registered: {0}")]
    ToolNotFound(String),
    #[error("tool is already registered: {0}")]
    ToolDuplicate(String),
    #[error("approval is required: {0}")]
    ApprovalRequired(String),
    #[error("approval was denied: {0}")]
    ApprovalDenied(String),
    #[error("approval was already consumed: {0}")]
    ApprovalReplayed(String),
    #[error("stale execution: {0}")]
    ToolStaleExecution(String),
    #[error("tool execution failed: {0}")]
    ToolExecutionFailed(String),
    #[error("tool audit failed: {0}")]
    ToolAuditFailed(String),
}

impl PolicyError {
    pub const fn code(&self) -> &'static str {
        self.error_code().code()
    }

    pub const fn error_code(&self) -> ErrorCode {
        match self {
            Self::PathOutsideWorkspace(_) => ErrorCode::PathOutsideWorkspace,
            Self::PathUnsupportedForm(_) => ErrorCode::PathUnsupportedForm,
            Self::PathAdsForbidden(_) => ErrorCode::PathAdsForbidden,
            Self::PathNotFound(_) => ErrorCode::PathNotFound,
            Self::NetworkAddressDenied(_) => ErrorCode::NetworkAddressDenied,
            Self::NetworkEndpointDenied(_) => ErrorCode::NetworkEndpointDenied,
            Self::ShellDisabled => ErrorCode::ShellDisabled,
            Self::ShellProgramDenied(_) => ErrorCode::ShellProgramDenied,
            Self::ToolArgumentsDenied(_) => ErrorCode::ToolArgumentsDenied,
            Self::ToolNotFound(_) => ErrorCode::ToolNotFound,
            Self::ToolDuplicate(_) => ErrorCode::ToolDuplicate,
            Self::ApprovalRequired(_) => ErrorCode::ApprovalRequired,
            Self::ApprovalDenied(_) => ErrorCode::ApprovalDenied,
            Self::ApprovalReplayed(_) => ErrorCode::ApprovalReplayed,
            Self::ToolStaleExecution(_) => ErrorCode::ToolStaleExecution,
            Self::ToolExecutionFailed(_) => ErrorCode::ToolExecutionFailed,
            Self::ToolAuditFailed(_) => ErrorCode::ToolAuditFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_error_codes_are_stable() {
        assert_eq!(
            ErrorCode::PathOutsideWorkspace.code(),
            "path.outside_workspace"
        );
        assert_eq!(
            ErrorCode::PathUnsupportedForm.code(),
            "path.unsupported_form"
        );
        assert_eq!(ErrorCode::PathAdsForbidden.code(), "path.ads_forbidden");
        assert_eq!(
            ErrorCode::NetworkAddressDenied.code(),
            "network.address_denied"
        );
        assert_eq!(
            ErrorCode::NetworkEndpointDenied.code(),
            "network.endpoint_denied"
        );
        assert_eq!(ErrorCode::ShellDisabled.code(), "shell.disabled");
        assert_eq!(ErrorCode::ShellProgramDenied.code(), "shell.program_denied");
        assert_eq!(
            ErrorCode::ToolArgumentsDenied.code(),
            "tool.arguments_denied"
        );
        assert_eq!(ErrorCode::ApprovalReplayed.code(), "approval.replayed");
        assert_eq!(ErrorCode::ToolStaleExecution.code(), "tool.stale_execution");
        assert_eq!(
            PolicyError::PathAdsForbidden("x".to_owned()).code(),
            "path.ads_forbidden"
        );
    }
}
