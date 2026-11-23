//! Deterministic, thread-safe fakes for the evaluation harness.
//!
//! [`ScriptedTransport`] replays a fixed response queue through the real
//! [`hive_llm::ChatTransport`] seam, so scenarios exercise the production
//! gateway and agent loop without touching the network. [`ScriptedToolExecutor`]
//! and [`SucceedingExecutor`] are deterministic [`NodeExecutor`]s used to drive
//! the DAG scheduler directly.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use hive_llm::{ChatRequest, ChatTransport, HttpResponse, LlmError};
use hive_runtime::{ExecOutcome, NodeExecutor, PlanNode};
use hive_store::LeaseGuard;
use hive_types::CancellationToken;

/// One scripted model-gateway response.
#[derive(Debug)]
pub enum ScriptedResponse {
    /// An HTTP response with the given status and raw body.
    Http { status: u16, body: Vec<u8> },
    /// A read/total timeout before any response is produced.
    Timeout,
    /// A transport-level failure (connection reset and so on).
    Failure(LlmError),
}

impl ScriptedResponse {
    pub fn ok_text(text: impl Into<String>) -> Self {
        Self::Http {
            status: 200,
            body: text.into().into_bytes(),
        }
    }

    pub fn ok_json(value: &serde_json::Value) -> Self {
        Self::ok_text(serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned()))
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Http {
            status,
            body: body.into().into_bytes(),
        }
    }

    /// A 200 response whose body is not a valid chat-completion object.
    pub fn malformed(body: impl Into<String>) -> Self {
        Self::Http {
            status: 200,
            body: body.into().into_bytes(),
        }
    }

    pub fn timeout() -> Self {
        Self::Timeout
    }

    pub fn failure(error: LlmError) -> Self {
        Self::Failure(error)
    }
}

/// In-memory [`ChatTransport`] that consumes the next [`ScriptedResponse`] per
/// call. The queue is guarded by a [`Mutex`], so a single instance can be
/// shared by several worker threads through [`Arc`].
#[derive(Default)]
pub struct ScriptedTransport {
    script: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedTransport {
    pub fn new(script: Vec<ScriptedResponse>) -> Self {
        let transport = Self::default();
        for response in script {
            transport.push(response);
        }
        transport
    }

    pub fn push(&self, response: ScriptedResponse) {
        self.script
            .lock()
            .expect("scripted transport script lock")
            .push_back(response);
    }

    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .expect("scripted transport requests lock")
            .len()
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests
            .lock()
            .expect("scripted transport requests lock")
            .clone()
    }

    fn record(&self, request: &ChatRequest) {
        self.requests
            .lock()
            .expect("scripted transport requests lock")
            .push(request.clone());
    }

    fn next(&self) -> Result<ScriptedResponse, LlmError> {
        self.script
            .lock()
            .expect("scripted transport script lock")
            .pop_front()
            .ok_or_else(|| LlmError::Transport("scripted transport exhausted".to_owned()))
    }
}

impl ChatTransport for ScriptedTransport {
    fn send(&self, request: &ChatRequest) -> Result<HttpResponse, LlmError> {
        self.record(request);
        match self.next()? {
            ScriptedResponse::Http { status, body } => Ok(HttpResponse::new(status, body)),
            ScriptedResponse::Timeout => Err(LlmError::Timeout),
            ScriptedResponse::Failure(error) => Err(error),
        }
    }
}

/// One deterministic outcome for [`ScriptedToolExecutor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptedToolStep {
    /// Tool succeeded; the node succeeds.
    Success,
    /// Tool was rejected by policy; the node fails non-retryably.
    PolicyDenied,
    /// Tool execution failed; the node fails retryably.
    ExecutionFailure,
    /// Tool hangs until the cancellation token fires, then the node cancels.
    HangThenCancel,
}

/// A deterministic [`NodeExecutor`] that models a node whose tool work returns
/// one of the four [`ScriptedToolStep`] outcomes. Steps are consumed in order
/// and the executor may be shared across worker threads.
pub struct ScriptedToolExecutor {
    steps: Arc<Mutex<VecDeque<ScriptedToolStep>>>,
}

impl ScriptedToolExecutor {
    pub fn new(steps: Vec<ScriptedToolStep>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
        }
    }

    /// A new executor sharing this one's step queue, for a per-worker factory.
    pub fn shared(&self) -> Self {
        Self {
            steps: Arc::clone(&self.steps),
        }
    }

    fn next(&self) -> Option<ScriptedToolStep> {
        self.steps
            .lock()
            .expect("scripted tool step lock")
            .pop_front()
    }
}

impl NodeExecutor for ScriptedToolExecutor {
    fn execute(
        &mut self,
        node: &PlanNode,
        _lease: &LeaseGuard,
        cancel: &CancellationToken,
    ) -> ExecOutcome {
        match self.next().unwrap_or(ScriptedToolStep::Success) {
            ScriptedToolStep::Success => ExecOutcome::Succeeded {
                summary: node.node_id.to_string(),
            },
            ScriptedToolStep::PolicyDenied => ExecOutcome::Failed {
                reason: "tool.policy_denied".to_owned(),
                retryable: false,
            },
            ScriptedToolStep::ExecutionFailure => ExecOutcome::Failed {
                reason: "tool.execution_failed".to_owned(),
                retryable: true,
            },
            ScriptedToolStep::HangThenCancel => {
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                ExecOutcome::Cancelled
            }
        }
    }
}

/// A [`NodeExecutor`] that always succeeds, recording peak concurrency and
/// optionally forcing all `mid`-role nodes to overlap via a barrier so DAG
/// parallelism is exercised deterministically without sleeping.
pub struct SucceedingExecutor {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    barrier: Option<Arc<Barrier>>,
}

impl SucceedingExecutor {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::new(AtomicUsize::new(0)),
            barrier: None,
        }
    }

    pub fn with_mid_barrier(barrier: Arc<Barrier>) -> Self {
        Self {
            barrier: Some(barrier),
            ..Self::new()
        }
    }

    /// A new executor sharing this one's counters and barrier, for use as a
    /// per-worker factory.
    pub fn shared(&self) -> Self {
        Self {
            active: Arc::clone(&self.active),
            max_active: Arc::clone(&self.max_active),
            barrier: self.barrier.clone(),
        }
    }

    pub fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }
}

impl Default for SucceedingExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeExecutor for SucceedingExecutor {
    fn execute(
        &mut self,
        node: &PlanNode,
        _lease: &LeaseGuard,
        _cancel: &CancellationToken,
    ) -> ExecOutcome {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        if node.spec.role == "mid" {
            if let Some(barrier) = &self.barrier {
                barrier.wait();
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        ExecOutcome::Succeeded {
            summary: node.node_id.to_string(),
        }
    }
}
