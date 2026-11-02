//! The SMART readings of the drives incus-osd lists: whether each listed drive passed, from one GET of the storage document every
//! 10 min. A drive with no usable SMART result is listed with its pass unknown.
//!
//! incus-osd names a drive by its `/dev/disk/by-id/` link, and that link holds the model and the serial number. So a drive's
//! reading carries a label made from a hash of that name, never the name itself. The model, the serial, the WWN and every other
//! field are dropped when the body is read, so none of them reaches a reading, a label, an error or a log.

use arca_incus::IncusRead;
use serde_json::Value;

/// The storage document: the drive list under `state.drives`, each with its SMART state.
pub const STORAGE_PATH: &str = "/os/1.0/system/storage";

/// The prefix of every drive label.
const LABEL_PREFIX: &str = "drive-";

/// One listed drive, as the label the export and the alerts carry, and whether its SMART health check passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveReading {
    /// `drive-` and twelve hex digits: a hash of the drive's incus-osd ID. It is stable across reads and carries no part of the ID.
    pub device: String,
    /// Whether incus-osd reports the SMART health check as passed. `None` when the drive has no usable SMART result: its SMART
    /// block is missing, null or disabled.
    pub passed: Option<bool>,
}

/// The SMART state of the listed drives, read at `at`. A listed drive with an ID has an entry, with `passed` unknown when it has
/// no usable SMART result. A drive with no ID and no usable SMART result has no entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivesReading {
    /// Epoch milliseconds when the read ran.
    pub at: u64,
    /// The drives, in the order incus-osd lists them.
    pub drives: Vec<DriveReading>,
}

/// Reads the storage document at `at`. `None` when the GET fails, as a plain Incus answers 404, so an unlike host is unknown.
pub fn read<R: IncusRead>(incus: &R, at: u64) -> Option<DrivesReading> {
    let document = incus.get(STORAGE_PATH).ok()?;
    from_document(&document, at)
}

/// The reading the storage document gives at `at`. A drive with SMART enabled needs an ID and a boolean `passed`; when one is
/// missing or the wrong type the whole reading is `None`, so a failing drive is never hidden as absent. A SMART block with a
/// non-empty `error` (incus-osd could not read the drive's SMART data) makes the whole reading `None` too. A listed drive with an
/// ID whose SMART block is missing, null or disabled has `passed: None`, so its alert is held as unknown until the drive leaves
/// the listing. A drive with no ID and no usable SMART block is skipped. A document without a drive list is `None`.
pub fn from_document(document: &Value, at: u64) -> Option<DrivesReading> {
    let drives = document.pointer("/state/drives")?.as_array()?;
    let mut readings = Vec::new();
    for drive in drives {
        let Some(smart) = drive.get("smart").filter(|smart| !smart.is_null()) else {
            readings.extend(without_result(drive));
            continue;
        };
        if !error_is_empty(smart)? {
            return None;
        }
        if !smart.get("enabled")?.as_bool()? {
            readings.extend(without_result(drive));
            continue;
        }
        let id = drive.get("id")?.as_str()?;
        let passed = smart.get("passed")?.as_bool()?;
        readings.push(DriveReading {
            device: device_label(id),
            passed: Some(passed),
        });
    }
    Some(DrivesReading {
        at,
        drives: readings,
    })
}

/// The reading of a listed drive that has no usable SMART result: `passed` unknown when the drive has an ID, else no reading.
fn without_result(drive: &Value) -> Option<DriveReading> {
    let id = drive.get("id")?.as_str()?;
    Some(DriveReading {
        device: device_label(id),
        passed: None,
    })
}

/// Whether a SMART block's `error` is empty. An absent or null `error` is empty; a string is empty when it has no text; any
/// other type is `None`, so the reading is unknown.
fn error_is_empty(smart: &Value) -> Option<bool> {
    match smart.get("error") {
        None | Some(Value::Null) => Some(true),
        Some(Value::String(text)) => Some(text.is_empty()),
        Some(_) => None,
    }
}

/// The label of a drive, from its incus-osd ID: `drive-` and the low 48 bits of the 64-bit FNV-1a hash of the ID, in hex. The
/// hash is fixed by its constants, so a label is the same on every read and across restarts.
pub fn device_label(id: &str) -> String {
    format!(
        "{LABEL_PREFIX}{:012x}",
        fnv1a(id.as_bytes()) & 0xffff_ffff_ffff
    )
}

/// The 64-bit FNV-1a hash of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_label_is_stable_has_twelve_hex_digits_and_carries_no_part_of_the_id() {
        let id = "/dev/disk/by-id/ata-FAKE-MODEL-NOT-REAL_FAKE-SERIAL-0001";
        let label = device_label(id);
        assert_eq!(label, device_label(id), "the same ID gives the same label");
        let digits = label.strip_prefix(LABEL_PREFIX).expect("the drive prefix");
        assert_eq!(digits.len(), 12);
        assert!(digits.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!label.contains("FAKE") && !label.contains("disk"));
        assert_ne!(
            label,
            device_label("/dev/disk/by-id/ata-FAKE-MODEL-NOT-REAL_FAKE-SERIAL-0002")
        );
    }

    #[test]
    fn a_listed_drive_without_a_usable_smart_result_reads_as_unknown() {
        let document = json!({ "state": { "drives": [
            { "id": "disk1", "smart": { "enabled": false, "passed": false } },
            { "id": "disk2" },
            { "id": "disk3", "smart": null },
        ] } });
        assert_eq!(
            from_document(&document, 7),
            Some(DrivesReading {
                at: 7,
                drives: vec![
                    DriveReading {
                        device: device_label("disk1"),
                        passed: None,
                    },
                    DriveReading {
                        device: device_label("disk2"),
                        passed: None,
                    },
                    DriveReading {
                        device: device_label("disk3"),
                        passed: None,
                    },
                ]
            })
        );
    }

    #[test]
    fn a_drive_without_an_id_and_without_a_smart_result_is_skipped() {
        let document = json!({ "state": { "drives": [
            { "smart": { "enabled": false, "passed": false } },
            { "smart": null },
            { },
        ] } });
        assert_eq!(
            from_document(&document, 7),
            Some(DrivesReading {
                at: 7,
                drives: Vec::new()
            })
        );
    }

    #[test]
    fn an_enabled_drive_missing_its_id_or_its_result_makes_the_reading_unknown() {
        for drive in [
            json!({ "smart": { "enabled": true, "passed": true } }),
            json!({ "id": "disk1", "smart": { "enabled": true } }),
            json!({ "id": "disk1", "smart": { "enabled": true, "passed": "yes" } }),
        ] {
            let document = json!({ "state": { "drives": [drive] } });
            assert_eq!(from_document(&document, 1), None);
        }
    }

    #[test]
    fn a_known_id_has_its_pinned_label() {
        // The FNV-1a hash of each ID, computed outside this code: a change to the constants or the mask fails here.
        assert_eq!(
            device_label("/dev/disk/by-id/ata-FAKE-MODEL-NOT-REAL_FAKE-SERIAL-0001"),
            "drive-2743af5e20a8"
        );
        assert_eq!(
            device_label("/dev/disk/by-id/ata-FAKE-MODEL-NOT-REAL_FAKE-SERIAL-0002"),
            "drive-2a43af5e25c1"
        );
        assert_eq!(device_label("disk1"), "drive-5d99c8e8b18f");
    }

    #[test]
    fn a_non_empty_smart_error_makes_the_whole_reading_unknown() {
        for smart in [
            json!({ "enabled": true, "passed": true, "error": "Bad SMART data from drive: parse" }),
            json!({ "enabled": true, "passed": false, "error": "x" }),
            json!({ "enabled": false, "passed": true, "error": "x" }),
            json!({ "enabled": true, "passed": true, "error": 7 }),
        ] {
            let document = json!({ "state": { "drives": [
                { "id": "disk2", "smart": { "enabled": true, "passed": true, "error": "" } },
                { "id": "disk1", "smart": smart },
            ] } });
            assert_eq!(from_document(&document, 1), None, "{document}");
        }
    }

    #[test]
    fn an_empty_or_absent_smart_error_reads_as_before() {
        let document = json!({ "state": { "drives": [
            { "id": "disk1", "smart": { "enabled": true, "passed": true, "error": "" } },
            { "id": "disk2", "smart": { "enabled": true, "passed": false } },
        ] } });
        let read = from_document(&document, 1).expect("both drives read");
        let passed: Vec<Option<bool>> = read.drives.iter().map(|drive| drive.passed).collect();
        assert_eq!(passed, vec![Some(true), Some(false)]);
    }

    #[test]
    fn a_document_without_a_drive_list_is_unknown() {
        assert_eq!(from_document(&json!({ "state": {} }), 1), None);
        assert_eq!(from_document(&json!({}), 1), None);
    }
}
