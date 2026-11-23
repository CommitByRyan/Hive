use hive_types::CancellationToken;

use crate::plan::PlanNode;

/// Outcome of one node execution attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecOutcome {
    Succeeded {
        summary: String,
    },
    Failed {
        reason: String,
        /// Whether the scheduler may retry this node within its budget.
        retryable: bool,
    },
    Cancelled,
}

/// Executes a single node. Implementations must be deterministic in tests and
/// must honour `cancel` promptly; the threaded executor is deferred to H05b.
pub trait NodeExecutor {
    fn execute(&mut self, node: &PlanNode, cancel: &CancellationToken) -> ExecOutcome;
}
