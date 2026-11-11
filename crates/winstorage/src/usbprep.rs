//! Rescue-USB preparation: wipe, GPT, one FAT32 partition labelled `WOARESCUE`.

use diskcore::{DiskId, FsKind, PartitionStyle, FAT32_MAX, MIB};

use crate::log::{is_winpe, Logger};
use crate::storage::{
    call_logged, create_partition, find_disk_by_id, format_partition, letter_from, update_cache,
    wait_for, StorageError, GPT_BASIC,
};
use crate::wmi::{Value, Wmi};

/// Size of the single data partition on a disk of `disk_size` bytes: `min(disk - 2 MiB, FAT32_MAX)`.
pub(crate) fn usb_partition_size(disk_size: u64) -> Option<u64> {
    let size = disk_size.checked_sub(2 * MIB)?.min(FAT32_MAX);
    (size >= MIB).then_some(size)
}

/// DESTRUCTIVE: clears `disk`, initialises it as GPT, creates one basic-data partition of
/// `min(disk - 2 MiB, FAT32_MAX)` bytes and formats it FAT32 `WOARESCUE`. Returns the drive
/// letter assigned to it.
///
/// This function does not check the bus type (the CLI does). It does refuse to touch the
/// running operating system's disk.
pub fn prepare_usb(wmi: &Wmi, disk: &DiskId, log: &Logger) -> Result<char, StorageError> {
    let row = find_disk_by_id(wmi, disk)?;
    let running_os = row.obj.get_bool("IsBoot").unwrap_or(false)
        || row.obj.get_bool("IsSystem").unwrap_or(false);
    if running_os && !is_winpe() {
        return Err(StorageError::Refused(format!(
            "disk {} holds the running operating system",
            row.number
        )));
    }
    let size = row.obj.get_u64("Size").unwrap_or(0);
    let part_size = usb_partition_size(size)
        .ok_or_else(|| StorageError::Invalid(format!("disk {} is too small", row.number)))?;
    log.info(&format!(
        "preparing disk {} ({} bytes) as rescue USB",
        row.number, size
    ));

    // PartitionStyle 0 is a raw disk, which has nothing to clear.
    if row.obj.get_u16("PartitionStyle").unwrap_or(0) != 0 {
        call_logged(
            wmi,
            log,
            &row.path,
            "Clear",
            &[
                ("RemoveData", Value::Bool(true)),
                ("RemoveOEM", Value::Bool(true)),
            ],
        )?;
        update_cache(wmi);
    }
    call_logged(
        wmi,
        log,
        &row.path,
        "Initialize",
        &[("PartitionStyle", Value::U16(2))],
    )?;
    update_cache(wmi);

    // No explicit offset: the provider rejects offsets below its first usable boundary
    // (about 16 MiB) and places the partition itself. Initialising a GPT disk also makes the
    // provider add a 16 MiB MSR first, so the disk ends up with [MSR, FAT32]. The data
    // partition starts at about 16 MiB, so on a disk smaller than FAT32_MAX plus that the
    // full size does not fit; retry with what is left (disk - 18 MiB).
    let create = |size| {
        create_partition(
            wmi,
            log,
            &row,
            PartitionStyle::Gpt,
            None,
            size,
            GPT_BASIC,
            true,
        )
    };
    let part = match create(part_size) {
        Ok(p) => p,
        Err(first) => {
            let smaller = size.saturating_sub(18 * MIB).min(part_size) / MIB * MIB;
            if smaller < MIB || smaller >= part_size {
                return Err(first);
            }
            log.warn(&format!(
                "creating {part_size} bytes failed ({first}); retrying with {smaller}"
            ));
            create(smaller)?
        }
    };
    format_partition(
        wmi,
        log,
        row.number,
        &part,
        &FsKind::Fat32,
        "WOARESCUE",
        false,
    )?;

    let offset = part.get_u64("Offset").unwrap_or(MIB);
    let letter = wait_for("a drive letter on the rescue partition", 20, || {
        update_cache(wmi);
        match crate::storage::find_partition(wmi, row.number, offset)? {
            Some(p) => Ok(letter_from(p.get_u16("DriveLetter"))),
            None => Ok(None),
        }
    })?;
    log.info(&format!("rescue USB ready on {letter}:"));
    Ok(letter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_size_is_capped_and_leaves_two_mib() {
        assert_eq!(usb_partition_size(40 * 1024 * MIB), Some(FAT32_MAX));
        assert_eq!(
            usb_partition_size(8 * 1024 * MIB),
            Some(8 * 1024 * MIB - 2 * MIB)
        );
        assert_eq!(usb_partition_size(MIB), None);
        assert_eq!(usb_partition_size(0), None);
    }
}
