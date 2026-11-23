//! Single-Agent vertical loop: lease a node, drive model turns, execute tools
//! through the policy dispatcher, persist turns/usage and finish the node.
//!
//! DAG scheduling, multi-Agent handoff and the HTTP server live in later
//! stages (H05/H06/H08).

mod approval;
mod error;
mod node_executor;
mod runner;
mod spec;
mod worker_executor;

pub use approval::{ApprovalPolicy, ApprovalRequest, StoreApprovalGate};
pub use error::AgentError;
pub use hive_types::CancellationToken;
pub use runner::{AgentOutcome, AgentRunner};
pub use spec::AgentSpec;
pub use worker_executor::WorkerAgentExecutor;
