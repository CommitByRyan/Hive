use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use hive_llm::{ModelCatalog, ModelEntry};
use hive_server::{build_router, AppState};
use hive_store::Store;
use serde_json::{json, Value};
use tower::ServiceExt;

fn catalog() -> ModelCatalog {
    ModelCatalog::from_entries(vec![ModelEntry {
        id: "mock-model".to_owned(),
        ..ModelEntry::default()
    }])
}

fn app_with_catalog(catalog: ModelCatalog) -> Router {
    let store = Store::open_in_memory().expect("store");
    let state = AppState::new(
        store,
        catalog,
        "demo".to_owned(),
        "127.0.0.1".to_owned(),
        8765,
    );
    build_router(state)
}

fn app() -> Router {
    app_with_catalog(catalog())
}

async fn json_body(response: axum::response::Response) -> Value {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| {
        panic!(
            "expected JSON body, status {status}: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn delete(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn health_reports_schema_version_without_secrets() {
    let response = app().oneshot(get("/api/health")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["schema_version"], hive_store::SCHEMA_VERSION);
    assert_eq!(body["event_schema_version"], 1);
    assert!(!body.to_string().contains("api"));
}

#[tokio::test]
async fn health_endpoints_add_status_checks_and_live_ready() {
    let app = app();
    let response = app.clone().oneshot(get("/api/health")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["checks"]["database"], "ok");
    assert!(body["retained_since_seq"].as_u64().is_some());
    assert!(body["budget"]["exceeded_total"].is_object());

    let live = app.clone().oneshot(get("/api/health/live")).await.unwrap();
    assert_eq!(live.status(), StatusCode::OK);
    assert_eq!(json_body(live).await["status"], "live");

    let ready = app.oneshot(get("/api/health/ready")).await.unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    assert_eq!(json_body(ready).await["status"], "ready");
}

#[tokio::test]
async fn ready_fails_without_model_catalog() {
    let app = app_with_catalog(ModelCatalog::new());
    let response = app.oneshot(get("/api/health/ready")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn health_stays_200_but_degraded_with_pending_approval() {
    use hive_store::ApprovalInput;
    use hive_types::{AttemptNo, LeaseToken, ToolCallId};

    let store = Store::open_in_memory().expect("store");
    let state = AppState::new(
        store,
        catalog(),
        "demo".to_owned(),
        "127.0.0.1".to_owned(),
        8765,
    );
    {
        let mut store = state.store.lock().unwrap();
        let mission = store.create_mission("m", "g", 0).expect("mission");
        let run = store.create_run(mission.mission_id, 0).expect("run");
        let node = store
            .add_node(mission.mission_id, run.run_id, 3, 0)
            .expect("node");
        store
            .create_approval(&ApprovalInput {
                mission_id: mission.mission_id,
                run_id: Some(run.run_id),
                node_id: node.node_id,
                session_id: None,
                tool_call_id: ToolCallId::new(),
                tool_name: "fs.write".to_owned(),
                tool_version: 1,
                request_hash: "deadbeef".to_owned(),
                arguments_preview: "{}".to_owned(),
                policy_version: "v1".to_owned(),
                attempt_no: AttemptNo::FIRST,
                lease_token: LeaseToken::generate(),
                expires_at_ms: hive_store::now_ms() + 60_000,
            })
            .expect("approval");
    }
    let app = build_router(state);
    let response = app.oneshot(get("/api/health")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["approvals"], "degraded");
}

#[tokio::test]
async fn archived_mission_forces_snapshot_for_old_cursors() {
    use hive_store::Store;
    use hive_types::{EventId, EventInput, EventKind, MissionState};

    let mut store = Store::open_in_memory().expect("store");
    let mission = store.create_mission("m", "g", 0).expect("mission");
    let run = store.create_run(mission.mission_id, 0).expect("run");
    store
        .add_node(mission.mission_id, run.run_id, 3, 0)
        .expect("node");
    for kind in [EventKind::MissionCreated, EventKind::MissionCancelled] {
        store
            .append_event(&EventInput {
                event_id: EventId::new(),
                kind,
                mission_id: mission.mission_id,
                run_id: None,
                node_id: None,
                session_id: None,
                attempt_no: None,
                trace_id: None,
                payload: json!({}),
            })
            .expect("event");
    }
    store
        .transition_mission(mission.mission_id, MissionState::Cancelled, 2)
        .expect("terminal");

    let archive_dir =
        std::env::temp_dir().join(format!("hive-server-archive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&archive_dir);
    std::fs::create_dir_all(&archive_dir).expect("archive dir");
    let report = store
        .archive_mission(&archive_dir, mission.mission_id, 3)
        .expect("archive");
    assert_eq!(report.retained_since_seq, 3);

    let state = AppState::new(
        store,
        catalog(),
        "demo".to_owned(),
        "127.0.0.1".to_owned(),
        8765,
    );
    let app = build_router(state);
    for after_seq in [0, 2] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/events?after_seq={after_seq}"))
                    .header(header::ACCEPT, "text/event-stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "after_seq {after_seq}"
        );
        let body = json_body(response).await;
        assert_eq!(body["snapshot_required"], true);
        assert_eq!(body["retained_since_seq"], 3);
        assert_eq!(body["after_seq"], after_seq);
    }

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/events?after_seq=3")
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = std::fs::remove_dir_all(&archive_dir);
}

#[tokio::test]
async fn create_mission_validates_goal_and_json() {
    let empty = post_json("/api/missions", json!({ "goal": "   " }));
    let response = app().oneshot(empty).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "invalid_request"
    );

    let bad = Request::builder()
        .method("POST")
        .uri("/api/missions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let response = app().oneshot(bad).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let created = post_json("/api/missions", json!({ "goal": "ship it" }));
    let response = app().oneshot(created).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_body(response).await;
    assert_eq!(body["mission"]["state"], "queued");
    assert!(body["last_seq"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn events_rejects_invalid_cursor_and_sequence_ahead() {
    let app = app();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?after_seq=-1")
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["error"]["code"], "invalid_seq");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?after_seq=999")
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["error"]["code"], "sequence_ahead");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events?after_seq=2")
                .header(header::ACCEPT, "text/event-stream")
                .header("last-event-id", "3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["error"]["code"], "invalid_cursor");
}

#[tokio::test]
async fn events_requires_event_stream_accept() {
    let response = app().oneshot(get("/api/events")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "unsupported_media_type"
    );
}

#[tokio::test]
async fn write_body_over_64kib_returns_413() {
    let body = vec![b' '; 65 * 1024];
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/missions")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "payload_too_large"
    );
}

#[tokio::test]
async fn malicious_origin_is_rejected() {
    for path in ["/api/state", "/api/events", "/api/missions"] {
        let request = Request::builder()
            .uri(path)
            .header(header::ORIGIN, "https://evil.example")
            .header(header::ACCEPT, "text/event-stream")
            .body(Body::empty())
            .unwrap();
        let response = app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "path {path}");
        assert_eq!(
            json_body(response).await["error"]["code"],
            "origin_forbidden"
        );
    }
}

#[tokio::test]
async fn unknown_query_parameter_is_rejected() {
    let response = app().oneshot(get("/api/missions?foo=1")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "unknown_parameter"
    );
}

#[tokio::test]
async fn concurrent_approval_decisions_have_single_winner() {
    use hive_store::ApprovalInput;
    use hive_types::{AttemptNo, LeaseToken, ToolCallId};

    let store = Store::open_in_memory().expect("store");
    let state = AppState::new(
        store,
        ModelCatalog::new(),
        "demo".to_owned(),
        "127.0.0.1".to_owned(),
        8765,
    );
    let approval_id;
    {
        let mut store = state.store.lock().unwrap();
        let m = store.create_mission("m", "g", 0).expect("mission");
        let r = store.create_run(m.mission_id, 0).expect("run");
        let n = store.add_node(m.mission_id, r.run_id, 3, 0).expect("node");
        let approval = store
            .create_approval(&ApprovalInput {
                mission_id: m.mission_id,
                run_id: Some(r.run_id),
                node_id: n.node_id,
                session_id: None,
                tool_call_id: ToolCallId::new(),
                tool_name: "fs.write".to_owned(),
                tool_version: 1,
                request_hash: "deadbeef".to_owned(),
                arguments_preview: "{}".to_owned(),
                policy_version: "v1".to_owned(),
                attempt_no: AttemptNo::FIRST,
                lease_token: LeaseToken::generate(),
                expires_at_ms: hive_store::now_ms() + 60_000,
            })
            .expect("approval");
        approval_id = approval.approval_id;
    }

    let app = build_router(state);
    let mut handles = Vec::new();
    for _ in 0..10 {
        let app = app.clone();
        let approval_id = approval_id.to_string();
        handles.push(tokio::spawn(async move {
            let response = app
                .oneshot(post_json(
                    &format!("/api/approvals/{approval_id}"),
                    json!({ "decision": "approve" }),
                ))
                .await
                .unwrap();
            (response.status(), json_body(response).await)
        }));
    }
    let mut winners = 0;
    for handle in handles {
        let (status, body) = handle.await.unwrap();
        if status == StatusCode::OK {
            winners += 1;
        } else {
            let code = body["error"]["code"].as_str().unwrap_or("");
            assert!(code == "approval_not_pending" || code == "approval_conflict");
        }
    }
    assert_eq!(winners, 1);
}

#[tokio::test]
async fn create_routine_validates_schedule_and_timezone() {
    for schedule in ["", "every:1m", "every:8d", "daily:24:00", "garbage"] {
        let response = app()
            .oneshot(post_json(
                "/api/routines",
                json!({ "name": "n", "goal": "g", "schedule": schedule }),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "schedule {schedule}"
        );
        assert_eq!(
            json_body(response).await["error"]["code"],
            "invalid_schedule",
            "schedule {schedule}"
        );
    }

    let response = app()
        .oneshot(post_json(
            "/api/routines",
            json!({ "name": "n", "goal": "g", "schedule": "every:5m", "timezone": "UTC" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "invalid_timezone"
    );
}

#[tokio::test]
async fn create_routine_returns_created_fields() {
    let response = app()
        .oneshot(post_json(
            "/api/routines",
            json!({ "name": "n", "goal": "g", "schedule": "hourly" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_body(response).await;
    assert!(body["routine"]["routine_id"]
        .as_str()
        .unwrap()
        .starts_with("rt_"));
    assert_eq!(body["routine"]["schedule"], "hourly");
    assert_eq!(body["routine"]["enabled"], true);
    assert_eq!(body["routine"]["timezone"], "Asia/Shanghai");
    assert!(body["routine"]["next_run_ms"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn create_routine_enforces_limit_of_fifty() {
    let app = app();
    for _ in 0..50 {
        let response = app
            .clone()
            .oneshot(post_json(
                "/api/routines",
                json!({ "name": "n", "goal": "g", "schedule": "every:5m" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    let response = app
        .oneshot(post_json(
            "/api/routines",
            json!({ "name": "n", "goal": "g", "schedule": "every:5m" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["error"]["code"], "routine_limit");
}

#[tokio::test]
async fn list_update_and_delete_routine() {
    let app = app();
    let created = app
        .clone()
        .oneshot(post_json(
            "/api/routines",
            json!({ "name": "n", "goal": "g", "schedule": "every:5m" }),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let routine_id = json_body(created).await["routine"]["routine_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let listed = app.clone().oneshot(get("/api/routines")).await.unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let body = json_body(listed).await;
    assert_eq!(body["routines"].as_array().unwrap().len(), 1);

    let updated = app
        .clone()
        .oneshot(post_json(
            &format!("/api/routines/{routine_id}"),
            json!({ "enabled": false }),
        ))
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(json_body(updated).await["routine"]["enabled"], false);

    let deleted = app
        .clone()
        .oneshot(delete(&format!("/api/routines/{routine_id}")))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(json_body(deleted).await["deleted"], routine_id);

    let listed = app.oneshot(get("/api/routines")).await.unwrap();
    assert_eq!(
        json_body(listed).await["routines"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}
