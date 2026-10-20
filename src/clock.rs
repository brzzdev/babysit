use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SLEEP_SLICE: Duration = Duration::from_secs(1);
const SECONDS_PER_DAY: u64 = 86_400;

pub trait Clock {
    fn now(&self) -> SystemTime;
    fn sleep_until(&self, until: SystemTime);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    /// Sleeps in short slices against the wall clock, so a machine waking from sleep past `until`
    /// returns at once.
    fn sleep_until(&self, until: SystemTime) {
        while let Ok(left) = until.duration_since(SystemTime::now()) {
            if left.is_zero() {
                return;
            }
            thread::sleep(left.min(SLEEP_SLICE));
        }
    }
}

/// Formats `time` as RFC 3339 UTC to the second, using Howard Hinnant's `civil_from_days`.
pub fn rfc3339(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (days, second_of_day) = (seconds / SECONDS_PER_DAY, seconds % SECONDS_PER_DAY);
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rfc3339_utc() {
        let at = |seconds| rfc3339(UNIX_EPOCH + Duration::from_secs(seconds));

        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_399), "2000-02-28T23:59:59Z");
        assert_eq!(at(1_792_523_957), "2026-10-20T19:19:17Z");
    }
}
