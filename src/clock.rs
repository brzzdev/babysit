use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAYS_PER_CENTURY: u64 = 36_524;
/// One era, after which the Gregorian calendar repeats.
const DAYS_PER_ERA: u64 = 146_097;
/// March to July, the 31-30-31-30-31 run the month formula is built on.
const DAYS_PER_FIVE_MONTHS: u64 = 153;
const DAYS_PER_FOUR_COMMON_YEARS: u64 = 1_460;
const DAYS_PER_YEAR: u64 = 365;
/// From 0000-03-01, where the shifted calendar starts its years, to 1970-01-01.
const EPOCH_DAYS_SINCE_SHIFTED_ORIGIN: u64 = 719_468;
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3_600;
pub const SECONDS_PER_MINUTE: u64 = 60;
const SLEEP_SLICE: Duration = Duration::from_secs(1);
const YEARS_PER_ERA: u64 = 400;

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
    let seconds = unix_seconds(time);
    let (days, second_of_day) = (seconds / SECONDS_PER_DAY, seconds % SECONDS_PER_DAY);
    let shifted = days + EPOCH_DAYS_SINCE_SHIFTED_ORIGIN;
    let era = shifted / DAYS_PER_ERA;
    let day_of_era = shifted % DAYS_PER_ERA;
    let year_of_era = (day_of_era - day_of_era / DAYS_PER_FOUR_COMMON_YEARS
        + day_of_era / DAYS_PER_CENTURY
        - day_of_era / (DAYS_PER_ERA - 1))
        / DAYS_PER_YEAR;
    let day_of_year =
        day_of_era - (DAYS_PER_YEAR * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / DAYS_PER_FIVE_MONTHS;
    let day = day_of_year - (DAYS_PER_FIVE_MONTHS * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * YEARS_PER_ERA + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / SECONDS_PER_HOUR,
        second_of_day % SECONDS_PER_HOUR / SECONDS_PER_MINUTE,
        second_of_day % SECONDS_PER_MINUTE,
    )
}

pub fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
