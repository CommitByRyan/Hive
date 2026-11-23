use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use hive_store::{
    ApprovalDecision, ApprovalRecord, ArtifactRecord, MissionListRecord, MissionRecord, NodeRecord,
    RunRecord,
};
use hive_types::{
    observe_mission, ApprovalId, ApprovalState, ArtifactId, EventId, EventInput, EventKind,
    MissionId, NodeState,
};

use crate::error::ApiError;
use crate::state::AppState;
use crate::{json_response, MAX_BODY_BYTES};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyQuery {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionsQuery {
    limit: Option<u64>,
    offset: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    mission_id: Option<String>,
    after_seq: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissionDetailQuery {
    events_after_seq: Option<u64>,
    events_limit: Option<u64>,
    runs_limit: Option<u64>,
    nodes_limit: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactQuery {
    download: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateMissionBody {
    title: Option<String>,
    goal: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageBody {
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecideBody {
    decision: String,
    note: Option<String>,
}

pub async fn health(State(state): State<AppState>) -> Result<Response, ApiError> {
    let schema_version = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        store.schema_version().map_err(ApiError::store)?
    };
    Ok(json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "schema_version": schema_version,
            "event_schema_version": hive_types::EVENT_SCHEMA_VERSION,
            "mode": state.mode,
            "model_catalog_ready": !state.catalog.is_empty(),
            "authentication_required": state.authentication_required(),
            "approver_identity": "unauthenticated_local_console",
        }),
    ))
}

pub async fn state(
    State(state): State<AppState>,
    query: Result<Query<EmptyQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    parse_query(query)?;
    let snapshot = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        store.state_snapshot().map_err(ApiError::store)?
    };
    let missions: Vec<Value> = snapshot
        .missions
        .iter()
        .map(mission_list_json)
        .collect::<Result<_, _>>()?;
    let approvals: Vec<Value> = snapshot
        .approvals
        .iter()
        .map(approval_json)
        .collect::<Result<_, _>>()?;
    Ok(json_response(
        StatusCode::OK,
        json!({
            "schema_version": hive_store::SCHEMA_VERSION,
            "event_schema_version": hive_types::EVENT_SCHEMA_VERSION,
            "last_seq": snapshot.last_seq,
            "config": config_json(&state),
            "missions": missions,
            "approvals": approvals,
            "stats": {
                "missions": snapshot.mission_count,
                "runs": snapshot.run_count,
                "nodes": snapshot.node_count,
                "events": snapshot.event_count,
                "pending_approvals": snapshot.pending_approval_count,
            },
        }),
    ))
}

pub async fn list_missions(
    State(state): State<AppState>,
    query: Result<Query<MissionsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let params = parse_query(query)?;
    let limit = params.limit.unwrap_or(60).clamp(1, 200);
    let offset = params.offset.unwrap_or(0);
    let missions = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        store
            .list_missions(limit, offset)
            .map_err(ApiError::store)?
    };
    let latest = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        store.latest_seq().map_err(ApiError::store)?
    };
    let missions: Vec<Value> = missions
        .iter()
        .map(mission_list_json)
        .collect::<Result<_, _>>()?;
    Ok(json_response(
        StatusCode::OK,
        json!({ "missions": missions, "last_seq": latest }),
    ))
}

pub async fn create_mission(
    State(state): State<AppState>,
    body: Body,
) -> Result<Response, ApiError> {
    let payload: CreateMissionBody = json_body(body).await?;
    let goal = payload.goal.trim();
    if goal.is_empty() || goal.len() > 16 * 1024 {
        return Err(ApiError::bad_request(
            "invalid_request",
            "goal must be 1..=16 KiB after trim",
        ));
    }
    let title = payload.title.unwrap_or_default().trim().to_owned();
    if title.len() > 16 * 1024 {
        return Err(ApiError::bad_request(
            "invalid_request",
            "title must be <= 16 KiB",
        ));
    }
    let (mission, envelope) = {
        let mut store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        let now = hive_store::now_ms();
        let mission = store
            .create_mission(&title, goal, now)
            .map_err(ApiError::store)?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::MissionCreated,
            mission_id: mission.mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({ "title": title, "goal": goal }),
        };
        let envelope = store.append_event(&event).map_err(ApiError::store)?;
        (mission, envelope)
    };
    state.broadcast(&envelope);
    let observation = observe_mission(mission.state, false, false);
    let mut mission_value = mission_json(&mission, observation, Some(envelope.seq), 0)?;
    mission_value["last_seq"] = json!(envelope.seq);
    Ok(json_response(
        StatusCode::CREATED,
        json!({ "mission": mission_value, "last_seq": envelope.seq }),
    ))
}

pub async fn get_mission(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<MissionDetailQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let mission_id = parse_mission_id(&id)?;
    let params = parse_query(query)?;
    let events_after_seq = params.events_after_seq.unwrap_or(0);
    let events_limit = params.events_limit.unwrap_or(500).clamp(1, 2000);
    let runs_limit = params.runs_limit.unwrap_or(1000).max(1);
    let nodes_limit = params.nodes_limit.unwrap_or(2000).max(1);
    let snapshot = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        store
            .mission_snapshot(
                mission_id,
                events_after_seq,
                events_limit,
                runs_limit,
                nodes_limit,
            )
            .map_err(ApiError::store)?
            .ok_or_else(|| ApiError::not_found("not_found", format!("mission {mission_id}")))?
    };
    let runnable = snapshot.nodes.iter().any(node_runnable);
    let pending = snapshot
        .approvals
        .iter()
        .any(|approval| approval.state == ApprovalState::Pending);
    let observation = observe_mission(snapshot.mission.state, runnable, pending);
    let mission = mission_json(
        &snapshot.mission,
        observation,
        snapshot.last_seq,
        snapshot
            .approvals
            .iter()
            .filter(|approval| approval.state == ApprovalState::Pending)
            .count() as u64,
    )?;
    let runs: Vec<Value> = snapshot.runs.iter().map(run_json).collect();
    let nodes: Vec<Value> = snapshot.nodes.iter().map(node_json).collect();
    let approvals: Vec<Value> = snapshot
        .approvals
        .iter()
        .map(approval_json)
        .collect::<Result<_, _>>()?;
    let artifacts: Vec<Value> = snapshot.artifacts.iter().map(artifact_meta_json).collect();
    let messages: Vec<Value> = snapshot.messages.iter().map(envelope_json).collect();
    let events: Vec<Value> = snapshot.events.iter().map(envelope_json).collect();
    Ok(json_response(
        StatusCode::OK,
        json!({
            "mission": mission,
            "runs": runs,
            "nodes": nodes,
            "events": events,
            "artifacts": artifacts,
            "messages": messages,
            "approvals": approvals,
            "last_seq": snapshot.last_seq,
        }),
    ))
}

pub async fn post_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Body,
) -> Result<Response, ApiError> {
    let mission_id = parse_mission_id(&id)?;
    let payload: MessageBody = json_body(body).await?;
    let content = payload.content.trim();
    if content.is_empty() || content.len() > 16 * 1024 {
        return Err(ApiError::bad_request(
            "invalid_request",
            "content must be 1..=16 KiB after trim",
        ));
    }
    let (mission, envelope) = {
        let mut store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        let mission = store
            .get_mission(mission_id)
            .map_err(ApiError::store)?
            .ok_or_else(|| ApiError::not_found("not_found", format!("mission {mission_id}")))?;
        if mission.state.is_terminal() {
            return Err(ApiError::conflict(
                "mission_not_accepting_messages",
                "terminal missions do not accept messages",
            ));
        }
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::MessageUser,
            mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({ "content": content }),
        };
        let envelope = store.append_event(&event).map_err(ApiError::store)?;
        (mission, envelope)
    };
    state.broadcast(&envelope);
    let observation = observe_mission(mission.state, false, false);
    let mut mission_value = mission_json(&mission, observation, Some(envelope.seq), 0)?;
    mission_value["last_seq"] = json!(envelope.seq);
    Ok(json_response(
        StatusCode::CREATED,
        json!({
            "message": {
                "mission_id": mission_id.to_string(),
                "content": content,
                "seq": envelope.seq,
            },
            "mission": mission_value,
            "last_seq": envelope.seq,
        }),
    ))
}

pub async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !accept.contains("text/event-stream") {
        return Err(ApiError::new(
            StatusCode::NOT_ACCEPTABLE,
            "unsupported_media_type",
            "Accept must include text/event-stream",
        ));
    }
    let params = parse_query(query)?;
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok());
    let after_seq = match (last_event_id, params.after_seq) {
        (Some(header_value), Some(query_value)) => {
            let header_seq = parse_cursor(header_value)?;
            let query_seq = parse_cursor(&query_value)?;
            if header_seq != query_seq {
                return Err(ApiError::bad_request(
                    "invalid_cursor",
                    "Last-Event-ID and after_seq disagree",
                ));
            }
            header_seq
        }
        (Some(header_value), None) => parse_cursor(header_value)?,
        (None, Some(query_value)) => parse_cursor(&query_value)?,
        (None, None) => 0,
    };
    let mission_id = match params.mission_id {
        Some(value) => Some(parse_mission_id(&value)?),
        None => None,
    };

    let (backlog, latest) = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        if let Some(mission_id) = mission_id {
            if store
                .get_mission(mission_id)
                .map_err(ApiError::store)?
                .is_none()
            {
                return Err(ApiError::not_found(
                    "mission_not_found",
                    format!("mission {mission_id}"),
                ));
            }
        }
        store
            .read_events_for_sse(mission_id, after_seq)
            .map_err(ApiError::store)?
    };
    let latest_seq = latest.unwrap_or(0);
    if after_seq > latest_seq {
        return Err(ApiError::conflict(
            "sequence_ahead",
            "local cursor is ahead of server sequence",
        ));
    }
    state
        .latest_seq
        .store(latest_seq, std::sync::atomic::Ordering::Relaxed);

    let rx = state.events_tx.subscribe();
    let body_stream = crate::sse::sse_stream(backlog, rx, state.latest_seq.clone());
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from_stream(body_stream))
        .expect("valid sse response");
    Ok(response)
}

pub async fn decide_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Body,
) -> Result<Response, ApiError> {
    let approval_id = parse_approval_id(&id)?;
    let payload: DecideBody = json_body(body).await?;
    let decision = match payload.decision.as_str() {
        "approve" => ApprovalDecision::Approved,
        "reject" => ApprovalDecision::Rejected,
        _ => {
            return Err(ApiError::bad_request(
                "invalid_request",
                "decision must be approve or reject",
            ))
        }
    };
    let note = payload
        .note
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    if note.len() > 4 * 1024 {
        return Err(ApiError::bad_request(
            "invalid_request",
            "note must be <= 4 KiB",
        ));
    }

    let (approval, node, envelope) = {
        let mut store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        let current = store
            .get_approval(approval_id)
            .map_err(ApiError::store)?
            .ok_or_else(|| ApiError::not_found("not_found", format!("approval {approval_id}")))?;
        if current.state != ApprovalState::Pending {
            return Err(ApiError::bad_request(
                "approval_not_pending",
                "approval is not pending",
            ));
        }
        let now = hive_store::now_ms();
        let decided = match store.decide_approval(approval_id, decision, "local_console", now) {
            Ok(record) => record,
            Err(hive_store::StoreError::ApprovalConflict(message))
                if message.contains("expired") =>
            {
                return Err(ApiError::bad_request(
                    "approval_expired",
                    "approval expired before decision",
                ));
            }
            Err(error) => return Err(ApiError::store(error)),
        };
        let node = store.get_node(decided.node_id).map_err(ApiError::store)?;
        let event = EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeApprovalDecided,
            mission_id: decided.mission_id,
            run_id: decided.run_id,
            node_id: Some(decided.node_id),
            session_id: decided.session_id,
            attempt_no: Some(decided.attempt_no),
            trace_id: None,
            payload: json!({
                "approval_id": decided.approval_id.to_string(),
                "decision": decided.state.as_str(),
                "note": note,
            }),
        };
        let envelope = store.append_event(&event).map_err(ApiError::store)?;
        (decided, node, envelope)
    };
    state.broadcast(&envelope);
    Ok(json_response(
        StatusCode::OK,
        json!({
            "approval": approval_json(&approval)?,
            "node": node.as_ref().map(node_json).unwrap_or(Value::Null),
            "last_seq": envelope.seq,
        }),
    ))
}

pub async fn get_artifact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<ArtifactQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let artifact_id = parse_artifact_id(&id)?;
    let params = parse_query(query)?;
    let artifact = {
        let store = state.store.lock().map_err(|_| ApiError::store_lock())?;
        store
            .get_artifact(artifact_id)
            .map_err(ApiError::store)?
            .ok_or_else(|| {
                ApiError::not_found("artifact_not_found", format!("artifact {artifact_id}"))
            })?
    };
    if params
        .download
        .as_deref()
        .is_some_and(|value| value == "1" || value == "true")
    {
        return artifact_download(&state, &artifact);
    }
    Ok(json_response(StatusCode::OK, artifact_meta_json(&artifact)))
}

pub async fn list_routines(State(state): State<AppState>) -> Result<Response, ApiError> {
    let _ = state;
    Ok(json_response(StatusCode::OK, json!({ "routines": [] })))
}

pub async fn create_routine() -> Result<Response, ApiError> {
    Err(ApiError::bad_request(
        "not_available_before_h09",
        "routines are reserved for H09",
    ))
}

fn config_json(state: &AppState) -> Value {
    json!({
        "mode": state.mode,
        "host": state.host,
        "port": state.port,
        "authentication_required": state.authentication_required(),
        "model_catalog_ready": !state.catalog.is_empty(),
        "approver_identity": "unauthenticated_local_console",
    })
}

fn mission_list_json(item: &MissionListRecord) -> Result<Value, ApiError> {
    let runnable = false;
    let pending = item.pending_approvals > 0;
    let observation = observe_mission(item.mission.state, runnable, pending);
    let mut value = mission_json(
        &item.mission,
        observation,
        item.last_seq,
        item.pending_approvals,
    )?;
    value["pending_approvals"] = json!(item.pending_approvals);
    Ok(value)
}

fn mission_json(
    mission: &MissionRecord,
    observation: hive_types::MissionObservation,
    last_seq: Option<u64>,
    pending_approvals: u64,
) -> Result<Value, ApiError> {
    Ok(json!({
        "id": mission.mission_id.to_string(),
        "state": mission.state.as_str(),
        "observation": observation.as_str(),
        "title": mission.title,
        "goal": mission.goal,
        "created_at": hive_store::rfc3339_from_ms(mission.created_at_ms),
        "updated_at": hive_store::rfc3339_from_ms(mission.updated_at_ms),
        "last_seq": last_seq,
        "pending_approvals": pending_approvals,
    }))
}

fn run_json(run: &RunRecord) -> Value {
    json!({
        "id": run.run_id.to_string(),
        "mission_id": run.mission_id.to_string(),
        "state": run.state.as_str(),
        "created_at": hive_store::rfc3339_from_ms(run.created_at_ms),
        "updated_at": hive_store::rfc3339_from_ms(run.updated_at_ms),
    })
}

fn node_json(node: &NodeRecord) -> Value {
    json!({
        "id": node.node_id.to_string(),
        "mission_id": node.mission_id.to_string(),
        "run_id": node.run_id.to_string(),
        "state": node.state.as_str(),
        "attempt_no": node.attempt_no.get(),
        "retry_budget": node.retry_budget,
        "lease_owner": node.lease_owner,
        "created_at": hive_store::rfc3339_from_ms(node.created_at_ms),
        "updated_at": hive_store::rfc3339_from_ms(node.updated_at_ms),
    })
}

fn approval_json(approval: &ApprovalRecord) -> Result<Value, ApiError> {
    Ok(json!({
        "id": approval.approval_id.to_string(),
        "mission_id": approval.mission_id.to_string(),
        "run_id": approval.run_id.map(|id| id.to_string()),
        "node_id": approval.node_id.to_string(),
        "session_id": approval.session_id.map(|id| id.to_string()),
        "tool_call_id": approval.tool_call_id.to_string(),
        "tool_name": approval.tool_name,
        "tool_version": approval.tool_version,
        "request_hash": approval.request_hash,
        "request_hash_prefix": approval.request_hash.chars().take(12).collect::<String>(),
        "arguments_preview": approval.arguments_preview,
        "policy_version": approval.policy_version,
        "attempt_no": approval.attempt_no.get(),
        "state": approval.state.as_str(),
        "expires_at": hive_store::rfc3339_from_ms(approval.expires_at_ms),
        "created_at": hive_store::rfc3339_from_ms(approval.created_at_ms),
        "decided_at": approval.decided_at_ms.map(hive_store::rfc3339_from_ms),
        "decided_by": approval.decided_by,
        "decision_note": approval.decision_note,
    }))
}

fn artifact_meta_json(artifact: &ArtifactRecord) -> Value {
    json!({
        "id": artifact.artifact_id.to_string(),
        "mission_id": artifact.mission_id.to_string(),
        "node_id": artifact.node_id.map(|id| id.to_string()),
        "tool_call_id": artifact.tool_call_id.map(|id| id.to_string()),
        "path": artifact.path,
        "sha256": artifact.sha256,
        "size_bytes": artifact.size_bytes,
        "created_at": hive_store::rfc3339_from_ms(artifact.created_at_ms),
    })
}

fn envelope_json(envelope: &hive_types::EventEnvelope) -> Value {
    serde_json::to_value(envelope).unwrap_or(Value::Null)
}

fn node_runnable(node: &NodeRecord) -> bool {
    matches!(
        node.state,
        NodeState::Pending
            | NodeState::Ready
            | NodeState::Leased
            | NodeState::Running
            | NodeState::RetryReady
    )
}

fn parse_mission_id(value: &str) -> Result<MissionId, ApiError> {
    MissionId::parse(value)
        .map_err(|_| ApiError::bad_request("invalid_id", format!("invalid mission id `{value}`")))
}

fn parse_approval_id(value: &str) -> Result<ApprovalId, ApiError> {
    ApprovalId::parse(value)
        .map_err(|_| ApiError::bad_request("invalid_id", format!("invalid approval id `{value}`")))
}

fn parse_artifact_id(value: &str) -> Result<ArtifactId, ApiError> {
    ArtifactId::parse(value)
        .map_err(|_| ApiError::bad_request("invalid_id", format!("invalid artifact id `{value}`")))
}

fn parse_cursor(value: &str) -> Result<u64, ApiError> {
    let seq: u64 = value
        .parse()
        .map_err(|_| ApiError::bad_request("invalid_seq", "cursor must be an integer"))?;
    if seq > hive_store::MAX_SEQ {
        return Err(ApiError::bad_request(
            "invalid_seq",
            "cursor exceeds 2^53-1",
        ));
    }
    Ok(seq)
}

fn parse_query<T: DeserializeOwned>(
    query: Result<Query<T>, QueryRejection>,
) -> Result<T, ApiError> {
    match query {
        Ok(query) => Ok(query.0),
        Err(error) => {
            let text = error.to_string();
            let code = if text.contains("unknown field") {
                "unknown_parameter"
            } else {
                "invalid_request"
            };
            Err(ApiError::bad_request(code, text))
        }
    }
}

async fn json_body<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "body exceeds 64 KiB",
            )
        })?;
    serde_json::from_slice::<T>(&bytes)
        .map_err(|_| ApiError::bad_request("invalid_request", "request body is not valid JSON"))
}

fn artifact_download(_state: &AppState, artifact: &ArtifactRecord) -> Result<Response, ApiError> {
    let root = match std::fs::canonicalize(std::env::current_dir().unwrap_or_default()) {
        Ok(root) => root,
        Err(_) => {
            return Err(ApiError::not_found(
                "artifact_missing",
                "cannot resolve workspace root",
            ))
        }
    };
    let candidate = root.join(&artifact.path);
    let canonical = match std::fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(_) => {
            return Err(ApiError::new(
                StatusCode::GONE,
                "artifact_missing",
                "artifact file is missing",
            ))
        }
    };
    if !canonical.starts_with(&root) {
        return Err(ApiError::not_found(
            "artifact_not_found",
            "artifact path escapes workspace root",
        ));
    }
    let metadata = match std::fs::metadata(&canonical) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => {
            return Err(ApiError::not_found(
                "artifact_missing",
                "artifact is not a regular file",
            ))
        }
    };
    let bytes = match std::fs::read(&canonical) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(ApiError::not_found(
                "artifact_missing",
                "artifact file could not be read",
            ))
        }
    };
    let _ = metadata;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", artifact.path),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .expect("valid artifact response"))
}
