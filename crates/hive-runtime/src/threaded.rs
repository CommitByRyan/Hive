use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use hive_store::Store;
use hive_types::{CancellationToken, NodeId, NodeState};

use crate::clock::{Clock, SystemClock};
use crate::error::RuntimeError;
use crate::executor::{ExecOutcome, NodeExecutor};
use crate::plan::{Plan, PlanNode};
use crate::scheduler::{install_plan, Action, Scheduler};

/// Outcome counters for one threaded run, used for reporting and tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunSummary {
    pub executed: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub skipped: usize,
}

/// Runs a plan on real OS threads.
///
/// The main thread owns the scheduler and its `Store`: it is the only writer
/// of node state and `node.*` events. Each worker is a scoped thread with its
/// own [`Store`] connection to the same database (WAL + `busy_timeout` are set
/// by `hive-store`) and a private [`NodeExecutor`] built by `make_executor`.
/// Workers execute one node at a time and hand the outcome back to the main
/// thread, which settles it through the scheduler.
pub struct ThreadedExecutor;

impl ThreadedExecutor {
    /// Execute `plan` with up to `max_parallel` concurrent workers.
    ///
    /// `make_executor` is called once per node execution with a fresh worker
    /// `Store` connection and must return a `Send` `NodeExecutor`. Workers
    /// never share an executor instance or the main scheduler's store.
    pub fn run<E, F>(
        db_path: &Path,
        plan: &Plan,
        max_parallel: usize,
        cancel: &CancellationToken,
        mut make_executor: F,
    ) -> Result<RunSummary, RuntimeError>
    where
        E: NodeExecutor + Send,
        F: FnMut(Store) -> E,
    {
        let topology = crate::plan::validate_plan(plan)?;
        let mut store = Store::open(db_path)?;
        install_plan(&mut store, plan, SystemClock.now_ms())?;
        let mut scheduler = Scheduler::new(topology.clone(), max_parallel, cancel.clone(), store)?;

        let nodes: Arc<HashMap<NodeId, PlanNode>> = Arc::new(
            plan.nodes
                .iter()
                .map(|node| (node.node_id, node.clone()))
                .collect(),
        );

        let mut summary = RunSummary::default();

        let run = std::thread::scope(|scope| -> Result<(), RuntimeError> {
            loop {
                let mut starts = Vec::new();
                let mut halted = false;
                for action in scheduler.next_actions() {
                    match action {
                        Action::Start(node_id) => starts.push(node_id),
                        Action::Wait => {}
                        Action::Halt => halted = true,
                    }
                }
                if halted {
                    break;
                }

                let mut handles = Vec::with_capacity(starts.len());
                for node_id in starts {
                    scheduler.on_started(node_id)?;
                    let plan_node = nodes.get(&node_id).cloned();
                    let cancel = cancel.clone();
                    let worker_store = Store::open(db_path)?;
                    let mut executor = make_executor(worker_store);
                    handles.push(scope.spawn(move || {
                        let outcome = match plan_node {
                            Some(plan_node) => executor.execute(&plan_node, &cancel),
                            None => ExecOutcome::Failed {
                                reason: format!("node {node_id} is missing from the plan"),
                                retryable: false,
                            },
                        };
                        (node_id, outcome)
                    }));
                }

                for handle in handles {
                    let (node_id, outcome) = handle
                        .join()
                        .map_err(|_| RuntimeError::Executor("worker panicked".to_owned()))?;
                    match &outcome {
                        ExecOutcome::Succeeded { .. } => summary.succeeded += 1,
                        ExecOutcome::Failed { .. } => summary.failed += 1,
                        ExecOutcome::Cancelled => summary.cancelled += 1,
                    }
                    summary.executed += 1;
                    scheduler.on_finished(node_id, outcome)?;
                }
            }
            Ok(())
        });
        run?;

        for node_id in topology.order() {
            if scheduler.node_state(*node_id) == NodeState::Skipped {
                summary.skipped += 1;
            }
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::Duration;

    use hive_types::{MissionId, RunId};

    use super::*;
    use crate::plan::NodeSpec;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!("hive-threaded-{name}-{}", std::process::id()));
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

    fn node(id: NodeId, deps: &[NodeId], role: &str) -> PlanNode {
        PlanNode {
            node_id: id,
            deps: deps.to_vec(),
            spec: NodeSpec::new(role, "auto/auto"),
            retry_budget: 0,
        }
    }

    fn diamond(mission_id: MissionId, run_id: RunId) -> (Plan, [NodeId; 4]) {
        let a = NodeId::new();
        let b = NodeId::new();
        let c = NodeId::new();
        let d = NodeId::new();
        let plan = Plan {
            plan_id: "diamond".to_owned(),
            mission_id,
            run_id,
            nodes: vec![
                node(a, &[], "root"),
                node(b, &[a], "mid"),
                node(c, &[a], "mid"),
                node(d, &[b, c], "leaf"),
            ],
        };
        (plan, [a, b, c, d])
    }

    /// A pure in-memory executor that records peak concurrency and forces the
    /// two `mid` nodes to overlap via a barrier, so the test is deterministic.
    struct OverlapExecutor {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        barrier: Arc<Barrier>,
    }

    impl NodeExecutor for OverlapExecutor {
        fn execute(&mut self, node: &PlanNode, _cancel: &CancellationToken) -> ExecOutcome {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            if node.spec.role == "mid" {
                self.barrier.wait();
            }
            std::thread::sleep(Duration::from_millis(10));
            self.active.fetch_sub(1, Ordering::SeqCst);
            ExecOutcome::Succeeded {
                summary: node.node_id.to_string(),
            }
        }
    }

    #[test]
    fn diamond_runs_mid_nodes_concurrently_within_cap() {
        let dir = TempDir::new("diamond");
        let db_path = dir.path().join("hive.db");
        let (mission_id, run_id) = {
            let mut store = Store::open(&db_path).expect("pre-open");
            let mission = store.create_mission("diamond", "goal", 0).expect("mission");
            let run = store.create_run(mission.mission_id, 0).expect("run");
            (mission.mission_id, run.run_id)
        };
        let (plan, [a, b, c, d]) = diamond(mission_id, run_id);

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let cancel = CancellationToken::new();

        let summary =
            ThreadedExecutor::run(&db_path, &plan, 2, &cancel, |_store| OverlapExecutor {
                active: Arc::clone(&active),
                max_active: Arc::clone(&max_active),
                barrier: Arc::clone(&barrier),
            })
            .expect("run");

        assert_eq!(summary.succeeded, 4);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.cancelled, 0);
        assert_eq!(summary.skipped, 0);

        // The barrier guarantees the two `mid` nodes were executing at once,
        // and the counter never exceeded the configured parallelism.
        assert_eq!(max_active.load(Ordering::SeqCst), 2);

        // Final persisted states match the deterministic scheduler.
        let store = Store::open(&db_path).expect("reopen");
        for id in [a, b, c, d] {
            assert_eq!(
                store.get_node(id).expect("get").expect("present").state,
                NodeState::Succeeded
            );
        }
    }
}
