//! The timestamp and the stderr logger. See [FR81], [FR82], and [FR83].
//!
//! One line for each record: `<time> <LEVEL> <message>`. The time is UTC, so a
//! log from one machine reads the same on every other machine.

use std::time::{SystemTime, UNIX_EPOCH};

/// The time as `YYYY-MM-DDTHH:MM:SSZ`. A time before the epoch reads as the
/// epoch, because the log needs a string, not an error.
pub fn utc_now(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (day, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(day);
    let (hh, mm, ss) = (rem / 3600, rem / 60 % 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// The year, the month, and the day for a count of days after 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`. It shifts the era to start on 1 March, so
/// the leap day falls at the end of the year and every era of 400 years holds
/// the same 146097 days.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + u64::from(m <= 2), m, d)
}

/// Writes each record to stderr. The maximum level filters the records, so
/// `enabled` accepts every record that reaches it.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        eprintln!(
            "{} {} {}",
            utc_now(SystemTime::now()),
            record.level(),
            record.args()
        );
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

/// Starts the logger at level `Info`. Call it once from `main`. A second call
/// returns an error.
pub fn init() -> Result<(), log::SetLoggerError> {
    log::set_logger(&LOGGER)?;
    log::set_max_level(log::LevelFilter::Info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> String {
        utc_now(UNIX_EPOCH + Duration::from_secs(secs))
    }

    const TIMES: [(u64, &str); 10] = [
        (0, "1970-01-01T00:00:00Z"),
        (86_399, "1970-01-01T23:59:59Z"),
        (86_400, "1970-01-02T00:00:00Z"),
        (951_782_400, "2000-02-29T00:00:00Z"),
        (951_868_800, "2000-03-01T00:00:00Z"),
        (1_709_164_800, "2024-02-29T00:00:00Z"),
        (1_709_251_200, "2024-03-01T00:00:00Z"),
        (1_735_689_599, "2024-12-31T23:59:59Z"),
        (1_735_689_600, "2025-01-01T00:00:00Z"),
        (4_107_542_400, "2100-03-01T00:00:00Z"),
    ];

    #[test]
    fn a_known_instant_writes_its_known_string() {
        for (secs, want) in TIMES {
            assert_eq!(at(secs), want, "at {secs}");
        }
    }

    #[test]
    fn a_time_shows_the_hour_the_minute_and_the_second() {
        assert_eq!(at(1_763_047_496), "2025-11-13T15:24:56Z");
    }

    #[test]
    fn a_year_without_a_leap_day_skips_29_february() {
        assert_eq!(at(4_075_920_000), "2099-02-28T00:00:00Z");
        assert_eq!(at(4_076_006_400), "2099-03-01T00:00:00Z");
        assert_eq!(at(4_107_456_000), "2100-02-28T00:00:00Z");
    }

    #[test]
    fn a_time_before_the_epoch_reads_as_the_epoch() {
        assert_eq!(
            utc_now(UNIX_EPOCH - Duration::from_secs(1)),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn a_fraction_of_a_second_drops() {
        assert_eq!(
            utc_now(UNIX_EPOCH + Duration::from_millis(1999)),
            "1970-01-01T00:00:01Z"
        );
    }

    #[test]
    fn a_second_init_returns_an_error() {
        let _ = init();
        assert!(init().is_err());
    }
}
