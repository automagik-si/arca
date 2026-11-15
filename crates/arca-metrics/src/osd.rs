//! The incus-osd readings: Secure Boot, the TPM and the update status, from two GETs under `/os/1.0/system/`.
//!
//! The security document carries live recovery keys in `state.drive_recovery_keys` and `state.pool_recovery_keys`. This module
//! reads four fields and nothing else. Each body is a local value that is dropped when the read returns, so no key, certificate,
//! public key or other field reaches a reading, an error, a log or a metric. The update status text is classified into a kind
//! and the text itself is never stored or formatted; the one exception is the version a completed `… has been updated to
//! version V` status names, kept only when it is a plain version token.
//!
//! The same update read also gives [`OsUpdate`], the plain update state the `os_update` verb serves: the version, channel,
//! check frequency, automatic flag, check and update times, the health versions, and the OS's own error kind and trust
//! fields, passed through as the OS classified them. The raw `last_error` text is never read.

use arca_incus::IncusRead;
use serde::Serialize;
use serde_json::Value;

/// The security document, with Secure Boot and TPM state. Its body also holds recovery keys, which this module never reads.
pub const SECURITY_PATH: &str = "/os/1.0/system/security";
/// The update document: the last check time and the status text.
pub const UPDATE_PATH: &str = "/os/1.0/system/update";
/// The OS document, whose `environment.os_version` is the version when the update document's health has none.
pub const OS_PATH: &str = "/os/1.0";

/// The error kinds incus-osd's update check reports in `state.last_error_kind`. Any other text is not passed on.
const ERROR_KINDS: [&str; 5] = ["network", "parse", "signature", "trust", "other"];

/// With no successful check for this long, in milliseconds, the update is failed (14 days).
pub const STALE_CHECK_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// A node up this long with no completed update check is failed, in milliseconds.
pub const NEVER_CHECKED_MS: u64 = 24 * 60 * 60 * 1000;

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
    /// the latest classified check succeeded within 14 days. Also `Some(true)` when no completed check is known, no usable
    /// check time exists and node uptime is at least 24 hours. `None` (unknown) when none of these can be established.
    pub update_failed: Option<bool>,
    /// When incus-osd last checked for updates, in epoch milliseconds. `None` when the document has no usable check time: the
    /// zero time incus-osd reports before its first check, a missing field, or text that is not an RFC 3339 time.
    pub last_check_ms: Option<u64>,
    /// The check time of the newest check this sampler saw complete, in epoch milliseconds. incus-osd keeps no success time and
    /// sets the check time when a check starts, so this is the check time read alongside a completed status. A failure or an
    /// in-progress status leaves it where it was. `None` until a completed check with a usable time is seen.
    pub last_success_ms: Option<u64>,
}

/// What the sampler carries from one update read to the next: the latest classified result, the last success time, and the
/// last update a completed status named. A read whose status is in progress, skipped or unrecognised keeps all three.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateMemory {
    /// `Some(true)` after a failed check, `Some(false)` after a completed one, `None` before either.
    failed: Option<bool>,
    /// The check time read alongside the newest completed status.
    last_success_ms: Option<u64>,
    /// The version the newest `… has been updated to version V` status named, with the check time read alongside it.
    updated: Option<(String, Option<u64>)>,
}

/// The update state of one node as the `os_update` verb serves it, in camelCase with `null` for what is unknown. `known` is
/// false when the update document could not be read, as on a plain Incus that answers 404: never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OsUpdate {
    pub known: bool,
    /// Epoch milliseconds of the read.
    pub as_of: u64,
    /// `state.health.current_version`, or `environment.os_version` of `/os/1.0` when the health has none.
    pub version: Option<String>,
    pub channel: Option<String>,
    /// `config.check_frequency` verbatim, such as `6h` or `never`.
    pub check_frequency: Option<String>,
    /// Whether automatic updates are on: `!config.auto_updates_off`, which incus-osd omits when false.
    pub automatic: Option<bool>,
    /// `state.last_check`, when it is a usable time.
    pub last_check_at: Option<u64>,
    /// The check time of the newest check this kernel saw complete.
    pub last_success_at: Option<u64>,
    /// The version the newest completed `… has been updated to version V` status named.
    pub last_updated_version: Option<String>,
    /// The check time read alongside that status.
    pub last_updated_at: Option<u64>,
    pub needs_reboot: Option<bool>,
    pub pending_version: Option<String>,
    pub previous_version: Option<String>,
    pub good_version: Option<String>,
    /// `state.health.last_result`, verbatim.
    pub last_result: Option<String>,
    /// `state.health.last_reason`, verbatim.
    pub last_reason: Option<String>,
    /// `state.last_error_kind` as the OS classified it: one of network, parse, signature, trust or other. `null` when absent,
    /// empty or any other text; the kernel never classifies an error itself.
    pub last_error_kind: Option<String>,
    /// The update CA trust fields, `null` when the OS reports none of them.
    pub trust: Option<UpdateTrust>,
}

/// The update CA trust fields of the update document, passed through. A fingerprint is kept only in its SHA-256 form (hex
/// pairs joined by colons).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateTrust {
    pub embedded_ca_fingerprint: Option<String>,
    pub presented_ca_fingerprint: Option<String>,
    /// Consecutive checks whose update server key was not trusted.
    pub failures: Option<u64>,
    pub escalated: Option<bool>,
}

impl OsUpdate {
    /// The state of a node whose update document could not be read at `at`: every field `null`.
    pub fn unknown(at: u64) -> Self {
        OsUpdate {
            known: false,
            as_of: at,
            version: None,
            channel: None,
            check_frequency: None,
            automatic: None,
            last_check_at: None,
            last_success_at: None,
            last_updated_version: None,
            last_updated_at: None,
            needs_reboot: None,
            pending_version: None,
            previous_version: None,
            good_version: None,
            last_result: None,
            last_reason: None,
            last_error_kind: None,
            trust: None,
        }
    }
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

/// Reads the security and update documents at `at`, carrying the update result in `memory`. The reading is `None` when either
/// GET fails, as a plain Incus answers 404, so an unreachable or unlike host is unknown and never a reading of `false`. A failed
/// read leaves `memory` as it was. The update state comes from the same update GET, after the reading has advanced `memory`;
/// `/os/1.0` is asked only when the update document's health names no version.
pub fn read<R: IncusRead>(
    incus: &R,
    at: u64,
    memory: &mut UpdateMemory,
    uptime_ms: Option<u64>,
) -> (Option<OsdReading>, OsUpdate) {
    // Both GETs are sent even when the first fails, so one tick never hides which document an Incus lacks.
    let security = incus.get(SECURITY_PATH);
    let update = incus.get(UPDATE_PATH);
    let reading = match (&security, &update) {
        (Ok(security), Ok(update)) => from_documents(security, update, at, memory, uptime_ms),
        _ => None,
    };
    let state = match &update {
        Ok(update) => {
            let os = text_at(update, "/state/health/current_version")
                .is_none()
                .then(|| incus.get(OS_PATH).ok())
                .flatten();
            update_state(update, os.as_ref(), at, memory)
        }
        Err(_) => OsUpdate::unknown(at),
    };
    (reading, state)
}

/// The update state the update document and, when given, the `/os/1.0` document say at `at`, with the success and update
/// times `memory` carries. A body that is not an object is not known.
pub fn update_state(
    update: &Value,
    os: Option<&Value>,
    at: u64,
    memory: &UpdateMemory,
) -> OsUpdate {
    if !update.is_object() {
        return OsUpdate::unknown(at);
    }
    let version = text_at(update, "/state/health/current_version")
        .or_else(|| os.and_then(|os| text_at(os, "/environment/os_version")));
    let automatic = update
        .pointer("/config")
        .filter(|config| config.is_object())
        .map(|config| {
            !config
                .get("auto_updates_off")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
    let last_error_kind = text_at(update, "/state/last_error_kind")
        .filter(|kind| ERROR_KINDS.contains(&kind.as_str()));
    let embedded = fingerprint_at(update, "/state/embedded_ca_fingerprint");
    let presented = fingerprint_at(update, "/state/presented_ca_fingerprint");
    let failures = update
        .pointer("/state/trust_failures")
        .and_then(Value::as_u64);
    let escalated = update
        .pointer("/state/trust_escalated")
        .and_then(Value::as_bool);
    let trust =
        (embedded.is_some() || presented.is_some() || failures.is_some() || escalated.is_some())
            .then_some(UpdateTrust {
                embedded_ca_fingerprint: embedded,
                presented_ca_fingerprint: presented,
                failures,
                escalated,
            });
    let (last_updated_version, last_updated_at) = match &memory.updated {
        Some((version, at)) => (Some(version.clone()), *at),
        None => (None, None),
    };
    OsUpdate {
        known: true,
        as_of: at,
        version,
        channel: text_at(update, "/config/channel"),
        check_frequency: text_at(update, "/config/check_frequency"),
        automatic,
        last_check_at: check_time(update),
        last_success_at: memory.last_success_ms,
        last_updated_version,
        last_updated_at,
        needs_reboot: update
            .pointer("/state/needs_reboot")
            .and_then(Value::as_bool),
        pending_version: text_at(update, "/state/health/pending_version"),
        previous_version: text_at(update, "/state/health/previous_version"),
        good_version: text_at(update, "/state/health/good_version"),
        last_result: text_at(update, "/state/health/last_result"),
        last_reason: text_at(update, "/state/health/last_reason"),
        last_error_kind,
        trust,
    }
}

/// The non-empty string at `pointer`. incus-osd omits or empties a field it has no value for, so both are `None`.
fn text_at(document: &Value, pointer: &str) -> Option<String> {
    document
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The SHA-256 fingerprint at `pointer`, as hex pairs joined by colons. Any other text is `None`.
fn fingerprint_at(document: &Value, pointer: &str) -> Option<String> {
    text_at(document, pointer).filter(|text| {
        text.len() <= 95
            && text
                .split(':')
                .all(|pair| pair.len() == 2 && pair.bytes().all(|b| b.is_ascii_hexdigit()))
    })
}

/// The version a `… has been updated to version V` status names, when it is a plain version token of at most 64 letters,
/// digits, dots, dashes, pluses and underscores. Any other text, and every other status, is `None`, so no other status text is
/// kept.
fn updated_version(status: &str) -> Option<String> {
    let (_, version) = status.split_once(" has been updated to version ")?;
    let plain = !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'));
    plain.then(|| version.to_string())
}

/// The reading the two bodies give at `at`, given what `memory` carries from earlier reads. Each field is taken by its JSON path,
/// and a missing or wrong-typed field makes the whole reading `None` and leaves `memory` as it was.
pub fn from_documents(
    security: &Value,
    update: &Value,
    at: u64,
    memory: &mut UpdateMemory,
    uptime_ms: Option<u64>,
) -> Option<OsdReading> {
    let secure_boot = security.pointer("/state/secure_boot_enabled")?.as_bool()?;
    let tpm_ok = security.pointer("/state/tpm_status")?.as_str()? == "ok";
    let status = update.pointer("/state/status")?.as_str()?;
    let kind = classify(status);
    let last_check_ms = check_time(update);
    match kind {
        StatusKind::Failure => memory.failed = Some(true),
        StatusKind::Success => {
            memory.failed = Some(false);
            // A completed status without a usable time keeps the earlier success time rather than guessing one.
            if last_check_ms.is_some() {
                memory.last_success_ms = last_check_ms;
            }
            if let Some(version) = updated_version(status) {
                memory.updated = Some((version, last_check_ms));
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
    // This check measures how long the node has run. Sampler start time resets on every arca restart and would postpone
    // the alert by another 24 hours, so use node uptime from procfs. Unknown uptime cannot establish a failure.
    let never_checked = last_check_ms.is_none()
        && memory.last_success_ms.is_none()
        && memory.failed != Some(false)
        && uptime_ms.is_some_and(|uptime| uptime >= NEVER_CHECKED_MS);
    let update_failed = update_failed.or(never_checked.then_some(true));
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
            None,
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

    fn never_checked_documents() -> Vec<Value> {
        vec![
            json!({ "state": { "status": "" } }),
            update("Checking for OS updates", "0001-01-01T00:00:00Z"),
            update("Skipping update check", "not a time"),
        ]
    }

    #[test]
    fn never_checked_at_23_hours_or_unknown_uptime_is_unknown() {
        for doc in never_checked_documents() {
            for uptime in [None, Some(23 * 60 * 60 * 1000), Some(NEVER_CHECKED_MS - 1)] {
                let reading = from_documents(
                    &security(true, "ok"),
                    &doc,
                    AT,
                    &mut UpdateMemory::default(),
                    uptime,
                )
                .expect("the reading");
                assert_eq!(reading.update_failed, None);
            }
        }
    }

    #[test]
    fn never_checked_at_24_hours_and_above_is_failed() {
        for doc in never_checked_documents() {
            for uptime in [NEVER_CHECKED_MS, NEVER_CHECKED_MS + 1, 2 * NEVER_CHECKED_MS] {
                let reading = from_documents(
                    &security(true, "ok"),
                    &doc,
                    AT,
                    &mut UpdateMemory::default(),
                    Some(uptime),
                )
                .expect("the reading");
                assert_eq!(reading.update_failed, Some(true));
                assert_eq!(reading.last_success_ms, None);
            }
        }
    }

    #[test]
    fn a_completed_check_clears_never_checked_and_the_next_read_stays_ok() {
        let mut memory = UpdateMemory::default();
        let sec = security(true, "ok");
        let initial = from_documents(
            &sec,
            &update("", ""),
            AT,
            &mut memory,
            Some(NEVER_CHECKED_MS),
        )
        .expect("the reading");
        assert_eq!(initial.update_failed, Some(true));
        let done = from_documents(
            &sec,
            &update("OS update check completed", "2026-11-01T10:00:00Z"),
            AT,
            &mut memory,
            Some(NEVER_CHECKED_MS),
        )
        .expect("the reading");
        assert_eq!(done.update_failed, Some(false));
        assert_eq!(done.last_success_ms, Some(AT));
        let next = from_documents(
            &sec,
            &update("Checking for OS updates", ""),
            AT + 300_000,
            &mut memory,
            Some(NEVER_CHECKED_MS + 300_000),
        )
        .expect("the reading");
        assert_eq!(next.update_failed, Some(false));
        assert_eq!(next.last_success_ms, Some(AT));
    }

    #[test]
    fn a_completed_status_without_a_usable_time_is_not_never_checked() {
        let mut memory = UpdateMemory::default();
        let sec = security(true, "ok");
        let reading = from_documents(
            &sec,
            &update("OS update check completed", "0001-01-01T00:00:00Z"),
            AT,
            &mut memory,
            Some(NEVER_CHECKED_MS),
        )
        .expect("the reading");
        assert_eq!(reading.update_failed, None);
        assert_eq!(reading.last_success_ms, None);
    }

    #[test]
    fn an_in_progress_status_carries_a_completed_result_without_a_usable_time() {
        let mut memory = UpdateMemory::default();
        let sec = security(true, "ok");
        from_documents(
            &sec,
            &update("OS update check completed", "0001-01-01T00:00:00Z"),
            AT,
            &mut memory,
            Some(NEVER_CHECKED_MS),
        )
        .expect("the reading");
        // The in-progress read carries the earlier completed result.
        let reading = from_documents(
            &sec,
            &update("Checking for OS updates", "0001-01-01T00:00:00Z"),
            AT,
            &mut memory,
            Some(NEVER_CHECKED_MS),
        )
        .expect("the reading");
        assert_eq!(reading.update_failed, None);
        assert_eq!(reading.last_success_ms, None);
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
            None,
        )
        .expect("the reading");
        let later = from_documents(
            &sec,
            &update("OS update check completed", "garbage"),
            AT + 15 * DAY,
            &mut memory,
            None,
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
                None,
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
        assert_eq!(
            from_documents(&no_tpm, &failed, AT, &mut memory, None),
            None
        );
        let no_status = json!({ "state": { "last_check": "2026-11-01T10:00:00Z" } });
        assert_eq!(
            from_documents(&security(true, "ok"), &no_status, AT, &mut memory, None),
            None
        );
        assert_eq!(memory, UpdateMemory::default());
    }

    /// A fake Incus serving the update document, and `/os/1.0` when given. Any other path, and both when `update` is `None`,
    /// answers 404 as a plain Incus does. It records every path asked.
    struct Fake {
        update: Option<Value>,
        os: Option<Value>,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(update: Option<Value>, os: Option<Value>) -> Self {
            Fake {
                update,
                os,
                asked: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl IncusRead for Fake {
        fn get(&self, path: &str) -> Result<Value, arca_incus::IncusError> {
            self.asked
                .lock()
                .expect("not poisoned")
                .push(path.to_string());
            let found = match path {
                SECURITY_PATH if self.update.is_some() => Some(security(true, "ok")),
                UPDATE_PATH => self.update.clone(),
                OS_PATH => self.os.clone(),
                _ => None,
            };
            found.ok_or(arca_incus::IncusError::Status {
                code: 404,
                message: "not found".to_string(),
            })
        }

        fn get_within(
            &self,
            path: &str,
            _timeout: std::time::Duration,
        ) -> Result<Value, arca_incus::IncusError> {
            self.get(path)
        }
    }

    /// A full Arca OS update document, with the trust fields of the OS's trust-status branch.
    fn arca_update(status: &str) -> Value {
        json!({
            "config": { "auto_reboot": false, "channel": "stable", "check_frequency": "6h" },
            "state": {
                "last_check": "2026-11-01T10:00:00Z",
                "status": status,
                "needs_reboot": true,
                "health": {
                    "current_version": "202611010000",
                    "previous_version": "202610150000",
                    "good_version": "202610150000",
                    "pending_version": "202611080000",
                    "last_result": "baking",
                    "last_reason": "waiting for the bake time",
                },
                "last_error_kind": "trust",
                "last_error": "FAKE-INTERNAL-ERROR-TEXT x509 unknown authority",
                "embedded_ca_fingerprint": "AA:BB:CC:DD",
                "presented_ca_fingerprint": "11:22:33:44",
                "trust_failures": 3,
                "trust_escalated": true,
            },
        })
    }

    #[test]
    fn the_update_state_maps_the_documents_fields() {
        let incus = Fake::new(Some(arca_update("OS update check completed")), None);
        let (reading, state) = read(&incus, AT, &mut UpdateMemory::default(), None);
        assert!(reading.is_some());
        assert!(state.known);
        assert_eq!(state.as_of, AT);
        assert_eq!(state.version.as_deref(), Some("202611010000"));
        assert_eq!(state.channel.as_deref(), Some("stable"));
        assert_eq!(state.check_frequency.as_deref(), Some("6h"));
        assert_eq!(
            state.automatic,
            Some(true),
            "auto_updates_off is omitted when false"
        );
        assert_eq!(state.last_check_at, Some(AT));
        assert_eq!(state.last_success_at, Some(AT));
        assert_eq!(state.needs_reboot, Some(true));
        assert_eq!(state.pending_version.as_deref(), Some("202611080000"));
        assert_eq!(state.previous_version.as_deref(), Some("202610150000"));
        assert_eq!(state.good_version.as_deref(), Some("202610150000"));
        assert_eq!(state.last_result.as_deref(), Some("baking"));
        assert_eq!(
            state.last_reason.as_deref(),
            Some("waiting for the bake time")
        );
        assert!(
            !incus
                .asked
                .lock()
                .expect("not poisoned")
                .iter()
                .any(|path| path == OS_PATH),
            "the health names the version, so /os/1.0 is not asked"
        );
    }

    #[test]
    fn automatic_updates_off_is_read_from_the_config() {
        let mut doc = arca_update("OS update check completed");
        doc["config"]["auto_updates_off"] = json!(true);
        let state = update_state(&doc, None, AT, &UpdateMemory::default());
        assert_eq!(state.automatic, Some(false));
        doc["config"]
            .as_object_mut()
            .expect("the config object")
            .remove("auto_updates_off");
        let state = update_state(&doc, None, AT, &UpdateMemory::default());
        assert_eq!(
            state.automatic,
            Some(true),
            "omitted means off is false: on by default"
        );
        let state = update_state(&json!({ "state": {} }), None, AT, &UpdateMemory::default());
        assert_eq!(state.automatic, None, "no config object: unknown, not on");
    }

    #[test]
    fn the_version_falls_back_to_the_os_document() {
        let mut doc = arca_update("OS update check completed");
        doc["state"]["health"]
            .as_object_mut()
            .expect("the health object")
            .remove("current_version");
        let os = json!({ "environment": { "os_name": "Arca OS", "os_version": "202610010000" } });
        let incus = Fake::new(Some(doc.clone()), Some(os));
        let (_, state) = read(&incus, AT, &mut UpdateMemory::default(), None);
        assert_eq!(state.version.as_deref(), Some("202610010000"));
        assert!(
            incus
                .asked
                .lock()
                .expect("not poisoned")
                .iter()
                .any(|path| path == OS_PATH)
        );
        // Without either version the field is null, and a failed /os/1.0 read is not a failed update read.
        let incus = Fake::new(Some(doc), None);
        let (_, state) = read(&incus, AT, &mut UpdateMemory::default(), None);
        assert!(state.known);
        assert_eq!(state.version, None);
    }

    #[test]
    fn the_error_kind_and_trust_fields_pass_through_unchanged() {
        let state = update_state(
            &arca_update("Failed to check for OS updates"),
            None,
            AT,
            &UpdateMemory::default(),
        );
        assert_eq!(state.last_error_kind.as_deref(), Some("trust"));
        assert_eq!(
            state.trust,
            Some(UpdateTrust {
                embedded_ca_fingerprint: Some("AA:BB:CC:DD".to_string()),
                presented_ca_fingerprint: Some("11:22:33:44".to_string()),
                failures: Some(3),
                escalated: Some(true),
            })
        );
        for kind in ERROR_KINDS {
            let mut doc = arca_update("Failed to check for OS updates");
            doc["state"]["last_error_kind"] = json!(kind);
            let state = update_state(&doc, None, AT, &UpdateMemory::default());
            assert_eq!(state.last_error_kind.as_deref(), Some(kind));
        }
    }

    #[test]
    fn absent_error_and_trust_fields_are_null_and_never_derived() {
        // An OS before the trust-status fields: a failed status still gives no kind, as the kernel never classifies.
        let doc = update("Failed to check for OS updates", "2026-11-01T10:00:00Z");
        let state = update_state(&doc, None, AT, &UpdateMemory::default());
        assert!(state.known);
        assert_eq!(state.last_error_kind, None);
        assert_eq!(state.trust, None);
        let mut doc = arca_update("Failed to check for OS updates");
        doc["state"]["last_error_kind"] = json!("");
        doc["state"]["embedded_ca_fingerprint"] = json!("not a fingerprint");
        let state = update_state(&doc, None, AT, &UpdateMemory::default());
        assert_eq!(state.last_error_kind, None, "an empty kind is no kind");
        assert_eq!(
            state.trust.and_then(|trust| trust.embedded_ca_fingerprint),
            None,
            "only a SHA-256 fingerprint is passed on"
        );
    }

    #[test]
    fn a_plain_incus_or_a_failed_read_is_not_known() {
        let incus = Fake::new(None, None);
        let (reading, state) = read(&incus, AT, &mut UpdateMemory::default(), None);
        assert_eq!(reading, None);
        assert_eq!(state, OsUpdate::unknown(AT));
        assert!(!state.known);
        let state = update_state(&json!("not a document"), None, AT, &UpdateMemory::default());
        assert!(!state.known);
    }

    #[test]
    fn an_updated_status_names_the_last_update_with_its_check_time() {
        let mut memory = UpdateMemory::default();
        let incus = Fake::new(
            Some(arca_update(
                "Arca OS has been updated to version 202611010000",
            )),
            None,
        );
        let (_, state) = read(&incus, AT, &mut memory, None);
        assert_eq!(state.last_updated_version.as_deref(), Some("202611010000"));
        assert_eq!(state.last_updated_at, Some(AT));
        // A later plain completed check keeps the last update.
        let mut later = arca_update("OS update check completed");
        later["state"]["last_check"] = json!("2026-11-02T10:00:00Z");
        let incus = Fake::new(Some(later), None);
        let (_, state) = read(&incus, AT + DAY, &mut memory, None);
        assert_eq!(state.last_updated_version.as_deref(), Some("202611010000"));
        assert_eq!(state.last_updated_at, Some(AT));
        assert_eq!(state.last_success_at, Some(AT + DAY));
        // A status naming anything but a plain version token is not kept.
        assert_eq!(
            updated_version("X has been updated to version 1.2 and more"),
            None
        );
        assert_eq!(updated_version("OS update check completed"), None);
    }

    #[test]
    fn no_raw_error_text_reaches_the_update_state() {
        let mut memory = UpdateMemory::default();
        let incus = Fake::new(Some(arca_update("Failed to check for OS updates")), None);
        let (_, state) = read(&incus, AT, &mut memory, None);
        let shown = [
            format!("{state:?}"),
            format!("{memory:?}"),
            serde_json::to_string(&state).expect("the state serializes"),
        ];
        for shown in shown {
            assert!(
                !shown.contains("FAKE-INTERNAL-ERROR-TEXT"),
                "the raw last_error was kept"
            );
            assert!(!shown.contains("x509"), "the raw last_error was kept");
            assert!(!shown.contains("Failed to"), "the status text was kept");
            assert!(!shown.contains(FAKE_KEY), "a fake key was shown");
        }
    }

    #[test]
    fn the_update_state_serializes_in_camel_case_with_nulls() {
        let value = serde_json::to_value(OsUpdate::unknown(AT)).expect("the state serializes");
        assert_eq!(value["known"], json!(false));
        assert_eq!(value["asOf"], json!(AT));
        for key in [
            "version",
            "channel",
            "checkFrequency",
            "automatic",
            "lastCheckAt",
            "lastSuccessAt",
            "lastUpdatedVersion",
            "lastUpdatedAt",
            "needsReboot",
            "pendingVersion",
            "previousVersion",
            "goodVersion",
            "lastResult",
            "lastReason",
            "lastErrorKind",
            "trust",
        ] {
            assert_eq!(
                value.get(key),
                Some(&Value::Null),
                "{key} is present and null"
            );
        }
    }

    #[test]
    fn no_secret_text_reaches_a_reading_or_its_debug_form() {
        let mut memory = UpdateMemory::default();
        let reading = from_documents(
            &security(true, "ok"),
            &update("Failed to check for OS updates", "2026-11-01T10:00:00Z"),
            AT,
            &mut memory,
            None,
        )
        .expect("the reading");
        for shown in [format!("{reading:?}"), format!("{memory:?}")] {
            assert!(!shown.contains(FAKE_KEY), "a fake key was shown");
            assert!(!shown.contains("FAKE-PUBLIC-KEY"), "a fake key was shown");
            assert!(!shown.contains("Failed to"), "the status text was kept");
        }
    }
}
