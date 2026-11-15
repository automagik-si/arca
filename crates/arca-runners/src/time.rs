//! Wall-clock helpers: epoch milliseconds, and RFC 3339 both ways.

use std::time::{SystemTime, UNIX_EPOCH};

/// Now, in epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Epoch milliseconds of an RFC 3339 time as GitHub writes it. `None` for anything else, and for GitHub's zero time.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    arca_metrics::rfc3339_ms(text)
}

/// An epoch-millisecond time as RFC 3339 in UTC, with milliseconds.
pub fn format_rfc3339(ms: u64) -> String {
    let seconds = ms / 1000;
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60,
        ms % 1000
    )
}

/// Howard Hinnant's days-to-civil, for days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips() {
        for ms in [0, 1_731_600_000_123, 1_763_123_456_789, 951_782_400_000] {
            assert_eq!(parse_rfc3339(&format_rfc3339(ms)), Some(ms), "{ms}");
        }
        assert_eq!(
            format_rfc3339(1_763_078_400_000),
            "2025-11-14T00:00:00.000Z"
        );
    }
}
