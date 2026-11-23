use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use futures_core::Stream;
use futures_util::stream::{self, StreamExt};
use hive_types::EventEnvelope;
use tokio::sync::broadcast;
use tokio::time::{sleep_until, Instant};

pub fn sse_stream(
    backlog: Vec<EventEnvelope>,
    rx: broadcast::Receiver<EventEnvelope>,
    latest_seq: Arc<AtomicU64>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let header =
        stream::once(async { Ok::<Bytes, Infallible>(Bytes::from_static(b"retry: 2500\n\n")) });
    let backlog = stream::iter(
        backlog
            .into_iter()
            .map(|envelope| Ok::<Bytes, Infallible>(Bytes::from(event_frame(&envelope)))),
    );

    let live = stream::unfold(
        (rx, latest_seq, Instant::now() + Duration::from_secs(20)),
        |(mut rx, latest_seq, deadline)| async move {
            let sleep = sleep_until(deadline);
            tokio::pin!(sleep);
            tokio::select! {
                received = rx.recv() => {
                    match received {
                        Ok(envelope) => {
                            let frame = Bytes::from(event_frame(&envelope));
                            Some((Ok(frame), (rx, latest_seq, Instant::now() + Duration::from_secs(20))))
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            let latest = latest_seq.load(Ordering::Relaxed);
                            let frame = Bytes::from(format!(
                                "event: hive.lagged\ndata: {{\"lagged\":{n},\"last_persisted_seq\":{latest}}}\n\n"
                            ));
                            Some((Ok(frame), (rx, latest_seq, Instant::now() + Duration::from_secs(20))))
                        }
                        Err(broadcast::error::RecvError::Closed) => None,
                    }
                }
                _ = &mut sleep => {
                    Some((Ok(Bytes::from_static(b": heartbeat\n\n")), (rx, latest_seq, Instant::now() + Duration::from_secs(20))))
                }
            }
        },
    );

    header.chain(backlog).chain(live)
}

fn event_frame(envelope: &EventEnvelope) -> String {
    let data = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".to_owned());
    format!(
        "id: {}\nevent: {}\ndata: {}\n\n",
        envelope.seq,
        envelope.kind.as_str(),
        data
    )
}
