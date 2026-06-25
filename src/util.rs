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
}
