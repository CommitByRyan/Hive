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

/// Default interval for the background routine/lease tick.
pub const TICK_PERIOD: Duration = Duration::from_secs(1);

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

    /// Start a stoppable background tick that fires due routines and reclaims
    /// expired leases once per `period`. The loop exits when this state is
    /// dropped (via the shared `shutdown` flag) and uses only async waits, so
    /// it leaves no blocking thread behind.
    pub fn start_tick_loop(&self, period: Duration) {
        let store = self.store.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let now_ms = hive_store::now_ms();
                if let Ok(mut guard) = store.lock() {
                    let _ = hive_runtime::tick_once(&mut guard, now_ms);
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
