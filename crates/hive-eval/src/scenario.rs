//! Fixed scenario set. Each scenario is deterministic and produces one
//! [`RunRecord`] JSONL line per `--runs` iteration.

use std::sync::{Arc, Barrier};
use std::time::Instant;

use hive_agent::WorkerAgentExecutor;
use hive_llm::{Gateway, RetryPolicy};
use hive_runtime::{
    install_plan, validate_plan, Action, ExecOutcome, NodeSpec, Plan, PlanNode, Scheduler,
    ThreadedExecutor,
};
use hive_store::Store;
use hive_tools::ToolDispatcher;
use hive_types::{CancellationToken, EventKind, MissionId, NodeId, NodeState, RunId};

use crate::fakes::{ScriptedResponse, ScriptedTransport, SucceedingExecutor};
use crate::harness::{
    completion, count_events, create_mission_and_run, db_bytes, now_ms, RunRecord, TempDir,
};

/// The fixed scenario identifiers understood by the CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scenario {
    Single,
    Dag,
    Handoff,
    Retry,
    Budget,
    Cancel,
}

impl Scenario {
    pub const ALL: [Scenario; 6] = [
        Scenario::Single,
        Scenario::Dag,
        Scenario::Handoff,
        Scenario::Retry,
        Scenario::Budget,
        Scenario::Cancel,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Single => "s1-single",
            Scenario::Dag => "s2-dag",
            Scenario::Handoff => "s3-handoff",
            Scenario::Retry => "s4-retry",
            Scenario::Budget => "s5-budget",
            Scenario::Cancel => "s6-cancel",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// Run one iteration of `scenario` and return its stable output record.
pub fn run_once(scenario: Scenario, commit: &str, run_index: u32) -> RunRecord {
    match scenario {
        Scenario::Single => run_single(commit, run_index),
        Scenario::Dag => run_dag(commit, run_index),
        Scenario::Handoff => run_handoff(commit, run_index),
        Scenario::Retry => run_retry(commit, run_index),
        Scenario::Budget => run_budget(commit, run_index),
        Scenario::Cancel => run_cancel(commit, run_index),
    }
}

fn node(id: NodeId, deps: Vec<NodeId>, role: &str, retry_budget: u32) -> PlanNode {
    PlanNode {
        node_id: id,
        deps,
        spec: NodeSpec::new(role, "auto/eval"),
        retry_budget,
    }
}

fn single_node_plan(
    mission_id: MissionId,
    run_id: RunId,
    node_id: NodeId,
    retry_budget: u32,
) -> Plan {
    Plan {
        plan_id: "single".to_owned(),
        mission_id,
        run_id,
        nodes: vec![node(node_id, Vec::new(), "worker", retry_budget)],
    }
}

fn run_single(commit: &str, run_index: u32) -> RunRecord {
    let dir = TempDir::new("s1-single");
    let db_path = dir.db_path();
    let start = Instant::now();

    let (mission_id, run_id) = {
        let mut store = Store::open(&db_path).expect("open");
        create_mission_and_run(&mut store, "s1-single", "finish one node")
    };
    let node_id = NodeId::new();
    let plan = single_node_plan(mission_id, run_id, node_id, 0);

    let transport =
        ScriptedTransport::new(vec![ScriptedResponse::ok_json(&completion("single done"))]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let cancel = CancellationToken::new();
    let factory = WorkerAgentExecutor::factory(Arc::new(dispatcher), Arc::new(gateway));

    let summary =
        ThreadedExecutor::run(&db_path, &plan, 1, &cancel, factory).expect("run s1-single");

    let store = Store::open(&db_path).expect("reopen");
    let events = store.read_all_events().expect("events");
    let kinds: Vec<EventKind> = events.iter().map(|event| event.kind).collect();
    let ready = kinds.iter().position(|k| *k == EventKind::NodeReady);
    let started = kinds.iter().position(|k| *k == EventKind::NodeStarted);
    let succeeded = kinds.iter().position(|k| *k == EventKind::NodeSucceeded);
    let order_ok = matches!(
        (ready, started, succeeded),
        (Some(r), Some(s), Some(d)) if r < s && s < d
    );
    let handoffs = store.list_handoffs(mission_id).expect("handoffs");
    let ok = summary.succeeded == 1 && handoffs.len() == 1 && order_ok;

    RunRecord {
        scenario: Scenario::Single.name().to_owned(),
        commit: commit.to_owned(),
        run: run_index,
        workers: 1,
        nodes: 1,
        wall_ms: start.elapsed().as_millis(),
        events: events.len() as u64,
        db_bytes: db_bytes(&db_path),
        ok,
        blocked: None,
    }
}

fn run_dag(commit: &str, run_index: u32) -> RunRecord {
    let dir = TempDir::new("s2-dag");
    let db_path = dir.db_path();

    let (mission_id, run_id) = {
        let mut store = Store::open(&db_path).expect("open");
        create_mission_and_run(&mut store, "s2-dag", "parallel fan-out")
    };
    let entry = NodeId::new();
    let mids: Vec<NodeId> = (0..8).map(|_| NodeId::new()).collect();
    let join = NodeId::new();
    let mut nodes = vec![node(entry, Vec::new(), "root", 0)];
    for mid in &mids {
        nodes.push(node(*mid, vec![entry], "mid", 0));
    }
    nodes.push(node(join, mids.clone(), "join", 0));
    let plan = Plan {
        plan_id: "s2-dag".to_owned(),
        mission_id,
        run_id,
        nodes,
    };

    let start = Instant::now();
    let template = SucceedingExecutor::with_mid_barrier(Arc::new(Barrier::new(4)));
    let observer = template.shared();
    let cancel = CancellationToken::new();
    let summary =
        ThreadedExecutor::run(&db_path, &plan, 4, &cancel, move |_store| template.shared())
            .expect("run s2-dag");

    let store = Store::open(&db_path).expect("reopen");
    let started = count_events(&store, EventKind::NodeStarted);
    let events = store.read_all_events().expect("events");
    let ok = summary.succeeded == 10 && started == 10 && observer.max_active() <= 4;

    RunRecord {
        scenario: Scenario::Dag.name().to_owned(),
        commit: commit.to_owned(),
        run: run_index,
        workers: 4,
        nodes: 10,
        wall_ms: start.elapsed().as_millis(),
        events: events.len() as u64,
        db_bytes: db_bytes(&db_path),
        ok,
        blocked: None,
    }
}

fn run_handoff(commit: &str, run_index: u32) -> RunRecord {
    let dir = TempDir::new("s3-handoff");
    let db_path = dir.db_path();
    let start = Instant::now();

    let (mission_id, run_id) = {
        let mut store = Store::open(&db_path).expect("open");
        create_mission_and_run(&mut store, "s3-handoff", "three-hop handoff chain")
    };
    let a = NodeId::new();
    let b = NodeId::new();
    let c = NodeId::new();
    let plan = Plan {
        plan_id: "s3-handoff".to_owned(),
        mission_id,
        run_id,
        nodes: vec![
            node(a, Vec::new(), "scout", 0),
            node(b, vec![a], "writer", 0),
            node(c, vec![b], "critic", 0),
        ],
    };

    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::ok_json(&completion("scout summary")),
        ScriptedResponse::ok_json(&completion("writer summary")),
        ScriptedResponse::ok_json(&completion("critic summary")),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let cancel = CancellationToken::new();
    let factory = WorkerAgentExecutor::factory(Arc::new(dispatcher), Arc::new(gateway));

    let summary =
        ThreadedExecutor::run(&db_path, &plan, 1, &cancel, factory).expect("run s3-handoff");

    let store = Store::open(&db_path).expect("reopen");
    let handoffs = store.list_handoffs(mission_id).expect("handoffs");
    let mut ids: Vec<String> = handoffs
        .iter()
        .map(|handoff| handoff.handoff_id.to_string())
        .collect();
    ids.sort();
    ids.dedup();
    let unique = ids.len() == handoffs.len();
    let base_revision_traceable = handoffs
        .iter()
        .all(|handoff| !handoff.base_revision.trim().is_empty());
    let events = store.read_all_events().expect("events");
    let ok = summary.succeeded == 3 && handoffs.len() == 3 && unique && base_revision_traceable;

    RunRecord {
        scenario: Scenario::Handoff.name().to_owned(),
        commit: commit.to_owned(),
        run: run_index,
        workers: 1,
        nodes: 3,
        wall_ms: start.elapsed().as_millis(),
        events: events.len() as u64,
        db_bytes: db_bytes(&db_path),
        ok,
        blocked: None,
    }
}

fn run_retry(commit: &str, run_index: u32) -> RunRecord {
    let dir = TempDir::new("s4-retry");
    let db_path = dir.db_path();
    let start = Instant::now();

    let (mission_id, run_id) = {
        let mut store = Store::open(&db_path).expect("open");
        create_mission_and_run(&mut store, "s4-retry", "retry a transient 500")
    };
    let node_id = NodeId::new();
    let plan = single_node_plan(mission_id, run_id, node_id, 1);

    let transport = ScriptedTransport::new(vec![
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::status(500, "{}"),
        ScriptedResponse::ok_json(&completion("recovered")),
    ]);
    let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(3));
    let dispatcher = ToolDispatcher::with_default_root(dir.path()).expect("dispatcher");
    let cancel = CancellationToken::new();
    let factory = WorkerAgentExecutor::factory(Arc::new(dispatcher), Arc::new(gateway));

    let summary =
        ThreadedExecutor::run(&db_path, &plan, 1, &cancel, factory).expect("run s4-retry");

    let store = Store::open(&db_path).expect("reopen");
    let failed = count_events(&store, EventKind::NodeFailed);
    let node_record = store.get_node(node_id).expect("get").expect("present");
    let events = store.read_all_events().expect("events");
    let ok = summary.succeeded == 1
        && failed == 1
        && node_record.state == NodeState::Succeeded
        && node_record.attempt_no.get() == 2;

    RunRecord {
        scenario: Scenario::Retry.name().to_owned(),
        commit: commit.to_owned(),
        run: run_index,
        workers: 1,
        nodes: 1,
        wall_ms: start.elapsed().as_millis(),
        events: events.len() as u64,
        db_bytes: db_bytes(&db_path),
        ok,
        blocked: None,
    }
}

fn run_budget(commit: &str, run_index: u32) -> RunRecord {
    // Budget accounting and `node.budget_exceeded` land with H09a; do not fake
    // the API before it merges.
    RunRecord {
        scenario: Scenario::Budget.name().to_owned(),
        commit: commit.to_owned(),
        run: run_index,
        workers: 0,
        nodes: 0,
        wall_ms: 0,
        events: 0,
        db_bytes: 0,
        ok: false,
        blocked: Some("pending H09a"),
    }
}

fn run_cancel(commit: &str, run_index: u32) -> RunRecord {
    let dir = TempDir::new("s6-cancel");
    let db_path = dir.db_path();
    let start = Instant::now();

    let a = NodeId::new();
    let b = NodeId::new();
    let c = NodeId::new();
    let mut store = Store::open(&db_path).expect("open");
    let (mission_id, run_id) = create_mission_and_run(&mut store, "s6-cancel", "cancel mid-run");
    let plan = Plan {
        plan_id: "s6-cancel".to_owned(),
        mission_id,
        run_id,
        nodes: vec![
            node(a, Vec::new(), "root", 0),
            node(b, vec![a], "mid", 0),
            node(c, vec![b], "leaf", 0),
        ],
    };
    let topology = validate_plan(&plan).expect("valid plan");
    install_plan(&mut store, &plan, now_ms()).expect("install");
    let mut sched =
        Scheduler::new(topology, 1, CancellationToken::new(), store).expect("scheduler");

    assert_eq!(sched.next_actions(), vec![Action::Start(a)]);
    sched.on_started(a).expect("start a");
    sched
        .on_finished(
            a,
            ExecOutcome::Succeeded {
                summary: "root".to_owned(),
            },
        )
        .expect("finish a");

    assert_eq!(sched.next_actions(), vec![Action::Start(b)]);
    sched.on_started(b).expect("start b");
    sched.start_running(b).expect("run b");
    sched.cancel().expect("cancel");

    let cancelled_b = sched.node_state(b) == NodeState::Cancelled;
    let cancelled_c = sched.node_state(c) == NodeState::Cancelled;
    let tool_started = count_events(sched.store(), EventKind::NodeToolStarted);
    let events_before_restart = sched.store().read_all_events().expect("events").len() as u64;

    // Simulate restart: a fresh connection re-runs the recovery scan; the
    // cancelled nodes are terminal so nothing may be replayed.
    let mut restarted = Store::open(&db_path).expect("restart store");
    let reclaimed = restarted
        .reclaim_expired_leases(now_ms() + 120_000)
        .expect("reclaim");
    let replayed = reclaimed
        .iter()
        .any(|node| node.node_id == b || node.node_id == c);
    let events_after_restart = restarted.read_all_events().expect("events").len() as u64;
    let b_terminal = restarted
        .get_node(b)
        .expect("get b")
        .expect("present")
        .state
        == NodeState::Cancelled;
    let c_terminal = restarted
        .get_node(c)
        .expect("get c")
        .expect("present")
        .state
        == NodeState::Cancelled;

    let ok = cancelled_b
        && cancelled_c
        && tool_started == 0
        && !replayed
        && b_terminal
        && c_terminal
        && events_after_restart == events_before_restart;

    RunRecord {
        scenario: Scenario::Cancel.name().to_owned(),
        commit: commit.to_owned(),
        run: run_index,
        workers: 1,
        nodes: 3,
        wall_ms: start.elapsed().as_millis(),
        events: events_after_restart,
        db_bytes: db_bytes(&db_path),
        ok,
        blocked: None,
    }
}
