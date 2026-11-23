use hive_policy::{ApprovalMode, ToolSpec};

use crate::{NormalizedToolRequest, ToolError, ToolResult};
use hive_policy::PolicyError;

pub trait ApprovalGate {
    fn check(&mut self, request: &NormalizedToolRequest, spec: &ToolSpec) -> ToolResult<()>;
}

/// Production default until H04 wires the store-backed approval consumer.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyApprovals;

impl ApprovalGate for DenyApprovals {
    fn check(&mut self, request: &NormalizedToolRequest, spec: &ToolSpec) -> ToolResult<()> {
        if spec.approval == ApprovalMode::Never {
            Ok(())
        } else {
            Err(PolicyError::ApprovalRequired(format!(
                "{} requires approval ({:?})",
                request.tool_name, spec.approval
            ))
            .into())
        }
    }
}

/// Test/diagnostic gate. Never use it for side-effecting tools in production.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllApprovals;

impl ApprovalGate for AllowAllApprovals {
    fn check(&mut self, _request: &NormalizedToolRequest, _spec: &ToolSpec) -> ToolResult<()> {
        Ok(())
    }
}

#[allow(dead_code)]
fn _assert_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ToolError>();
}
