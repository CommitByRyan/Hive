//! Tool execution boundary. Every call follows
//! `validate -> policy -> approval -> execute -> audit`.
//!
//! `shell.run` executes argv-only programs with bounded output, a hard timeout
//! and whole-process-tree cleanup. It remains disabled by default and must be
//! enabled with an explicit program allow-list.

mod approval;
mod audit;
mod dispatcher;
mod error;
mod shell;
mod types;

pub use approval::{AllowAllApprovals, ApprovalGate, DenyApprovals};
pub use audit::{AuditSink, RecordingAuditSink, StoreAuditSink};
pub use dispatcher::{default_registry, ToolDispatcher};
pub use error::{ToolError, ToolResult};
pub use hive_policy::ShellPolicy;
pub use types::{NormalizedToolRequest, ToolCall, ToolCallOutcome, ToolContext, ToolOutput};
