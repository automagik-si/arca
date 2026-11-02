//! The incus-osd readings: Secure Boot, the TPM and the update status, from two GETs under `/os/1.0/system/`.
//!
//! The security document carries live recovery keys in `state.drive_recovery_keys` and `state.pool_recovery_keys`. This module
//! reads four fields and nothing else. Each body is a local value that is dropped when the read returns, so no key, certificate,
//! public key or other field reaches a reading, an error, a log or a metric. The one piece of update text kept is its failure
//! flag, and the text itself is never stored or formatted.

use arca_incus::IncusRead;
use serde_json::Value;

/// The security document, with Secure Boot and TPM state. Its body also holds recovery keys, which this module never reads.
pub const SECURITY_PATH: &str = "/os/1.0/system/security";
/// The update document: the last check time and the status text.
pub const UPDATE_PATH: &str = "/os/1.0/system/update";

/// A last check this old, in milliseconds, makes the update failed (14 days).
pub const STALE_CHECK_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// What incus-osd reported at one read. A read that fails, or a body without the fields this module needs, is no reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsdReading {
    /// Epoch milliseconds when the read ran.
    pub at: u64,
    /// Whether Secure Boot is enabled.
    pub secure_boot: bool,
    /// Whether incus-osd reports the TPM as `ok`. `pcr mismatch` and `swtpm` are not ok.
    pub tpm_ok: bool,
    /// Whether the status reports a failure, or the last check is at least 14 days old.
    pub update_failed: bool,
    /// When incus-osd last checked for updates, in epoch milliseconds. `None` when the document has no usable check time, as
    /// before the first check, when the time is the zero time.
    pub last_check_ms: Option<u64>,
    /// The last check that reported no failure, in epoch milliseconds. incus-osd keeps no success time, and it sets the check
    /// time when a check starts, so this is the check time while the status reports no failure. `None` otherwise.
    pub last_success_ms: Option<u64>,
}

/// Reads the security and update documents at `at`. `None` when either GET fails, as a plain Incus answers 404, so an unreachable
/// or unlike host is unknown and never a reading of `false`.
pub fn read<R: IncusRead>(incus: &R, at: u64) -> Option<OsdReading> {
    // Both GETs are sent even when the first fails, so one tick never hides which document an Incus lacks.
    let security = incus.get(SECURITY_PATH);
    let update = incus.get(UPDATE_PATH);
    let (Ok(security), Ok(update)) = (security, update) else {
        return None;
    };
    from_documents(&security, &update, at)
}

/// The reading the two bodies give at `at`. Each field is taken by its JSON path, and a missing or wrong-typed field makes the
/// whole reading `None`.
pub fn from_documents(security: &Value, update: &Value, at: u64) -> Option<OsdReading> {
    let secure_boot = security.pointer("/state/secure_boot_enabled")?.as_bool()?;
    let tpm_ok = security.pointer("/state/tpm_status")?.as_str()? == "ok";
    let failed = reports_failure(update.pointer("/state/status")?.as_str()?);
    let last_check_ms = update
        .pointer("/state/last_check")
        .and_then(Value::as_str)
        .and_then(rfc3339_ms);
    let stale = last_check_ms.is_some_and(|checked| at.saturating_sub(checked) >= STALE_CHECK_MS);
    Some(OsdReading {
        at,
        secure_boot,
        tpm_ok,
        update_failed: failed || stale,
        last_check_ms,
        last_success_ms: if failed { None } else { last_check_ms },
    })
}

/// Whether an update status reports a failure. Every failure message incus-osd writes starts with `Failed`. A status that carries
/// an error's own text is not classified here, so it reads as no failure until the check time ages past 14 days.
fn reports_failure(status: &str) -> bool {
    status.starts_with("Failed")
}

/// Epoch milliseconds of an RFC 3339 time as incus-osd writes it, such as `2026-11-01T10:00:00.123456789-03:00` or a `Z` time.
/// `None` for any other text, and for a time before 1970, which includes the zero time incus-osd reports before its first check.
pub fn rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let layout = bytes.len() >= 19
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !layout {
        return None;
    }
    let year = digits(text.get(0..4)?)?;
    let month = digits(text.get(5..7)?)?;
    let day = digits(text.get(8..10)?)?;
    let hour = digits(text.get(11..13)?)?;
    let minute = digits(text.get(14..16)?)?;
    let second = digits(text.get(17..19)?)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        let first_three: String = fraction[..end]
            .chars()
            .chain(std::iter::repeat('0'))
            .take(3)
            .collect();
        millis = digits(&first_three)?;
        if end == 0 {
            return None;
        }
        rest = &fraction[end..];
    }
    let offset = match rest {
        "Z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let zone = &rest[1..];
            if zone.len() != 5 || zone.as_bytes()[2] != b':' {
                return None;
            }
            sign * (digits(zone.get(0..2)?)? * 3_600 + digits(zone.get(3..5)?)? * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(u64::try_from(millis).ok()?)
}

/// The value of a non-empty run of ASCII digits, as a signed number. `None` for anything else.
fn digits(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days from 1970-01-01 to a proleptic Gregorian date, negative before it (Howard Hinnant's civil-day count).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let march_based = (month + 9) % 12;
    let day_of_year = (153 * march_based + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A fixture secret. It is obviously fake and must never appear in any reading.
    const FAKE_KEY: &str = "FAKE-RECOVERY-KEY-NOT-REAL-0001";

    fn security(secure_boot: bool, tpm: &str) -> Value {
        json!({
            "config": { "encryption_recovery_keys": [FAKE_KEY] },
            "state": {
                "secure_boot_enabled": secure_boot,
                "tpm_status": tpm,
                "drive_recovery_keys": { "drive1": FAKE_KEY },
                "pool_recovery_keys": { "pool1": FAKE_KEY },
                "tpm_public_key": "FAKE-PUBLIC-KEY-NOT-REAL",
            },
        })
    }

    fn update(status: &str, last_check: &str) -> Value {
        json!({ "config": {}, "state": { "status": status, "last_check": last_check, "needs_reboot": false } })
    }

    #[test]
    fn rfc3339_reads_utc_offsets_and_fractions() {
        // 2026-11-01T00:00:00Z is 1793491200 seconds.
        assert_eq!(rfc3339_ms("2026-11-01T00:00:00Z"), Some(1_793_491_200_000));
        assert_eq!(rfc3339_ms("2026-11-01T10:00:00Z"), Some(1_793_527_200_000));
        assert_eq!(
            rfc3339_ms("2026-11-01T10:00:00-03:00"),
            Some(1_793_538_000_000)
        );
        assert_eq!(
            rfc3339_ms("2026-11-01T10:00:00.5+01:00"),
            Some(1_793_523_600_500)
        );
        assert_eq!(
            rfc3339_ms("2026-11-01T10:00:00.123456789Z"),
            Some(1_793_527_200_123)
        );
    }

    #[test]
    fn rfc3339_refuses_the_zero_time_and_malformed_text() {
        assert_eq!(rfc3339_ms("0001-01-01T00:00:00Z"), None, "before 1970");
        assert_eq!(rfc3339_ms(""), None);
        assert_eq!(rfc3339_ms("2026-13-01T00:00:00Z"), None, "month 13");
        assert_eq!(rfc3339_ms("2026-11-01 10:00:00Z"), None, "a space, not T");
        assert_eq!(rfc3339_ms("2026-11-01T10:00:00"), None, "no zone");
        assert_eq!(
            rfc3339_ms("2026-11-01T10:00:00.Z"),
            None,
            "an empty fraction"
        );
        assert_eq!(
            rfc3339_ms("2026-11-01T10:00:00+0100"),
            None,
            "an offset without a colon"
        );
    }

    #[test]
    fn a_failed_status_gives_update_failed_and_no_success() {
        let reading = from_documents(
            &security(true, "ok"),
            &update(
                "Failed to refresh system extensions",
                "2026-11-01T10:00:00Z",
            ),
            1_793_527_200_000,
        )
        .expect("both documents have their fields");
        assert!(reading.update_failed);
        assert_eq!(reading.last_success_ms, None);
        assert_eq!(reading.last_check_ms, Some(1_793_527_200_000));
    }

    #[test]
    fn a_check_15_days_old_is_a_failure_and_one_day_old_is_not() {
        let checked = "2026-11-01T10:00:00Z";
        let at = 1_793_527_200_000;
        let fifteen_days = at + 15 * 86_400_000;
        let one_day = at + 86_400_000;
        let old = from_documents(
            &security(true, "ok"),
            &update("OS update check completed", checked),
            fifteen_days,
        )
        .expect("the reading");
        assert!(old.update_failed, "15 days with no failed status");
        assert_eq!(
            old.last_success_ms,
            Some(at),
            "the status is not failed, so the check time is the success"
        );
        let fresh = from_documents(
            &security(true, "ok"),
            &update("OS update check completed", checked),
            one_day,
        )
        .expect("the reading");
        assert!(!fresh.update_failed, "1 day is not stale");
    }

    #[test]
    fn a_zero_check_time_is_no_check_and_no_staleness() {
        let reading = from_documents(
            &security(true, "ok"),
            &update("Checking for Secure Boot updates", "0001-01-01T00:00:00Z"),
            1_793_527_200_000,
        )
        .expect("the reading");
        assert_eq!(reading.last_check_ms, None);
        assert_eq!(reading.last_success_ms, None);
        assert!(!reading.update_failed);
    }

    #[test]
    fn tpm_is_ok_only_for_the_ok_status_and_secure_boot_is_the_flag() {
        let at = 1_793_527_200_000;
        let checked = update("OS update check completed", "2026-11-01T10:00:00Z");
        for (status, ok) in [("ok", true), ("pcr mismatch", false), ("swtpm", false)] {
            let reading =
                from_documents(&security(false, status), &checked, at).expect("the reading");
            assert_eq!(reading.tpm_ok, ok, "{status}");
            assert!(!reading.secure_boot);
        }
    }

    #[test]
    fn a_body_missing_a_needed_field_is_no_reading() {
        let at = 1_793_527_200_000;
        let no_tpm = json!({ "state": { "secure_boot_enabled": true } });
        let checked = update("OS update check completed", "2026-11-01T10:00:00Z");
        assert_eq!(from_documents(&no_tpm, &checked, at), None);
        let no_status = json!({ "state": { "last_check": "2026-11-01T10:00:00Z" } });
        assert_eq!(from_documents(&security(true, "ok"), &no_status, at), None);
    }

    #[test]
    fn no_secret_text_reaches_a_reading_or_its_debug_form() {
        let reading = from_documents(
            &security(true, "ok"),
            &update("Failed to check for OS updates", "2026-11-01T10:00:00Z"),
            1_793_527_200_000,
        )
        .expect("the reading");
        let shown = format!("{reading:?}");
        assert!(!shown.contains(FAKE_KEY), "{shown}");
        assert!(!shown.contains("FAKE-PUBLIC-KEY"), "{shown}");
    }
}
