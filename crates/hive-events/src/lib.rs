//! Event bridge from the persistent store to bounded in-memory subscribers.
//!
//! Events are committed to SQLite first (the durable, authoritative path) and
//! then fanned out to per-subscriber bounded queues. Publishing never blocks
//! the writer: when a slow subscriber's queue is full the oldest buffered
//! event is dropped and the subscriber's lagged counter is incremented. A
//! subscriber can always catch up from the store using its `seq` cursor.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use hive_store::{Store, StoreError};
use hive_types::{EventEnvelope, EventInput, MissionId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissionFilter {
    All,
    Mission(MissionId),
}

impl MissionFilter {
    fn matches(self, envelope: &EventEnvelope) -> bool {
        match self {
            Self::All => true,
            Self::Mission(mission_id) => mission_id == envelope.mission_id,
        }
    }
}

struct Channel {
    queue: Mutex<VecDeque<EventEnvelope>>,
    not_empty: Condvar,
    capacity: usize,
    lagged: AtomicU64,
}

impl Channel {
    fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            not_empty: Condvar::new(),
            capacity,
            lagged: AtomicU64::new(0),
        }
    }

    fn push(&self, envelope: EventEnvelope) {
        let mut queue = self.queue.lock().expect("channel lock poisoned");
        if queue.len() == self.capacity {
            queue.pop_front();
            self.lagged.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(envelope);
        drop(queue);
        self.not_empty.notify_all();
    }

    fn try_recv(&self) -> Option<EventEnvelope> {
        self.queue
            .lock()
            .expect("channel lock poisoned")
            .pop_front()
    }

    fn recv_timeout(&self, timeout: Duration) -> Option<EventEnvelope> {
        let mut queue = self.queue.lock().expect("channel lock poisoned");
        if let Some(envelope) = queue.pop_front() {
            return Some(envelope);
        }
        let (mut queue, _timed_out) = self
            .not_empty
            .wait_timeout(queue, timeout)
            .expect("channel lock poisoned");
        queue.pop_front()
    }

    fn lagged(&self) -> u64 {
        self.lagged.load(Ordering::Relaxed)
    }
}

struct SubscriberEntry {
    filter: MissionFilter,
    channel: Arc<Channel>,
}

struct BusInner {
    next_id: u64,
    subscribers: HashMap<u64, SubscriberEntry>,
}

pub struct EventBus {
    inner: Mutex<BusInner>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BusInner {
                next_id: 0,
                subscribers: HashMap::new(),
            }),
        }
    }

    pub fn subscribe(&self, filter: MissionFilter, capacity: usize) -> Subscriber {
        assert!(capacity > 0, "subscriber capacity must be positive");
        let channel = Arc::new(Channel::new(capacity));
        let mut inner = self.inner.lock().expect("bus lock poisoned");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.subscribers.insert(
            id,
            SubscriberEntry {
                filter,
                channel: channel.clone(),
            },
        );
        Subscriber { channel }
    }

    pub fn publish(&self, envelope: &EventEnvelope) {
        let channels: Vec<Arc<Channel>> = {
            let inner = self.inner.lock().expect("bus lock poisoned");
            inner
                .subscribers
                .values()
                .filter(|entry| entry.filter.matches(envelope))
                .map(|entry| entry.channel.clone())
                .collect()
        };
        for channel in channels {
            channel.push(envelope.clone());
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Subscriber {
    channel: Arc<Channel>,
}

impl Subscriber {
    pub fn try_recv(&self) -> Option<EventEnvelope> {
        self.channel.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<EventEnvelope> {
        self.channel.recv_timeout(timeout)
    }

    pub fn lagged(&self) -> u64 {
        self.channel.lagged()
    }
}

/// Persist an event and then broadcast it. The store write happens first and
/// is the only fallible step; a slow subscriber cannot fail or block it.
pub fn append_and_publish(
    store: &mut Store,
    bus: &EventBus,
    input: &EventInput,
) -> Result<EventEnvelope, StoreError> {
    let envelope = store.append_event(input)?;
    bus.publish(&envelope);
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_types::{EventId, EventKind};

    fn envelope(mission_id: MissionId, seq: u64) -> EventEnvelope {
        EventEnvelope::new(
            EventInput {
                event_id: EventId::new(),
                kind: EventKind::NodeThought,
                mission_id,
                run_id: None,
                node_id: None,
                session_id: None,
                attempt_no: None,
                trace_id: None,
                payload: Default::default(),
            },
            seq,
            "1970-01-01T00:00:00Z".to_owned(),
        )
    }

    fn event_input(mission_id: MissionId) -> EventInput {
        EventInput {
            event_id: EventId::new(),
            kind: EventKind::MissionCreated,
            mission_id,
            run_id: None,
            node_id: None,
            session_id: None,
            attempt_no: None,
            trace_id: None,
            payload: Default::default(),
        }
    }

    #[test]
    fn slow_subscriber_drops_oldest_and_counts_lag() {
        let bus = EventBus::new();
        let sub = bus.subscribe(MissionFilter::All, 2);
        let mission = MissionId::new();
        for seq in 1..=5 {
            bus.publish(&envelope(mission, seq));
        }

        assert_eq!(sub.lagged(), 3);
        assert_eq!(sub.try_recv().expect("first retained").seq, 4);
        assert_eq!(sub.try_recv().expect("second retained").seq, 5);
        assert!(sub.try_recv().is_none());
    }

    #[test]
    fn mission_filter_only_delivers_matching_events() {
        let bus = EventBus::new();
        let m1 = MissionId::new();
        let m2 = MissionId::new();
        let all = bus.subscribe(MissionFilter::All, 8);
        let only_m1 = bus.subscribe(MissionFilter::Mission(m1), 8);

        bus.publish(&envelope(m1, 1));
        bus.publish(&envelope(m2, 2));
        bus.publish(&envelope(m1, 3));

        assert_eq!(all.try_recv().map(|event| event.seq), Some(1));
        assert_eq!(all.try_recv().map(|event| event.seq), Some(2));
        assert_eq!(all.try_recv().map(|event| event.seq), Some(3));
        assert!(all.try_recv().is_none());

        assert_eq!(only_m1.try_recv().map(|event| event.seq), Some(1));
        assert_eq!(only_m1.try_recv().map(|event| event.seq), Some(3));
        assert!(only_m1.try_recv().is_none());
    }

    #[test]
    fn subscribers_have_independent_buffers() {
        let bus = EventBus::new();
        let first = bus.subscribe(MissionFilter::All, 8);
        let second = bus.subscribe(MissionFilter::All, 8);
        let mission = MissionId::new();
        bus.publish(&envelope(mission, 1));
        bus.publish(&envelope(mission, 2));

        assert_eq!(first.try_recv().map(|event| event.seq), Some(1));
        assert_eq!(first.try_recv().map(|event| event.seq), Some(2));
        assert!(first.try_recv().is_none());

        assert_eq!(second.try_recv().map(|event| event.seq), Some(1));
        assert_eq!(second.try_recv().map(|event| event.seq), Some(2));
    }

    #[test]
    fn recv_timeout_returns_none_when_empty() {
        let bus = EventBus::new();
        let sub = bus.subscribe(MissionFilter::All, 2);
        assert!(sub.recv_timeout(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn append_and_publish_persists_then_broadcasts() {
        let mut store = Store::open_in_memory().expect("store");
        let bus = EventBus::new();
        let sub = bus.subscribe(MissionFilter::All, 8);
        let mission = store.create_mission("t", "g", 0).expect("mission");

        let envelope =
            append_and_publish(&mut store, &bus, &event_input(mission.mission_id)).expect("append");

        assert_eq!(envelope.seq, 1);
        assert_eq!(store.latest_seq().expect("latest"), Some(1));
        assert_eq!(
            sub.try_recv().expect("broadcast").event_id,
            envelope.event_id
        );
    }

    #[test]
    fn slow_subscriber_does_not_block_writes() {
        let mut store = Store::open_in_memory().expect("store");
        let bus = EventBus::new();
        let _slow = bus.subscribe(MissionFilter::All, 1);
        let mission = store.create_mission("t", "g", 0).expect("mission");

        for _ in 0..10 {
            append_and_publish(&mut store, &bus, &event_input(mission.mission_id)).expect("append");
        }

        assert_eq!(store.latest_seq().expect("latest"), Some(10));
    }
}
