use std::time::Duration;

use hive_policy::{ApprovalMode, RiskLevel, ToolSpec};
use hive_types::{AgentSessionId, AttemptNo, LeaseToken, MissionId, NodeId, RunId, ToolCallId};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub session_id: Option<AgentSessionId>,
    pub attempt_no: AttemptNo,
    pub lease_token: LeaseToken,
    pub policy_version: String,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_name: String,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(tool_name: impl Into<String>, arguments: Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NormalizedToolRequest {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub tool_version: u32,
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub session_id: Option<AgentSessionId>,
    pub attempt_no: AttemptNo,
    pub lease_token: LeaseToken,
    pub arguments: Value,
    pub request_hash: String,
    pub policy_version: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub bytes: usize,
    pub truncated: bool,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallOutcome {
    pub tool_call_id: ToolCallId,
    pub request_hash: String,
    pub output: ToolOutput,
}

pub(crate) fn builtin_spec(
    name: &str,
    risk: RiskLevel,
    approval: ApprovalMode,
    max_output: usize,
    required: &[&str],
    allowed: &[&str],
    max_argument_bytes: usize,
) -> ToolSpec {
    ToolSpec::new(name, 1, risk, approval, Duration::from_secs(5), max_output)
        .with_arguments(
            required.iter().map(|value| (*value).to_owned()),
            allowed.iter().map(|value| (*value).to_owned()),
        )
        .with_max_argument_bytes(max_argument_bytes)
}
