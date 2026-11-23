//! Routine schedule parsing and next-run computation.
//!
//! Schedules are validated at creation time and never parsed lazily during a
//! tick. Only the fixed `Asia/Shanghai` timezone is accepted (UTC+8, no DST);
//! `daily` windows are interpreted as that wall clock and stored as UTC epoch
//! milliseconds.

use std::fmt;

pub const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";
pub const SHANGHAI_OFFSET_MS: i64 = 8 * 60 * 60 * 1000;

const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;
const MIN_INTERVAL_MS: i64 = 5 * MINUTE_MS;
const MAX_INTERVAL_MS: i64 = 7 * DAY_MS;

/// A parsed, valid schedule expression.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Schedule {
    Every { interval_ms: i64 },
    Daily { hour: u32, minute: u32 },
}

/// Parse the minimal schedule grammar: `every:<N>m|h`, `hourly`, `daily:HH:MM`.
///
/// The returned error is a short human-readable reason; the stable error code
/// is attached by the caller (`invalid_schedule`).
pub fn parse_schedule(value: &str) -> Result<Schedule, ScheduleError> {
    let value = value.trim();
    if value == "hourly" {
        return Ok(Schedule::Every {
            interval_ms: HOUR_MS,
        });
    }
    if let Some(rest) = value.strip_prefix("every:") {
        return parse_every(rest);
    }
    if let Some(rest) = value.strip_prefix("daily:") {
        return parse_daily(rest);
    }
    Err(ScheduleError::new(format!(
        "unsupported schedule `{value}`"
    )))
}

fn parse_every(rest: &str) -> Result<Schedule, ScheduleError> {
    let split = rest
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map(|(index, _)| index);
    let Some(split) = split else {
        return Err(ScheduleError::new(format!(
            "missing unit in `every:{rest}`"
        )));
    };
    let (digits, unit) = rest.split_at(split);
    if digits.is_empty() {
        return Err(ScheduleError::new(format!(
            "missing number in `every:{rest}`"
        )));
    }
    let amount: i64 = digits
        .parse()
        .map_err(|_| ScheduleError::new(format!("invalid number `{digits}`")))?;
    let unit_ms = match unit {
        "m" => MINUTE_MS,
        "h" => HOUR_MS,
        _ => {
            return Err(ScheduleError::new(format!(
                "unsupported unit `{unit}` (expected m or h)"
            )))
        }
    };
    let interval_ms = amount
        .checked_mul(unit_ms)
        .ok_or_else(|| ScheduleError::new("interval overflow".to_owned()))?;
    if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&interval_ms) {
        return Err(ScheduleError::new(
            "interval must be between 5 minutes and 7 days".to_owned(),
        ));
    }
    Ok(Schedule::Every { interval_ms })
}

fn parse_daily(rest: &str) -> Result<Schedule, ScheduleError> {
    let mut parts = rest.splitn(2, ':');
    let (Some(hour), Some(minute)) = (parts.next(), parts.next()) else {
        return Err(ScheduleError::new(format!(
            "expected HH:MM in `daily:{rest}`"
        )));
    };
    if hour.is_empty() || minute.is_empty() {
        return Err(ScheduleError::new(format!(
            "expected HH:MM in `daily:{rest}`"
        )));
    }
    let hour: u32 = hour
        .parse()
        .map_err(|_| ScheduleError::new(format!("invalid hour `{hour}`")))?;
    let minute: u32 = minute
        .parse()
        .map_err(|_| ScheduleError::new(format!("invalid minute `{minute}`")))?;
    if hour > 23 {
        return Err(ScheduleError::new("hour must be 0..=23".to_owned()));
    }
    if minute > 59 {
        return Err(ScheduleError::new("minute must be 0..=59".to_owned()));
    }
    Ok(Schedule::Daily { hour, minute })
}

/// First `next_run_ms` for a freshly created routine.
pub fn first_next_run_ms(schedule: Schedule, now_ms: i64) -> i64 {
    match schedule {
        Schedule::Every { interval_ms } => now_ms + interval_ms,
        Schedule::Daily { hour, minute } => daily_occurrence(hour, minute, now_ms),
    }
}

/// The window after `fire_at_ms`, advanced past `now_ms` so a process that was
/// down for several windows skips the missed ones instead of storming them.
pub fn next_run_after(schedule: Schedule, fire_at_ms: i64, now_ms: i64) -> i64 {
    match schedule {
        Schedule::Every { interval_ms } => {
            let mut next = fire_at_ms + interval_ms;
            while next <= now_ms {
                next += interval_ms;
            }
            next
        }
        Schedule::Daily { hour, minute } => {
            // Asia/Shanghai has no DST, so each daily window is exactly one day
            // after the previous one.
            let mut next = fire_at_ms + DAY_MS;
            while next <= now_ms {
                next += DAY_MS;
            }
            let _ = (hour, minute);
            next
        }
    }
}

/// Whether `timezone` is the single accepted value. Any other value is
/// rejected at creation time with `invalid_timezone`.
pub fn validate_timezone(timezone: &str) -> bool {
    timezone == DEFAULT_TIMEZONE
}

/// Earliest UTC epoch millisecond where the `Asia/Shanghai` wall clock reads
/// `hour:minute` on or after `now_ms`.
fn daily_occurrence(hour: u32, minute: u32, now_ms: i64) -> i64 {
    let shifted = now_ms + SHANGHAI_OFFSET_MS;
    let day = shifted.div_euclid(DAY_MS);
    let ms_of_day = shifted.rem_euclid(DAY_MS);
    let target = (hour as i64) * HOUR_MS + (minute as i64) * MINUTE_MS;
    let day = if target >= ms_of_day { day } else { day + 1 };
    day * DAY_MS + target - SHANGHAI_OFFSET_MS
}

/// Parse error carrying a stable error code and human-readable reason.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ScheduleError {
    reason: String,
}

impl ScheduleError {
    pub fn new(reason: String) -> Self {
        Self { reason }
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_schedules() {
        assert_eq!(
            parse_schedule("hourly").expect("hourly"),
            Schedule::Every {
                interval_ms: HOUR_MS
            }
        );
        assert_eq!(
            parse_schedule("every:5m").expect("every 5m"),
            Schedule::Every {
                interval_ms: 5 * MINUTE_MS
            }
        );
        assert_eq!(
            parse_schedule("every:168h").expect("every 168h"),
            Schedule::Every {
                interval_ms: 7 * DAY_MS
            }
        );
        assert_eq!(
            parse_schedule("daily:09:00").expect("daily"),
            Schedule::Daily { hour: 9, minute: 0 }
        );
    }

    #[test]
    fn rejects_invalid_schedules() {
        for value in ["", "every:1m", "every:8d", "daily:24:00", "garbage"] {
            assert!(
                parse_schedule(value).is_err(),
                "expected `{value}` to be rejected"
            );
        }
        assert!(parse_schedule("every:169h").is_err());
        assert!(parse_schedule("every:10081m").is_err());
        assert!(parse_schedule("daily:9").is_err());
        assert!(parse_schedule("daily:09:60").is_err());
    }

    #[test]
    fn every_first_and_next_run_are_interval_aligned() {
        let schedule = Schedule::Every {
            interval_ms: 5 * MINUTE_MS,
        };
        let first = first_next_run_ms(schedule, 0);
        assert_eq!(first, 5 * MINUTE_MS);
        assert_eq!(next_run_after(schedule, first, first), 10 * MINUTE_MS);
        // Missed windows are skipped, not replayed.
        assert_eq!(
            next_run_after(schedule, first, 100 * MINUTE_MS),
            105 * MINUTE_MS
        );
    }

    #[test]
    fn daily_occurrence_converts_asia_shanghai_to_epoch() {
        let now = 1_795_392_000_000; // 2026-11-23T00:00:00Z (08:00 +08)
        let next = first_next_run_ms(Schedule::Daily { hour: 9, minute: 0 }, now);
        assert_eq!(next, 1_795_395_600_000); // 2026-11-23T01:00:00Z
        assert_eq!(crate::time::rfc3339_from_ms(next), "2026-11-23T01:00:00Z");
    }

    #[test]
    fn timezone_accepts_only_shanghai() {
        assert!(validate_timezone("Asia/Shanghai"));
        assert!(!validate_timezone("UTC"));
        assert!(!validate_timezone("Asia/Tokyo"));
        assert!(!validate_timezone(""));
    }
}
