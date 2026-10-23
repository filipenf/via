//! Small cross-cutting helpers shared between modules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time in milliseconds since the Unix epoch (0 if the clock is before the epoch).
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Current time in whole seconds since the Unix epoch (`None` if the clock is before the epoch).
pub fn unix_seconds_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// UTC `YYYY-MM-DD HH:MM` from Unix milliseconds. Seconds are dropped.
pub fn format_utc_minute(millis: u64) -> String {
    let secs = millis / 1000;
    let days = (secs / 86_400) as i64;
    let tod = (secs % 86_400) as u32;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a civil date.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_millis_is_after_2020() {
        // 2020-01-01T00:00:00Z in ms; a sanity check that the clock helper is wired up.
        assert!(now_millis() > 1_577_836_800_000);
    }

    #[test]
    fn unix_seconds_now_is_some() {
        assert!(unix_seconds_now().is_some());
    }

    #[test]
    fn format_utc_minute_known_instants() {
        assert_eq!(format_utc_minute(0), "1970-01-01 00:00");
        assert_eq!(format_utc_minute(1_577_836_800_000), "2020-01-01 00:00");
        assert_eq!(format_utc_minute(1_709_164_800_000), "2024-02-29 00:00");
        // Same instant the task-board switcher formats in nvim/tests/tasks_spec.lua.
        assert_eq!(format_utc_minute(1_783_693_555_327), "2026-07-10 14:25");
    }
}
