use hive_types::{
    AgentSessionId, ApprovalId, ApprovalState, AttemptNo, LeaseToken, MissionId, MissionState,
    NodeId, NodeState, RunId, RunState, ToolCallId, TurnId,
};
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissionRecord {
    pub mission_id: MissionId,
    pub state: MissionState,
    pub title: String,
    pub goal: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRecord {
    pub run_id: RunId,
    pub mission_id: MissionId,
    pub state: RunState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeRecord {
    pub node_id: NodeId,
    pub mission_id: MissionId,
    pub run_id: RunId,
    pub state: NodeState,
    pub attempt_no: AttemptNo,
    pub retry_budget: u32,
    pub lease_token: Option<LeaseToken>,
    pub lease_owner: Option<String>,
    pub lease_expires_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub schema_version: u32,
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: Option<NodeId>,
    pub kind: String,
    pub last_seq: u64,
    pub payload: Value,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotInput<'a> {
    pub mission_id: MissionId,
    pub kind: &'a str,
    pub last_seq: u64,
    pub payload: Value,
    pub run_id: Option<RunId>,
    pub node_id: Option<NodeId>,
    pub now_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MissionAggregate {
    pub mission: MissionRecord,
    pub runs: Vec<RunRecord>,
    pub nodes: Vec<NodeRecord>,
    pub last_seq: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalInput {
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub session_id: Option<AgentSessionId>,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub tool_version: u32,
    pub request_hash: String,
    pub arguments_preview: String,
    pub policy_version: String,
    pub attempt_no: AttemptNo,
    pub lease_token: LeaseToken,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRecord {
    pub approval_id: ApprovalId,
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub session_id: Option<AgentSessionId>,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub tool_version: u32,
    pub request_hash: String,
    pub arguments_preview: String,
    pub policy_version: String,
    pub attempt_no: AttemptNo,
    pub lease_token: LeaseToken,
    pub state: ApprovalState,
    pub expires_at_ms: i64,
    pub created_at_ms: i64,
    pub decided_at_ms: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub consumed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolAuditRecord {
    pub audit_id: i64,
    pub mission_id: MissionId,
    pub tool_call_id: ToolCallId,
    pub event_kind: String,
    pub record: Value,
    pub prev_hash: String,
    pub entry_hash: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentSessionState {
    Active,
    Completed,
    Failed,
    Cancelled,
}

impl AgentSessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Active)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSessionRecord {
    pub session_id: AgentSessionId,
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub role: String,
    pub model: String,
    pub system_prompt: String,
    pub state: AgentSessionState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub closed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAgentSession<'a> {
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub role: &'a str,
    pub model: &'a str,
    pub system_prompt: &'a str,
    pub now_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnState {
    Started,
    Completed,
    Failed,
    Cancelled,
}

impl TurnState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "started" => Some(Self::Started),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnRecord {
    pub turn_id: TurnId,
    pub session_id: AgentSessionId,
    pub node_id: NodeId,
    pub attempt_no: AttemptNo,
    pub turn_index: u32,
    pub request_model: String,
    pub finish_reason: Option<String>,
    pub state: TurnState,
    pub output_preview: String,
    pub tool_call_count: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageSource {
    Estimated,
    Authoritative,
}

impl UsageSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Estimated => "estimated",
            Self::Authoritative => "authoritative",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "estimated" => Some(Self::Estimated),
            "authoritative" => Some(Self::Authoritative),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnUsageRecord {
    pub turn_id: TurnId,
    pub session_id: AgentSessionId,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub source: UsageSource,
    pub request_id: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewTurnUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub source: UsageSource,
    pub request_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    Approved,
    Rejected,
}

impl ApprovalDecision {
    pub const fn as_state(self) -> ApprovalState {
        match self {
            Self::Approved => ApprovalState::Approved,
            Self::Rejected => ApprovalState::Rejected,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaseGuard {
    pub attempt_no: AttemptNo,
    pub lease_token: LeaseToken,
}

/// `retry_budget` is the number of extra executions after the first one, so
/// an attempt may be retried while `attempt_no <= retry_budget`. With the
/// default budget of 3 the node gets at most 4 attempts.
pub fn can_retry(attempt_no: AttemptNo, retry_budget: u32) -> bool {
    attempt_no.get() <= retry_budget
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SideEffectState {
    Intent,
    Started,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryDecision {
    SafeToRetry,
    NeedsRecovery,
}

/// Decide whether a node with unknown termination can be safely retried.
///
/// Empty or `intent`-only side effects mean nothing irreversible happened, so
/// the node is safe to retry. Any `started` or `completed` side effect leaves
/// the true outcome ambiguous and requires manual `needs_recovery`.
pub fn classify_recovery(side_effects: &[SideEffectState]) -> RecoveryDecision {
    if side_effects.is_empty()
        || side_effects
            .iter()
            .all(|state| *state == SideEffectState::Intent)
    {
        RecoveryDecision::SafeToRetry
    } else {
        RecoveryDecision::NeedsRecovery
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_intent_only_side_effects_are_safe_to_retry() {
        assert_eq!(classify_recovery(&[]), RecoveryDecision::SafeToRetry);
        assert_eq!(
            classify_recovery(&[SideEffectState::Intent]),
            RecoveryDecision::SafeToRetry
        );
        assert_eq!(
            classify_recovery(&[SideEffectState::Intent, SideEffectState::Intent]),
            RecoveryDecision::SafeToRetry
        );
    }

    #[test]
    fn started_or_completed_side_effects_need_recovery() {
        assert_eq!(
            classify_recovery(&[SideEffectState::Started]),
            RecoveryDecision::NeedsRecovery
        );
        assert_eq!(
            classify_recovery(&[SideEffectState::Completed]),
            RecoveryDecision::NeedsRecovery
        );
        assert_eq!(
            classify_recovery(&[SideEffectState::Intent, SideEffectState::Started]),
            RecoveryDecision::NeedsRecovery
        );
    }
}
