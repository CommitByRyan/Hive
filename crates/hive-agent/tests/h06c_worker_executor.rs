//! H06c: real `ThreadedExecutor::run` + `WorkerAgentExecutor<MockTransport>`
//! integration tests. These prove the scheduler lease, Handoff persistence,
//! retry and cancellation all close over a real agent loop, not just a guard
//! recording stub.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

use hive_agent::WorkerAgentExecutor;
use hive_llm::{
    ChatRequest, ChatTransport, Gateway, HttpResponse, LlmError, MockResponse, MockTransport,
    RetryPolicy,
};
use hive_runtime::{
    ExecOutcome, NodeExecutor, NodeSpec, Plan, PlanNode, RunSummary, ThreadedExecutor,
};
use hive_store::{LeaseGuard, Store};
use hive_tools::ToolDispatcher;
use hive_types::{CancellationToken, EventKind, LeaseToken, MissionId, NodeId, NodeState, RunId};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let mut dir = std::env::temp_dir();
        dir.push(format!("hive-h06c-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

fn node(id: NodeId, deps: &[NodeId], retry_budget: u32) -> PlanNode {
    PlanNode {
        node_id: id,
        deps: deps.to_vec(),
        spec: NodeSpec::new("worker", "mock-model"),
        retry_budget,
    }
}

fn setup_db(db_path: &Path) -> (MissionId, RunId) {
    let mut store = Store::open(db_path).expect("open");
    let mission = store
        .create_mission("h06c", "real worker integration", 0)
        .expect("mission");
    let run = store.create_run(mission.mission_id, 0).expect("run");
    (mission.mission_id, run.run_id)
}

/// Records the scheduler-provided lease guard, then delegates the whole agent
/// loop to [`WorkerAgentExecutor`]. Used only to assert the two parallel nodes
/// were granted distinct lease tokens.
struct RecordingExecutor<T: ChatTransport> {
    inner: WorkerAgentExecutor<T>,
    tokens: Arc<Mutex<HashMap<NodeId, LeaseToken>>>,
}

impl<T: ChatTransport> NodeExecutor for RecordingExecutor<T> {
    fn execute(
        &mut self,
        node: &PlanNode,
        lease: &LeaseGuard,
        cancel: &CancellationToken,
    ) -> ExecOutcome {
        self.tokens
            .lock()
            .expect("tokens")
            .insert(node.node_id, lease.lease_token.clone());
        self.inner.execute(node, lease, cancel)
    }
}

#[test]
fn two_independent_nodes_succeed_in_parallel_with_distinct_handoffs_and_leases() {
    let dir = TempDir::new("parallel");
    let db_path = dir.path().join("hive.db");
    let (mission_id, run_id) = setup_db(&db_path);
    let a = NodeId::new();
    let b = NodeId::new();
    let plan = Plan {
        plan_id: "parallel".to_owned(),
        mission_id,
        run_id,
        nodes: vec![node(a, &[], 0), node(b, &[], 0)],
    };

    let transport = MockTransport::new(vec![
        MockResponse::json(200, &completion("done-a")),
        MockResponse::json(200, &completion("done-b")),
    ]);
    let gateway = Gateway::new(transport);
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let tokens: Arc<Mutex<HashMap<NodeId, LeaseToken>>> = Arc::default();
    let seen = Arc::clone(&tokens);

    let factory = {
        let tools = Arc::new(dispatcher);
        let gateway = Arc::new(gateway);
        move |store: Store| RecordingExecutor {
            inner: WorkerAgentExecutor::new(store, Arc::clone(&tools), Arc::clone(&gateway)),
            tokens: Arc::clone(&seen),
        }
    };

    let summary: RunSummary =
        ThreadedExecutor::run(&db_path, &plan, 2, &CancellationToken::new(), factory).expect("run");
    assert_eq!(summary.succeeded, 2);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.cancelled, 0);
    assert_eq!(summary.skipped, 0);

    let tokens = tokens.lock().expect("tokens");
    assert_eq!(tokens.len(), 2);
    assert_ne!(
        tokens[&a], tokens[&b],
        "parallel nodes need distinct leases"
    );

    let store = Store::open(&db_path).expect("reopen");
    let handoffs = store.list_handoffs(mission_id).expect("handoffs");
    assert_eq!(handoffs.len(), 2);
    let events = store.read_all_events().expect("events");

    for id in [a, b] {
        let record = store.get_node(id).expect("get").expect("present");
        assert_eq!(record.state, NodeState::Succeeded);
        assert_eq!(record.attempt_no.get(), 1);
        let started = events
            .iter()
            .filter(|event| event.kind == EventKind::NodeStarted && event.node_id == Some(id))
            .count();
        assert_eq!(started, 1, "exactly one node.started per node");
        let handoff = handoffs
            .iter()
            .find(|handoff| handoff.node_id == id)
            .expect("handoff per node");
        let succeeded = events
            .iter()
            .find(|event| event.kind == EventKind::NodeSucceeded && event.node_id == Some(id))
            .expect("succeeded");
        assert_eq!(
            succeeded.payload["handoff_id"],
            handoff.handoff_id.to_string()
        );
    }
}

#[test]
fn retryable_500_fails_once_then_succeeds_on_next_attempt() {
    let dir = TempDir::new("retry");
    let db_path = dir.path().join("hive.db");
    let (mission_id, run_id) = setup_db(&db_path);
    let n = NodeId::new();
    let plan = Plan {
        plan_id: "retry".to_owned(),
        mission_id,
        run_id,
        nodes: vec![node(n, &[], 1)],
    };

    let transport = MockTransport::new(vec![
        MockResponse::text(500, r#"{"error":{"message":"boom"}}"#),
        MockResponse::json(200, &completion("recovered")),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(0));
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let factory = WorkerAgentExecutor::factory(Arc::new(dispatcher), Arc::new(gateway));

    let summary: RunSummary =
        ThreadedExecutor::run(&db_path, &plan, 1, &CancellationToken::new(), factory).expect("run");
    assert_eq!(summary.executed, 2);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(summary.failed, 1);

    let store = Store::open(&db_path).expect("reopen");
    let record = store.get_node(n).expect("get").expect("present");
    assert_eq!(record.state, NodeState::Succeeded);
    assert_eq!(record.attempt_no.get(), 2);

    let events = store.read_all_events().expect("events");
    let failed = events
        .iter()
        .filter(|event| event.kind == EventKind::NodeFailed)
        .count();
    assert_eq!(failed, 1, "exactly one node.failed, no duplicate settle");
    let started = events
        .iter()
        .filter(|event| event.kind == EventKind::NodeStarted)
        .count();
    assert_eq!(started, 2, "one node.started per attempt");
}

struct BlockingTransport {
    release: Mutex<mpsc::Receiver<()>>,
}

impl ChatTransport for BlockingTransport {
    fn send(&self, _request: &ChatRequest) -> Result<HttpResponse, LlmError> {
        let _ = self.release.lock().expect("release lock").recv();
        Ok(HttpResponse::new(200, completion("done").to_string()))
    }
}

#[test]
fn cancelling_token_stops_worker_and_marks_node_cancelled() {
    let dir = TempDir::new("cancel");
    let db_path = dir.path().join("hive.db");
    let (mission_id, run_id) = setup_db(&db_path);
    let n = NodeId::new();
    let plan = Plan {
        plan_id: "cancel".to_owned(),
        mission_id,
        run_id,
        nodes: vec![node(n, &[], 0)],
    };

    let (tx, rx) = mpsc::channel();
    let transport = BlockingTransport {
        release: Mutex::new(rx),
    };
    let gateway = Arc::new(Gateway::with_policy(transport, RetryPolicy::immediate(0)));
    let dispatcher = Arc::new(ToolDispatcher::with_default_root(dir.path()).expect("dispatcher"));
    let cancel = CancellationToken::new();

    let factory = WorkerAgentExecutor::factory(Arc::clone(&dispatcher), Arc::clone(&gateway));
    let db_path_thread = db_path.clone();
    let plan_thread = plan.clone();
    let cancel_thread = cancel.clone();
    let handle = std::thread::spawn(move || {
        ThreadedExecutor::run(&db_path_thread, &plan_thread, 1, &cancel_thread, factory)
    });

    cancel.cancel();
    tx.send(()).expect("release worker");
    let summary: RunSummary = handle.join().expect("join").expect("run");

    assert_eq!(summary.cancelled, 1);
    assert_eq!(summary.succeeded, 0);

    let store = Store::open(&db_path).expect("reopen");
    let record = store.get_node(n).expect("get").expect("present");
    assert_eq!(record.state, NodeState::Cancelled);

    let events = store.read_all_events().expect("events");
    let cancelled = events
        .iter()
        .filter(|event| event.kind == EventKind::NodeCancelled)
        .count();
    assert_eq!(cancelled, 1);
    let tool_started = events
        .iter()
        .filter(|event| event.kind == EventKind::NodeToolStarted)
        .count();
    assert_eq!(tool_started, 0, "no tool side effects after cancellation");
}
