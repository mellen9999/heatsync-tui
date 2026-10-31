//! local wall-clock for line timestamps — libc's localtime on unix, utc elsewhere
//! (no date-time dependency for one `HH:MM`).

use std::time::{SystemTime, UNIX_EPOCH};

/// unix seconds now.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// (hour, minute) of a unix timestamp in local time.
#[cfg(unix)]
pub(crate) fn hhmm(secs: u64) -> (u32, u32) {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r writes only into the zeroed tm we hand it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return utc_hhmm(secs);
        }
        tm
    };
    (tm.tm_hour as u32 % 24, tm.tm_min as u32 % 60)
}

#[cfg(not(unix))]
pub(crate) fn hhmm(secs: u64) -> (u32, u32) {
    utc_hhmm(secs)
}

fn utc_hhmm(secs: u64) -> (u32, u32) {
    ((secs / 3600 % 24) as u32, (secs / 60 % 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_fallback_reads_hour_and_minute() {
        assert_eq!(utc_hhmm(0), (0, 0));
        assert_eq!(utc_hhmm(13 * 3600 + 5 * 60 + 59), (13, 5));
        assert_eq!(utc_hhmm(86_400 + 61), (0, 1));
    }

    #[test]
    fn local_time_is_a_valid_clock_reading() {
        let (h, m) = hhmm(1_700_000_000);
        assert!(h < 24 && m < 60);
    }
}
