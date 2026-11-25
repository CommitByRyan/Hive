use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use hive_llm::ModelCatalog;
use hive_server::{build_router, AppState};
use hive_store::Store;
use hive_types::{EventEnvelope, EventId, EventInput, EventKind, MissionId};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> Router {
    let store = Store::open_in_memory().expect("store");
    let state = AppState::new(
        store,
        ModelCatalog::new(),
        "demo".to_owned(),
        "127.0.0.1".to_owned(),
        8765,
    );
    build_router(state)
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

#[tokio::test]
async fn app_state_forwards_broadcasts_to_sse_subscribers() {
    let state = AppState::new(
        Store::open_in_memory().expect("store"),
        ModelCatalog::new(),
        "demo".to_owned(),
        "127.0.0.1".to_owned(),
        8765,
    );
    let mut rx = state.events_tx.subscribe();
    let mission_id = MissionId::new();
    let envelope = EventEnvelope::new(
        EventInput {
            event_id: EventId::new(),
            kind: EventKind::MessageUser,
            mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: json!({ "content": "live probe" }),
        },
        1,
        "1970-01-01T00:00:00Z".to_owned(),
    );

    state.broadcast(&envelope);
    let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("bridge timeout")
        .expect("bridge receive");
    assert_eq!(received.seq, 1);
}

#[tokio::test]
async fn live_sse_delivers_message_event_for_selected_mission() {
    let app = app();
    let created = app
        .clone()
        .oneshot(post_json(
            "/api/missions",
            json!({ "title": "live", "goal": "sse" }),
        ))
        .await
        .expect("create response");
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json_body(created).await;
    let mission_id = created["mission"]["id"].as_str().expect("mission id");
    let after_seq = created["last_seq"].as_u64().expect("seq");

    let events = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/events?mission_id={mission_id}&after_seq={after_seq}"
                ))
                .header(header::ACCEPT, "text/event-stream")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("events response");
    assert_eq!(events.status(), StatusCode::OK);
    let mut body = events.into_body();

    let retry = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("retry timeout")
        .expect("retry frame")
        .expect("retry data");
    assert!(
        String::from_utf8_lossy(&retry.into_data().expect("retry data")).contains("retry: 2500")
    );

    let message = app
        .clone()
        .oneshot(post_json(
            &format!("/api/missions/{mission_id}/messages"),
            json!({ "content": "live probe" }),
        ))
        .await
        .expect("message response");
    assert_eq!(message.status(), StatusCode::CREATED);

    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("live event timeout")
        .expect("live event frame")
        .expect("live event data");
    let frame = frame.into_data().expect("live event data");
    let frame = String::from_utf8_lossy(&frame);
    assert!(frame.contains("event: message.user"), "frame: {frame}");
    assert!(frame.contains("live probe"), "frame: {frame}");
}
