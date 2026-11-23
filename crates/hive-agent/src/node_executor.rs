//! Adapt H04's [`AgentRunner`] to the runtime's [`NodeExecutor`] contract.
//!
//! The runtime drives DAG scheduling and leases; this impl wraps `run_node`
//! so a node spec can be turned into a fresh agent session and executed, then
//! maps the H04 outcome/error surface onto the runtime's [`ExecOutcome`].

use hive_llm::ChatTransport;
use hive_runtime::{ExecOutcome, NodeExecutor, PlanNode};
use hive_store::{LeaseGuard, NewAgentSession, StoreError};
use hive_types::CancellationToken;

use crate::{AgentError, AgentOutcome, AgentRunner, AgentSpec};

impl<T: ChatTransport> NodeExecutor for AgentRunner<'_, T> {
    fn execute(
        &mut self,
        node: &PlanNode,
        lease: &LeaseGuard,
        cancel: &CancellationToken,
    ) -> ExecOutcome {
        map_outcome(self.run_for(node, lease, cancel))
    }
}

impl<T: ChatTransport> AgentRunner<'_, T> {
    /// Create an agent session for `node` and drive it through
    /// [`AgentRunner::run_node_with_lease`] using the scheduler's lease.
    pub(crate) fn run_for(
        &mut self,
        node: &PlanNode,
        lease: &LeaseGuard,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let record = self
            .store_mut()
            .get_node(node.node_id)?
            .ok_or_else(|| StoreError::NotFound(format!("node {}", node.node_id)))?;
        let spec = AgentSpec::new(node.spec.role.clone(), node.spec.model.clone())
            .with_system_prompt(node.spec.objective.clone());
        let session = self.store_mut().create_agent_session(&NewAgentSession {
            mission_id: record.mission_id,
            run_id: Some(record.run_id),
            node_id: record.node_id,
            role: spec.role.as_str(),
            model: spec.model.as_str(),
            system_prompt: spec.system_prompt.as_str(),
            now_ms: now_ms(),
        })?;
        self.run_node_with_lease(session.session_id, node.node_id, lease, &spec, cancel)
    }
}

pub(crate) fn map_outcome(outcome: Result<AgentOutcome, AgentError>) -> ExecOutcome {
    match outcome {
        Ok(AgentOutcome::Completed { final_text, .. }) => ExecOutcome::Succeeded {
            summary: final_text,
        },
        Ok(AgentOutcome::TurnLimitExceeded { turns, .. }) => ExecOutcome::Failed {
            reason: format!("turn_limit_exceeded after {turns} turns"),
            retryable: false,
        },
        Ok(AgentOutcome::Cancelled { .. }) => ExecOutcome::Cancelled,
        Ok(AgentOutcome::WaitingApproval { tool_name, .. }) => ExecOutcome::Failed {
            reason: format!("waiting_approval: {tool_name}"),
            retryable: false,
        },
        Err(error) => map_error(error),
    }
}

fn map_error(error: AgentError) -> ExecOutcome {
    match error {
        AgentError::Llm(llm) if llm.is_cancelled() => ExecOutcome::Cancelled,
        AgentError::Llm(llm) if is_retryable_status(llm.status()) => ExecOutcome::Failed {
            reason: llm.to_string(),
            retryable: true,
        },
        AgentError::Llm(llm) => ExecOutcome::Failed {
            reason: llm.to_string(),
            retryable: false,
        },
        other => ExecOutcome::Failed {
            reason: other.to_string(),
            retryable: false,
        },
    }
}

/// Model-side 429 and 5xx responses are retryable at the Node level; anything
/// else (400, transport, timeout, malformed response) is a final failure.
fn is_retryable_status(status: Option<u16>) -> bool {
    matches!(status, Some(429) | Some(500..=599))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use hive_llm::{Gateway, LlmError, MockResponse, MockTransport};
    use hive_runtime::NodeSpec;
    use hive_store::Store;
    use hive_tools::ToolDispatcher;
    use hive_types::{AgentSessionId, ApprovalId, EventKind, LeaseToken, NodeId, NodeState};
    use serde_json::json;

    use super::*;

    fn completion(content: &str) -> serde_json::Value {
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": content}
            }]
        })
    }

    #[test]
    fn turn_limit_maps_to_non_retryable_failure() {
        let outcome = AgentOutcome::TurnLimitExceeded {
            session_id: AgentSessionId::new(),
            turns: 3,
        };
        match map_outcome(Ok(outcome)) {
            ExecOutcome::Failed { reason, retryable } => {
                assert!(!retryable);
                assert!(reason.contains("turn_limit_exceeded"));
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn api_429_and_5xx_are_retryable() {
        for status in [429u16, 500, 502, 599] {
            let error = AgentError::Llm(LlmError::api(status, "upstream"));
            match map_error(error) {
                ExecOutcome::Failed { retryable, .. } => assert!(retryable, "status {status}"),
                other => panic!("expected failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn api_400_is_not_retryable() {
        let error = AgentError::Llm(LlmError::api(400, "bad request"));
        match map_error(error) {
            ExecOutcome::Failed { retryable, .. } => assert!(!retryable),
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_llm_maps_to_cancelled() {
        assert_eq!(
            map_error(AgentError::Llm(LlmError::Cancelled)),
            ExecOutcome::Cancelled
        );
    }

    #[test]
    fn waiting_approval_maps_to_non_retryable_failure() {
        let outcome = AgentOutcome::WaitingApproval {
            session_id: AgentSessionId::new(),
            turns: 1,
            approval_id: ApprovalId::new(),
            tool_name: "fs.write".to_owned(),
        };
        match map_outcome(Ok(outcome)) {
            ExecOutcome::Failed { reason, retryable } => {
                assert!(!retryable);
                assert!(reason.contains("waiting_approval"));
            }
            other => panic!("expected failed, got {other:?}"),
        }
    }

    #[test]
    fn execute_drives_run_node_to_completion() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("hive-node-executor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "goal", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node_id = NodeId::new();
        store
            .add_node_with_id(node_id, mission.mission_id, run.run_id, 0, 0)
            .expect("node");
        store
            .update_node_state(node_id, NodeState::Ready, 0)
            .expect("ready");
        let lease_token = LeaseToken::generate();
        let leased = store
            .lease_node(node_id, &lease_token, "scheduler", 60_000, 0)
            .expect("lease");
        let guard = LeaseGuard {
            attempt_no: leased.attempt_no,
            lease_token,
        };

        let transport = MockTransport::new(vec![MockResponse::json(200, &completion("done"))]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(&dir).expect("dispatcher");
        let token = CancellationToken::new();
        let plan_node = PlanNode {
            node_id,
            deps: Vec::new(),
            spec: NodeSpec::new("worker", "mock-model"),
            retry_budget: 0,
        };

        let outcome = {
            let mut runner = AgentRunner::new(&mut store, &dispatcher, &gateway);
            runner.execute(&plan_node, &guard, &token)
        };

        assert!(matches!(outcome, ExecOutcome::Succeeded { .. }));
        assert_eq!(
            store
                .get_node(node_id)
                .expect("get")
                .expect("present")
                .state,
            NodeState::Succeeded
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn execute_with_stale_guard_returns_failure_without_starting() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("hive-node-executor-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "goal", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node_id = NodeId::new();
        store
            .add_node_with_id(node_id, mission.mission_id, run.run_id, 0, 0)
            .expect("node");
        store
            .update_node_state(node_id, NodeState::Ready, 0)
            .expect("ready");
        let lease_token = LeaseToken::generate();
        let leased = store
            .lease_node(node_id, &lease_token, "scheduler", 60_000, 0)
            .expect("lease");
        let wrong_guard = LeaseGuard {
            attempt_no: leased.attempt_no,
            lease_token: LeaseToken::generate(),
        };

        let transport = MockTransport::default();
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(&dir).expect("dispatcher");
        let token = CancellationToken::new();
        let plan_node = PlanNode {
            node_id,
            deps: Vec::new(),
            spec: NodeSpec::new("worker", "mock-model"),
            retry_budget: 0,
        };

        let outcome = {
            let mut runner = AgentRunner::new(&mut store, &dispatcher, &gateway);
            runner.execute(&plan_node, &wrong_guard, &token)
        };

        assert!(matches!(
            outcome,
            ExecOutcome::Failed {
                retryable: false,
                ..
            }
        ));
        let persisted = store.get_node(node_id).expect("get").expect("present");
        assert_eq!(persisted.state, NodeState::Leased);
        assert_eq!(persisted.lease_token.as_ref(), Some(&lease_token));
        let started = store
            .read_all_events()
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == EventKind::NodeStarted)
            .count();
        assert_eq!(started, 0, "a stale guard must not emit node.started");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
