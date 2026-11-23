use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use hive_server::sse::sse_stream;
use hive_types::{EventId, EventInput, EventKind, MissionId};
use tokio::sync::broadcast;

fn envelope(mission_id: MissionId, seq: u64) -> hive_types::EventEnvelope {
    hive_types::EventEnvelope::new(
        EventInput {
            event_id: EventId::new(),
            kind: EventKind::NodeReady,
            mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: serde_json::json!({}),
        },
        seq,
        "1970-01-01T00:00:00Z".to_owned(),
    )
}

#[tokio::test]
async fn stream_compensates_backlog_then_delivers_live_in_seq_order() {
    let (tx, rx) = broadcast::channel(256);
    let latest = Arc::new(AtomicU64::new(2));
    let mission = MissionId::new();
    let backlog = vec![envelope(mission, 1), envelope(mission, 2)];
    let stream = sse_stream(backlog, rx, latest.clone());
    futures_util::pin_mut!(stream);

    let retry = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&retry).contains("retry: 2500"));

    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("id: 1"));
    assert!(String::from_utf8_lossy(&first).contains("event: node.ready"));

    let second = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&second).contains("id: 2"));

    latest.store(3, Ordering::Relaxed);
    tx.send(envelope(mission, 3)).unwrap();
    let third = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&third).contains("id: 3"));
}

#[tokio::test]
async fn slow_receiver_gets_lagged_control_event_with_latest_seq() {
    let (tx, rx) = broadcast::channel(2);
    let latest = Arc::new(AtomicU64::new(5));
    let mission = MissionId::new();
    for seq in 1..=5 {
        tx.send(envelope(mission, seq)).unwrap();
    }
    let stream = sse_stream(Vec::new(), rx, latest);
    futures_util::pin_mut!(stream);

    let retry = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&retry).contains("retry: 2500"));

    let lagged = stream.next().await.unwrap().unwrap();
    let frame = String::from_utf8_lossy(&lagged);
    assert!(frame.contains("event: hive.lagged"), "frame: {frame}");
    assert!(frame.contains("last_persisted_seq\":5"), "frame: {frame}");
}
