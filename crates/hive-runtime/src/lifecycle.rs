//! Startup and periodic lifecycle orchestration for a running server.
//!
//! These helpers wire the store-level recovery, budget and lease primitives
//! into the fixed order the server must run them: startup recovery runs before
//! any work is accepted, and each background tick advances due routines and
//! reclaims expired leases. A single failed tick step is counted, never fatal
//! to the loop.

use hive_store::Store;

use crate::error::RuntimeError;
use crate::recovery::{run_startup_recovery, RecoveryReport};
use crate::routine::{tick_routines, RoutineTickReport};

/// Run the fixed startup sequence before the server accepts new work:
/// recovery scan first, then budget counter rebuild. Any failure aborts
/// startup and must cause the caller to refuse to serve.
pub fn run_startup(store: &mut Store, now_ms: i64) -> Result<RecoveryReport, RuntimeError> {
    let report = run_startup_recovery(store, now_ms)?;
    store.rebuild_budget_counters(now_ms)?;
    Ok(report)
}

/// Counters for one background tick pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TickReport {
    pub routines: RoutineTickReport,
    pub leases_reclaimed: usize,
    pub errors: usize,
}

/// Run one background tick: fire due routines, then reclaim expired leases.
/// Per-step errors are counted in [`TickReport::errors`] and never abort the
/// loop.
pub fn tick_once(store: &mut Store, now_ms: i64) -> TickReport {
    let mut report = TickReport::default();
    match tick_routines(store, now_ms) {
        Ok(routines) => report.routines = routines,
        Err(_) => report.errors += 1,
    }
    match store.reclaim_expired_leases(now_ms) {
        Ok(nodes) => report.leases_reclaimed = nodes.len(),
        Err(_) => report.errors += 1,
    }
    report
}

#[cfg(test)]
mod tests {
    use hive_store::{BudgetDimension, BudgetLimits, NewTurnUsage, Store, UsageSource};
    use hive_types::{AttemptNo, EventKind, LeaseToken, NodeState};

    use super::*;

    fn setup() -> (
        Store,
        hive_types::MissionId,
        hive_types::RunId,
        hive_types::NodeId,
    ) {
        let mut store = Store::open_in_memory().expect("store");
        let mission = store.create_mission("m", "g", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node = store
            .add_node(mission.mission_id, run.run_id, 3, 0)
            .expect("node");
        (store, mission.mission_id, run.run_id, node.node_id)
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
    fn startup_helper_runs_recovery_then_rebuilds_budget() {
        let (mut store, mission, run, node) = setup();
        // A surviving lease: startup recovery revokes it back to ready.
        store
            .update_node_state(node, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node, &token, "exec", 10_000_000, 0)
            .expect("lease");
        // A budget row seeded with a stale consumed value that rebuild corrects.
        store
            .set_budget(
                "node",
                &node.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(1_000),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        let session = store
            .create_agent_session(&hive_store::NewAgentSession {
                mission_id: mission,
                run_id: Some(run),
                node_id: node,
                role: "worker",
                model: "auto/auto",
                system_prompt: "",
                now_ms: 0,
            })
            .expect("session");
        let turn = store
            .start_turn(
                session.session_id,
                node,
                AttemptNo::FIRST,
                1,
                "auto/auto",
                0,
            )
            .expect("turn");
        store
            .record_turn_usage(
                turn.turn_id,
                &NewTurnUsage {
                    prompt_tokens: 5,
                    completion_tokens: 2,
                    total_tokens: 7,
                    source: UsageSource::Authoritative,
                    request_id: Some("r1".to_owned()),
                },
                0,
            )
            .expect("usage");

        let report = run_startup(&mut store, 5_000).expect("startup");
        assert_eq!(report.leases_revoked, 1);
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::Ready);
        assert!(events(&store).contains(&EventKind::NodeReady));
        assert_eq!(
            store
                .get_budget("node", &node.to_string(), BudgetDimension::Tokens)
                .expect("get")
                .expect("present")
                .consumed,
            7
        );
    }

    #[test]
    fn server_startup_rejects_work_before_recovery() {
        let (mut store, _mission, _run, node) = setup();
        // A running node at the final attempt: startup recovery's safe-retry
        // arm fails on attempt exhaustion, so `run_startup` must abort (fail
        // closed) before rebuilding budgets and before any work is served.
        store
            .update_node_state(node, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .claim_node(
                node,
                &token,
                "exec",
                10_000_000,
                AttemptNo::new(u32::MAX).expect("max attempt"),
                0,
            )
            .expect("claim");
        store
            .update_node_state(node, NodeState::Running, 0)
            .expect("running");
        // A budget row seeded with a stale consumed value; rebuild would fix it
        // to 7 but must not run after recovery fails.
        store
            .set_budget(
                "node",
                &node.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(1_000),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");

        let result = run_startup(&mut store, 5_000);
        assert!(result.is_err(), "startup must fail closed");
        assert_eq!(
            store
                .get_budget("node", &node.to_string(), BudgetDimension::Tokens)
                .expect("get")
                .expect("present")
                .consumed,
            0,
            "rebuild must not run after a failed recovery"
        );
    }

    #[test]
    fn tick_loop_fires_due_routine_and_reclaims_expired_lease() {
        let (mut store, _mission, _run, node) = setup();
        // Due routine.
        store
            .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("routine");
        // Expired lease.
        store
            .update_node_state(node, NodeState::Ready, 0)
            .expect("ready");
        let token = LeaseToken::generate();
        store
            .lease_node(node, &token, "exec", 1_000, 0)
            .expect("lease");

        let report = tick_once(&mut store, 300_000);
        assert_eq!(report.routines.fired, 1);
        assert_eq!(report.leases_reclaimed, 1);
        assert_eq!(report.errors, 0);

        let kinds = events(&store);
        assert!(kinds.contains(&EventKind::RoutineTriggered));
        assert!(kinds.contains(&EventKind::NodeReady));
        let node_record = store.get_node(node).expect("get").expect("node");
        assert_eq!(node_record.state, NodeState::Ready);
        assert!(node_record.lease_token.is_none());
    }
}
