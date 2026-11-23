use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hive_events::{EventBus, MissionFilter};
use hive_llm::ModelCatalog;
use hive_store::Store;
use hive_types::{EventEnvelope, EventInput};
use tokio::sync::broadcast;

use crate::error::ApiError;
use crate::SSE_QUEUE_CAPACITY;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub bus: Arc<EventBus>,
    pub events_tx: broadcast::Sender<EventEnvelope>,
    pub latest_seq: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    pub catalog: ModelCatalog,
    pub mode: String,
    pub host: String,
    pub port: u16,
}

impl AppState {
    pub fn new(store: Store, catalog: ModelCatalog, mode: String, host: String, port: u16) -> Self {
        let (events_tx, _receiver) = broadcast::channel(SSE_QUEUE_CAPACITY);
        let latest_seq = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Self {
            store: Arc::new(Mutex::new(store)),
            bus: Arc::new(EventBus::new()),
            events_tx,
            latest_seq,
            shutdown: shutdown.clone(),
            catalog,
            mode,
            host,
            port,
        };
        state.start_event_bridge();
        state
    }

    pub fn authentication_required(&self) -> bool {
        self.host
            .parse::<IpAddr>()
            .map(|ip| !ip.is_loopback())
            .unwrap_or(true)
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn broadcast(&self, envelope: &EventEnvelope) {
        self.latest_seq.store(envelope.seq, Ordering::Relaxed);
        self.bus.publish(envelope);
    }

    pub fn append_event(&self, input: &EventInput) -> Result<EventEnvelope, ApiError> {
        let mut store = self.store.lock().map_err(|_| ApiError::store_lock())?;
        let envelope = store.append_event(input).map_err(ApiError::store)?;
        drop(store);
        self.broadcast(&envelope);
        Ok(envelope)
    }

    pub fn current_latest_seq(&self) -> u64 {
        self.latest_seq.load(Ordering::Relaxed)
    }

    pub fn refresh_latest_seq(&self) -> u64 {
        let store = self.store.lock();
        let latest = match store {
            Ok(store) => store.latest_seq().unwrap_or(None).unwrap_or(0),
            Err(_) => self.current_latest_seq(),
        };
        self.latest_seq.store(latest, Ordering::Relaxed);
        latest
    }

    fn start_event_bridge(&self) {
        let subscriber = self
            .bus
            .subscribe(MissionFilter::All, SSE_QUEUE_CAPACITY * 4);
        let events_tx = self.events_tx.clone();
        let shutdown = self.shutdown.clone();
        tokio::task::spawn_blocking(move || {
            while !shutdown.load(Ordering::Relaxed) {
                if let Some(envelope) = subscriber.recv_timeout(Duration::from_millis(100)) {
                    let _ = events_tx.send(envelope);
                }
            }
        });
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}
