//! Self-owned [`NodeExecutor`] that lets the threaded runtime drive real
//! [`AgentRunner`] work over a worker-local [`Store`] connection.
//!
//! [`AgentRunner`] borrows its store, tool dispatcher and gateway, so it cannot
//! be returned from [`hive_runtime::ThreadedExecutor::run`]'s `FnMut(Store) -> E` factory,
//! which owns a fresh `Store` per worker. This type owns the store and shares
//! the stateless dispatcher and gateway behind [`Arc`], so it can be built
//! once per worker and executed on the worker thread.

use std::sync::Arc;

use hive_llm::{ChatTransport, Gateway};
use hive_runtime::{ExecOutcome, NodeExecutor, PlanNode};
use hive_store::{LeaseGuard, Store};
use hive_tools::ToolDispatcher;
use hive_types::CancellationToken;

use crate::node_executor::map_outcome;
use crate::AgentRunner;

/// A real-agent [`NodeExecutor`] that owns its worker [`Store`] and shares the
/// tool dispatcher and model gateway across workers.
///
/// It never claims a lease itself: the scheduler hands in the already-granted
/// [`LeaseGuard`] and [`AgentRunner::run_node_with_lease`] drives the node from
/// `leased -> running` through to a terminal state.
pub struct WorkerAgentExecutor<T: ChatTransport> {
    store: Store,
    tools: Arc<ToolDispatcher>,
    gateway: Arc<Gateway<T>>,
}

impl<T: ChatTransport> WorkerAgentExecutor<T> {
    pub fn new(store: Store, tools: Arc<ToolDispatcher>, gateway: Arc<Gateway<T>>) -> Self {
        Self {
            store,
            tools,
            gateway,
        }
    }

    /// Build from owned parts, wrapping the shared dispatcher and gateway in
    /// [`Arc`] so the executor can be sent to a worker thread.
    pub fn from_parts(store: Store, tools: ToolDispatcher, gateway: Gateway<T>) -> Self {
        Self::new(store, Arc::new(tools), Arc::new(gateway))
    }

    /// A factory compatible with [`hive_runtime::ThreadedExecutor::run`]: each call receives
    /// a fresh worker [`Store`] and returns an executor that shares the same
    /// dispatcher and gateway.
    pub fn factory(
        tools: Arc<ToolDispatcher>,
        gateway: Arc<Gateway<T>>,
    ) -> impl FnMut(Store) -> Self {
        move |store| Self::new(store, Arc::clone(&tools), Arc::clone(&gateway))
    }
}

impl<T: ChatTransport> NodeExecutor for WorkerAgentExecutor<T> {
    fn execute(
        &mut self,
        node: &PlanNode,
        lease: &LeaseGuard,
        cancel: &CancellationToken,
    ) -> ExecOutcome {
        let mut runner =
            AgentRunner::new(&mut self.store, self.tools.as_ref(), self.gateway.as_ref());
        map_outcome(runner.run_for(node, lease, cancel))
    }
}
