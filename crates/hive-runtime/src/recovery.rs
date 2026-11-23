//! Startup recovery scan.
//!
//! Runs once before the scheduler accepts new work. The order is fixed and
//! every step is a CAS / state-guarded operation, so the whole scan is
//! idempotent and can be re-entered without re-executing side effects.

use hive_store::{can_retry, classify_recovery, NodeRecord, RecoveryDecision, Store};
use hive_types::{EventId, EventInput, EventKind, NodeState};
use serde_json::json;

use crate::error::RuntimeError;

/// Counters for one startup recovery pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    pub leases_revoked: usize,
    pub safe_retried: usize,
    pub kept_needs_recovery: usize,
    pub orphaned_retried: usize,
    pub orphaned_finalized: usize,
    pub orphaned_needs_recovery: usize,
    pub retry_ready_normalized: usize,
}

/// Run the startup recovery scan in the order mandated by ADR 0005 D2/D3/D4:
/// revoke active leases -> expire overdue approvals -> classify side effects ->
/// orphan-approval fail-closed -> defensive `retry_ready` normalization.
pub fn run_startup_recovery(
    store: &mut Store,
    now_ms: i64,
) -> Result<RecoveryReport, RuntimeError> {
    let mut report = RecoveryReport::default();

    // 1. Unconditionally revoke the previous instance's surviving leases.
    let revoked = store.revoke_all_active_leases(now_ms)?;
    report.leases_revoked = revoked.len();

    // 2. Overdue approvals (reuses the existing idempotent tick helper).
    store.expire_overdue_approvals(now_ms)?;

    // 3. Classify `running -> needs_recovery` nodes produced by step 1.
    for node in revoked
        .iter()
        .filter(|node| node.state == NodeState::NeedsRecovery)
    {
        recover_running_node(store, node, now_ms, &mut report)?;
    }

    // 4. Orphan approvals: fail closed, never replaying a started side effect.
    let waiting = store.find_waiting_approval_nodes()?;
    for node in waiting {
        store.expire_node_approvals(node.node_id, now_ms)?;
        let side_effects = store.side_effect_states(node.node_id, node.attempt_no)?;
        match classify_recovery(&side_effects) {
            RecoveryDecision::SafeToRetry => {
                store.fail_waiting_approval_orphaned(node.node_id, now_ms)?;
                if can_retry(node.attempt_no, node.retry_budget) {
                    let next_attempt = node
                        .attempt_no
                        .checked_next()
                        .ok_or_else(|| RuntimeError::Executor("attempt exhausted".to_owned()))?;
                    let ready = EventInput {
                        event_id: EventId::new(),
                        kind: EventKind::NodeReady,
                        mission_id: node.mission_id,
                        run_id: Some(node.run_id),
                        node_id: Some(node.node_id),
                        session_id: None,
                        attempt_no: Some(next_attempt),
                        trace_id: None,
                        payload: json!({}),
                    };
                    store.retry_failed_node_and_emit(node.node_id, &ready, now_ms)?;
                    report.orphaned_retried += 1;
                } else {
                    store.abort_failed_node(node.node_id, now_ms)?;
                    report.orphaned_finalized += 1;
                }
            }
            RecoveryDecision::NeedsRecovery => {
                store.transition_waiting_approval_to_needs_recovery(node.node_id, now_ms)?;
                report.orphaned_needs_recovery += 1;
            }
        }
    }

    // 5. Defensive retry_ready normalization.
    let normalized = store.normalize_retry_ready(now_ms)?;
    report.retry_ready_normalized = normalized.len();

    Ok(report)
}

fn recover_running_node(
    store: &mut Store,
    node: &NodeRecord,
    now_ms: i64,
    report: &mut RecoveryReport,
) -> Result<(), RuntimeError> {
    let side_effects = store.side_effect_states(node.node_id, node.attempt_no)?;
    match classify_recovery(&side_effects) {
        RecoveryDecision::SafeToRetry => {
            store.retry_needs_recovery_and_emit(node.node_id, now_ms)?;
            report.safe_retried += 1;
        }
        RecoveryDecision::NeedsRecovery => {
            report.kept_needs_recovery += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use hive_store::{ApprovalDecision, ApprovalInput, LeaseGuard, Store};
    use hive_types::{
        ApprovalState, AttemptNo, EventId, EventInput, EventKind, LeaseToken, MissionId, NodeId,
        NodeState, RunId, ToolCallId,
    };
    use serde_json::json;

    use super::*;

    fn store_with_node(retry_budget: u32) -> (Store, MissionId, RunId, NodeId) {
        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "g", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node = store
            .add_node(mission.mission_id, run.run_id, retry_budget, 0)
            .expect("node");
        (store, mission.mission_id, run.run_id, node.node_id)
    }

    fn tool_input(
        kind: EventKind,
        mission_id: MissionId,
        run_id: RunId,
        node_id: NodeId,
        tool_call_id: &str,
    ) -> EventInput {
        let mut payload = json!({ "tool_call_id": tool_call_id });
        if kind == EventKind::NodeToolCall {
            payload = json!({
                "tool_call_id": tool_call_id,
                "tool_name": "fs.read",
                "tool_version": 1,
                "request_hash": "hash",
            });
        }
        EventInput {
            event_id: EventId::new(),
            kind,
            mission_id,
            run_id: Some(run_id),
            node_id: Some(node_id),
            session_id: None,
            attempt_no: Some(AttemptNo::FIRST),
            trace_id: None,
            payload,
        }
    }

    fn drive_running(store: &mut Store, node_id: NodeId) -> LeaseGuard {
        store
            .update_node_state(node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node_id, &token, "exec", 100_000, 0)
            .expect("lease");
        store
            .update_node_state(node_id, NodeState::Running, 0)
            .expect("running");
        LeaseGuard {
            attempt_no: AttemptNo::FIRST,
            lease_token: token,
        }
    }

    fn drive_waiting_approval(
        store: &mut Store,
        mission_id: MissionId,
        run_id: RunId,
        node_id: NodeId,
        expires_at_ms: i64,
    ) -> (hive_types::ApprovalId, LeaseToken) {
        store
            .update_node_state(node_id, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node_id, &token, "exec", 100_000, 0)
            .expect("lease");
        store
            .update_node_state(node_id, NodeState::Running, 0)
            .expect("running");
        let input = ApprovalInput {
            mission_id,
            run_id: Some(run_id),
            node_id,
            session_id: None,
            tool_call_id: ToolCallId::new(),
            tool_name: "fs.write".to_owned(),
            tool_version: 1,
            request_hash: "hash".to_owned(),
            arguments_preview: "x".to_owned(),
            policy_version: "v1".to_owned(),
            attempt_no: AttemptNo::FIRST,
            lease_token: token.clone(),
            expires_at_ms,
        };
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeApprovalRequested,
            mission_id,
            run_id: Some(run_id),
            node_id: Some(node_id),
            session_id: None,
            attempt_no: Some(AttemptNo::FIRST),
            trace_id: None,
            payload: json!({}),
        };
        let (_node, approval, _event) = store
            .transition_node_and_request_approval(node_id, &input, &event, 0)
            .expect("request approval");
        (approval.approval_id, token)
    }

    fn events(store: &Store) -> Vec<EventKind> {
        store
            .read_all_events()
            .expect("events")
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    #[test]
    fn startup_revokes_unexpired_lease_to_ready() {
        let (mut store, _mission, _run, node) = store_with_node(3);
        store
            .update_node_state(node, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node, &token, "exec", 10_000_000, 0)
            .expect("lease");

        let report = run_startup_recovery(&mut store, 5_000).expect("recovery");
        assert_eq!(report.leases_revoked, 1);

        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::Ready);
        assert_eq!(node_record.attempt_no.get(), 1);
        assert!(node_record.lease_token.is_none());
        assert!(events(&store).contains(&EventKind::NodeReady));
    }

    #[test]
    fn running_intent_only_auto_retries_with_node_ready() {
        let (mut store, mission, run, node) = store_with_node(3);
        let guard = drive_running(&mut store, node);
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolCall, mission, run, node, "tc_1"),
                0,
            )
            .expect("intent");

        let report = run_startup_recovery(&mut store, 5_000).expect("recovery");
        assert_eq!(report.safe_retried, 1);
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::Ready);
        assert_eq!(node_record.attempt_no.get(), 2);
        assert!(events(&store).contains(&EventKind::NodeReady));
    }

    #[test]
    fn running_started_side_effect_needs_recovery() {
        let (mut store, mission, run, node) = store_with_node(3);
        let guard = drive_running(&mut store, node);
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolCall, mission, run, node, "tc_1"),
                0,
            )
            .expect("intent");
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolStarted, mission, run, node, "tc_1"),
                0,
            )
            .expect("started");

        let report = run_startup_recovery(&mut store, 5_000).expect("recovery");
        assert_eq!(report.kept_needs_recovery, 1);
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::NeedsRecovery);
        assert_eq!(node_record.attempt_no.get(), 1);
        assert!(events(&store).contains(&EventKind::NodeNeedsRecovery));
        assert!(!events(&store).contains(&EventKind::NodeReady));
    }

    #[test]
    fn waiting_approval_expired_retries_without_side_effect() {
        let (mut store, mission, run, node) = store_with_node(3);
        let (approval_id, _token) = drive_waiting_approval(&mut store, mission, run, node, 100);

        let report = run_startup_recovery(&mut store, 5_000).expect("recovery");
        assert_eq!(report.orphaned_retried, 1);
        let approval = store
            .get_approval(approval_id)
            .expect("get")
            .expect("approval");
        assert_eq!(approval.state, ApprovalState::Expired);
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::Ready);
        assert!(events(&store).contains(&EventKind::NodeFailed));
        assert!(events(&store).contains(&EventKind::NodeReady));
    }

    #[test]
    fn waiting_approval_approved_unconsumed_fails_closed() {
        let (mut store, mission, run, node) = store_with_node(3);
        let (approval_id, _token) = drive_waiting_approval(&mut store, mission, run, node, 100_000);
        store
            .decide_approval(approval_id, ApprovalDecision::Approved, "human", 0)
            .expect("approve");

        let report = run_startup_recovery(&mut store, 5_000).expect("recovery");
        assert_eq!(report.orphaned_retried, 1);
        let approval = store
            .get_approval(approval_id)
            .expect("get")
            .expect("approval");
        assert_eq!(approval.state, ApprovalState::Expired);
        assert_eq!(approval.decided_by.as_deref(), Some("recovery"));
        assert_eq!(approval.decision_note.as_deref(), Some("owner_lost"));
        assert!(events(&store).contains(&EventKind::NodeApprovalDecided));
        assert_eq!(
            store
                .side_effect_states(node, AttemptNo::FIRST)
                .expect("side effects")
                .len(),
            0
        );
    }

    #[test]
    fn waiting_approval_with_started_side_effect_needs_recovery() {
        let (mut store, mission, run, node) = store_with_node(3);
        let (approval_id, token) = drive_waiting_approval(&mut store, mission, run, node, 100_000);
        let guard = LeaseGuard {
            attempt_no: AttemptNo::FIRST,
            lease_token: token,
        };
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolCall, mission, run, node, "tc_s"),
                0,
            )
            .expect("intent");
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolStarted, mission, run, node, "tc_s"),
                0,
            )
            .expect("started");

        let report = run_startup_recovery(&mut store, 5_000).expect("recovery");
        assert_eq!(report.orphaned_needs_recovery, 1);
        let approval = store
            .get_approval(approval_id)
            .expect("get")
            .expect("approval");
        assert_eq!(approval.state, ApprovalState::Expired);
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::NeedsRecovery);
        assert!(events(&store).contains(&EventKind::NodeNeedsRecovery));
        assert!(!events(&store).contains(&EventKind::NodeReady));
    }

    #[test]
    fn recovery_scan_is_idempotent() {
        let (mut store, mission, run, node) = store_with_node(3);
        let guard = drive_running(&mut store, node);
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolCall, mission, run, node, "tc_1"),
                0,
            )
            .expect("intent");

        let first = run_startup_recovery(&mut store, 5_000).expect("first");
        assert_eq!(first.safe_retried, 1);
        let second = run_startup_recovery(&mut store, 5_000).expect("second");
        assert_eq!(second, RecoveryReport::default());
    }

    #[test]
    fn recovery_does_not_replay_started_tools() {
        let (mut store, mission, run, node) = store_with_node(3);
        let guard = drive_running(&mut store, node);
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolCall, mission, run, node, "tc_x"),
                0,
            )
            .expect("intent");
        store
            .append_tool_event(
                node,
                &guard,
                &tool_input(EventKind::NodeToolStarted, mission, run, node, "tc_x"),
                0,
            )
            .expect("started");

        let _ = run_startup_recovery(&mut store, 5_000).expect("recovery");
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::NeedsRecovery);
        let side_effects = store
            .side_effect_states(node, AttemptNo::FIRST)
            .expect("side effects");
        assert_eq!(side_effects.len(), 1);
    }
}
