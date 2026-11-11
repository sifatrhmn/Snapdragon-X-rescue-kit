//! BitLocker status, unlock and suspend over `Win32_EncryptableVolume`.
//!
//! Every public function connects its own [`Wmi`] (which is `!Send`), so each is safe to
//! call from any thread. A recovery key is never logged and never appears in an error
//! message.

use diskcore::Protection;

use crate::log::Logger;
use crate::storage::{call_logged, rel_path, StorageError};
use crate::wmi::{Value, Wmi};

/// A recovery key that failed validation. Variants carry only a 1-based group position,
/// never any digits of the key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("recovery key must have 8 groups of 6 digits (found {0} groups)")]
    GroupCount(usize),
    #[error("recovery key group {0} must be exactly 6 digits")]
    BadGroup(usize),
    #[error("recovery key group {0} is not valid (not divisible by 11)")]
    NotMultipleOf11(usize),
    #[error("recovery key group {0} is out of range")]
    OutOfRange(usize),
}

/// Number of 6-digit groups in a recovery password.
const GROUPS: usize = 8;
/// Every group is `n * 11` with `n < 65536`, so a group is below this bound.
const GROUP_LIMIT: u32 = 720_896;

/// Validates a 48-digit numerical recovery password and returns it in canonical form.
///
/// Accepts 8 groups of 6 digits separated by `-` and/or whitespace. Each group must be
/// divisible by 11 and below 720896. Error values only identify the offending group by
/// position; they never contain key digits.
pub fn parse_recovery_key(s: &str) -> Result<String, KeyError> {
    let groups: Vec<&str> = s
        .split(|c: char| c == '-' || c.is_whitespace())
        .filter(|g| !g.is_empty())
        .collect();
    if groups.len() != GROUPS {
        return Err(KeyError::GroupCount(groups.len()));
    }
    for (i, g) in groups.iter().enumerate() {
        let pos = i + 1;
        if g.len() != 6 || !g.bytes().all(|b| b.is_ascii_digit()) {
            return Err(KeyError::BadGroup(pos));
        }
        let n: u32 = g.parse().map_err(|_| KeyError::BadGroup(pos))?;
        if !n.is_multiple_of(11) {
            return Err(KeyError::NotMultipleOf11(pos));
        }
        if n >= GROUP_LIMIT {
            return Err(KeyError::OutOfRange(pos));
        }
    }
    Ok(groups.join("-"))
}

/// The log line written after a successful unlock. It names the volume only.
pub fn unlock_log_line(volume_path: &str) -> String {
    format!("Unlocked volume {volume_path}")
}

/// WMI namespace of the BitLocker provider.
pub const BITLOCKER_NS: &str = r"root\CIMV2\Security\MicrosoftVolumeEncryption";

/// `GetConversionStatus` value for "fully decrypted".
const FULLY_DECRYPTED: u32 = 0;

fn norm_path(p: &str) -> String {
    p.trim().trim_end_matches('\\').to_ascii_lowercase()
}

/// True when `volume_path` names the volume with this `DeviceID` / `DriveLetter`
/// (`\\?\Volume{guid}\` or `X:\`; case and a trailing backslash are ignored).
fn same_volume(device_id: Option<&str>, drive_letter: Option<&str>, volume_path: &str) -> bool {
    let want = norm_path(volume_path);
    if want.is_empty() {
        return false;
    }
    device_id.is_some_and(|d| norm_path(d) == want)
        || drive_letter.is_some_and(|l| !l.trim().is_empty() && norm_path(l) == want)
}

/// Pure mapping of the provider's status values to [`Protection`].
///
/// `lock`: `GetLockStatus` (1 = locked). `conversion`: `GetConversionStatus` (0 = fully
/// decrypted). `protection`: `GetProtectionStatus` (1 = on), used only when the conversion
/// status could not be read.
fn classify(lock: u32, conversion: Option<u32>, protection: Option<u32>) -> Protection {
    if lock == 1 {
        return Protection::Locked;
    }
    match conversion {
        Some(FULLY_DECRYPTED) => Protection::None,
        Some(_) => Protection::Unlocked,
        None if protection == Some(1) => Protection::Unlocked,
        None => Protection::None,
    }
}

/// One `Win32_EncryptableVolume` instance, as listed once by [`Provider::open`].
struct Entry {
    device_id: Option<String>,
    drive_letter: Option<String>,
    /// Relative object path, usable with [`Wmi::call`].
    path: String,
}

/// A connection to the BitLocker provider plus the list of its volumes, queried once.
///
/// Enumeration opens one `Provider` per snapshot and asks it about every partition, so the
/// per-partition cost is just `GetLockStatus` and `GetConversionStatus`. The public
/// functions of this module each open their own.
///
/// # Thread affinity
/// Wraps a [`Wmi`], so it is `!Send`: use it on the thread that opened it.
pub(crate) struct Provider {
    wmi: Wmi,
    volumes: Vec<Entry>,
}

impl Provider {
    /// Connects to the BitLocker namespace and lists its volumes.
    pub(crate) fn open() -> Result<Provider, StorageError> {
        let wmi = Wmi::connect(BITLOCKER_NS)?;
        let mut volumes = Vec::new();
        for v in wmi.query("SELECT * FROM Win32_EncryptableVolume")? {
            volumes.push(Entry {
                device_id: v.get_str("DeviceID"),
                drive_letter: v.get_str("DriveLetter"),
                path: rel_path(&v)?,
            });
        }
        Ok(Provider { wmi, volumes })
    }

    /// Relative object path of the volume named by `volume_path`.
    fn find(&self, volume_path: &str) -> Result<&str, StorageError> {
        self.volumes
            .iter()
            .find(|e| {
                same_volume(
                    e.device_id.as_deref(),
                    e.drive_letter.as_deref(),
                    volume_path,
                )
            })
            .map(|e| e.path.as_str())
            .ok_or_else(|| StorageError::NotFound(format!("no BitLocker volume {volume_path}")))
    }

    /// BitLocker state of the volume at `volume_path`. See [`status`].
    pub(crate) fn status(&self, volume_path: &str) -> Result<Protection, StorageError> {
        let path = self.find(volume_path)?;
        let lock = self
            .wmi
            .call(path, "GetLockStatus", &[])?
            .get_u32("LockStatus")
            .ok_or_else(|| StorageError::Invalid("GetLockStatus returned no LockStatus".into()))?;
        if lock == 1 {
            // A locked volume refuses every other query.
            return Ok(Protection::Locked);
        }
        let conversion = self
            .wmi
            .call(path, "GetConversionStatus", &[])
            .ok()
            .and_then(|o| o.get_u32("ConversionStatus"));
        let protection = if conversion.is_none() {
            self.wmi
                .call(path, "GetProtectionStatus", &[])
                .ok()
                .and_then(|o| o.get_u32("ProtectionStatus"))
        } else {
            None
        };
        Ok(classify(lock, conversion, protection))
    }
}

/// BitLocker state of the volume at `volume_path` (`\\?\Volume{guid}\` or `X:\`).
///
/// Fully decrypted is [`Protection::None`], encrypted and readable is `Unlocked`, locked is
/// `Locked`. Errors when the volume is unknown to BitLocker or WMI is unreachable (for
/// example without administrator rights).
pub fn status(volume_path: &str) -> Result<Protection, StorageError> {
    Provider::open()?.status(volume_path)
}

/// Unlocks the volume with a numerical recovery password. The key is validated and
/// normalised first. Neither the log nor any error contains it.
pub fn unlock(volume_path: &str, key: &str, log: &Logger) -> Result<(), StorageError> {
    let key = parse_recovery_key(key).map_err(|e| StorageError::Invalid(e.to_string()))?;
    let bl = Provider::open()?;
    let path = bl.find(volume_path)?;
    // Deliberately not `call_logged`: it would log the input parameters (the key).
    match bl.wmi.call(
        path,
        "UnlockWithNumericalPassword",
        &[("NumericalPassword", Value::Str(key))],
    ) {
        Ok(_) => {
            log.info(&unlock_log_line(volume_path));
            Ok(())
        }
        Err(e) => {
            log.error(&format!("Unlock of volume {volume_path} failed: {e}"));
            Err(StorageError::Wmi(e))
        }
    }
}

/// Suspends BitLocker key protectors until the next reboot (`DisableCount = 1`), so a
/// boot-files change does not trigger recovery mode.
pub fn suspend_for_one_reboot(volume_path: &str, log: &Logger) -> Result<(), StorageError> {
    let bl = Provider::open()?;
    let path = bl.find(volume_path)?;
    call_logged(
        &bl.wmi,
        log,
        path,
        "DisableKeyProtectors",
        &[("DisableCount", Value::U32(1))],
    )?;
    log.info(&format!(
        "Suspended BitLocker on volume {volume_path} for one reboot"
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "110011 220022 330033 440044 550055 660066 000011 000022";

    #[test]
    fn valid_key_with_spaces_is_normalised_to_hyphens() {
        assert_eq!(
            parse_recovery_key(KEY).unwrap(),
            "110011-220022-330033-440044-550055-660066-000011-000022"
        );
    }

    #[test]
    fn hyphenated_mixed_and_padded_input_is_accepted() {
        let canon = "110011-220022-330033-440044-550055-660066-000011-000022";
        assert_eq!(parse_recovery_key(canon).unwrap(), canon);
        assert_eq!(
            parse_recovery_key("  110011-220022 330033-440044 550055-660066 000011-000022\n")
                .unwrap(),
            canon
        );
    }

    #[test]
    fn group_not_divisible_by_11_is_rejected() {
        let k = "110011 220022 330033 440044 550055 660066 000011 000023";
        assert_eq!(parse_recovery_key(k), Err(KeyError::NotMultipleOf11(8)));
    }

    #[test]
    fn wrong_group_count_is_rejected() {
        let seven = "110011 220022 330033 440044 550055 660066 000011";
        assert_eq!(parse_recovery_key(seven), Err(KeyError::GroupCount(7)));
        let nine = format!("{KEY} 000011");
        assert_eq!(parse_recovery_key(&nine), Err(KeyError::GroupCount(9)));
        assert_eq!(parse_recovery_key(""), Err(KeyError::GroupCount(0)));
    }

    #[test]
    fn out_of_range_group_is_rejected() {
        // 720896 = 11 * 65536 is divisible by 11 but not below the limit.
        let k = "110011 220022 330033 440044 550055 660066 000011 720896";
        assert_eq!(parse_recovery_key(k), Err(KeyError::OutOfRange(8)));
        // 720885 = 11 * 65535 is the largest valid group.
        let ok = "720885 000000 000011 000022 000033 000044 000055 000066";
        assert!(parse_recovery_key(ok).is_ok());
    }

    #[test]
    fn non_six_digit_groups_are_rejected() {
        let short = "11011 220022 330033 440044 550055 660066 000011 000022";
        assert_eq!(parse_recovery_key(short), Err(KeyError::BadGroup(1)));
        let letters = "11001a 220022 330033 440044 550055 660066 000011 000022";
        assert_eq!(parse_recovery_key(letters), Err(KeyError::BadGroup(1)));
    }

    #[test]
    fn key_error_messages_never_contain_digits_of_the_key() {
        let k = "110011 220022 330033 440044 550055 660066 000011 000023";
        let msg = parse_recovery_key(k).unwrap_err().to_string();
        assert!(
            !regex::Regex::new(r"\d{6}").unwrap().is_match(&msg),
            "{msg}"
        );
    }

    #[test]
    fn unlock_log_line_has_no_six_digit_run() {
        let line = unlock_log_line(r"\\?\Volume{1234abcd-12ab-34cd-56ef-1a2b3c4d5e6f}\");
        assert!(line.starts_with("Unlocked volume "), "{line}");
        assert!(
            !regex::Regex::new(r"\d{6}").unwrap().is_match(&line),
            "{line}"
        );
    }

    #[test]
    fn classify_maps_status_values() {
        assert_eq!(classify(1, None, None), Protection::Locked);
        assert_eq!(classify(0, Some(0), Some(0)), Protection::None);
        assert_eq!(classify(0, Some(1), Some(1)), Protection::Unlocked);
        // Encrypted but suspended (protection off) is still BitLocker-encrypted.
        assert_eq!(classify(0, Some(1), Some(0)), Protection::Unlocked);
        assert_eq!(classify(0, Some(2), None), Protection::Unlocked);
        assert_eq!(classify(0, None, Some(1)), Protection::Unlocked);
        assert_eq!(classify(0, None, Some(0)), Protection::None);
    }

    #[test]
    fn same_volume_ignores_case_and_trailing_backslash() {
        let id = r"\\?\Volume{AbC}\";
        assert!(same_volume(Some(id), None, r"\\?\volume{abc}"));
        assert!(same_volume(Some(id), Some("E:"), r"E:\"));
        assert!(!same_volume(Some(id), Some("E:"), r"F:\"));
        assert!(!same_volume(Some(id), None, ""));
        assert!(!same_volume(None, Some(""), ""));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Read-only smoke test on the system drive (the BitLocker provider answers without
    /// administrator rights). Skips quietly when the namespace is not available (Home
    /// editions).
    #[test]
    fn status_of_the_system_drive_is_answered() {
        let Ok(bl) = Provider::open() else {
            eprintln!("BitLocker namespace unavailable; skipping");
            return;
        };
        let drive = format!(
            r"{}\",
            std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())
        );
        let p = bl.status(&drive).expect("status of the system drive");
        eprintln!("system drive protection: {p:?}");
        // The same provider snapshot answers repeatedly.
        assert_eq!(bl.status(&drive).unwrap(), p);
        assert!(matches!(
            bl.status(r"\\?\Volume{00000000-0000-0000-0000-000000000000}\"),
            Err(StorageError::NotFound(_))
        ));
    }
}
