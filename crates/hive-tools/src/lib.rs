//! Tool execution boundary. Every call follows
//! `validate -> policy -> approval -> execute -> audit`.
//!
//! `shell.run` is deliberately a stub in H03: it returns `shell.disabled`
//! without spawning anything. Real argv execution, timeouts, and process-tree
//! cleanup belong to H03b.

mod approval;
mod audit;
mod dispatcher;
mod error;
mod types;

pub use approval::{AllowAllApprovals, ApprovalGate, DenyApprovals};
pub use audit::{AuditSink, RecordingAuditSink, StoreAuditSink};
pub use dispatcher::{default_registry, ToolDispatcher};
pub use error::{ToolError, ToolResult};
pub use types::{NormalizedToolRequest, ToolCall, ToolCallOutcome, ToolContext, ToolOutput};
