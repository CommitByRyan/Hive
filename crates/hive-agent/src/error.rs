use hive_llm::LlmError;
use hive_store::StoreError;
use hive_types::{AgentSessionId, NodeId};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("agent session {0} not found")]
    SessionNotFound(AgentSessionId),
    #[error("agent session {session} belongs to node {actual}, not {expected}")]
    SessionNodeMismatch {
        session: AgentSessionId,
        expected: NodeId,
        actual: NodeId,
    },
    #[error("model returned no content and no tool calls")]
    EmptyResponse,
}
