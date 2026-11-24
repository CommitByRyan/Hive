use std::time::Duration;

use hive_llm::{ChatRequest, ChatResponse, ChatTransport, Gateway, Message, Role, Tool, Usage};
use hive_policy::RiskLevel;
use hive_runtime::{emit_info, global_metrics, LogContext};
use hive_store::{
    now_rfc3339, AgentSessionState, BudgetDimension, LeaseGuard, NewTurnUsage, Store, TurnState,
    UsageSource,
};
use hive_tools::{
    AllowAllApprovals, ApprovalGate, AuditSink, DenyApprovals, ToolCall, ToolContext,
    ToolDispatcher, ToolError, ToolResult,
};
use hive_types::{
    AgentSessionId, ApprovalId, AttemptNo, EventId, EventInput, EventKind, Handoff,
    HandoffProvenance, LeaseToken, MissionId, NodeId, NodeState, RunId, TraceId,
    HANDOFF_SCHEMA_VERSION, HANDOFF_SUMMARY_MAX_BYTES,
};
use serde_json::{json, Map, Value};

use crate::{
    AgentError, AgentSpec, ApprovalPolicy, ApprovalRequest, CancellationToken, StoreApprovalGate,
};

const PREVIEW_CHARS: usize = 512;
const DEFAULT_LEASE_SECONDS: u64 = 60;

#[derive(Clone, Debug, PartialEq)]
pub enum AgentOutcome {
    Completed {
        session_id: AgentSessionId,
        turns: u32,
        final_text: String,
    },
    TurnLimitExceeded {
        session_id: AgentSessionId,
        turns: u32,
    },
    Cancelled {
        session_id: AgentSessionId,
        turns: u32,
    },
    WaitingApproval {
        session_id: AgentSessionId,
        turns: u32,
        approval_id: ApprovalId,
        tool_name: String,
    },
}

/// Single-node executor.
///
/// Every node transition goes through [`Store::transition_node_cas`] with the
/// lease guard; tool events are written through [`GuardedAuditSink`] so a stale
/// executor cannot append results after its lease was replaced.
pub struct AgentRunner<'a, T: ChatTransport> {
    store: &'a mut Store,
    tools: &'a ToolDispatcher,
    gateway: &'a Gateway<T>,
    lease_owner: String,
    lease_ttl: Duration,
    approval_policy: ApprovalPolicy,
}

impl<'a, T: ChatTransport> AgentRunner<'a, T> {
    pub fn new(store: &'a mut Store, tools: &'a ToolDispatcher, gateway: &'a Gateway<T>) -> Self {
        Self {
            store,
            tools,
            gateway,
            lease_owner: format!("h04-{}", std::process::id()),
            lease_ttl: Duration::from_secs(DEFAULT_LEASE_SECONDS),
            approval_policy: ApprovalPolicy::Deny,
        }
    }

    /// Private-in-crate access for the runtime [`NodeExecutor`] adapter, which
    /// creates the agent session before handing off to [`Self::run_node`].
    pub(crate) fn store_mut(&mut self) -> &mut Store {
        self.store
    }

    pub fn with_lease(mut self, owner: impl Into<String>, ttl: Duration) -> Self {
        self.lease_owner = owner.into();
        self.lease_ttl = ttl;
        self
    }

    pub fn with_approval_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.approval_policy = policy;
        self
    }

    pub fn run_node(
        &mut self,
        session: AgentSessionId,
        node: NodeId,
        spec: &AgentSpec,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let now = now_ms();
        let lease_token = LeaseToken::generate();
        let leased = self.store.lease_node(
            node,
            &lease_token,
            &self.lease_owner,
            now + self.lease_ttl_ms(),
            now,
        )?;
        let guard = LeaseGuard {
            attempt_no: leased.attempt_no,
            lease_token,
        };
        self.run_node_with_lease(session, node, &guard, spec, cancel)
    }

    /// Run a node using a lease already granted by the scheduler.
    ///
    /// This method never calls [`Store::lease_node`]; the caller owns the
    /// lease in `guard`. On success it constructs, validates and persists a
    /// structured Handoff, then writes its `handoff_id` into the
    /// `node.succeeded` event payload.
    pub fn run_node_with_lease(
        &mut self,
        session: AgentSessionId,
        node: NodeId,
        guard: &LeaseGuard,
        spec: &AgentSpec,
        cancel: &CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let guard = (*guard).clone();
        let session_record = self
            .store
            .get_agent_session(session)?
            .ok_or(AgentError::SessionNotFound(session))?;
        if session_record.node_id != node {
            return Err(AgentError::SessionNodeMismatch {
                session,
                expected: node,
                actual: session_record.node_id,
            });
        }
        let node_record = self
            .store
            .get_node(node)?
            .ok_or_else(|| hive_store::StoreError::NotFound(format!("node {node}")))?;
        let mission = self
            .store
            .get_mission(node_record.mission_id)?
            .ok_or_else(|| {
                hive_store::StoreError::NotFound(format!("mission {}", node_record.mission_id))
            })?;
        let mission_id = mission.mission_id;
        let run_id = node_record.run_id;
        let trace_id = TraceId::new();
        let scope = NodeScope {
            node,
            mission_id,
            run_id,
            session,
            trace_id,
        };
        let log_context = LogContext::node(trace_id, mission_id, run_id, node, guard.attempt_no)
            .with_session(session)
            .with_model(spec.model.as_str());
        emit_info("hive_agent", "node execution started", &log_context);

        if cancel.is_cancelled() {
            return self.cancel_node(scope, &guard, NodeState::Leased, 0);
        }

        let now = now_ms();
        let started = node_event(
            mission_id,
            Some(run_id),
            node,
            session,
            guard.attempt_no,
            EventKind::NodeStarted,
            json!({ "role": spec.role, "model": spec.model }),
            Some(trace_id),
        );
        self.store
            .transition_node_cas(node, &guard, NodeState::Running, &started, now)?;

        let mut messages = Vec::new();
        if !spec.system_prompt.is_empty() {
            messages.push(Message::system(spec.system_prompt.clone()));
        }
        let goal = if mission.goal.is_empty() {
            "Execute this node.".to_owned()
        } else {
            mission.goal.clone()
        };
        messages.push(Message::user(goal.clone()));

        let tools = self.tool_definitions(spec);
        let mut turn_index = 0u32;

        loop {
            if cancel.is_cancelled() {
                return self.cancel_node(scope, &guard, NodeState::Running, turn_index);
            }
            if turn_index >= spec.max_turns {
                self.fail_node(scope, &guard, turn_index, "turn_limit_exceeded")?;
                self.store
                    .set_agent_session_state(session, AgentSessionState::Failed, now_ms())?;
                return Ok(AgentOutcome::TurnLimitExceeded {
                    session_id: session,
                    turns: turn_index,
                });
            }
            turn_index += 1;

            let turn_now = now_ms();
            self.store.renew_lease(
                node,
                &guard.lease_token,
                &self.lease_owner,
                turn_now + self.lease_ttl_ms(),
                turn_now,
            )?;
            let turn_event = node_event(
                mission_id,
                Some(run_id),
                node,
                session,
                guard.attempt_no,
                EventKind::NodeTurn,
                json!({
                    "turn_index": turn_index,
                    "model": spec.model,
                    "message_count": messages.len(),
                }),
                Some(trace_id),
            );
            self.store.append_event_guarded(node, &guard, &turn_event)?;
            let turn = match self.store.start_turn(
                session,
                node,
                guard.attempt_no,
                turn_index,
                &spec.model,
                turn_now,
            ) {
                Ok(turn) => turn,
                Err(hive_store::StoreError::BudgetExceeded { dimension }) => {
                    return self.fail_budget(scope, &guard, &dimension, turn_index);
                }
                Err(error) => return Err(error.into()),
            };

            // Hard token pre-check: refuse to issue a model request when the
            // node-level token budget is already exhausted. The authoritative
            // settlement still happens when usage is recorded below.
            if self
                .store
                .budget_remaining(node, BudgetDimension::Tokens)?
                .is_some_and(|remaining| remaining <= 0)
            {
                self.store
                    .finish_turn(turn.turn_id, TurnState::Failed, None, "", 0, now_ms())?;
                return self.fail_budget(scope, &guard, "tokens", turn_index);
            }

            let mut request = ChatRequest::new(spec.model.clone(), messages.clone())
                .with_max_tokens(spec.max_tokens);
            if !tools.is_empty() {
                request.tools = Some(tools.clone());
            }

            let response = match self.gateway.complete(&request) {
                Ok(response) => response,
                Err(error) => {
                    let message = error.to_string();
                    self.store.finish_turn(
                        turn.turn_id,
                        TurnState::Failed,
                        None,
                        "",
                        0,
                        now_ms(),
                    )?;
                    self.fail_node(scope, &guard, turn_index, &message)?;
                    self.store.set_agent_session_state(
                        session,
                        AgentSessionState::Failed,
                        now_ms(),
                    )?;
                    return Err(AgentError::Llm(error));
                }
            };

            let output_text = response.text().unwrap_or_default().to_owned();
            let tool_calls = response
                .message()
                .and_then(|message| message.tool_calls.clone())
                .unwrap_or_default();
            let finish_reason = response
                .finish_reason()
                .map(|reason| reason.as_str().to_owned());
            let preview = preview(&output_text);
            let usage = build_usage(&response, &request, &output_text, &tool_calls);
            if let Err(error) = self.store.record_turn_usage(turn.turn_id, &usage, now_ms()) {
                if let hive_store::StoreError::BudgetExceeded { dimension } = error {
                    self.store.finish_turn(
                        turn.turn_id,
                        TurnState::Failed,
                        finish_reason.as_deref(),
                        &preview,
                        tool_calls.len() as u32,
                        now_ms(),
                    )?;
                    return self.fail_budget(scope, &guard, &dimension, turn_index);
                }
                return Err(error.into());
            }

            if cancel.is_cancelled() {
                self.store.finish_turn(
                    turn.turn_id,
                    TurnState::Cancelled,
                    finish_reason.as_deref(),
                    &preview,
                    tool_calls.len() as u32,
                    now_ms(),
                )?;
                return self.cancel_node(scope, &guard, NodeState::Running, turn_index);
            }

            if !tool_calls.is_empty() {
                let assistant = response.message().cloned().unwrap_or_else(|| Message {
                    role: Role::Assistant,
                    tool_calls: Some(tool_calls.clone()),
                    ..Message::default()
                });
                messages.push(assistant);

                let mut results = Vec::with_capacity(tool_calls.len());
                for (index, call) in tool_calls.iter().enumerate() {
                    let call_id = call
                        .id
                        .clone()
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| format!("call_{turn_index}_{index}"));
                    let tool_name = call.name().to_owned();
                    let arguments = match call.parse_arguments() {
                        Ok(arguments) => arguments,
                        Err(error) => {
                            self.record_tool_denied(
                                scope,
                                &guard,
                                &tool_name,
                                "tool.invalid_arguments",
                                &error.to_string(),
                            )?;
                            results.push(Message::tool_result(
                                call_id,
                                json!({
                                    "error": "tool.invalid_arguments",
                                    "message": error.to_string(),
                                })
                                .to_string(),
                            ));
                            continue;
                        }
                    };
                    if !spec
                        .tool_allowlist
                        .iter()
                        .any(|allowed| allowed == &tool_name)
                    {
                        self.record_tool_denied(
                            scope,
                            &guard,
                            &tool_name,
                            "tool.not_allowed",
                            "tool is not in the agent allowlist",
                        )?;
                        results.push(Message::tool_result(
                            call_id,
                            json!({
                                "error": "tool.not_allowed",
                                "message": format!("{tool_name} is not in the agent allowlist"),
                            })
                            .to_string(),
                        ));
                        continue;
                    }

                    let (risk, tool_version) = self
                        .tools
                        .registry()
                        .get(&tool_name)
                        .map(|tool| (tool.risk, tool.version))
                        .unwrap_or((RiskLevel::ReadOnly, 1));
                    global_metrics().inc_tool_calls(&tool_name);
                    if self.approval_policy.requires_approval(risk) {
                        let approval = {
                            let mut gate = StoreApprovalGate::new(
                                &mut *self.store,
                                StoreApprovalGate::DEFAULT_TIMEOUT,
                            );
                            gate.request(
                                &ApprovalRequest {
                                    mission_id,
                                    run_id: Some(run_id),
                                    node_id: node,
                                    session_id: session,
                                    attempt_no: guard.attempt_no,
                                    lease_token: guard.lease_token.clone(),
                                    policy_version: "v1",
                                    tool_name: &tool_name,
                                    tool_version,
                                    arguments: &arguments,
                                },
                                &guard,
                                now_ms(),
                            )?
                            .1
                        };
                        self.store.finish_turn(
                            turn.turn_id,
                            TurnState::Completed,
                            finish_reason.as_deref(),
                            &preview,
                            tool_calls.len() as u32,
                            now_ms(),
                        )?;
                        return Ok(AgentOutcome::WaitingApproval {
                            session_id: session,
                            turns: turn_index,
                            approval_id: approval.approval_id,
                            tool_name,
                        });
                    }

                    let context = ToolContext {
                        mission_id,
                        run_id: Some(run_id),
                        node_id: node,
                        session_id: Some(session),
                        attempt_no: guard.attempt_no,
                        lease_token: guard.lease_token.clone(),
                        policy_version: "v1".to_owned(),
                    };
                    let mut audit = GuardedAuditSink {
                        store: &mut *self.store,
                        node_id: node,
                        guard: guard.clone(),
                        budget_exceeded: None,
                    };
                    let mut approvals: Box<dyn ApprovalGate> = match self.approval_policy {
                        ApprovalPolicy::Request => Box::new(AllowAllApprovals),
                        ApprovalPolicy::Deny => Box::new(DenyApprovals),
                    };
                    let outcome = self.tools.dispatch(
                        &context,
                        ToolCall::new(tool_name, arguments),
                        &mut audit,
                        &mut *approvals,
                    );
                    if let Some(dimension) = audit.budget_exceeded.take() {
                        self.store.finish_turn(
                            turn.turn_id,
                            TurnState::Failed,
                            finish_reason.as_deref(),
                            &preview,
                            tool_calls.len() as u32,
                            now_ms(),
                        )?;
                        return self.fail_budget(scope, &guard, &dimension, turn_index);
                    }
                    match outcome {
                        Ok(outcome) => {
                            results.push(Message::tool_result(call_id, outcome.output.content));
                        }
                        Err(error) if error.code() == "tool.stale_execution" => {
                            return Err(AgentError::Store(hive_store::StoreError::StaleExecution));
                        }
                        Err(error) => {
                            results.push(Message::tool_result(
                                call_id,
                                json!({
                                    "error": error.code(),
                                    "message": error.message(),
                                })
                                .to_string(),
                            ));
                        }
                    }
                }
                messages.extend(results);
                self.store.finish_turn(
                    turn.turn_id,
                    TurnState::Completed,
                    finish_reason.as_deref(),
                    &preview,
                    tool_calls.len() as u32,
                    now_ms(),
                )?;
                continue;
            }

            if output_text.trim().is_empty() {
                self.store.finish_turn(
                    turn.turn_id,
                    TurnState::Failed,
                    finish_reason.as_deref(),
                    "",
                    0,
                    now_ms(),
                )?;
                self.fail_node(scope, &guard, turn_index, "empty_response")?;
                self.store
                    .set_agent_session_state(session, AgentSessionState::Failed, now_ms())?;
                return Err(AgentError::EmptyResponse);
            }

            self.store.finish_turn(
                turn.turn_id,
                TurnState::Completed,
                finish_reason.as_deref(),
                &preview,
                tool_calls.len() as u32,
                now_ms(),
            )?;
            let handoff = build_handoff(
                mission_id,
                run_id,
                node,
                spec,
                guard.attempt_no,
                &goal,
                &output_text,
            );
            let handoff_record = self.store.insert_handoff(&handoff)?;
            let succeeded = node_event(
                mission_id,
                Some(run_id),
                node,
                session,
                guard.attempt_no,
                EventKind::NodeSucceeded,
                json!({
                    "turns": turn_index,
                    "finish_reason": finish_reason,
                    "final_preview": preview,
                    "handoff_id": handoff_record.handoff_id,
                    "summary": handoff.summary,
                }),
                Some(trace_id),
            );
            self.store.transition_node_cas(
                node,
                &guard,
                NodeState::Succeeded,
                &succeeded,
                now_ms(),
            )?;
            self.store
                .set_agent_session_state(session, AgentSessionState::Completed, now_ms())?;
            return Ok(AgentOutcome::Completed {
                session_id: session,
                turns: turn_index,
                final_text: output_text,
            });
        }
    }

    fn lease_ttl_ms(&self) -> i64 {
        i64::try_from(self.lease_ttl.as_millis()).unwrap_or(i64::MAX)
    }

    fn tool_definitions(&self, spec: &AgentSpec) -> Vec<Tool> {
        let mut tools = Vec::new();
        for name in &spec.tool_allowlist {
            let Some(tool_spec) = self.tools.registry().get(name) else {
                continue;
            };
            let mut properties = Map::new();
            for argument in &tool_spec.allowed_arguments {
                properties.insert(argument.clone(), json!({ "type": "string" }));
            }
            let schema = json!({
                "type": "object",
                "properties": properties,
                "required": tool_spec.required_arguments,
            });
            tools.push(Tool::function(tool_spec.name.clone(), schema));
        }
        tools
    }

    fn record_tool_denied(
        &mut self,
        scope: NodeScope,
        guard: &LeaseGuard,
        tool_name: &str,
        code: &str,
        message: &str,
    ) -> Result<(), AgentError> {
        let intent = node_event(
            scope.mission_id,
            Some(scope.run_id),
            scope.node,
            scope.session,
            guard.attempt_no,
            EventKind::NodeToolCall,
            json!({ "tool_name": tool_name, "phase": "intent" }),
            Some(scope.trace_id),
        );
        self.store
            .append_event_guarded(scope.node, guard, &intent)?;
        let result = node_event(
            scope.mission_id,
            Some(scope.run_id),
            scope.node,
            scope.session,
            guard.attempt_no,
            EventKind::NodeToolResult,
            json!({
                "tool_name": tool_name,
                "status": "error",
                "code": code,
                "message": message,
            }),
            Some(scope.trace_id),
        );
        self.store
            .append_event_guarded(scope.node, guard, &result)?;
        Ok(())
    }

    fn fail_node(
        &mut self,
        scope: NodeScope,
        guard: &LeaseGuard,
        turns: u32,
        reason: &str,
    ) -> Result<(), AgentError> {
        let event = node_event(
            scope.mission_id,
            Some(scope.run_id),
            scope.node,
            scope.session,
            guard.attempt_no,
            EventKind::NodeFailed,
            json!({ "reason": reason, "turns": turns }),
            Some(scope.trace_id),
        );
        self.store
            .transition_node_cas(scope.node, guard, NodeState::Failed, &event, now_ms())?;
        Ok(())
    }

    /// Structured budget failure: emit `node.budget_exceeded`, move the node
    /// to `failed` in the same transaction, then surface a non-retryable
    /// budget error. The scheduler finalizes `failed -> failed_final` without
    /// retrying or emitting `node.ready`.
    fn fail_budget(
        &mut self,
        scope: NodeScope,
        guard: &LeaseGuard,
        dimension: &str,
        turns: u32,
    ) -> Result<AgentOutcome, AgentError> {
        let budget = BudgetDimension::parse(dimension).unwrap_or(BudgetDimension::Tokens);
        let record = self
            .store
            .get_budget("node", &scope.node.to_string(), budget)?;
        let consumed = record.as_ref().map(|record| record.consumed);
        let hard_limit = record.as_ref().and_then(|record| record.hard_limit);
        let event = node_event(
            scope.mission_id,
            Some(scope.run_id),
            scope.node,
            scope.session,
            guard.attempt_no,
            EventKind::NodeBudgetExceeded,
            json!({
                "dimension": dimension,
                "consumed": consumed,
                "hard_limit": hard_limit,
                "estimated": false,
                "turns": turns,
            }),
            Some(scope.trace_id),
        );
        self.store
            .transition_node_cas(scope.node, guard, NodeState::Failed, &event, now_ms())?;
        self.store
            .set_agent_session_state(scope.session, AgentSessionState::Failed, now_ms())?;
        Err(AgentError::BudgetExceeded {
            dimension: dimension.to_owned(),
        })
    }

    fn cancel_node(
        &mut self,
        scope: NodeScope,
        guard: &LeaseGuard,
        from: NodeState,
        turns: u32,
    ) -> Result<AgentOutcome, AgentError> {
        debug_assert!(matches!(from, NodeState::Leased | NodeState::Running));
        let event = node_event(
            scope.mission_id,
            Some(scope.run_id),
            scope.node,
            scope.session,
            guard.attempt_no,
            EventKind::NodeCancelled,
            json!({ "turns": turns }),
            Some(scope.trace_id),
        );
        self.store.transition_node_cas(
            scope.node,
            guard,
            NodeState::Cancelled,
            &event,
            now_ms(),
        )?;
        self.store.set_agent_session_state(
            scope.session,
            AgentSessionState::Cancelled,
            now_ms(),
        )?;
        Ok(AgentOutcome::Cancelled {
            session_id: scope.session,
            turns,
        })
    }
}

#[derive(Clone, Copy)]
struct NodeScope {
    node: NodeId,
    mission_id: MissionId,
    run_id: RunId,
    session: AgentSessionId,
    trace_id: TraceId,
}

struct GuardedAuditSink<'a> {
    store: &'a mut Store,
    node_id: NodeId,
    guard: LeaseGuard,
    budget_exceeded: Option<String>,
}

impl AuditSink for GuardedAuditSink<'_> {
    fn append(&mut self, event: &EventInput) -> ToolResult<()> {
        match self
            .store
            .append_tool_event(self.node_id, &self.guard, event, now_ms())
        {
            Ok(_) => Ok(()),
            Err(hive_store::StoreError::BudgetExceeded { dimension }) => {
                self.budget_exceeded = Some(dimension.clone());
                Err(ToolError::from(hive_store::StoreError::BudgetExceeded {
                    dimension,
                }))
            }
            Err(error) => Err(ToolError::from(error)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn node_event(
    mission_id: MissionId,
    run_id: Option<RunId>,
    node_id: NodeId,
    session_id: AgentSessionId,
    attempt_no: hive_types::AttemptNo,
    kind: EventKind,
    payload: Value,
    trace_id: Option<TraceId>,
) -> EventInput {
    EventInput {
        event_id: EventId::new(),
        kind,
        mission_id,
        run_id,
        node_id: Some(node_id),
        session_id: Some(session_id),
        attempt_no: Some(attempt_no),
        trace_id,
        payload,
    }
}

fn build_usage(
    response: &ChatResponse,
    request: &ChatRequest,
    output_text: &str,
    tool_calls: &[hive_llm::ToolCall],
) -> NewTurnUsage {
    if let Some(usage) = response.usage.as_ref() {
        return authoritative_usage(usage, request_id(response));
    }
    let prompt = serde_json::to_string(&request.messages)
        .map(|encoded| estimate_tokens(&encoded))
        .unwrap_or(1);
    let mut completion_text = output_text.to_owned();
    for call in tool_calls {
        completion_text.push_str(&call.function.arguments);
    }
    let completion = estimate_tokens(&completion_text);
    NewTurnUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt.saturating_add(completion),
        source: UsageSource::Estimated,
        request_id: None,
    }
}

fn authoritative_usage(usage: &Usage, request_id: Option<String>) -> NewTurnUsage {
    let prompt_tokens = u64::from(usage.prompt_tokens);
    let completion_tokens = u64::from(usage.completion_tokens);
    let total_tokens = u64::from(usage.total_tokens).max(prompt_tokens + completion_tokens);
    NewTurnUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        source: UsageSource::Authoritative,
        request_id,
    }
}

fn request_id(response: &ChatResponse) -> Option<String> {
    (!response.id.is_empty()).then(|| response.id.clone())
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64 / 4).max(1)
}

fn preview(text: &str) -> String {
    text.chars().take(PREVIEW_CHARS).collect()
}

fn build_handoff(
    mission_id: MissionId,
    run_id: RunId,
    node: NodeId,
    spec: &AgentSpec,
    attempt_no: AttemptNo,
    objective: &str,
    final_text: &str,
) -> Handoff {
    let (base_revision, risks) = match &spec.workspace_root {
        Some(root) => match hive_workspace::head_revision_at(root) {
            Ok(sha) => (sha, Vec::new()),
            Err(error) => (
                "unknown".to_owned(),
                vec![format!("base_revision unavailable: {error}")],
            ),
        },
        None => ("unknown".to_owned(), Vec::new()),
    };
    Handoff {
        schema_version: HANDOFF_SCHEMA_VERSION,
        mission_id,
        run_id,
        node_id: node,
        agent: spec.role.clone(),
        objective: objective.to_owned(),
        summary: clamp_summary(final_text),
        inputs: Vec::new(),
        artifacts: Vec::new(),
        decisions: Vec::new(),
        assumptions: Vec::new(),
        risks,
        open_questions: Vec::new(),
        acceptance: Vec::new(),
        recommended_next: Vec::new(),
        provenance: HandoffProvenance {
            attempt_no,
            base_revision,
            created_at: now_rfc3339(),
        },
    }
}

fn clamp_summary(text: &str) -> String {
    if text.len() <= HANDOFF_SUMMARY_MAX_BYTES {
        return text.to_owned();
    }
    let mut end = HANDOFF_SUMMARY_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use hive_llm::{
        ChatTransport, HttpResponse, LlmError, MockResponse, MockTransport, RetryPolicy,
    };
    use hive_store::{
        AgentSessionRecord, BudgetDimension, BudgetLimits, MissionRecord, NewAgentSession,
        NodeRecord, RunRecord, SideEffectState,
    };
    use hive_types::{ApprovalState, AttemptNo, MissionId};
    use serde_json::json;

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "hive-agent-{name}-{}-{counter}",
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

    struct Fixture {
        store: Store,
        mission: MissionRecord,
        run: RunRecord,
        node: NodeRecord,
        session: AgentSessionRecord,
        dir: TempDir,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = TempDir::new(name);
        let mut store = Store::open_in_memory().expect("store");
        let mission = store
            .create_mission("m", "read the note", 0)
            .expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node = store
            .add_node(mission.mission_id, run.run_id, 3, 0)
            .expect("node");
        store
            .update_node_state(node.node_id, NodeState::Ready, 0)
            .expect("ready");
        let session = store
            .create_agent_session(&NewAgentSession {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                role: "worker",
                model: "mock-model",
                system_prompt: "be concise",
                now_ms: 0,
            })
            .expect("session");
        Fixture {
            store,
            mission,
            run,
            node,
            session,
            dir,
        }
    }

    fn spec() -> AgentSpec {
        AgentSpec::new("worker", "mock-model")
            .with_system_prompt("be concise")
            .with_tools(["fs.read".to_owned()])
    }

    fn completion(content: &str, usage: Option<Value>) -> Value {
        let mut value = json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": content}
            }]
        });
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        value
    }

    fn tool_completion(tool_name: &str, arguments: Value) -> Value {
        json!({
            "id": "chatcmpl-tool",
            "object": "chat.completion",
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": tool_name,
                            "arguments": serde_json::to_string(&arguments).expect("arguments")
                        }
                    }]
                }
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        })
    }

    fn kinds(store: &Store, mission_id: MissionId) -> Vec<EventKind> {
        store
            .read_all_events()
            .expect("events")
            .into_iter()
            .filter(|event| event.mission_id == mission_id)
            .map(|event| event.kind)
            .collect()
    }

    fn init_git_repo(dir: &Path) -> String {
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), b"base\n").expect("write");
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "init"]);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .expect("git");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn single_turn_answer_succeeds_with_expected_events() {
        let mut f = fixture("single");
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion(
                "the answer",
                Some(json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8})),
            ),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run")
        };
        assert_eq!(
            outcome,
            AgentOutcome::Completed {
                session_id: f.session.session_id,
                turns: 1,
                final_text: "the answer".to_owned(),
            }
        );
        assert_eq!(
            kinds(&f.store, f.mission.mission_id),
            vec![
                EventKind::NodeStarted,
                EventKind::NodeTurn,
                EventKind::HandoffCreated,
                EventKind::NodeSucceeded
            ]
        );
        let node = f
            .store
            .get_node(f.node.node_id)
            .expect("node")
            .expect("present");
        assert_eq!(node.state, NodeState::Succeeded);
        assert!(node.lease_token.is_none());

        let turns = f.store.list_turns(f.session.session_id).expect("turns");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, TurnState::Completed);
        let usage = f
            .store
            .get_turn_usage(turns[0].turn_id)
            .expect("usage")
            .expect("present");
        assert_eq!(usage.source, UsageSource::Authoritative);
        assert_eq!(usage.total_tokens, 8);
        assert_eq!(usage.request_id.as_deref(), Some("chatcmpl-test"));
        assert_eq!(
            f.store
                .get_agent_session(f.session.session_id)
                .expect("session")
                .expect("present")
                .state,
            AgentSessionState::Completed
        );
    }

    #[test]
    fn node_start_log_shares_trace_with_node_started_event() {
        use serde_json::Value;

        let mut f = fixture("trace");
        let capture = hive_runtime::start_capture();
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion(
                "the answer",
                Some(json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8})),
            ),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run");
        }

        let node_id = f.node.node_id.to_string();
        let log = capture
            .records()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|record| {
                record["msg"] == "node execution started"
                    && record["node_id"].as_str() == Some(node_id.as_str())
            })
            .expect("node start log line");
        let started = f
            .store
            .read_all_events()
            .expect("events")
            .into_iter()
            .find(|event| {
                event.kind == EventKind::NodeStarted && event.node_id == Some(f.node.node_id)
            })
            .expect("node.started event");
        let event_trace = started.trace_id.expect("node.started trace").to_string();
        assert_eq!(log["trace_id"].as_str(), Some(event_trace.as_str()));
        assert!(log.get("prompt").is_none());
        assert!(log.get("api_key").is_none());
    }

    #[test]
    fn tool_call_round_trip_reads_file_and_feeds_result_back() {
        let mut f = fixture("tool");
        std::fs::write(f.dir.path().join("note.txt"), b"hello hive").expect("note");
        let transport = MockTransport::new(vec![
            MockResponse::json(
                200,
                &tool_completion("fs.read", json!({"path": "note.txt"})),
            ),
            MockResponse::json(200, &completion("read it", None)),
        ]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { turns: 2, .. }));
        assert_eq!(
            kinds(&f.store, f.mission.mission_id),
            vec![
                EventKind::NodeStarted,
                EventKind::NodeTurn,
                EventKind::NodeToolCall,
                EventKind::NodeToolStarted,
                EventKind::NodeToolResult,
                EventKind::NodeTurn,
                EventKind::HandoffCreated,
                EventKind::NodeSucceeded
            ]
        );
        let requests = gateway.transport().requests();
        assert_eq!(requests.len(), 2);
        let last = requests[1].messages.last().expect("tool message");
        assert_eq!(last.role, Role::Tool);
        assert_eq!(last.tool_call_id.as_deref(), Some("call_1"));
        assert!(last.text().expect("content").contains("hello hive"));
    }

    #[test]
    fn tool_error_is_fed_back_and_model_continues() {
        let mut f = fixture("tool-error");
        let transport = MockTransport::new(vec![
            MockResponse::json(
                200,
                &tool_completion("fs.read", json!({"path": "missing.txt"})),
            ),
            MockResponse::json(200, &completion("recovered", None)),
        ]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { turns: 2, .. }));
        let requests = gateway.transport().requests();
        let last = requests[1].messages.last().expect("tool message");
        assert_eq!(last.role, Role::Tool);
        let payload: Value =
            serde_json::from_str(last.text().expect("content")).expect("json result");
        assert_eq!(payload["error"], "path.not_found");
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Succeeded
        );
    }

    #[test]
    fn turn_limit_marks_node_failed() {
        let mut f = fixture("turn-limit");
        let transport = MockTransport::new(vec![
            MockResponse::json(
                200,
                &tool_completion("fs.read", json!({"path": "note.txt"})),
            ),
            MockResponse::json(200, &completion("never reached", None)),
        ]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        std::fs::write(f.dir.path().join("note.txt"), b"x").expect("note");
        let token = CancellationToken::new();
        let limited = spec().with_max_turns(1);

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &limited, &token)
                .expect("run")
        };
        assert_eq!(
            outcome,
            AgentOutcome::TurnLimitExceeded {
                session_id: f.session.session_id,
                turns: 1,
            }
        );
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Failed
        );
        let events = f.store.read_all_events().expect("events");
        let failed = events
            .iter()
            .find(|event| event.kind == EventKind::NodeFailed)
            .expect("failed event");
        assert_eq!(failed.payload["reason"], "turn_limit_exceeded");
    }

    struct CancelAfterFirstResponse {
        inner: MockTransport,
        token: CancellationToken,
    }

    impl ChatTransport for CancelAfterFirstResponse {
        fn send(&self, request: &ChatRequest) -> Result<HttpResponse, LlmError> {
            let response = self.inner.send(request)?;
            if self.inner.request_count() == 1 {
                self.token.cancel();
            }
            Ok(response)
        }
    }

    #[test]
    fn cancellation_between_turns_marks_node_cancelled() {
        let mut f = fixture("cancel");
        let token = CancellationToken::new();
        let transport = CancelAfterFirstResponse {
            inner: MockTransport::new(vec![
                MockResponse::json(
                    200,
                    &tool_completion("fs.read", json!({"path": "note.txt"})),
                ),
                MockResponse::json(200, &completion("unused", None)),
            ]),
            token: token.clone(),
        };
        let gateway = Gateway::with_policy(transport, RetryPolicy::immediate(0));
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run")
        };
        assert_eq!(
            outcome,
            AgentOutcome::Cancelled {
                session_id: f.session.session_id,
                turns: 1,
            }
        );
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Cancelled
        );
        assert_eq!(
            kinds(&f.store, f.mission.mission_id),
            vec![
                EventKind::NodeStarted,
                EventKind::NodeTurn,
                EventKind::NodeCancelled
            ]
        );
        assert_eq!(
            f.store
                .get_agent_session(f.session.session_id)
                .expect("session")
                .expect("present")
                .state,
            AgentSessionState::Cancelled
        );
    }

    #[test]
    fn stale_lease_commit_is_rejected() {
        let mut f = fixture("stale");
        let token = LeaseToken::generate();
        f.store
            .lease_node(f.node.node_id, &token, "first", 10_000, 0)
            .expect("lease");
        let guard = LeaseGuard {
            attempt_no: AttemptNo::FIRST,
            lease_token: token,
        };
        let started = node_event(
            f.mission.mission_id,
            Some(f.run.run_id),
            f.node.node_id,
            f.session.session_id,
            AttemptNo::FIRST,
            EventKind::NodeStarted,
            json!({}),
            Some(TraceId::new()),
        );
        f.store
            .transition_node_cas(f.node.node_id, &guard, NodeState::Running, &started, 1)
            .expect("running");

        let stale = LeaseGuard {
            attempt_no: AttemptNo::FIRST,
            lease_token: LeaseToken::generate(),
        };
        let succeeded = node_event(
            f.mission.mission_id,
            Some(f.run.run_id),
            f.node.node_id,
            f.session.session_id,
            AttemptNo::FIRST,
            EventKind::NodeSucceeded,
            json!({}),
            Some(TraceId::new()),
        );
        let error = f
            .store
            .transition_node_cas(f.node.node_id, &stale, NodeState::Succeeded, &succeeded, 2)
            .expect_err("stale");
        assert!(matches!(error, hive_store::StoreError::StaleExecution));
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Running
        );
    }

    #[test]
    fn missing_usage_is_recorded_as_estimated() {
        let mut f = fixture("estimated");
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion("some answer", None),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run");
        }
        let turns = f.store.list_turns(f.session.session_id).expect("turns");
        let usage = f
            .store
            .get_turn_usage(turns[0].turn_id)
            .expect("usage")
            .expect("present");
        assert_eq!(usage.source, UsageSource::Estimated);
        assert!(usage.prompt_tokens >= 1);
        assert!(usage.completion_tokens >= 1);
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens
        );
    }

    #[test]
    fn disallowed_tool_is_reported_without_execution() {
        let mut f = fixture("not-allowed");
        std::fs::write(f.dir.path().join("note.txt"), b"secret").expect("note");
        let transport = MockTransport::new(vec![
            MockResponse::json(
                200,
                &tool_completion("fs.read", json!({"path": "note.txt"})),
            ),
            MockResponse::json(200, &completion("ok", None)),
        ]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        let no_tools = AgentSpec::new("worker", "mock-model");

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &no_tools, &token)
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { turns: 2, .. }));
        let requests = gateway.transport().requests();
        assert!(requests[0].tools.is_none());
        let last = requests[1].messages.last().expect("tool message");
        let payload: Value =
            serde_json::from_str(last.text().expect("content")).expect("json result");
        assert_eq!(payload["error"], "tool.not_allowed");
        let events = f.store.read_all_events().expect("events");
        assert!(events
            .iter()
            .all(|event| event.kind != EventKind::NodeToolStarted));
    }

    #[test]
    fn turn_limit_zero_fails_without_calling_model() {
        let mut f = fixture("zero-turns");
        let transport = MockTransport::default();
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        let zero = spec().with_max_turns(0);

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &zero, &token)
                .expect("run")
        };
        assert_eq!(
            outcome,
            AgentOutcome::TurnLimitExceeded {
                session_id: f.session.session_id,
                turns: 0,
            }
        );
        assert_eq!(gateway.transport().request_count(), 0);
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Failed
        );
    }

    #[test]
    fn write_tool_requests_approval_and_parks_node() {
        let mut f = fixture("approval-request");
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &tool_completion("fs.write", json!({"path": "out.txt", "content": "payload"})),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        let write_spec = spec().with_tools(["fs.write".to_owned()]);

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway)
                .with_approval_policy(ApprovalPolicy::Request);
            runner
                .run_node(f.session.session_id, f.node.node_id, &write_spec, &token)
                .expect("run")
        };
        let AgentOutcome::WaitingApproval {
            approval_id,
            tool_name,
            turns,
            ..
        } = outcome
        else {
            panic!("expected waiting approval, got {outcome:?}");
        };
        assert_eq!(tool_name, "fs.write");
        assert_eq!(turns, 1);
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::WaitingApproval
        );
        let approval = f
            .store
            .get_approval(approval_id)
            .expect("approval")
            .expect("present");
        assert_eq!(approval.state, ApprovalState::Pending);
        assert_eq!(approval.tool_name, "fs.write");
        assert!(!f.dir.path().join("out.txt").exists());
        assert_eq!(
            kinds(&f.store, f.mission.mission_id),
            vec![
                EventKind::NodeStarted,
                EventKind::NodeTurn,
                EventKind::NodeApprovalRequested
            ]
        );
        let requests = gateway.transport().requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tools.as_ref().map(|tools| tools.len()), Some(1));
    }

    #[test]
    fn approved_tool_is_consumed_once_then_executes() {
        use hive_store::ApprovalDecision;

        let mut f = fixture("approval-consume");
        let lease = LeaseToken::generate();
        f.store
            .lease_node(f.node.node_id, &lease, "exec", 60_000, 0)
            .expect("lease");
        let guard = LeaseGuard {
            attempt_no: AttemptNo::FIRST,
            lease_token: lease.clone(),
        };
        let started = node_event(
            f.mission.mission_id,
            Some(f.run.run_id),
            f.node.node_id,
            f.session.session_id,
            AttemptNo::FIRST,
            EventKind::NodeStarted,
            json!({}),
            Some(TraceId::new()),
        );
        f.store
            .transition_node_cas(f.node.node_id, &guard, NodeState::Running, &started, 1)
            .expect("running");

        let arguments = json!({"path": "approved.txt", "content": "approved"});
        let (parked, approval) = {
            let mut gate = StoreApprovalGate::new(&mut f.store, StoreApprovalGate::DEFAULT_TIMEOUT);
            gate.request(
                &ApprovalRequest {
                    mission_id: f.mission.mission_id,
                    run_id: Some(f.run.run_id),
                    node_id: f.node.node_id,
                    session_id: f.session.session_id,
                    attempt_no: AttemptNo::FIRST,
                    lease_token: lease.clone(),
                    policy_version: "v1",
                    tool_name: "fs.write",
                    tool_version: 1,
                    arguments: &arguments,
                },
                &guard,
                2,
            )
            .expect("request")
        };
        assert_eq!(parked.state, NodeState::WaitingApproval);
        f.store
            .decide_approval(approval.approval_id, ApprovalDecision::Approved, "user", 3)
            .expect("approve");

        {
            let mut gate = StoreApprovalGate::new(&mut f.store, StoreApprovalGate::DEFAULT_TIMEOUT);
            let consumed = gate
                .consume(
                    approval.approval_id,
                    approval.tool_call_id,
                    &approval.request_hash,
                    approval.attempt_no,
                    &approval.lease_token,
                    4,
                )
                .expect("consume");
            assert_eq!(consumed.consumed_at_ms, Some(4));
            let replay = gate
                .consume(
                    approval.approval_id,
                    approval.tool_call_id,
                    &approval.request_hash,
                    approval.attempt_no,
                    &approval.lease_token,
                    5,
                )
                .expect_err("replay");
            assert!(matches!(
                replay,
                AgentError::Store(hive_store::StoreError::ApprovalReplayed(_))
            ));
        }

        let resumed = node_event(
            f.mission.mission_id,
            Some(f.run.run_id),
            f.node.node_id,
            f.session.session_id,
            AttemptNo::FIRST,
            EventKind::NodeApprovalDecided,
            json!({"approval_id": approval.approval_id, "decision": "approved"}),
            Some(TraceId::new()),
        );
        f.store
            .transition_node_cas(f.node.node_id, &guard, NodeState::Running, &resumed, 6)
            .expect("resume");

        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let context = ToolContext {
            mission_id: f.mission.mission_id,
            run_id: Some(f.run.run_id),
            node_id: f.node.node_id,
            session_id: Some(f.session.session_id),
            attempt_no: AttemptNo::FIRST,
            lease_token: lease,
            policy_version: "v1".to_owned(),
        };
        let outcome = {
            let mut audit = GuardedAuditSink {
                store: &mut f.store,
                node_id: f.node.node_id,
                guard: guard.clone(),
                budget_exceeded: None,
            };
            let mut approvals = AllowAllApprovals;
            dispatcher
                .dispatch(
                    &context,
                    ToolCall::new("fs.write", arguments),
                    &mut audit,
                    &mut approvals,
                )
                .expect("execute")
        };
        assert_eq!(outcome.output.bytes, 8);
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("approved.txt")).expect("file"),
            "approved"
        );
    }

    #[test]
    fn external_lease_runs_without_second_lease_and_persists_handoff() {
        let mut f = fixture("external-lease");
        let external = LeaseToken::generate();
        let leased = f
            .store
            .lease_node(f.node.node_id, &external, "scheduler", 60_000, 0)
            .expect("lease");
        assert_eq!(leased.lease_owner.as_deref(), Some("scheduler"));
        let guard = LeaseGuard {
            attempt_no: leased.attempt_no,
            lease_token: external,
        };

        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion("delivered", None),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node_with_lease(
                    f.session.session_id,
                    f.node.node_id,
                    &guard,
                    &spec(),
                    &token,
                )
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));

        assert_eq!(
            kinds(&f.store, f.mission.mission_id),
            vec![
                EventKind::NodeStarted,
                EventKind::NodeTurn,
                EventKind::HandoffCreated,
                EventKind::NodeSucceeded
            ]
        );

        let handoffs = f
            .store
            .list_handoffs(f.mission.mission_id)
            .expect("handoffs");
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].node_id, f.node.node_id);

        let succeeded = f
            .store
            .read_all_events()
            .expect("events")
            .into_iter()
            .find(|event| event.kind == EventKind::NodeSucceeded)
            .expect("succeeded");
        assert_eq!(
            succeeded.payload["handoff_id"],
            handoffs[0].handoff_id.to_string()
        );
    }

    #[test]
    fn handoff_with_workspace_records_head_revision() {
        let mut f = fixture("handoff-workspace");
        let sha = init_git_repo(f.dir.path());
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion("delivered", None),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        let with_root = spec().with_workspace_root(f.dir.path().to_path_buf());

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &with_root, &token)
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));

        let handoffs = f
            .store
            .list_handoffs(f.mission.mission_id)
            .expect("handoffs");
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].base_revision, sha);
        assert!(handoffs[0]
            .base_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
        assert!(matches!(handoffs[0].base_revision.len(), 40 | 64));
        assert!(handoffs[0].risks.is_empty());
    }

    #[test]
    fn handoff_without_workspace_keeps_unknown_base_revision() {
        let mut f = fixture("handoff-no-workspace");
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion("delivered", None),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));

        let handoffs = f
            .store
            .list_handoffs(f.mission.mission_id)
            .expect("handoffs");
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].base_revision, "unknown");
        assert!(handoffs[0].risks.is_empty());
    }

    #[test]
    fn handoff_with_bad_workspace_path_fails_open_and_node_succeeds() {
        let mut f = fixture("handoff-bad-workspace");
        let bad = f.dir.path().join("not-a-repo");
        std::fs::create_dir_all(&bad).expect("bad dir");
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion("delivered", None),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();
        let bad_spec = spec().with_workspace_root(bad);

        let outcome = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &bad_spec, &token)
                .expect("run")
        };
        assert!(matches!(outcome, AgentOutcome::Completed { .. }));

        let node = f
            .store
            .get_node(f.node.node_id)
            .expect("node")
            .expect("present");
        assert_eq!(node.state, NodeState::Succeeded);

        let handoffs = f
            .store
            .list_handoffs(f.mission.mission_id)
            .expect("handoffs");
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].base_revision, "unknown");
        assert_eq!(handoffs[0].risks.len(), 1);
        assert!(handoffs[0].risks[0].contains("base_revision unavailable"));
    }

    #[test]
    fn external_lease_mismatch_returns_stale_execution() {
        let mut f = fixture("stale-external-lease");
        let external = LeaseToken::generate();
        f.store
            .lease_node(f.node.node_id, &external, "scheduler", 60_000, 0)
            .expect("lease");
        let guard = LeaseGuard {
            attempt_no: AttemptNo::FIRST,
            lease_token: LeaseToken::generate(),
        };

        let transport = MockTransport::default();
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let error = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node_with_lease(
                    f.session.session_id,
                    f.node.node_id,
                    &guard,
                    &spec(),
                    &token,
                )
                .expect_err("stale lease")
        };
        assert!(matches!(
            error,
            AgentError::Store(hive_store::StoreError::StaleExecution)
        ));
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Leased
        );
    }

    #[test]
    fn token_budget_exceeded_fails_node_without_retry() {
        let mut f = fixture("budget-tokens");
        f.store
            .set_budget(
                "node",
                &f.node.node_id.to_string(),
                BudgetDimension::Tokens,
                None,
                BudgetLimits {
                    hard_limit: Some(10),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        let transport = MockTransport::new(vec![MockResponse::json(
            200,
            &completion(
                "way too long",
                Some(json!({"prompt_tokens": 15, "completion_tokens": 5, "total_tokens": 20})),
            ),
        )]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let error = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect_err("budget")
        };
        assert!(matches!(
            error,
            AgentError::BudgetExceeded { dimension } if dimension == "tokens"
        ));
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Failed
        );
        assert_eq!(
            f.store
                .get_agent_session(f.session.session_id)
                .expect("session")
                .expect("present")
                .state,
            AgentSessionState::Failed
        );
        let events = f.store.read_all_events().expect("events");
        let exceeded = events
            .iter()
            .find(|event| event.kind == EventKind::NodeBudgetExceeded)
            .expect("budget_exceeded event");
        assert_eq!(exceeded.payload["dimension"], "tokens");
        assert_eq!(exceeded.payload["hard_limit"], 10);
        assert!(!events
            .iter()
            .any(|event| event.kind == EventKind::NodeReady));
    }

    #[test]
    fn tool_call_budget_exceeded_fails_before_side_effect_replay() {
        let mut f = fixture("budget-tools");
        f.store
            .set_budget(
                "node",
                &f.node.node_id.to_string(),
                BudgetDimension::ToolCalls,
                None,
                BudgetLimits {
                    hard_limit: Some(1),
                    soft_limit: None,
                },
                0,
            )
            .expect("budget");
        std::fs::write(f.dir.path().join("note.txt"), b"x").expect("note");
        let transport = MockTransport::new(vec![
            MockResponse::json(
                200,
                &tool_completion("fs.read", json!({"path": "note.txt"})),
            ),
            MockResponse::json(
                200,
                &tool_completion("fs.read", json!({"path": "note.txt"})),
            ),
        ]);
        let gateway = Gateway::new(transport);
        let dispatcher = ToolDispatcher::with_default_root(f.dir.path()).expect("dispatcher");
        let token = CancellationToken::new();

        let error = {
            let mut runner = AgentRunner::new(&mut f.store, &dispatcher, &gateway);
            runner
                .run_node(f.session.session_id, f.node.node_id, &spec(), &token)
                .expect_err("budget")
        };
        assert!(matches!(
            error,
            AgentError::BudgetExceeded { dimension } if dimension == "tool_calls"
        ));
        assert_eq!(
            f.store
                .get_node(f.node.node_id)
                .expect("node")
                .expect("present")
                .state,
            NodeState::Failed
        );
        let events = f.store.read_all_events().expect("events");
        let exceeded = events
            .iter()
            .find(|event| event.kind == EventKind::NodeBudgetExceeded)
            .expect("budget_exceeded event");
        assert_eq!(exceeded.payload["dimension"], "tool_calls");
        assert_eq!(exceeded.payload["hard_limit"], 1);
        let started = events
            .iter()
            .filter(|event| event.kind == EventKind::NodeToolStarted)
            .count();
        assert_eq!(
            started, 1,
            "second tool intent must not start a side effect"
        );
        let states = f
            .store
            .side_effect_states(f.node.node_id, AttemptNo::FIRST)
            .expect("side effects");
        assert_eq!(states, vec![SideEffectState::Completed]);
    }
}
