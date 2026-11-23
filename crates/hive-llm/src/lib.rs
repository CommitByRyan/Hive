//! OpenAI-compatible chat protocol support for the Hive model gateway.
//!
//! The crate owns the protocol surface only: tolerant wire types, incremental
//! SSE framing, streamed tool-call accumulation, model capability lookup and
//! retry classification. HTTP is injected through [`ChatTransport`], so the
//! default build performs no network I/O and the test suite stays offline.

mod accumulate;
mod catalog;
mod error;
mod gateway;
mod retry;
mod sse;
mod transport;
mod ureq_transport;
mod wire;

pub use accumulate::{
    CompletedToolCall, PendingToolCall, SseAction, StreamAccumulator, ToolCallAccumulator,
};
pub use catalog::{ModelCapabilities, ModelCatalog, ModelEntry, ModelsResponse};
pub use error::{ApiErrorBody, LlmError};
pub use gateway::Gateway;
pub use retry::{FailureKind, RetryDecision, RetryPolicy};
pub use sse::{SseEvent, SseParser};
pub use transport::{collect_stream, ChatTransport, HttpResponse, MockResponse, MockTransport};
pub use ureq_transport::UreqTransport;
pub use wire::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, CompletionTokensDetails, Delta,
    FinishReason, FunctionCall, FunctionCallDelta, FunctionDefinition, Message,
    PromptTokensDetails, QuotaInfo, Role, Tool, ToolCall, ToolCallDelta, ToolChoice, Usage,
};
