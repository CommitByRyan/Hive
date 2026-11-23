use std::collections::HashMap;

use hive_store::{can_retry, LeaseGuard, NodeRecord, Store, StoreError, StoreResult};
use hive_types::{
    AttemptNo, CancellationToken, EventEnvelope, EventId, EventInput, EventKind, LeaseToken,
    MissionId, NodeId, NodeState, RunId,
};
use serde_json::{json, Value};

use crate::clock::{Clock, SystemClock};
use crate::error::RuntimeError;
use crate::executor::ExecOutcome;
use crate::plan::{Plan, Topology};

const LEASE_OWNER: &str = "hive-runtime";
const LEASE_TTL_MS: i64 = 60_000;

/// Persistence boundary for the scheduler. Implementations must route every
/// transition through a CAS entry point (state + lease guard) and never allow
/// the runtime to issue raw `UPDATE` statements.
pub trait StoreLike {
    fn append_event(&mut self, event: &EventInput) -> StoreResult<EventEnvelope>;
    fn get_node(&self, node_id: NodeId) -> StoreResult<Option<NodeRecord>>;
    fn create_node(
        &mut self,
        node_id: NodeId,
        mission_id: MissionId,
        run_id: RunId,
        retry_budget: u32,
        now_ms: i64,
    ) -> StoreResult<NodeRecord>;
    fn mark_ready(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)>;
    fn claim_node(
        &mut self,
        node_id: NodeId,
        token: &LeaseToken,
        owner: &str,
        expires_at_ms: i64,
        next_attempt_no: AttemptNo,
        now_ms: i64,
    ) -> StoreResult<NodeRecord>;
    fn start_node(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)>;
    fn finish_node(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        next: NodeState,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)>;
    fn retry_failed_node_and_emit(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)>;
    fn abort_failed_node(&mut self, node_id: NodeId, now_ms: i64) -> StoreResult<NodeRecord>;
    fn skip_node(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)>;
    fn cancel_node(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)>;
    fn reclaim_expired_leases(&mut self, now_ms: i64) -> StoreResult<Vec<NodeRecord>>;
}

impl StoreLike for Store {
    fn append_event(&mut self, event: &EventInput) -> StoreResult<EventEnvelope> {
        Store::append_event(self, event)
    }

    fn get_node(&self, node_id: NodeId) -> StoreResult<Option<NodeRecord>> {
        Store::get_node(self, node_id)
    }

    fn create_node(
        &mut self,
        node_id: NodeId,
        mission_id: MissionId,
        run_id: RunId,
        retry_budget: u32,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        Store::add_node_with_id(self, node_id, mission_id, run_id, retry_budget, now_ms)
    }

    fn mark_ready(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        Store::transition_node_and_emit(self, node_id, NodeState::Ready, event, now_ms)
    }

    fn claim_node(
        &mut self,
        node_id: NodeId,
        token: &LeaseToken,
        owner: &str,
        expires_at_ms: i64,
        next_attempt_no: AttemptNo,
        now_ms: i64,
    ) -> StoreResult<NodeRecord> {
        Store::claim_node(
            self,
            node_id,
            token,
            owner,
            expires_at_ms,
            next_attempt_no,
            now_ms,
        )
    }

    fn start_node(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        Store::transition_node_cas(self, node_id, guard, NodeState::Running, event, now_ms)
    }

    fn finish_node(
        &mut self,
        node_id: NodeId,
        guard: &LeaseGuard,
        next: NodeState,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        Store::transition_node_cas(self, node_id, guard, next, event, now_ms)
    }

    fn retry_failed_node_and_emit(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        Store::retry_failed_node_and_emit(self, node_id, event, now_ms)
    }

    fn abort_failed_node(&mut self, node_id: NodeId, now_ms: i64) -> StoreResult<NodeRecord> {
        Store::abort_failed_node(self, node_id, now_ms)
    }

    fn skip_node(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        Store::transition_node_and_emit(self, node_id, NodeState::Skipped, event, now_ms)
    }

    fn cancel_node(
        &mut self,
        node_id: NodeId,
        event: &EventInput,
        now_ms: i64,
    ) -> StoreResult<(NodeRecord, EventEnvelope)> {
        Store::transition_node_and_emit(self, node_id, NodeState::Cancelled, event, now_ms)
    }

    fn reclaim_expired_leases(&mut self, now_ms: i64) -> StoreResult<Vec<NodeRecord>> {
        Store::reclaim_expired_leases(self, now_ms)
    }
}

/// What the caller should do next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Start(NodeId),
    /// In-flight nodes exist but nothing new can start yet.
    Wait,
    /// The run reached a terminal state; no further actions.
    Halt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Pending,
    Ready,
    Offered,
    Leased,
    Running,
    NeedsRecovery,
    Succeeded,
    FailedFinal,
    Cancelled,
    Skipped,
}

/// Deterministic DAG state machine. No threads, no network: the caller drives
/// it with [`Scheduler::next_actions`], [`Scheduler::on_started`] and
/// [`Scheduler::on_finished`]. The threaded executor is H05b.
pub struct Scheduler<S: StoreLike> {
    topology: Topology,
    max_parallel: usize,
    cancel: CancellationToken,
    store: S,
    phases: HashMap<NodeId, Phase>,
    attempts: HashMap<NodeId, u32>,
    guards: HashMap<NodeId, LeaseGuard>,
    halted: bool,
    clock: Box<dyn Clock>,
}

impl<S: StoreLike> Scheduler<S> {
    /// Build a scheduler and mark all zero-dependency nodes ready.
    ///
    /// The nodes must already exist in `store` (see [`install_plan`]).
    pub fn new(
        topology: Topology,
        max_parallel: usize,
        cancel: CancellationToken,
        store: S,
    ) -> Result<Self, RuntimeError> {
        if max_parallel == 0 {
            return Err(RuntimeError::ZeroParallelism);
        }

        let mut phases = HashMap::with_capacity(topology.order().len());
        let mut attempts = HashMap::with_capacity(topology.order().len());
        for node_id in topology.order() {
            phases.insert(*node_id, Phase::Pending);
            attempts.insert(*node_id, 1);
        }

        let mut scheduler = Self {
            topology,
            max_parallel,
            cancel,
            store,
            phases,
            attempts,
            guards: HashMap::new(),
            halted: false,
            clock: Box::new(SystemClock),
        };

        let initial: Vec<NodeId> = scheduler
            .topology
            .order()
            .iter()
            .copied()
            .filter(|node_id| scheduler.topology.deps_of(*node_id).is_empty())
            .collect();
        for node_id in initial {
            scheduler.mark_ready(node_id)?;
        }
        Ok(scheduler)
    }

    /// Replace the scheduler's time source. Useful for tests that advance a
    /// controllable clock to exercise lease expiry without sleeping.
    pub fn with_clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Reclaim expired leases: a `leased` node whose lease lapsed returns to
    /// `ready`, while a `running` node whose lease lapsed becomes
    /// `needs_recovery` (side effects uncertain, manual decision required).
    pub fn reclaim_expired(&mut self) -> Result<Vec<NodeRecord>, RuntimeError> {
        let now = self.now_ms();
        let reclaimed = self.store.reclaim_expired_leases(now)?;
        for node in &reclaimed {
            match node.state {
                NodeState::Ready => {
                    self.guards.remove(&node.node_id);
                    self.phases.insert(node.node_id, Phase::Ready);
                }
                NodeState::NeedsRecovery => {
                    self.guards.remove(&node.node_id);
                    self.phases.insert(node.node_id, Phase::NeedsRecovery);
                }
                _ => {}
            }
        }
        Ok(reclaimed)
    }

    /// Compute the next batch of actions, reserving offered nodes so the same
    /// node is never started twice.
    pub fn next_actions(&mut self) -> Vec<Action> {
        if self.halted || self.all_terminal() {
            return vec![Action::Halt];
        }

        let in_flight = self.in_flight_count();
        let ready: Vec<NodeId> = self
            .topology
            .order()
            .iter()
            .copied()
            .filter(|node_id| self.phases[node_id] == Phase::Ready)
            .collect();

        if ready.is_empty() {
            return if in_flight > 0 {
                vec![Action::Wait]
            } else {
                vec![Action::Halt]
            };
        }

        let slots = self.max_parallel.saturating_sub(in_flight);
        if slots == 0 {
            return vec![Action::Wait];
        }

        ready
            .into_iter()
            .take(slots)
            .map(|node_id| {
                self.phases.insert(node_id, Phase::Offered);
                Action::Start(node_id)
            })
            .collect()
    }

    /// Claim an offered/ready node: move it `ready -> leased`, stamp the next
    /// `attempt_no` and return the [`LeaseGuard`] the executor must use.
    ///
    /// This is the scheduler's only guard acquisition entry point. The
    /// `leased -> running` transition and the `node.started` event belong to
    /// the executor ([`Self::start_running`] exists for the deterministic
    /// scheduler and recovery tests).
    pub fn on_started(&mut self, node_id: NodeId) -> Result<LeaseGuard, RuntimeError> {
        if self.halted {
            return Err(RuntimeError::NodeNotStartable(node_id));
        }
        match self.phases.get(&node_id).copied() {
            Some(Phase::Ready | Phase::Offered) => {}
            _ => return Err(RuntimeError::NodeNotStartable(node_id)),
        }

        let token = LeaseToken::generate();
        let next_attempt = self.attempts[&node_id];
        let leased = self.store.claim_node(
            node_id,
            &token,
            LEASE_OWNER,
            self.now_ms() + LEASE_TTL_MS,
            AttemptNo::new(next_attempt).expect("attempt starts at 1"),
            self.now_ms(),
        )?;
        let guard = LeaseGuard {
            attempt_no: leased.attempt_no,
            lease_token: token,
        };
        self.attempts.insert(node_id, guard.attempt_no.get());
        self.guards.insert(node_id, guard.clone());
        self.phases.insert(node_id, Phase::Leased);
        Ok(guard)
    }

    /// Move a claimed (`leased`) node to `running` and emit `node.started`.
    ///
    /// The threaded executor performs this transition inside the executor
    /// itself (`run_node_with_lease`); this method is for the deterministic
    /// scheduler and for callers that need an explicit running state (e.g.
    /// lease-recovery tests) without settling the node.
    pub fn start_running(&mut self, node_id: NodeId) -> Result<(), RuntimeError> {
        if self.phases.get(&node_id).copied() != Some(Phase::Leased) {
            return Err(RuntimeError::NodeNotStartable(node_id));
        }
        let guard = self
            .guards
            .get(&node_id)
            .cloned()
            .ok_or(RuntimeError::NodeNotStartable(node_id))?;
        let started = self.node_event(
            node_id,
            guard.attempt_no.get(),
            EventKind::NodeStarted,
            json!({}),
        );
        self.store
            .start_node(node_id, &guard, &started, self.now_ms())?;
        self.phases.insert(node_id, Phase::Running);
        Ok(())
    }

    /// Settle a claimed or running node and propagate readiness.
    ///
    /// When the executor settled the node itself (the threaded path), the store
    /// already holds a terminal state and this method only updates the
    /// scheduler's DAG bookkeeping. In the deterministic path the scheduler
    /// owns the settle: it starts a still-`leased` node first, then persists
    /// the terminal transition and its event.
    pub fn on_finished(
        &mut self,
        node_id: NodeId,
        outcome: ExecOutcome,
    ) -> Result<(), RuntimeError> {
        match self.phases.get(&node_id).copied() {
            Some(Phase::Leased | Phase::Running) => {}
            _ => return Err(RuntimeError::NodeNotInFlight(node_id)),
        }
        let guard = self
            .guards
            .get(&node_id)
            .cloned()
            .ok_or(RuntimeError::NodeNotInFlight(node_id))?;
        let attempt = self.attempts[&node_id];
        let persisted = self
            .store
            .get_node(node_id)?
            .ok_or_else(|| RuntimeError::Store(StoreError::NotFound(format!("node {node_id}"))))?;

        if persisted.state.is_terminal() {
            return self.propagate_terminal(node_id, outcome);
        }

        if persisted.state == NodeState::Leased {
            self.start_running(node_id)?;
        }

        match outcome {
            ExecOutcome::Succeeded { summary } => {
                let event = self.node_event(
                    node_id,
                    attempt,
                    EventKind::NodeSucceeded,
                    json!({ "summary": summary }),
                );
                self.store.finish_node(
                    node_id,
                    &guard,
                    NodeState::Succeeded,
                    &event,
                    self.now_ms(),
                )?;
                self.phases.insert(node_id, Phase::Succeeded);
                self.guards.remove(&node_id);
                self.unblock_dependents(node_id)?;
            }
            ExecOutcome::Failed { reason, retryable } => {
                let event = self.node_event(
                    node_id,
                    attempt,
                    EventKind::NodeFailed,
                    json!({ "reason": reason }),
                );
                self.store.finish_node(
                    node_id,
                    &guard,
                    NodeState::Failed,
                    &event,
                    self.now_ms(),
                )?;
                self.guards.remove(&node_id);
                self.handle_failure(node_id, attempt, retryable)?;
            }
            ExecOutcome::Cancelled => {
                let event = self.node_event(node_id, attempt, EventKind::NodeCancelled, json!({}));
                self.store.finish_node(
                    node_id,
                    &guard,
                    NodeState::Cancelled,
                    &event,
                    self.now_ms(),
                )?;
                self.phases.insert(node_id, Phase::Cancelled);
                self.guards.remove(&node_id);
                self.skip_downstream(node_id)?;
            }
        }
        Ok(())
    }

    /// Propagate the outcome of a node that was already settled by an external
    /// executor (the threaded path). The store is the authority for terminal
    /// state; this only updates DAG phases and downstream readiness.
    fn propagate_terminal(
        &mut self,
        node_id: NodeId,
        outcome: ExecOutcome,
    ) -> Result<(), RuntimeError> {
        let attempt = self.attempts[&node_id];
        self.guards.remove(&node_id);
        match outcome {
            ExecOutcome::Succeeded { .. } => {
                self.phases.insert(node_id, Phase::Succeeded);
                self.unblock_dependents(node_id)?;
            }
            ExecOutcome::Failed { retryable, .. } => {
                self.handle_failure(node_id, attempt, retryable)?;
            }
            ExecOutcome::Cancelled => {
                self.phases.insert(node_id, Phase::Cancelled);
                self.skip_downstream(node_id)?;
            }
        }
        Ok(())
    }

    /// Retry or finalize a node that is already in `failed` state, advancing
    /// `attempt_no` only when the retry budget allows it.
    fn handle_failure(
        &mut self,
        node_id: NodeId,
        attempt: u32,
        retryable: bool,
    ) -> Result<(), RuntimeError> {
        let budget = self
            .topology
            .node(node_id)
            .map(|node| node.retry_budget)
            .unwrap_or(0);
        if retryable
            && can_retry(
                AttemptNo::new(attempt).expect("attempt starts at 1"),
                budget,
            )
        {
            let next_attempt = attempt + 1;
            self.attempts.insert(node_id, next_attempt);
            let ready = self.node_event(node_id, next_attempt, EventKind::NodeReady, json!({}));
            self.store
                .retry_failed_node_and_emit(node_id, &ready, self.now_ms())?;
            self.phases.insert(node_id, Phase::Ready);
        } else {
            self.store.abort_failed_node(node_id, self.now_ms())?;
            self.phases.insert(node_id, Phase::FailedFinal);
            self.skip_downstream(node_id)?;
        }
        Ok(())
    }

    /// Cancel the whole run: running and ready nodes become `cancelled`, the
    /// shared token fires and no new node is started afterwards.
    pub fn cancel(&mut self) -> Result<(), RuntimeError> {
        if self.halted {
            return Ok(());
        }
        self.halted = true;
        self.cancel.cancel();

        let snapshot: Vec<(NodeId, Phase)> = self
            .phases
            .iter()
            .map(|(id, phase)| (*id, *phase))
            .collect();
        for (node_id, phase) in snapshot {
            match phase {
                Phase::Running | Phase::Leased => {
                    if let Some(guard) = self.guards.remove(&node_id) {
                        let attempt = self.attempts[&node_id];
                        let event =
                            self.node_event(node_id, attempt, EventKind::NodeCancelled, json!({}));
                        self.store.finish_node(
                            node_id,
                            &guard,
                            NodeState::Cancelled,
                            &event,
                            self.now_ms(),
                        )?;
                        self.phases.insert(node_id, Phase::Cancelled);
                    }
                }
                Phase::Ready | Phase::Offered | Phase::Pending | Phase::NeedsRecovery => {
                    let attempt = self.attempts[&node_id];
                    let event =
                        self.node_event(node_id, attempt, EventKind::NodeCancelled, json!({}));
                    self.store.cancel_node(node_id, &event, self.now_ms())?;
                    self.phases.insert(node_id, Phase::Cancelled);
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn in_flight_count(&self) -> usize {
        self.phases
            .values()
            .filter(|phase| matches!(phase, Phase::Offered | Phase::Leased | Phase::Running))
            .count()
    }

    pub fn node_state(&self, node_id: NodeId) -> NodeState {
        match self.phases.get(&node_id).copied().unwrap_or(Phase::Pending) {
            Phase::Pending => NodeState::Pending,
            Phase::Ready | Phase::Offered => NodeState::Ready,
            Phase::Leased => NodeState::Leased,
            Phase::Running => NodeState::Running,
            Phase::NeedsRecovery => NodeState::NeedsRecovery,
            Phase::Succeeded => NodeState::Succeeded,
            Phase::FailedFinal => NodeState::FailedFinal,
            Phase::Cancelled => NodeState::Cancelled,
            Phase::Skipped => NodeState::Skipped,
        }
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    fn node_event(
        &self,
        node_id: NodeId,
        attempt: u32,
        kind: EventKind,
        payload: Value,
    ) -> EventInput {
        EventInput {
            event_id: EventId::new(),
            kind,
            mission_id: self.topology.mission_id,
            run_id: Some(self.topology.run_id),
            node_id: Some(node_id),
            session_id: None,
            attempt_no: Some(AttemptNo::new(attempt).expect("attempt starts at 1")),
            trace_id: None,
            payload,
        }
    }

    fn mark_ready(&mut self, node_id: NodeId) -> Result<(), RuntimeError> {
        let attempt = self.attempts[&node_id];
        let event = self.node_event(node_id, attempt, EventKind::NodeReady, json!({}));
        self.store.mark_ready(node_id, &event, self.now_ms())?;
        self.phases.insert(node_id, Phase::Ready);
        Ok(())
    }

    fn unblock_dependents(&mut self, node_id: NodeId) -> Result<(), RuntimeError> {
        let dependents = self.topology.dependents_of(node_id).to_vec();
        for dependent in dependents {
            let deps_satisfied = self
                .topology
                .deps_of(dependent)
                .iter()
                .all(|dep| self.phases[dep] == Phase::Succeeded);
            if self.phases[&dependent] == Phase::Pending && deps_satisfied {
                self.mark_ready(dependent)?;
            }
        }
        Ok(())
    }

    fn skip_downstream(&mut self, node_id: NodeId) -> Result<(), RuntimeError> {
        let dependents = self.topology.dependents_of(node_id).to_vec();
        for dependent in dependents {
            self.skip_node(dependent)?;
        }
        Ok(())
    }

    fn skip_node(&mut self, node_id: NodeId) -> Result<(), RuntimeError> {
        match self.phases.get(&node_id).copied() {
            Some(Phase::Pending) => {
                let attempt = self.attempts[&node_id];
                let event = self.node_event(node_id, attempt, EventKind::NodeSkipped, json!({}));
                self.store.skip_node(node_id, &event, self.now_ms())?;
                self.phases.insert(node_id, Phase::Skipped);
                let dependents = self.topology.dependents_of(node_id).to_vec();
                for dependent in dependents {
                    self.skip_node(dependent)?;
                }
                Ok(())
            }
            Some(Phase::Succeeded | Phase::FailedFinal | Phase::Cancelled | Phase::Skipped) => {
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn all_terminal(&self) -> bool {
        self.phases.values().all(|phase| {
            matches!(
                phase,
                Phase::Succeeded | Phase::FailedFinal | Phase::Cancelled | Phase::Skipped
            )
        })
    }
}

/// Persist a plan: emit `plan.created` and create every node in `pending`.
/// Call before [`Scheduler::new`].
pub fn install_plan(
    store: &mut impl StoreLike,
    plan: &Plan,
    now_ms: i64,
) -> Result<(), RuntimeError> {
    let nodes = plan
        .nodes
        .iter()
        .map(|node| {
            json!({
                "node_id": node.node_id.to_string(),
                "deps": node.deps.iter().map(|dep| dep.to_string()).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let event = EventInput {
        event_id: EventId::new(),
        kind: EventKind::PlanCreated,
        mission_id: plan.mission_id,
        run_id: Some(plan.run_id),
        node_id: None,
        session_id: None,
        attempt_no: None,
        trace_id: None,
        payload: json!({ "plan_id": plan.plan_id, "nodes": nodes }),
    };
    store.append_event(&event)?;
    for node in &plan.nodes {
        store.create_node(
            node.node_id,
            plan.mission_id,
            plan.run_id,
            node.retry_budget,
            now_ms,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::plan::{NodeSpec, PlanNode};
    use hive_store::{MissionRecord, RunRecord, Store};

    fn node(node_id: NodeId, deps: &[NodeId], retry_budget: u32) -> PlanNode {
        PlanNode {
            node_id,
            deps: deps.to_vec(),
            spec: NodeSpec::new("worker", "auto/auto"),
            retry_budget,
        }
    }

    fn plan(mission_id: MissionId, run_id: RunId, nodes: Vec<PlanNode>) -> Plan {
        Plan {
            plan_id: "plan-1".to_owned(),
            mission_id,
            run_id,
            nodes,
        }
    }

    fn make_store() -> (Store, MissionRecord, RunRecord) {
        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("mission", "goal", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        (store, mission, run)
    }

    #[test]
    fn diamond_readies_b_and_c_together_then_d() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let d = NodeId::new();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![
                node(a, &[], 3),
                node(b, &[a], 3),
                node(c, &[a], 3),
                node(d, &[b, c], 3),
            ],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 2, CancellationToken::new(), store).expect("scheduler");

        assert_eq!(sched.next_actions(), vec![Action::Start(a)]);
        sched.on_started(a).expect("start a");
        assert_eq!(sched.next_actions(), vec![Action::Wait]);
        sched
            .on_finished(
                a,
                ExecOutcome::Succeeded {
                    summary: "a".into(),
                },
            )
            .expect("finish a");

        let actions = sched.next_actions();
        let mut starts: Vec<NodeId> = actions
            .iter()
            .filter_map(|action| match action {
                Action::Start(id) => Some(*id),
                _ => None,
            })
            .collect();
        let mut expected = vec![b, c];
        starts.sort();
        expected.sort();
        assert_eq!(starts, expected, "B and C must be ready simultaneously");
        assert_eq!(sched.node_state(d), NodeState::Pending);

        sched.on_started(b).expect("start b");
        sched.on_started(c).expect("start c");
        sched
            .on_finished(
                b,
                ExecOutcome::Succeeded {
                    summary: "b".into(),
                },
            )
            .expect("finish b");
        assert_eq!(
            sched.next_actions(),
            vec![Action::Wait],
            "D must wait for C"
        );
        sched
            .on_finished(
                c,
                ExecOutcome::Succeeded {
                    summary: "c".into(),
                },
            )
            .expect("finish c");
        assert_eq!(sched.next_actions(), vec![Action::Start(d)]);
        sched.on_started(d).expect("start d");
        sched
            .on_finished(
                d,
                ExecOutcome::Succeeded {
                    summary: "d".into(),
                },
            )
            .expect("finish d");
        assert_eq!(sched.next_actions(), vec![Action::Halt]);

        for id in [a, b, c, d] {
            assert_eq!(
                sched
                    .store()
                    .get_node(id)
                    .expect("get")
                    .expect("present")
                    .state,
                NodeState::Succeeded
            );
        }
    }

    #[test]
    fn max_parallel_is_never_exceeded() {
        let (mut store, mission, run) = make_store();
        let root = NodeId::new();
        let leaves: Vec<NodeId> = (0..5).map(|_| NodeId::new()).collect();
        let mut nodes = vec![node(root, &[], 0)];
        for leaf in &leaves {
            nodes.push(node(*leaf, &[root], 0));
        }
        let plan = plan(mission.mission_id, run.run_id, nodes);
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 2, CancellationToken::new(), store).expect("scheduler");

        assert_eq!(sched.next_actions(), vec![Action::Start(root)]);
        sched.on_started(root).expect("start root");
        assert!(sched.in_flight_count() <= 2);
        sched
            .on_finished(
                root,
                ExecOutcome::Succeeded {
                    summary: "r".into(),
                },
            )
            .expect("finish root");

        let batch = sched.next_actions();
        assert_eq!(
            batch
                .iter()
                .filter(|a| matches!(a, Action::Start(_)))
                .count(),
            2
        );
        for action in &batch {
            if let Action::Start(id) = action {
                sched.on_started(*id).expect("start leaf");
            }
        }
        assert_eq!(sched.in_flight_count(), 2);

        // Finish leaves one at a time; each replacement batch respects the cap.
        let mut finished = 0;
        loop {
            let actions = sched.next_actions();
            let starts: Vec<NodeId> = actions
                .iter()
                .filter_map(|action| match action {
                    Action::Start(id) => Some(*id),
                    _ => None,
                })
                .collect();
            assert!(starts.len() <= 1, "only one slot can open at a time");
            for id in &starts {
                sched.on_started(*id).expect("start");
            }
            assert!(sched.in_flight_count() <= 2, "parallel cap violated");

            // finish the oldest in-flight leaf
            let running: Vec<NodeId> = leaves
                .iter()
                .copied()
                .filter(|id| {
                    matches!(
                        sched.node_state(*id),
                        NodeState::Leased | NodeState::Running
                    )
                })
                .collect();
            if running.is_empty() {
                break;
            }
            sched
                .on_finished(
                    running[0],
                    ExecOutcome::Succeeded {
                        summary: "leaf".into(),
                    },
                )
                .expect("finish leaf");
            finished += 1;
            if finished == leaves.len() {
                break;
            }
        }
        assert_eq!(sched.next_actions(), vec![Action::Halt]);
    }

    #[test]
    fn sibling_failure_skips_only_its_downstream() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let d = NodeId::new();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![
                node(a, &[], 0),
                node(b, &[a], 0),
                node(c, &[a], 0),
                node(d, &[b, c], 0),
            ],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 2, CancellationToken::new(), store).expect("scheduler");

        sched.on_started(a).expect("start a");
        sched
            .on_finished(
                a,
                ExecOutcome::Succeeded {
                    summary: "a".into(),
                },
            )
            .expect("finish a");
        sched.on_started(b).expect("start b");
        sched.on_started(c).expect("start c");
        sched
            .on_finished(
                b,
                ExecOutcome::Failed {
                    reason: "boom".into(),
                    retryable: false,
                },
            )
            .expect("fail b");

        assert_eq!(sched.node_state(b), NodeState::FailedFinal);
        assert_eq!(sched.node_state(d), NodeState::Skipped);
        // C is a sibling and must stay in flight (claimed but not settled).
        assert_eq!(sched.node_state(c), NodeState::Leased);
        sched
            .on_finished(
                c,
                ExecOutcome::Succeeded {
                    summary: "c".into(),
                },
            )
            .expect("finish c");
        assert_eq!(sched.next_actions(), vec![Action::Halt]);
        assert_eq!(
            sched
                .store()
                .get_node(c)
                .expect("c")
                .expect("present")
                .state,
            NodeState::Succeeded
        );
    }

    #[test]
    fn retry_budget_exhausts_on_fourth_failure() {
        let (mut store, mission, run) = make_store();
        let n = NodeId::new();
        let plan = plan(mission.mission_id, run.run_id, vec![node(n, &[], 3)]);
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 1, CancellationToken::new(), store).expect("scheduler");

        for attempt in 1..=4 {
            assert_eq!(
                sched.next_actions(),
                vec![Action::Start(n)],
                "attempt {attempt}"
            );
            sched.on_started(n).expect("start");
            let node = sched.store().get_node(n).expect("get").expect("present");
            assert_eq!(node.attempt_no.get(), attempt);
            sched
                .on_finished(
                    n,
                    ExecOutcome::Failed {
                        reason: format!("fail-{attempt}"),
                        retryable: true,
                    },
                )
                .expect("fail");
        }

        let final_node = sched.store().get_node(n).expect("get").expect("present");
        assert_eq!(final_node.state, NodeState::FailedFinal);
        assert_eq!(final_node.attempt_no.get(), 4);
        let failed_events = sched
            .store()
            .read_all_events()
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == EventKind::NodeFailed)
            .count();
        assert_eq!(failed_events, 4);
        assert_eq!(sched.next_actions(), vec![Action::Halt]);
    }

    #[test]
    fn cancel_transitions_running_and_ready_and_stops_starting() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![node(a, &[], 0), node(b, &[a], 0), node(c, &[a], 0)],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 2, CancellationToken::new(), store).expect("scheduler");

        sched.on_started(a).expect("start a");
        sched
            .on_finished(
                a,
                ExecOutcome::Succeeded {
                    summary: "a".into(),
                },
            )
            .expect("finish a");
        let actions = sched.next_actions();
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, Action::Start(_)))
                .count(),
            2
        );
        // start only B; leave C offered (still ready in the store).
        let b_action = actions
            .iter()
            .find_map(|a| match a {
                Action::Start(id) if *id == b => Some(*id),
                _ => None,
            })
            .expect("b offered");
        sched.on_started(b_action).expect("start b");

        sched.cancel().expect("cancel");
        assert_eq!(
            sched
                .store()
                .get_node(b)
                .expect("b")
                .expect("present")
                .state,
            NodeState::Cancelled
        );
        assert_eq!(
            sched
                .store()
                .get_node(c)
                .expect("c")
                .expect("present")
                .state,
            NodeState::Cancelled
        );
        assert_eq!(sched.next_actions(), vec![Action::Halt]);
        assert_eq!(sched.next_actions(), vec![Action::Halt]);
    }

    #[test]
    fn same_node_is_never_started_twice() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let plan = plan(mission.mission_id, run.run_id, vec![node(a, &[], 0)]);
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 1, CancellationToken::new(), store).expect("scheduler");

        assert_eq!(sched.next_actions(), vec![Action::Start(a)]);
        assert_eq!(
            sched.next_actions(),
            vec![Action::Wait],
            "must not re-offer A"
        );
        sched.on_started(a).expect("start");
        assert!(matches!(
            sched.on_started(a),
            Err(RuntimeError::NodeNotStartable(_))
        ));
    }

    #[test]
    fn on_started_returns_guard_matching_the_claim_without_starting() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let plan = plan(mission.mission_id, run.run_id, vec![node(a, &[], 0)]);
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 1, CancellationToken::new(), store).expect("scheduler");

        let guard = sched.on_started(a).expect("claim");
        let persisted = sched.store().get_node(a).expect("get").expect("present");
        assert_eq!(persisted.state, NodeState::Leased);
        assert_eq!(persisted.lease_token.as_ref(), Some(&guard.lease_token));
        assert_eq!(persisted.attempt_no, guard.attempt_no);
        assert_eq!(persisted.attempt_no.get(), 1);

        // The scheduler only claims; node.started belongs to the executor.
        let events = sched.store().read_all_events().expect("events");
        assert!(!events
            .iter()
            .any(|event| event.kind == EventKind::NodeStarted));

        // Running to completion must not claim a second time: the attempt stays
        // at 1 and exactly one node.started is emitted.
        sched.start_running(a).expect("running");
        sched
            .on_finished(
                a,
                ExecOutcome::Succeeded {
                    summary: "a".into(),
                },
            )
            .expect("finish");
        let finished = sched.store().get_node(a).expect("get").expect("present");
        assert_eq!(finished.state, NodeState::Succeeded);
        assert_eq!(finished.attempt_no.get(), 1);
        let started = sched
            .store()
            .read_all_events()
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == EventKind::NodeStarted)
            .count();
        assert_eq!(started, 1);
    }

    #[test]
    fn on_finished_propagates_when_executor_settled_the_node() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let b = NodeId::new();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![node(a, &[], 0), node(b, &[a], 0)],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 1, CancellationToken::new(), store).expect("scheduler");

        let guard = sched.on_started(a).expect("claim");
        // Simulate the threaded executor settling the node out-of-band:
        // leased -> running -> succeeded, without the scheduler's phase moving.
        let started = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeStarted,
            mission_id: mission.mission_id,
            run_id: Some(run.run_id),
            node_id: Some(a),
            session_id: None,
            attempt_no: Some(guard.attempt_no),
            trace_id: None,
            payload: json!({}),
        };
        sched
            .store_mut()
            .transition_node_cas(a, &guard, NodeState::Running, &started, 0)
            .expect("executor start");
        let succeeded = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeSucceeded,
            mission_id: mission.mission_id,
            run_id: Some(run.run_id),
            node_id: Some(a),
            session_id: None,
            attempt_no: Some(guard.attempt_no),
            trace_id: None,
            payload: json!({}),
        };
        sched
            .store_mut()
            .transition_node_cas(a, &guard, NodeState::Succeeded, &succeeded, 1)
            .expect("executor settle");

        sched
            .on_finished(
                a,
                ExecOutcome::Succeeded {
                    summary: "a".into(),
                },
            )
            .expect("propagate");

        assert_eq!(sched.node_state(a), NodeState::Succeeded);
        assert_eq!(sched.next_actions(), vec![Action::Start(b)]);
    }

    #[test]
    fn events_sequence_matches_final_node_states() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let d = NodeId::new();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![
                node(a, &[], 0),
                node(b, &[a], 0),
                node(c, &[a], 0),
                node(d, &[b, c], 0),
            ],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 2, CancellationToken::new(), store).expect("scheduler");

        sched.on_started(a).expect("start a");
        sched
            .on_finished(
                a,
                ExecOutcome::Succeeded {
                    summary: "a".into(),
                },
            )
            .expect("finish a");
        sched.on_started(b).expect("start b");
        sched.on_started(c).expect("start c");
        sched
            .on_finished(
                b,
                ExecOutcome::Succeeded {
                    summary: "b".into(),
                },
            )
            .expect("finish b");
        sched
            .on_finished(
                c,
                ExecOutcome::Succeeded {
                    summary: "c".into(),
                },
            )
            .expect("finish c");
        sched.on_started(d).expect("start d");
        sched
            .on_finished(
                d,
                ExecOutcome::Succeeded {
                    summary: "d".into(),
                },
            )
            .expect("finish d");

        let kinds: Vec<EventKind> = sched
            .store()
            .read_all_events()
            .expect("events")
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::PlanCreated,
                EventKind::NodeReady,
                EventKind::NodeStarted,
                EventKind::NodeSucceeded,
                EventKind::NodeReady,
                EventKind::NodeReady,
                EventKind::NodeStarted,
                EventKind::NodeSucceeded,
                EventKind::NodeStarted,
                EventKind::NodeSucceeded,
                EventKind::NodeReady,
                EventKind::NodeStarted,
                EventKind::NodeSucceeded,
            ]
        );
        for id in [a, b, c, d] {
            assert_eq!(
                sched
                    .store()
                    .get_node(id)
                    .expect("get")
                    .expect("present")
                    .state,
                NodeState::Succeeded
            );
        }
    }

    #[test]
    fn zero_parallelism_is_rejected() {
        let (store, mission, run) = make_store();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![node(NodeId::new(), &[], 0)],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        assert!(matches!(
            Scheduler::new(topology, 0, CancellationToken::new(), store),
            Err(RuntimeError::ZeroParallelism)
        ));
    }

    #[test]
    fn reclaim_expired_uses_injected_clock_running_becomes_needs_recovery() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let plan = plan(mission.mission_id, run.run_id, vec![node(a, &[], 0)]);
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let clock = TestClock::new(0);
        let mut sched = Scheduler::new(topology, 1, CancellationToken::new(), store)
            .expect("scheduler")
            .with_clock(clock.clone());

        sched.on_started(a).expect("start");
        sched.start_running(a).expect("running");
        assert_eq!(sched.node_state(a), NodeState::Running);

        // The node's lease was granted for now_ms + 60_000; advancing the
        // injected clock past it must move the running node to needs_recovery.
        clock.advance(61_000);
        let reclaimed = sched.reclaim_expired().expect("reclaim");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(sched.node_state(a), NodeState::NeedsRecovery);
        let persisted = sched.store().get_node(a).expect("get").expect("present");
        assert_eq!(persisted.state, NodeState::NeedsRecovery);
        assert!(persisted.lease_token.is_none());
    }

    #[test]
    fn cancel_cascades_to_pending_nodes() {
        let (mut store, mission, run) = make_store();
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let plan = plan(
            mission.mission_id,
            run.run_id,
            vec![node(a, &[], 0), node(b, &[a], 0), node(c, &[a], 0)],
        );
        let topology = crate::plan::validate_plan(&plan).expect("valid");
        install_plan(&mut store, &plan, 0).expect("install");
        let mut sched =
            Scheduler::new(topology, 2, CancellationToken::new(), store).expect("scheduler");

        // Only A is ready; B and C are still pending. Cancel before starting.
        sched.cancel().expect("cancel");

        for id in [a, b, c] {
            assert_eq!(
                sched
                    .store()
                    .get_node(id)
                    .expect("get")
                    .expect("present")
                    .state,
                NodeState::Cancelled
            );
        }
        let cancelled_events = sched
            .store()
            .read_all_events()
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == EventKind::NodeCancelled)
            .count();
        assert_eq!(cancelled_events, 3);
        assert_eq!(sched.next_actions(), vec![Action::Halt]);
    }
}
