//! Routine tick loop.
//!
//! Drives due routine windows one at a time. A single bad row (for example a
//! schedule corrupted out-of-band) is disabled and reported by the store, and
//! any unexpected per-row error is caught here so it cannot abort the loop.

use hive_store::{RoutineFireOutcome, Store};

use crate::error::RuntimeError;

/// Counters for one tick pass over the due routines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RoutineTickReport {
    pub fired: usize,
    pub disabled: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// Fire every routine whose `next_run_ms` is due. Missed windows are skipped:
/// each routine advances to its next future window and fires at most once.
pub fn tick_routines(store: &mut Store, now_ms: i64) -> Result<RoutineTickReport, RuntimeError> {
    let due = store.list_due_routines(now_ms)?;
    let mut report = RoutineTickReport::default();
    for routine_id in due {
        match store.fire_routine(routine_id, now_ms) {
            Ok(outcome) => match outcome {
                RoutineFireOutcome::Fired { .. } => report.fired += 1,
                RoutineFireOutcome::Disabled { .. } => report.disabled += 1,
                RoutineFireOutcome::Failed { .. } => report.failed += 1,
                RoutineFireOutcome::AlreadyFired
                | RoutineFireOutcome::NotDue
                | RoutineFireOutcome::NotFound => report.skipped += 1,
            },
            Err(_) => report.errors += 1,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use hive_store::Store;
    use hive_types::EventKind;

    use super::*;

    #[test]
    fn tick_fires_due_window_once_and_skips_missed() {
        let mut store = Store::open_in_memory().expect("store");
        let routine = store
            .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
            .expect("routine");
        // Process was down for several windows: now is far past the first due
        // time. Only the first window fires; next_run_ms advances past `now`.
        let report = tick_routines(&mut store, 1_000_000).expect("tick");
        assert_eq!(report.fired, 1);
        let missions = store.list_missions(100, 0).expect("missions");
        assert_eq!(missions.len(), 1);
        let row = store
            .get_routine(routine.routine_id)
            .expect("get")
            .expect("row");
        assert!(row.next_run_ms > 1_000_000);

        // A second tick at the same instant finds nothing due.
        let second = tick_routines(&mut store, 1_000_000).expect("tick");
        assert_eq!(second.fired, 0);
        let events = store.read_all_events().expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::RoutineTriggered)
                .count(),
            1
        );
    }

    #[test]
    fn tick_fires_all_due_routines() {
        let mut store = Store::open_in_memory().expect("store");
        for _ in 0..3 {
            store
                .create_routine("n", "g", "every:5m", "Asia/Shanghai", 0)
                .expect("routine");
        }
        let report = tick_routines(&mut store, 300_000).expect("tick");
        assert_eq!(report.fired, 3);
        let missions = store.list_missions(100, 0).expect("missions");
        assert_eq!(missions.len(), 3);
    }
}
