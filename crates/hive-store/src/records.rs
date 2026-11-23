use hive_types::{
    AgentSessionId, ApprovalId, ApprovalState, AttemptNo, LeaseToken, MissionId, MissionState,
    NodeId, NodeState, RunId, RunState, ToolCallId,
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
