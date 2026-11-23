use hive_types::{
    AgentSessionId, ApprovalId, ApprovalState, AttemptNo, EventEnvelope, HandoffArtifact,
    HandoffDecision, HandoffId, LeaseToken, MissionId, MissionState, NodeId, NodeState, RoutineId,
    RunId, RunState, ToolCallId, TurnId,
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

/// A list projection of a Mission: the persisted row plus the derived cursor
/// and pending-approval count the HTTP console needs for its list view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissionListRecord {
    pub mission: MissionRecord,
    pub last_seq: Option<u64>,
    pub pending_approvals: u64,
}

/// A single read-only transaction snapshot of the global console state.
#[derive(Clone, Debug)]
pub struct StateSnapshotRecord {
    pub missions: Vec<MissionListRecord>,
    pub approvals: Vec<ApprovalRecord>,
    pub last_seq: Option<u64>,
    pub mission_count: u64,
    pub run_count: u64,
    pub node_count: u64,
    pub event_count: u64,
    pub pending_approval_count: u64,
}

/// A single read-only transaction snapshot of one Mission detail view.
#[derive(Clone, Debug)]
pub struct MissionSnapshotRecord {
    pub mission: MissionRecord,
    pub runs: Vec<RunRecord>,
    pub nodes: Vec<NodeRecord>,
    pub approvals: Vec<ApprovalRecord>,
    pub artifacts: Vec<ArtifactRecord>,
    pub messages: Vec<EventEnvelope>,
    pub events: Vec<EventEnvelope>,
    pub last_seq: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRecord {
    pub artifact_id: hive_types::ArtifactId,
    pub mission_id: MissionId,
    pub node_id: Option<NodeId>,
    pub tool_call_id: Option<ToolCallId>,
    pub path: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub created_at_ms: i64,
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

/// Persisted projection of a structured Handoff (PROTOCOL.md V4 contract).
///
/// Structured list fields are stored as JSON text in the `handoffs` table;
/// `created_at_ms` is the store-assigned timestamp.
#[derive(Clone, Debug, PartialEq)]
pub struct HandoffRecord {
    pub handoff_id: HandoffId,
    pub mission_id: MissionId,
    pub run_id: RunId,
    pub node_id: NodeId,
    pub attempt_no: AttemptNo,
    pub agent: String,
    pub schema_version: u32,
    pub objective: String,
    pub summary: String,
    pub base_revision: String,
    pub created_at_ms: i64,
    pub inputs: Vec<String>,
    pub artifacts: Vec<HandoffArtifact>,
    pub decisions: Vec<HandoffDecision>,
    pub assumptions: Vec<String>,
    pub risks: Vec<String>,
    pub open_questions: Vec<String>,
    pub acceptance: Vec<String>,
    pub recommended_next: Vec<String>,
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
pub enum BudgetDimension {
    Tokens,
    Cost,
    Requests,
    ToolCalls,
    WallMs,
}

impl BudgetDimension {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Cost => "cost",
            Self::Requests => "requests",
            Self::ToolCalls => "tool_calls",
            Self::WallMs => "wall_ms",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tokens" => Some(Self::Tokens),
            "cost" => Some(Self::Cost),
            "requests" => Some(Self::Requests),
            "tool_calls" => Some(Self::ToolCalls),
            "wall_ms" => Some(Self::WallMs),
            _ => None,
        }
    }
}

/// A persisted budget ledger row: the allocation (`hard_limit`/`soft_limit`)
/// plus the materialized `consumed` counter. Facts remain authoritative in
/// `turn_usage` (tokens/cost/requests) and `tool_calls` (tool_calls count).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetRecord {
    pub scope_kind: String,
    pub scope_id: String,
    pub dimension: String,
    pub currency: Option<String>,
    pub hard_limit: Option<i64>,
    pub soft_limit: Option<i64>,
    pub consumed: i64,
    pub updated_at_ms: i64,
}

/// Allocation limits for one budget dimension. Grouped into a single value so
/// the store setters stay below clippy's argument-count ceiling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetLimits {
    pub hard_limit: Option<i64>,
    pub soft_limit: Option<i64>,
}

/// Read-only budget summary for one node. Deliberately exposes only the
/// dimension, counters and limits — never API keys, base URLs or unit prices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetSummary {
    pub node_id: NodeId,
    pub dimension: String,
    pub consumed: i64,
    pub hard_limit: Option<i64>,
    pub remaining: Option<i64>,
}

/// A persisted Routine row (V6). `enabled` is `false` when a tick has disabled
/// the row after encountering an invalid schedule or an unrecoverable fire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutineRecord {
    pub routine_id: RoutineId,
    pub name: String,
    pub goal: String,
    pub schedule: String,
    pub enabled: bool,
    pub timezone: String,
    pub last_run_ms: Option<i64>,
    pub next_run_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutineFireState {
    Created,
    Failed,
}

impl RoutineFireState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "created" => Some(Self::Created),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One fire window for a routine. `(routine_id, fire_at_ms)` is the primary
/// key, so a window can be fired at most once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutineFireRecord {
    pub routine_id: RoutineId,
    pub fire_at_ms: i64,
    pub mission_id: Option<MissionId>,
    pub state: RoutineFireState,
    pub created_at_ms: i64,
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
