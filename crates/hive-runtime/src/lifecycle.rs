//! Startup and periodic lifecycle orchestration for a running server.
//!
//! These helpers wire the store-level recovery, budget and lease primitives
//! into the fixed order the server must run them: startup recovery runs before
//! any work is accepted, and each background tick advances due routines and
//! reclaims expired leases. A single failed tick step is counted, never fatal
//! to the loop.

use std::path::PathBuf;

use hive_store::Store;

use crate::error::RuntimeError;
use crate::recovery::{run_startup_recovery, RecoveryReport};
use crate::routine::{tick_routines, RoutineTickReport};

/// Run the fixed startup sequence before the server accepts new work:
/// recovery scan first, then budget counter rebuild. Any failure aborts
/// startup and must cause the caller to refuse to serve.
pub fn run_startup(store: &mut Store, now_ms: i64) -> Result<RecoveryReport, RuntimeError> {
    run_startup_with_retention(store, now_ms, None).map(|report| report.recovery)
}

/// Retention policy for automatic archiving of terminal missions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    /// Days after a terminal mission's last update before it is archived.
    /// Zero disables automatic archiving.
    pub days: i64,
    pub archive_dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionSummary {
    pub archived_missions: usize,
    pub events_archived: u64,
    pub audits_archived: u64,
    pub retained_since_seq: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StartupReport {
    pub recovery: RecoveryReport,
    pub retention: RetentionSummary,
    /// Retention failures are reported but never abort startup.
    pub retention_error: Option<String>,
}

/// Run startup recovery, rebuild budgets, then archive expired terminal
/// missions once. A retention failure is captured in the report and never
/// prevents the server from accepting work; the failed mission keeps its rows.
pub fn run_startup_with_retention(
    store: &mut Store,
    now_ms: i64,
    policy: Option<&RetentionPolicy>,
) -> Result<StartupReport, RuntimeError> {
    let recovery = run_startup_recovery(store, now_ms)?;
    store.rebuild_budget_counters(now_ms)?;
    let mut report = StartupReport {
        recovery,
        ..StartupReport::default()
    };
    if let Some(policy) = policy {
        match retention_once(store, now_ms, policy) {
            Ok(summary) => report.retention = summary,
            Err(error) => report.retention_error = Some(error.to_string()),
        }
    }
    Ok(report)
}

/// Archive every terminal mission older than the policy window. Failures are
/// returned so callers can log them; no row is deleted when persistence of the
/// new floor fails.
pub fn retention_once(
    store: &mut Store,
    now_ms: i64,
    policy: &RetentionPolicy,
) -> Result<RetentionSummary, RuntimeError> {
    let reports =
        store.archive_expired_missions_with_days(&policy.archive_dir, now_ms, policy.days)?;
    let retained_since_seq = match reports.last() {
        Some(report) => report.retained_since_seq,
        None => store.retained_since_seq()?,
    };
    Ok(RetentionSummary {
        archived_missions: reports.len(),
        events_archived: reports.iter().map(|report| report.events_archived).sum(),
        audits_archived: reports.iter().map(|report| report.audits_archived).sum(),
        retained_since_seq,
    })
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
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use hive_store::{BudgetDimension, BudgetLimits, NewTurnUsage, Store, UsageSource};
    use hive_types::{
        AttemptNo, EventId, EventInput, EventKind, LeaseToken, MissionId, MissionState, NodeState,
    };
    use serde_json::json;

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-runtime-{name}-{}-{nanos}-{counter}",
                std::process::id()
            ));
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

    fn event_input(mission_id: MissionId, kind: EventKind) -> EventInput {
        EventInput {
            event_id: EventId::new(),
            kind,
            mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({}),
        }
    }

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

    fn mission_event_count(store: &Store, mission_id: MissionId) -> usize {
        store
            .read_all_events()
            .expect("events")
            .iter()
            .filter(|envelope| envelope.mission_id == mission_id)
            .count()
    }

    #[test]
    fn startup_retention_archives_expired_terminal_and_spares_active() {
        let (mut store, expired, _run, _node) = setup();
        store
            .append_event(&event_input(expired, EventKind::MissionCancelled))
            .expect("expired event");
        store
            .transition_mission(expired, MissionState::Cancelled, 0)
            .expect("terminal");
        let active = store.create_mission("active", "g", 0).expect("active");
        store
            .append_event(&event_input(active.mission_id, EventKind::MissionCreated))
            .expect("active event");

        let dir = TempDir::new("startup-retention");
        let policy = RetentionPolicy {
            days: 30,
            archive_dir: dir.path().join("archive"),
        };
        let report =
            run_startup_with_retention(&mut store, 31 * 24 * 60 * 60 * 1000, Some(&policy))
                .expect("startup");
        assert!(report.retention_error.is_none());
        assert_eq!(report.retention.archived_missions, 1);
        assert_eq!(report.retention.events_archived, 1);
        assert_eq!(report.retention.retained_since_seq, 2);
        assert_eq!(mission_event_count(&store, expired), 0);
        assert_eq!(mission_event_count(&store, active.mission_id), 1);
        assert!(store.get_mission(expired).expect("mission").is_some());
    }

    #[test]
    fn retention_days_zero_disables_archiving() {
        let (mut store, mission, _run, _node) = setup();
        store
            .append_event(&event_input(mission, EventKind::MissionCancelled))
            .expect("event");
        store
            .transition_mission(mission, MissionState::Cancelled, 0)
            .expect("terminal");

        let dir = TempDir::new("retention-disabled");
        let policy = RetentionPolicy {
            days: 0,
            archive_dir: dir.path().join("archive"),
        };
        let report =
            run_startup_with_retention(&mut store, 31 * 24 * 60 * 60 * 1000, Some(&policy))
                .expect("startup");
        assert!(report.retention_error.is_none());
        assert_eq!(report.retention.archived_missions, 0);
        assert_eq!(mission_event_count(&store, mission), 1);
        assert_eq!(store.retained_since_seq().expect("floor"), 0);
    }

    #[test]
    fn retention_once_is_idempotent() {
        let (mut store, mission, _run, _node) = setup();
        store
            .append_event(&event_input(mission, EventKind::MissionCancelled))
            .expect("event");
        store
            .transition_mission(mission, MissionState::Cancelled, 0)
            .expect("terminal");

        let dir = TempDir::new("retention-idempotent");
        let policy = RetentionPolicy {
            days: 30,
            archive_dir: dir.path().join("archive"),
        };
        let now_ms = 31 * 24 * 60 * 60 * 1000;
        let first = retention_once(&mut store, now_ms, &policy).expect("first");
        assert_eq!(first.archived_missions, 1);
        let second = retention_once(&mut store, now_ms, &policy).expect("second");
        assert_eq!(second.archived_missions, 0);
        assert_eq!(second.retained_since_seq, first.retained_since_seq);
    }

    #[test]
    fn retention_failure_keeps_rows_and_does_not_block_startup() {
        let (mut store, mission, _run, _node) = setup();
        store
            .append_event(&event_input(mission, EventKind::MissionCancelled))
            .expect("event");
        store
            .transition_mission(mission, MissionState::Cancelled, 0)
            .expect("terminal");

        let dir = TempDir::new("retention-blocked");
        let blocked = dir.path().join("archive-blocked");
        std::fs::write(&blocked, b"not a directory").expect("block archive path");
        let policy = RetentionPolicy {
            days: 30,
            archive_dir: blocked,
        };
        let report =
            run_startup_with_retention(&mut store, 31 * 24 * 60 * 60 * 1000, Some(&policy))
                .expect("startup survives retention failure");
        assert!(report.retention_error.is_some());
        assert_eq!(report.retention.archived_missions, 0);
        assert_eq!(mission_event_count(&store, mission), 1);
        assert_eq!(store.retained_since_seq().expect("floor"), 0);
    }
}
