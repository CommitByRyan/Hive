use std::time::Duration;

use hive_policy::{request_hash, HashRequest, RiskLevel};
use hive_store::{ApprovalInput, ApprovalRecord, LeaseGuard, NodeRecord, Store};
use hive_types::{
    AgentSessionId, ApprovalId, AttemptNo, EventId, EventInput, EventKind, LeaseToken, MissionId,
    NodeId, RunId, ToolCallId,
};
use serde_json::{json, Value};

use crate::AgentError;

const PREVIEW_CHARS: usize = 512;

/// How the runner treats side-effecting tools.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApprovalPolicy {
    /// Side-effecting tools return `approval.required` to the model.
    #[default]
    Deny,
    /// Side-effecting tools create a pending approval and park the node in
    /// `waiting_approval`; the run returns [`crate::AgentOutcome::WaitingApproval`].
    Request,
}

impl ApprovalPolicy {
    pub const fn requires_approval(self, risk: RiskLevel) -> bool {
        matches!(
            (self, risk),
            (
                Self::Request,
                RiskLevel::Write | RiskLevel::Execute | RiskLevel::Network
            )
        )
    }
}

/// One tool call that needs an approval decision.
pub struct ApprovalRequest<'a> {
    pub mission_id: MissionId,
    pub run_id: Option<RunId>,
    pub node_id: NodeId,
    pub session_id: AgentSessionId,
    pub attempt_no: AttemptNo,
    pub lease_token: LeaseToken,
    pub policy_version: &'a str,
    pub tool_name: &'a str,
    pub tool_version: u32,
    pub arguments: &'a Value,
}

/// Store-backed approval boundary.
///
/// `request` writes the approval and moves the node to `waiting_approval` in
/// one lease-guarded transaction. `consume` performs the one-shot consumption
/// after a human approves; a second call returns `ApprovalReplayed`.
pub struct StoreApprovalGate<'a> {
    store: &'a mut Store,
    timeout: Duration,
}

impl<'a> StoreApprovalGate<'a> {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(store: &'a mut Store, timeout: Duration) -> Self {
        Self { store, timeout }
    }

    pub fn request(
        &mut self,
        request: &ApprovalRequest<'_>,
        guard: &LeaseGuard,
        now_ms: i64,
    ) -> Result<(NodeRecord, ApprovalRecord), AgentError> {
        let tool_call_id = ToolCallId::new();
        let mission = request.mission_id.to_string();
        let run = request.run_id.map(|id| id.to_string());
        let node = request.node_id.to_string();
        let session = request.session_id.to_string();
        let hash = request_hash(&HashRequest {
            tool_name: request.tool_name,
            tool_version: request.tool_version,
            arguments: request.arguments,
            mission_id: &mission,
            run_id: run.as_deref(),
            node_id: &node,
            session_id: Some(&session),
            attempt_no: request.attempt_no.get(),
            lease_token: request.lease_token.as_str(),
            policy_version: request.policy_version,
        });
        let arguments_preview = preview(request.arguments);
        let expires_at_ms =
            now_ms.saturating_add(i64::try_from(self.timeout.as_millis()).unwrap_or(i64::MAX));
        let input = ApprovalInput {
            mission_id: request.mission_id,
            run_id: request.run_id,
            node_id: request.node_id,
            session_id: Some(request.session_id),
            tool_call_id,
            tool_name: request.tool_name.to_owned(),
            tool_version: request.tool_version,
            request_hash: hash.clone(),
            arguments_preview: arguments_preview.clone(),
            policy_version: request.policy_version.to_owned(),
            attempt_no: request.attempt_no,
            lease_token: request.lease_token.clone(),
            expires_at_ms,
        };
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeApprovalRequested,
            mission_id: request.mission_id,
            run_id: request.run_id,
            node_id: Some(request.node_id),
            session_id: Some(request.session_id),
            attempt_no: Some(request.attempt_no),
            trace_id: None,
            payload: json!({
                "tool_call_id": tool_call_id.to_string(),
                "tool_name": request.tool_name,
                "tool_version": request.tool_version,
                "request_hash": hash,
                "arguments_preview": arguments_preview,
                "expires_at_ms": expires_at_ms,
            }),
        };
        let (node, approval, _) = self.store.transition_node_cas_and_request_approval(
            request.node_id,
            guard,
            &input,
            &event,
            now_ms,
        )?;
        Ok((node, approval))
    }

    pub fn consume(
        &mut self,
        approval_id: ApprovalId,
        tool_call_id: ToolCallId,
        request_hash: &str,
        attempt_no: AttemptNo,
        lease_token: &LeaseToken,
        now_ms: i64,
    ) -> Result<ApprovalRecord, AgentError> {
        self.store
            .consume_approval(
                approval_id,
                tool_call_id,
                request_hash,
                attempt_no,
                lease_token,
                now_ms,
            )
            .map_err(AgentError::Store)
    }
}

fn preview(arguments: &Value) -> String {
    let encoded = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
    encoded.chars().take(PREVIEW_CHARS).collect()
}
