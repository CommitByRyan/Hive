//! Fault-injection matrix: model 429/5xx, timeout, tool policy/execution
//! failures, database write lock and strong-kill recovery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hive_agent::WorkerAgentExecutor;
use hive_eval::{
    ScriptedResponse, ScriptedToolExecutor, ScriptedToolStep, ScriptedTransport, TempDir,
};
use hive_llm::{ChatRequest, Gateway, LlmError, Message, RetryPolicy};
use hive_runtime::{NodeSpec, Plan, PlanNode, ThreadedExecutor};
use hive_store::Store;
use hive_tools::{DenyApprovals, RecordingAuditSink, ToolCall, ToolContext, ToolDispatcher};
use hive_types::{
    AttemptNo, CancellationToken, EventId, EventInput, EventKind, LeaseToken, MissionId, NodeId,
    NodeState, RunId,
};
use serde_json::json;

fn completion(text: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl-fi",
        "object": "chat.completion",
        "created": 1,
        "model": "auto/eval",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": text}
        }]
    })
}

fn request() -> ChatRequest {
    ChatRequest::new("auto/eval", vec![Message::user("hi")])
}

fn create_mission_run(store: &mut Store) -> (MissionId, RunId) {
    let mission = store.create_mission("fault", "goal", 0).expect("mission");
    let run = store.create_run(mission.mission_id, 0).expect("run");
    (mission.mission_id, run.run_id)
}

fn single_plan(mission_id: MissionId, run_id: RunId, retry_budget: u32) -> (Plan, NodeId) {
    let node_id = NodeId::new();
    let plan = Plan {
        plan_id: "fault".to_owned(),
        mission_id,
        run_id,
        nodes: vec![PlanNode {
            node_id,
            deps: Vec::new(),
            spec: NodeSpec::new("worker", "auto/eval"),
            retry_budget,
        }],
    };
    (plan, node_id)
}

fn tool_context() -> ToolContext {
    ToolContext {
        mission_id: MissionId::new(),
        run_id: None,
        node_id: NodeId::new(),
        session_id: None,
        attempt_no: AttemptNo::FIRST,
        lease_token: LeaseToken::generate(),
        policy_version: "v1".to_owned(),
    }
}

#[test]
fn gateway_retries_429_bounded() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::status(429, "{}"),
        ScriptedResponse::status(429, "{}"),
        ScriptedResponse::status(429, "{}"),
        ScriptedResponse::status(429, "{}"),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let error = gateway.complete(&request()).expect_err("must exhaust");
    assert_eq!(error.status(), Some(429));
    assert_eq!(gateway.transport().request_count(), 4);
}

#[test]
fn model_5xx_fails_final_without_duplicate_node_failed() {
    let dir = TempDir::new("fi-5xx");
    let db_path = dir.db_path();
    let (mission_id, run_id) = {
        let mut store = Store::open(&db_path).expect("open");
        create_mission_run(&mut store)
    };
    let (plan, node_id) = single_plan(mission_id, run_id, 0);

    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::status(500, "{}"),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let cancel = CancellationToken::new();
    let factory = WorkerAgentExecutor::factory(Arc::new(dispatcher), Arc::new(gateway));
    let summary = ThreadedExecutor::run(&db_path, &plan, 1, &cancel, factory).expect("run");

    assert_eq!(summary.succeeded, 0);
    assert_eq!(summary.failed, 1);
    let store = Store::open(&db_path).expect("reopen");
    let node = store.get_node(node_id).expect("get").expect("present");
    assert_eq!(node.state, NodeState::FailedFinal);
    let failed = store
        .read_all_events()
        .expect("events")
        .into_iter()
        .filter(|event| event.kind == EventKind::NodeFailed)
        .count();
    assert_eq!(failed, 1, "exactly one node.failed, no duplicates");
}

#[test]
fn gateway_retries_timeout_then_succeeds() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::timeout(),
        ScriptedResponse::ok_json(&completion("recovered")),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let response = gateway.complete(&request()).expect("recover after timeout");
    assert!(response.text().is_some());
    assert_eq!(gateway.transport().request_count(), 2);
}

#[test]
fn gateway_timeout_exhausts_with_timeout_error() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::timeout(),
        ScriptedResponse::timeout(),
        ScriptedResponse::timeout(),
        ScriptedResponse::timeout(),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let error = gateway.complete(&request()).expect_err("must exhaust");
    assert!(matches!(error, LlmError::Timeout));
    assert_eq!(gateway.transport().request_count(), 4);
}

#[test]
fn tool_policy_reject_records_denial_without_side_effect() {
    let dir = TempDir::new("fi-policy");
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let mut audit = RecordingAuditSink::default();
    let error = dispatcher
        .dispatch(
            &tool_context(),
            ToolCall::new("fs.write", json!({ "path": "out.txt", "content": "x" })),
            &mut audit,
            &mut DenyApprovals,
        )
        .expect_err("denied");
    assert_eq!(error.code(), "approval.required");
    assert!(!dir.path().join("out.txt").exists());
    assert!(audit
        .events()
        .iter()
        .all(|event| event.kind != EventKind::NodeToolStarted));
}

#[test]
fn tool_execution_failure_retries_within_budget() {
    let dir = TempDir::new("fi-tool-fail");
    let db_path = dir.db_path();
    let (mission_id, run_id) = {
        let mut store = Store::open(&db_path).expect("open");
        create_mission_run(&mut store)
    };
    let (plan, node_id) = single_plan(mission_id, run_id, 1);

    let template = ScriptedToolExecutor::new(vec![
        ScriptedToolStep::ExecutionFailure,
        ScriptedToolStep::Success,
    ]);
    let cancel = CancellationToken::new();
    let summary =
        ThreadedExecutor::run(&db_path, &plan, 1, &cancel, move |_store| template.shared())
            .expect("run");

    assert_eq!(summary.succeeded, 1);
    assert_eq!(summary.failed, 1, "first attempt fails, then is retried");
    assert_eq!(summary.executed, 2);
    let store = Store::open(&db_path).expect("reopen");
    let node = store.get_node(node_id).expect("get").expect("present");
    assert_eq!(node.state, NodeState::Succeeded);
    assert_eq!(node.attempt_no.get(), 2);
}

#[test]
fn database_write_lock_does_not_lose_events() {
    let dir = TempDir::new("fi-lock");
    let db_path = dir.db_path();
    let mut store = Store::open(&db_path).expect("open");
    let mission_id = store
        .create_mission("lock", "goal", 0)
        .expect("mission")
        .mission_id;

    let raw = rusqlite::Connection::open(&db_path).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE")
        .expect("hold write lock");

    let handle = std::thread::spawn(move || {
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeReady,
            mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({}),
        };
        store.append_event(&event)
    });

    std::thread::sleep(Duration::from_millis(200));
    raw.execute_batch("COMMIT").expect("release write lock");
    let result = handle.join().expect("join writer");
    assert!(
        result.is_ok(),
        "append must succeed after lock release: {result:?}"
    );

    let store = Store::open(&db_path).expect("reopen");
    let ready = store
        .read_all_events()
        .expect("events")
        .into_iter()
        .filter(|event| event.kind == EventKind::NodeReady)
        .count();
    assert_eq!(ready, 1, "event written exactly once, none lost");
}

#[test]
fn strong_kill_recovery_marks_running_node_needs_recovery() {
    let dir = TempDir::new("fi-kill");
    let db_path = dir.db_path();
    let marker = dir.path().join("started.marker");
    let node_id = NodeId::new();
    {
        let mut store = Store::open(&db_path).expect("open");
        let mission = store.create_mission("kill", "goal", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        store
            .add_node_with_id(node_id, mission.mission_id, run.run_id, 0, 0)
            .expect("node");
        store
            .update_node_state(node_id, NodeState::Ready, 0)
            .expect("ready");
    }

    let bin = env!("CARGO_BIN_EXE_hive-eval");
    let mut child = std::process::Command::new(bin)
        .args([
            "kill-worker",
            db_path.to_str().expect("db path"),
            &node_id.to_string(),
            marker.to_str().expect("marker path"),
        ])
        .spawn()
        .expect("spawn worker");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(marker.exists(), "worker never reached running state");

    let status = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &child.id().to_string()])
        .status()
        .expect("taskkill");
    assert!(status.success(), "taskkill failed: {status}");
    let _ = child.wait();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let mut store = Store::open(&db_path).expect("reopen after kill");
    let reclaimed = store
        .reclaim_expired_leases(now + 120_000)
        .expect("reclaim");
    assert!(
        reclaimed.iter().any(|node| node.node_id == node_id),
        "killed running node must be reclaimed"
    );
    let node = store.get_node(node_id).expect("get").expect("present");
    assert_eq!(node.state, NodeState::NeedsRecovery);
    let events = store.read_all_events().expect("events");
    assert!(
        events
            .iter()
            .all(|event| event.kind != EventKind::NodeSucceeded),
        "no automatic replay of side effects after kill"
    );
}
