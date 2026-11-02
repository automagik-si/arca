//! The incus-osd readings: Secure Boot, the TPM and the update status, from two GETs under `/os/1.0/system/`.
//!
//! The security document carries live recovery keys in `state.drive_recovery_keys` and `state.pool_recovery_keys`. This module
//! reads four fields and nothing else. Each body is a local value that is dropped when the read returns, so no key, certificate,
//! public key or other field reaches a reading, an error, a log or a metric. The update status text is classified into a kind
//! and the text itself is never stored or formatted.

use arca_incus::IncusRead;
use serde_json::Value;

/// The security document, with Secure Boot and TPM state. Its body also holds recovery keys, which this module never reads.
pub const SECURITY_PATH: &str = "/os/1.0/system/security";
/// The update document: the last check time and the status text.
pub const UPDATE_PATH: &str = "/os/1.0/system/update";

/// With no successful check for this long, in milliseconds, the update is failed (14 days).
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
    /// `Some(true)` when the latest classified check failed or no successful check is known within 14 days, `Some(false)` when
    /// the latest classified check succeeded within 14 days, and `None` (unknown) when neither can be told: no check has been
    /// classified since the sampler started and no check time says the last success is 14 days old.
    pub update_failed: Option<bool>,
    /// When incus-osd last checked for updates, in epoch milliseconds. `None` when the document has no usable check time: the
    /// zero time incus-osd reports before its first check, a missing field, or text that is not an RFC 3339 time.
    pub last_check_ms: Option<u64>,
    /// The check time of the newest check this sampler saw complete, in epoch milliseconds. incus-osd keeps no success time and
    /// sets the check time when a check starts, so this is the check time read alongside a completed status. A failure or an
    /// in-progress status leaves it where it was. `None` until a completed check with a usable time is seen.
    pub last_success_ms: Option<u64>,
}

/// What the sampler carries from one update read to the next: the latest classified result and the last success time. A read
/// whose status is in progress, skipped or unrecognised keeps both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateMemory {
    /// `Some(true)` after a failed check, `Some(false)` after a completed one, `None` before either.
    failed: Option<bool>,
    /// The check time read alongside the newest completed status.
    last_success_ms: Option<u64>,
}

/// What one update status says about the check it describes, by the exact texts incus-osd writes (`internal/update/update.go`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    /// A `Failed …` status.
    Failure,
    /// `… update check completed`, or `… has been updated to version …`.
    Success,
    /// `Checking for …` (a check in progress), `Skipping update check …` (outside a maintenance window), and any other text,
    /// such as an error's own message. It says nothing new about the last check, so the previous result is kept.
    Carry,
}

/// The kind of an update status. Only the kind leaves this function; the text is never kept.
fn classify(status: &str) -> StatusKind {
    if status.starts_with("Failed") {
        StatusKind::Failure
    } else if status.ends_with(" update check completed")
        || status.contains(" has been updated to version ")
    {
        StatusKind::Success
    } else {
        StatusKind::Carry
    }
}

/// Reads the security and update documents at `at`, carrying the update result in `memory`. `None` when either GET fails, as a
/// plain Incus answers 404, so an unreachable or unlike host is unknown and never a reading of `false`. A failed read leaves
/// `memory` as it was.
pub fn read<R: IncusRead>(incus: &R, at: u64, memory: &mut UpdateMemory) -> Option<OsdReading> {
    // Both GETs are sent even when the first fails, so one tick never hides which document an Incus lacks.
    let security = incus.get(SECURITY_PATH);
    let update = incus.get(UPDATE_PATH);
    let (Ok(security), Ok(update)) = (security, update) else {
        return None;
    };
    from_documents(&security, &update, at, memory)
}

/// The reading the two bodies give at `at`, given what `memory` carries from earlier reads. Each field is taken by its JSON path,
/// and a missing or wrong-typed field makes the whole reading `None` and leaves `memory` as it was.
pub fn from_documents(
    security: &Value,
    update: &Value,
    at: u64,
    memory: &mut UpdateMemory,
) -> Option<OsdReading> {
    let secure_boot = security.pointer("/state/secure_boot_enabled")?.as_bool()?;
    let tpm_ok = security.pointer("/state/tpm_status")?.as_str()? == "ok";
    let kind = classify(update.pointer("/state/status")?.as_str()?);
    let last_check_ms = check_time(update);
    match kind {
        StatusKind::Failure => memory.failed = Some(true),
        StatusKind::Success => {
            memory.failed = Some(false);
            // A completed status without a usable time keeps the earlier success time rather than guessing one.
            if last_check_ms.is_some() {
                memory.last_success_ms = last_check_ms;
            }
        }
        StatusKind::Carry => {}
    }
    // The age of the last success. incus-osd sets the check time when each check starts, so the last success is never newer
    // than the last check: with no success seen, the check time still bounds its age from below. With neither, staleness is
    // unknown, never "not stale".
    let stale = memory
        .last_success_ms
        .or(last_check_ms)
        .map(|since| at.saturating_sub(since) >= STALE_CHECK_MS);
    let update_failed = match (memory.failed, stale) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    };
    Some(OsdReading {
        at,
        secure_boot,
        tpm_ok,
        update_failed,
        last_check_ms,
        last_success_ms: memory.last_success_ms,
    })
}

/// The update document's check time. The zero time, a missing field and text that does not parse as RFC 3339 are all "no
/// time", alike: staleness then rests on the last success time alone, and is unknown when there is none. The text is not
/// kept or shown either way.
fn check_time(update: &Value) -> Option<u64> {
    update
        .pointer("/state/last_check")
        .and_then(Value::as_str)
        .and_then(rfc3339_ms)
}

/// Epoch milliseconds of an RFC 3339 time as incus-osd writes it, such as `2026-11-01T10:00:00.123456789-03:00` or a `Z` time;
/// `t` and `z` may be lowercase. `None` for any other text, for a date that does not exist, for a leap second, for an offset
/// past ±23:59, and for a time before 1970, which includes the zero time incus-osd reports before its first check.
pub fn rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let layout = bytes.len() >= 19
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && matches!(bytes[10], b'T' | b't')
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
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        if end == 0 {
            return None;
        }
        let first_three: String = fraction[..end]
            .chars()
            .chain(std::iter::repeat('0'))
            .take(3)
            .collect();
        millis = digits(&first_three)?;
        rest = &fraction[end..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
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
            let (zone_hours, zone_minutes) = (digits(zone.get(0..2)?)?, digits(zone.get(3..5)?)?);
            if zone_hours > 23 || zone_minutes > 59 {
                return None;
            }
            sign * (zone_hours * 3_600 + zone_minutes * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(u64::try_from(millis).ok()?)
}

/// The number of days in `month` (1 to 12) of a proleptic Gregorian `year`.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
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
    /// 2026-11-01T10:00:00Z in epoch milliseconds.
    const AT: u64 = 1_793_527_200_000;
    const DAY: u64 = 86_400_000;

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

    /// One read against a fresh memory.
    fn once(update: &Value, at: u64) -> OsdReading {
        from_documents(
            &security(true, "ok"),
            update,
            at,
            &mut UpdateMemory::default(),
        )
        .expect("both documents have their fields")
    }

    #[test]
    fn rfc3339_reads_utc_offsets_and_fractions() {
        // 2026-11-01T00:00:00Z is 1793491200 seconds.
        assert_eq!(rfc3339_ms("2026-11-01T00:00:00Z"), Some(1_793_491_200_000));
        assert_eq!(rfc3339_ms("2026-11-01T10:00:00Z"), Some(AT));
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
    fn rfc3339_accepts_real_dates_and_lowercase_separators() {
        for text in [
            "2000-02-29T00:00:00Z",
            "2024-02-29T00:00:00Z",
            "2026-01-31T23:59:59Z",
            "2026-04-30T00:00:00Z",
            "2026-11-01t10:00:00z",
            "2026-11-01t10:00:00Z",
            "2026-11-01T10:00:00z",
            "2026-11-01T10:00:00+23:59",
            "2026-11-01T10:00:00-23:59",
        ] {
            assert!(rfc3339_ms(text).is_some(), "{text} is a real time");
        }
        assert_eq!(rfc3339_ms("2026-11-01t10:00:00z"), Some(AT));
    }

    #[test]
    fn rfc3339_refuses_impossible_dates_offsets_and_leap_seconds() {
        for (text, why) in [
            ("2026-02-31T00:00:00Z", "February has no 31st"),
            ("2026-02-29T00:00:00Z", "2026 is not a leap year"),
            ("2100-02-29T00:00:00Z", "2100 is not a leap year"),
            ("2026-04-31T00:00:00Z", "April has 30 days"),
            ("2026-11-00T00:00:00Z", "day 0"),
            ("2026-11-32T00:00:00Z", "day 32"),
            ("2026-11-01T10:00:60Z", "second 60"),
            ("2026-11-01T24:00:00Z", "hour 24"),
            ("2026-11-01T10:60:00Z", "minute 60"),
            ("2026-11-01T10:00:00+24:00", "offset hour 24"),
            ("2026-11-01T10:00:00-23:60", "offset minute 60"),
            ("2026-11-01T10:00:00+99:99", "offset 99:99"),
            ("0001-01-01T00:00:00Z", "before 1970"),
            ("", "empty"),
            ("2026-13-01T00:00:00Z", "month 13"),
            ("2026-11-01 10:00:00Z", "a space, not T"),
            ("2026-11-01T10:00:00", "no zone"),
            ("2026-11-01T10:00:00.Z", "an empty fraction"),
            ("2026-11-01T10:00:00+0100", "an offset without a colon"),
            ("2026-11-01X10:00:00Z", "an unknown separator"),
        ] {
            assert_eq!(rfc3339_ms(text), None, "{why}");
        }
    }

    #[test]
    fn statuses_classify_by_the_texts_incus_osd_writes() {
        for status in [
            "Failed to check for Secure Boot key updates",
            "Failed to check for update for application 'incus'",
            "Failed to refresh system extensions",
            "Failed to reload application 'incus'",
            "Failed to check for OS updates",
        ] {
            assert_eq!(classify(status), StatusKind::Failure, "{status}");
        }
        for status in [
            "Secure Boot update check completed",
            "Application update check completed",
            "OS update check completed",
            "IncusOS has been updated to version 202611010000",
        ] {
            assert_eq!(classify(status), StatusKind::Success, "{status}");
        }
        for status in [
            "Checking for Secure Boot updates",
            "Checking for application updates",
            "Checking for OS updates",
            "Skipping update check outside of maintenance window(s), will check at the beginning of the next maintenance window in 3h0m0s",
            "some error text from an application lookup",
            "",
        ] {
            assert_eq!(classify(status), StatusKind::Carry, "{status}");
        }
    }

    #[test]
    fn a_failed_status_gives_update_failed_and_no_success() {
        let reading = once(
            &update(
                "Failed to refresh system extensions",
                "2026-11-01T10:00:00Z",
            ),
            AT,
        );
        assert_eq!(reading.update_failed, Some(true));
        assert_eq!(reading.last_success_ms, None);
        assert_eq!(reading.last_check_ms, Some(AT));
    }

    #[test]
    fn a_check_15_days_old_is_a_failure_and_one_day_old_is_not() {
        let checked = "2026-11-01T10:00:00Z";
        let old = once(&update("OS update check completed", checked), AT + 15 * DAY);
        assert_eq!(old.update_failed, Some(true), "15 days with no new success");
        assert_eq!(old.last_success_ms, Some(AT));
        let fresh = once(&update("OS update check completed", checked), AT + DAY);
        assert_eq!(fresh.update_failed, Some(false), "1 day is not stale");
    }

    #[test]
    fn an_in_progress_status_with_no_history_is_unknown_and_not_a_success() {
        let reading = once(
            &update("Checking for OS updates", "2026-11-01T10:00:00Z"),
            AT,
        );
        assert_eq!(reading.update_failed, None);
        assert_eq!(reading.last_success_ms, None);
    }

    #[test]
    fn a_zero_or_unparseable_check_time_is_no_time_and_staleness_is_unknown_not_off() {
        for last_check in ["0001-01-01T00:00:00Z", "not a time", "2026-02-31T00:00:00Z"] {
            let reading = once(&update("OS update check completed", last_check), AT);
            assert_eq!(reading.last_check_ms, None, "{last_check}");
            assert_eq!(reading.last_success_ms, None, "{last_check}");
            assert_eq!(
                reading.update_failed, None,
                "{last_check}: with no time the 14-day leg is unknown, so the result is not ok"
            );
        }
        // With a success carried from an earlier read, an unparseable time leaves staleness running from that success.
        let mut memory = UpdateMemory::default();
        let sec = security(true, "ok");
        from_documents(
            &sec,
            &update("OS update check completed", "2026-11-01T10:00:00Z"),
            AT,
            &mut memory,
        )
        .expect("the reading");
        let later = from_documents(
            &sec,
            &update("OS update check completed", "garbage"),
            AT + 15 * DAY,
            &mut memory,
        )
        .expect("the reading");
        assert_eq!(later.update_failed, Some(true));
        assert_eq!(later.last_success_ms, Some(AT));
    }

    #[test]
    fn tpm_is_ok_only_for_the_ok_status_and_secure_boot_is_the_flag() {
        let checked = update("OS update check completed", "2026-11-01T10:00:00Z");
        for (status, ok) in [("ok", true), ("pcr mismatch", false), ("swtpm", false)] {
            let reading = from_documents(
                &security(false, status),
                &checked,
                AT,
                &mut UpdateMemory::default(),
            )
            .expect("the reading");
            assert_eq!(reading.tpm_ok, ok, "{status}");
            assert!(!reading.secure_boot);
        }
    }

    #[test]
    fn a_body_missing_a_needed_field_is_no_reading_and_keeps_the_memory() {
        let mut memory = UpdateMemory::default();
        let no_tpm = json!({ "state": { "secure_boot_enabled": true } });
        let failed = update("Failed to check for OS updates", "2026-11-01T10:00:00Z");
        assert_eq!(from_documents(&no_tpm, &failed, AT, &mut memory), None);
        let no_status = json!({ "state": { "last_check": "2026-11-01T10:00:00Z" } });
        assert_eq!(
            from_documents(&security(true, "ok"), &no_status, AT, &mut memory),
            None
        );
        assert_eq!(memory, UpdateMemory::default());
    }

    #[test]
    fn no_secret_text_reaches_a_reading_or_its_debug_form() {
        let mut memory = UpdateMemory::default();
        let reading = from_documents(
            &security(true, "ok"),
            &update("Failed to check for OS updates", "2026-11-01T10:00:00Z"),
            AT,
            &mut memory,
        )
        .expect("the reading");
        for shown in [format!("{reading:?}"), format!("{memory:?}")] {
            assert!(!shown.contains(FAKE_KEY), "a fake key was shown");
            assert!(!shown.contains("FAKE-PUBLIC-KEY"), "a fake key was shown");
            assert!(!shown.contains("Failed to"), "the status text was kept");
        }
    }
}
