//! Boot repair: rebuilds the UEFI boot files of an installed Windows with `bcdboot`.
//!
//! # Thread affinity
//! [`run_boot_repair`] connects its own [`Wmi`] and holds it for the whole call, so it must
//! run start to finish on one thread. It is safe to call from any thread.

use std::ffi::OsString;
use std::path::PathBuf;

use diskcore::{BootRepairPlan, Disk, Protection};

use crate::bitlocker;
use crate::log::Logger;
use crate::process::run_tool;
use crate::storage::{
    call_logged, find_disk_by_id, find_partition, letter_from, rel_path, update_cache, wait_for,
    StorageError, STORAGE_NS,
};
use crate::wmi::{Value, Wmi};

/// Arguments of `bcdboot <win>:\Windows /s <esp>: /f UEFI /l en-GB`.
pub(crate) fn bcdboot_args(windows_letter: char, esp_letter: char) -> Vec<OsString> {
    [
        format!(r"{windows_letter}:\Windows"),
        "/s".to_string(),
        format!("{esp_letter}:"),
        "/f".to_string(),
        "UEFI".to_string(),
        "/l".to_string(),
        "en-GB".to_string(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

/// `%SystemRoot%\System32\bcdboot.exe` when present (always, on Windows and WinPE), else
/// whatever `PATH` offers.
fn bcdboot_program() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join(r"System32\bcdboot.exe"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("bcdboot.exe"))
}

/// Whether BitLocker must be suspended: the plan asked for it, or the volume is live
/// encrypted and readable right now (a stale or unknown plan value must never send the next
/// boot to the recovery screen). `live` is `None` when the status could not be read.
fn needs_suspend(plan_says: bool, live: Option<Protection>) -> bool {
    plan_says || live == Some(Protection::Unlocked)
}

/// Combines the repair result with the temporary-letter cleanup messages. A repair that
/// worked but left a letter behind is reported as [`StorageError::CleanupFailed`]; an earlier
/// error stays the primary error (cleanup problems were already logged).
fn combine(primary: Result<(), StorageError>, leftovers: &[String]) -> Result<(), StorageError> {
    primary?;
    if leftovers.is_empty() {
        Ok(())
    } else {
        Err(StorageError::CleanupFailed(format!(
            "boot files were written, but the temporary drive letter cleanup failed: {}",
            leftovers.join("; ")
        )))
    }
}

/// A drive letter on a partition, removed again by [`TempLetter::release`] or, failing that,
/// on drop.
///
/// If the partition already had a letter it is used as is and left alone. Otherwise the
/// letter is assigned with `AddAccessPath(AssignDriveLetter = true)`. Removal re-reads the
/// partition to learn the letter, so it also cleans up when waiting for the letter timed out
/// after the assignment had already happened. `Drop` is the safety net for early returns;
/// callers that must report a failed removal call `release` instead.
struct TempLetter<'a> {
    wmi: &'a Wmi,
    log: &'a Logger,
    disk: u32,
    offset: u64,
    /// Valid once `acquire` returned.
    letter: char,
    added: bool,
}

fn current_letter(wmi: &Wmi, disk: u32, offset: u64) -> Result<Option<char>, StorageError> {
    Ok(find_partition(wmi, disk, offset)?.and_then(|p| letter_from(p.get_u16("DriveLetter"))))
}

impl<'a> TempLetter<'a> {
    fn acquire(
        wmi: &'a Wmi,
        log: &'a Logger,
        disk: u32,
        offset: u64,
        what: &str,
    ) -> Result<TempLetter<'a>, StorageError> {
        let part = find_partition(wmi, disk, offset)?.ok_or_else(|| {
            StorageError::NotFound(format!("{what} partition at offset {offset} not found"))
        })?;
        if let Some(letter) = letter_from(part.get_u16("DriveLetter")) {
            log.info(&format!(
                "{what} partition already has drive letter {letter}:"
            ));
            return Ok(TempLetter {
                wmi,
                log,
                disk,
                offset,
                letter,
                added: false,
            });
        }
        let path = rel_path(&part)?;
        call_logged(
            wmi,
            log,
            &path,
            "AddAccessPath",
            &[("AssignDriveLetter", Value::Bool(true))],
        )?;
        // From here on removal must happen, whatever comes next.
        let mut guard = TempLetter {
            wmi,
            log,
            disk,
            offset,
            letter: '?',
            added: true,
        };
        guard.letter = wait_for(
            &format!("a drive letter on the {what} partition"),
            20,
            || {
                update_cache(wmi);
                current_letter(wmi, disk, offset)
            },
        )?;
        log.info(&format!(
            "assigned temporary drive letter {}: to the {what} partition",
            guard.letter
        ));
        Ok(guard)
    }

    /// Removes the letter now. Marks the guard done even on failure (one attempt only), logs
    /// the failure and returns it as a message naming the actual letter.
    fn remove(&mut self) -> Result<(), String> {
        if !self.added {
            return Ok(());
        }
        self.added = false;
        let part = match find_partition(self.wmi, self.disk, self.offset) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(()); // partition gone: its letter went with it
            }
            Err(e) => {
                let msg = format!(
                    "could not re-read the partition at offset {} to remove its temporary drive letter: {e}",
                    self.offset
                );
                self.log.error(&msg);
                return Err(msg);
            }
        };
        let Some(letter) = letter_from(part.get_u16("DriveLetter")) else {
            return Ok(()); // nothing assigned
        };
        let result = rel_path(&part).map_err(|e| e.to_string()).and_then(|path| {
            call_logged(
                self.wmi,
                self.log,
                &path,
                "RemoveAccessPath",
                &[("AccessPath", Value::Str(format!(r"{letter}:\")))],
            )
            .map_err(|e| e.to_string())
        });
        match result {
            Ok(_) => {
                self.log
                    .info(&format!("removed temporary drive letter {letter}:"));
                Ok(())
            }
            Err(e) => {
                let msg = format!("could not remove temporary drive letter {letter}: ({e})");
                self.log.error(&msg);
                Err(msg)
            }
        }
    }

    /// Removes the letter and reports a failure to the caller.
    fn release(mut self) -> Result<(), String> {
        self.remove()
    }
}

impl Drop for TempLetter<'_> {
    fn drop(&mut self) {
        // Failure is already logged by `remove`.
        let _ = self.remove();
    }
}

/// Runs boot repair for `plan`.
///
/// 1. Suspends BitLocker on the Windows volume for one reboot when `plan.suspend_bitlocker`,
///    or when the volume reads as encrypted and unlocked right now even though the plan said
///    not to (the live status is re-checked, so a stale plan cannot cause a recovery prompt).
/// 2. Gives the ESP a temporary drive letter (and the Windows volume one, if it has none).
/// 3. Runs `bcdboot <win>:\Windows /s <esp>: /f UEFI /l en-GB`.
/// 4. Removes every temporary letter it added, also when an earlier step failed.
///
/// Suspending happens first and is not undone: if a later step fails, BitLocker stays
/// suspended for one reboot (a safe state, since protection resumes by itself).
///
/// A failure to remove a temporary letter is returned as
/// [`StorageError::CleanupFailed`] when the repair itself succeeded (the boot files are
/// written), and logged when an earlier error is returned instead.
///
/// `disks` supplies the Windows volume path (a snapshot taken by the caller).
pub fn run_boot_repair(
    plan: &BootRepairPlan,
    disks: &[Disk],
    log: &Logger,
) -> Result<(), StorageError> {
    let disk = disks
        .iter()
        .find(|d| d.id == plan.disk)
        .ok_or_else(|| StorageError::NotFound(format!("disk {} not found", plan.disk.0)))?;
    let windows = disk
        .partitions
        .iter()
        .find(|p| p.offset == plan.windows_offset)
        .ok_or_else(|| {
            StorageError::NotFound(format!(
                "no partition at offset {} on disk {}",
                plan.windows_offset, disk.number
            ))
        })?;

    match windows.volume_path.as_deref() {
        Some(vp) => {
            let live = match bitlocker::status(vp) {
                Ok(p) => Some(p),
                // Not a BitLocker-capable volume: nothing to suspend.
                Err(StorageError::NotFound(_)) => Some(Protection::None),
                Err(e) => {
                    log.warn(&format!("could not read the live BitLocker status: {e}"));
                    None
                }
            };
            if needs_suspend(plan.suspend_bitlocker, live) {
                if !plan.suspend_bitlocker {
                    log.warn("the plan did not ask to suspend BitLocker, but the Windows volume is encrypted; suspending anyway");
                }
                log.info("suspending BitLocker on the Windows volume before boot repair");
                bitlocker::suspend_for_one_reboot(vp, log)?;
            }
        }
        None if plan.suspend_bitlocker => {
            return Err(StorageError::Invalid(
                "the Windows partition has no volume path to suspend".into(),
            ));
        }
        None => {}
    }

    let wmi = Wmi::connect(STORAGE_NS)?;
    // Resolve the live disk number from the stable id; numbers can change between snapshots.
    let number = find_disk_by_id(&wmi, &plan.disk)?.number;

    let mut esp: Option<TempLetter> = None;
    let mut win: Option<TempLetter> = None;
    let primary = (|| -> Result<(), StorageError> {
        let e = esp.insert(TempLetter::acquire(
            &wmi,
            log,
            number,
            plan.esp_offset,
            "ESP",
        )?);
        let w = win.insert(TempLetter::acquire(
            &wmi,
            log,
            number,
            plan.windows_offset,
            "Windows",
        )?);
        let args = bcdboot_args(w.letter, e.letter);
        let out = run_tool(log, &bcdboot_program(), &args)?;
        if out.code != 0 {
            log.error(&format!("bcdboot failed with exit code {}", out.code));
            return Err(StorageError::Invalid(format!(
                "bcdboot failed with exit code {}",
                out.code
            )));
        }
        log.info("boot files written to the ESP");
        Ok(())
    })();

    // Explicit cleanup so a failure can be reported (Drop alone could only log it).
    let leftovers: Vec<String> = [win.take(), esp.take()]
        .into_iter()
        .flatten()
        .filter_map(|g| g.release().err())
        .collect();
    combine(primary, &leftovers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcdboot_args_match_the_documented_command_line() {
        let args: Vec<String> = bcdboot_args('D', 'S')
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [r"D:\Windows", "/s", "S:", "/f", "UEFI", "/l", "en-GB"]
        );
    }

    #[test]
    fn suspend_when_plan_says_or_live_volume_is_encrypted() {
        assert!(needs_suspend(true, Some(Protection::None)));
        assert!(needs_suspend(true, None));
        assert!(needs_suspend(false, Some(Protection::Unlocked)));
        assert!(!needs_suspend(false, Some(Protection::None)));
        assert!(!needs_suspend(false, None));
    }

    #[test]
    fn cleanup_failure_after_success_is_an_error() {
        assert!(combine(Ok(()), &[]).is_ok());
        let e = combine(
            Ok(()),
            &["could not remove temporary drive letter S:".into()],
        )
        .unwrap_err();
        assert!(matches!(e, StorageError::CleanupFailed(_)), "{e:?}");
        assert!(e.to_string().contains("S:"), "{e}");
    }

    #[test]
    fn earlier_error_wins_over_cleanup_failure() {
        let e = combine(
            Err(StorageError::Invalid("bcdboot failed".into())),
            &["leftover".into()],
        )
        .unwrap_err();
        assert!(matches!(e, StorageError::Invalid(_)), "{e:?}");
    }
}
