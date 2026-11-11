//! Storage backend: disk enumeration and partition operations over the Storage WMI provider
//! (`root\Microsoft\Windows\Storage`).
//!
//! # Thread affinity
//! [`StorageBackend`] owns a [`Wmi`] connection, which is `!Send`. Construct it on the
//! thread that will call [`Backend::snapshot`] / [`Backend::execute`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::thread;
use std::time::Duration;

use diskcore::{
    align_up, part_kind_from_gpt, Backend, BackendError, BusType, Disk, DiskId, FsKind, Op,
    PartKind, Partition, PartitionStyle, Protection, MIB,
};

use crate::bitlocker;
use crate::log::{is_winpe, Logger};
use crate::wmi::{Value, Wmi, WmiError, WmiObject};

/// WMI namespace of the Storage provider.
pub const STORAGE_NS: &str = r"root\Microsoft\Windows\Storage";

/// GPT type GUID of a basic data partition (braced form the Storage provider expects).
pub(crate) const GPT_BASIC: &str = "{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}";

/// Errors from the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Wmi(#[from] WmiError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{what} failed (Win32 error {code})")]
    Win32 { what: &'static str, code: u32 },
    #[error("{0}")]
    NotFound(String),
    /// A safety guard refused the request before any change was made.
    #[error("refused: {0}")]
    Refused(String),
    #[error("{0}")]
    Invalid(String),
    /// The operation itself succeeded but a temporary change could not be undone.
    #[error("{0}")]
    CleanupFailed(String),
}

// ---------------------------------------------------------------------------------------
// Pure mapping helpers
// ---------------------------------------------------------------------------------------

/// Maps `MSFT_Volume.FileSystem` to [`FsKind`].
pub(crate) fn fs_kind_from_str(s: &str) -> FsKind {
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "raw" => FsKind::Raw,
        "ntfs" => FsKind::Ntfs,
        "fat32" => FsKind::Fat32,
        "exfat" => FsKind::ExFat,
        "refs" => FsKind::Refs,
        _ => FsKind::Other(s.trim().to_string()),
    }
}

/// The `FileSystem` argument of `MSFT_Volume.Format` for `fs`.
pub(crate) fn fs_format_name(fs: &FsKind) -> Result<&'static str, StorageError> {
    match fs {
        FsKind::Ntfs => Ok("NTFS"),
        FsKind::Fat32 => Ok("FAT32"),
        FsKind::ExFat => Ok("exFAT"),
        FsKind::Refs => Ok("ReFS"),
        other => Err(StorageError::Invalid(format!(
            "cannot format a volume as {other:?}"
        ))),
    }
}

/// Maps `MSFT_Disk.PartitionStyle` (0 unknown, 1 MBR, 2 GPT).
pub(crate) fn partition_style_from(v: u16) -> PartitionStyle {
    match v {
        1 => PartitionStyle::Mbr,
        2 => PartitionStyle::Gpt,
        _ => PartitionStyle::Raw,
    }
}

/// Maps an MBR partition type byte to a [`PartKind`] (used when there is no GPT type).
pub(crate) fn part_kind_from_mbr(t: u16) -> PartKind {
    match t {
        0x07 | 0x06 | 0x0B | 0x0C | 0x0E => PartKind::Basic,
        0x27 => PartKind::Recovery,
        0xEF => PartKind::Esp,
        _ => PartKind::Other,
    }
}

/// `MSFT_Partition.DriveLetter` / `MSFT_Volume.DriveLetter` (a char16, 0 when none).
pub(crate) fn letter_from(v: Option<u16>) -> Option<char> {
    let c = char::from_u32(u32::from(v?))?;
    c.is_ascii_alphabetic().then(|| c.to_ascii_uppercase())
}

// ---------------------------------------------------------------------------------------
// WMI helpers shared with vhd.rs / usbprep.rs
// ---------------------------------------------------------------------------------------

/// A disk row with its resolved identity.
pub(crate) struct DiskRow {
    pub obj: WmiObject,
    pub number: u32,
    pub id: DiskId,
    /// Relative object path, usable with [`Wmi::call`].
    pub path: String,
}

/// Relative object path (`MSFT_Disk.ObjectId="..."`), usable for `ExecMethod` and
/// `ASSOCIATORS OF`.
pub(crate) fn rel_path(obj: &WmiObject) -> Result<String, StorageError> {
    obj.get_str("__RELPATH")
        .or_else(|| obj.path())
        .ok_or_else(|| StorageError::NotFound("WMI object has no path".into()))
}

fn disk_id_of(obj: &WmiObject, number: u32) -> DiskId {
    let pick = |n: &str| {
        obj.get_str(n)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    DiskId(
        pick("UniqueId")
            .or_else(|| pick("ObjectId"))
            .unwrap_or_else(|| format!("disk-{number}")),
    )
}

pub(crate) fn query_disks(wmi: &Wmi) -> Result<Vec<DiskRow>, StorageError> {
    let mut rows = Vec::new();
    for obj in wmi.query("SELECT * FROM MSFT_Disk")? {
        let Some(number) = obj.get_u32("Number") else {
            continue;
        };
        let id = disk_id_of(&obj, number);
        let path = rel_path(&obj)?;
        rows.push(DiskRow {
            obj,
            number,
            id,
            path,
        });
    }
    rows.sort_by_key(|r| r.number);
    Ok(rows)
}

pub(crate) fn find_disk_by_id(wmi: &Wmi, id: &DiskId) -> Result<DiskRow, StorageError> {
    query_disks(wmi)?
        .into_iter()
        .find(|r| r.id == *id)
        .ok_or_else(|| StorageError::NotFound(format!("disk {} not found", id.0)))
}

pub(crate) fn find_disk_by_number(wmi: &Wmi, number: u32) -> Result<Option<DiskRow>, StorageError> {
    Ok(query_disks(wmi)?.into_iter().find(|r| r.number == number))
}

pub(crate) fn query_partitions(wmi: &Wmi, disk: u32) -> Result<Vec<WmiObject>, StorageError> {
    let mut v = wmi.query(&format!(
        "SELECT * FROM MSFT_Partition WHERE DiskNumber = {disk}"
    ))?;
    v.sort_by_key(|p| p.get_u64("Offset").unwrap_or(0));
    Ok(v)
}

pub(crate) fn find_partition(
    wmi: &Wmi,
    disk: u32,
    offset: u64,
) -> Result<Option<WmiObject>, StorageError> {
    Ok(query_partitions(wmi, disk)?
        .into_iter()
        .find(|p| p.get_u64("Offset") == Some(offset)))
}

/// The volume mounted on `part`, via `MSFT_PartitionToVolume`; falls back to matching the
/// partition's drive letter against `MSFT_Volume`.
pub(crate) fn volume_of(wmi: &Wmi, part: &WmiObject) -> Result<Option<WmiObject>, StorageError> {
    let rel = rel_path(part)?;
    let found = wmi.query(&format!(
        "ASSOCIATORS OF {{{rel}}} WHERE AssocClass = MSFT_PartitionToVolume"
    ));
    if let Ok(mut v) = found {
        if !v.is_empty() {
            return Ok(Some(v.remove(0)));
        }
    }
    if let Some(letter) = letter_from(part.get_u16("DriveLetter")) {
        for v in wmi.query("SELECT * FROM MSFT_Volume")? {
            if letter_from(v.get_u16("DriveLetter")) == Some(letter) {
                return Ok(Some(v));
            }
        }
    }
    Ok(None)
}

/// Polls `f` (every 500 ms, up to `tries` times) until it yields a value.
pub(crate) fn wait_for<T>(
    what: &str,
    tries: u32,
    mut f: impl FnMut() -> Result<Option<T>, StorageError>,
) -> Result<T, StorageError> {
    for i in 0..tries {
        if let Some(v) = f()? {
            return Ok(v);
        }
        if i + 1 < tries {
            thread::sleep(Duration::from_millis(500));
        }
    }
    Err(StorageError::NotFound(format!(
        "timed out waiting for {what}"
    )))
}

/// Best-effort `Update-HostStorageCache`.
pub(crate) fn update_cache(wmi: &Wmi) {
    let _ = wmi.call("MSFT_StorageSetting", "UpdateHostStorageCache", &[]);
}

/// `wmi.call` with the method, target and failure logged.
pub(crate) fn call_logged(
    wmi: &Wmi,
    log: &Logger,
    path: &str,
    method: &str,
    inputs: &[(&str, Value)],
) -> Result<WmiObject, StorageError> {
    log.info(&format!("wmi: {method} on {path} with {inputs:?}"));
    wmi.call(path, method, inputs).map_err(|e| {
        log.error(&format!("wmi: {method} failed: {e}"));
        StorageError::Wmi(e)
    })
}

/// `MSFT_Disk.CreatePartition`; returns the new partition.
///
/// With `offset: Some(..)` the partition is created at that explicit offset (valid for real
/// free extents, which are at least 16 MiB into the disk). With `None` no `Offset` is
/// passed and the provider places the partition itself: it rejects explicit offsets below
/// its first usable boundary (about 16 MiB) with code 41012, so template and USB layouts
/// must not pass one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_partition(
    wmi: &Wmi,
    log: &Logger,
    disk: &DiskRow,
    style: PartitionStyle,
    offset: Option<u64>,
    size: u64,
    gpt_type: &str,
    assign_letter: bool,
) -> Result<WmiObject, StorageError> {
    let before: HashSet<u64> = query_partitions(wmi, disk.number)?
        .iter()
        .filter_map(|p| p.get_u64("Offset"))
        .collect();
    let mut inputs = Vec::new();
    if let Some(o) = offset {
        inputs.push(("Offset", Value::U64(o)));
    }
    inputs.push(("Size", Value::U64(size)));
    inputs.push(("AssignDriveLetter", Value::Bool(assign_letter)));
    if style == PartitionStyle::Mbr {
        inputs.push(("MbrType", Value::U16(7)));
    } else {
        inputs.push(("GptType", Value::Str(gpt_type.to_string())));
    }
    call_logged(wmi, log, &disk.path, "CreatePartition", &inputs)?;
    wait_for("the new partition", 20, || {
        update_cache(wmi);
        Ok(query_partitions(wmi, disk.number)?
            .into_iter()
            .find(|p| p.get_u64("Offset").is_some_and(|o| !before.contains(&o))))
    })
}

/// Waits for the volume on `part` and formats it.
pub(crate) fn format_partition(
    wmi: &Wmi,
    log: &Logger,
    disk: u32,
    part: &WmiObject,
    fs: &FsKind,
    label: &str,
    force: bool,
) -> Result<(), StorageError> {
    let fs_name = fs_format_name(fs)?;
    let offset = part.get_u64("Offset").unwrap_or(0);
    let vol = wait_for(
        &format!("volume on partition at offset {offset}"),
        20,
        || {
            update_cache(wmi);
            // Re-read the partition: a stale object can predate the volume.
            match find_partition(wmi, disk, offset)? {
                Some(p) => volume_of(wmi, &p),
                None => Ok(None),
            }
        },
    )?;
    let vol_path = rel_path(&vol)?;
    call_logged(
        wmi,
        log,
        &vol_path,
        "Format",
        &[
            ("FileSystem", Value::Str(fs_name.into())),
            ("FileSystemLabel", Value::Str(label.into())),
            ("Full", Value::Bool(false)),
            ("Force", Value::Bool(force)),
        ],
    )?;
    update_cache(wmi);
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------------------

fn supported_size(part: &WmiObject, wmi: &Wmi) -> Option<(u64, u64)> {
    let rel = rel_path(part).ok()?;
    let out = wmi.call(&rel, "GetSupportedSize", &[]).ok()?;
    Some((out.get_u64("SizeMin")?, out.get_u64("SizeMax")?))
}

fn volume_file_exists(volume_path: &str, relative: &str) -> bool {
    Path::new(volume_path).join(relative).exists()
}

/// Returns the partition and whether its volume carries `sources\boot.wim`.
/// `bitlocker` is the once-per-enumeration BitLocker provider snapshot, or `None` when it is
/// unavailable; protection is then reported as [`Protection::None`]. Any failure to read a
/// volume's status is logged to `log`.
fn read_partition(
    wmi: &Wmi,
    bitlocker: Option<&bitlocker::Provider>,
    log: Option<&Logger>,
    p: &WmiObject,
    disk: &DiskId,
    cache: &mut SupportedCache,
    seen: &mut HashSet<SupportedKey>,
) -> Result<(Partition, bool), StorageError> {
    let kind = match p.get_str("GptType").filter(|s| !s.trim().is_empty()) {
        Some(g) => part_kind_from_gpt(&g),
        None => p
            .get_u16("MbrType")
            .map_or(PartKind::Other, part_kind_from_mbr),
    };
    let mut letter = letter_from(p.get_u16("DriveLetter"));
    let vol = volume_of(wmi, p)?;

    let mut fs = FsKind::Raw;
    let mut label = String::new();
    let mut used = None;
    let mut volume_path = None;
    if let Some(v) = &vol {
        fs = fs_kind_from_str(&v.get_str("FileSystem").unwrap_or_default());
        label = v.get_str("FileSystemLabel").unwrap_or_default();
        if fs != FsKind::Raw {
            if let (Some(total), Some(rem)) = (v.get_u64("Size"), v.get_u64("SizeRemaining")) {
                used = total.checked_sub(rem);
            }
        }
        volume_path = v.get_str("Path").filter(|s| !s.is_empty());
        letter = letter.or_else(|| letter_from(v.get_u16("DriveLetter")));
    }
    let offset = p.get_u64("Offset").unwrap_or(0);
    let size = p.get_u64("Size").unwrap_or(0);
    let supported = if fs == FsKind::Ntfs {
        // GetSupportedSize can take tens of seconds on a large live volume, so reuse the
        // answer while the partition is unchanged.
        let key = (disk.clone(), offset, size, used.map(|u| u / USED_BUCKET));
        seen.insert(key.clone());
        *cache.entry(key).or_insert_with(|| supported_size(p, wmi))
    } else {
        None
    };
    let protection = match (bitlocker, volume_path.as_deref()) {
        (Some(bl), Some(vp)) => match bl.status(vp) {
            Ok(p) => p,
            // Not a BitLocker-capable volume (ESP, recovery, ...): expected, not an error.
            Err(StorageError::NotFound(_)) => Protection::None,
            Err(e) => {
                // Unknown must not look like "not encrypted" without a trace.
                if let Some(l) = log {
                    l.warn(&format!(
                        "BitLocker status of {vp} unavailable, reporting no protection: {e}"
                    ));
                }
                Protection::None
            }
        },
        _ => Protection::None,
    };
    let has_windows = volume_path
        .as_deref()
        .is_some_and(|vp| volume_file_exists(vp, r"Windows\System32\config\SYSTEM"));
    let boot_wim = volume_path
        .as_deref()
        .is_some_and(|vp| volume_file_exists(vp, r"sources\boot.wim"));

    Ok((
        Partition {
            number: p.get_u32("PartitionNumber").unwrap_or(0),
            offset,
            size,
            kind,
            letter,
            label,
            fs,
            used,
            protection,
            supported,
            volume_path,
            has_windows,
        },
        boot_wim,
    ))
}

/// `used` is bucketed in the cache key: on a live volume it drifts by kilobytes between
/// snapshots, which would otherwise defeat the cache. A cached range can therefore be stale
/// by less than one bucket; Windows still enforces the real limit in `Resize`, and
/// `validate` checks the fresh `used` value.
const USED_BUCKET: u64 = 256 * MIB;

/// `(disk, offset, size, used / USED_BUCKET)` identifies a partition state for the
/// `supported` cache.
type SupportedKey = (DiskId, u64, u64, Option<u64>);
type SupportedCache = HashMap<SupportedKey, Option<(u64, u64)>>;

/// A disk is a simulation disk only if its number was listed AND it is a virtual disk.
/// Windows reuses disk numbers, so a number alone could later name a physical disk.
fn is_simulation_disk(simulation_disks: &[u32], number: u32, bus: BusType) -> bool {
    bus == BusType::Virtual && simulation_disks.contains(&number)
}

/// Reads every disk, partition and volume. `simulation_disks` are disk numbers that are
/// flagged `is_simulation` (only when the disk's bus is also virtual).
///
/// Every call runs `GetSupportedSize` for each NTFS volume; [`StorageBackend`] caches it.
pub fn enumerate(wmi: &Wmi, simulation_disks: &[u32]) -> Result<Vec<Disk>, StorageError> {
    enumerate_cached(wmi, simulation_disks, &mut SupportedCache::new(), None)
}

fn enumerate_cached(
    wmi: &Wmi,
    simulation_disks: &[u32],
    cache: &mut SupportedCache,
    log: Option<&Logger>,
) -> Result<Vec<Disk>, StorageError> {
    let mut seen = HashSet::new();
    let out = enumerate_inner(wmi, simulation_disks, cache, &mut seen, log)?;
    // Drop entries for partition states that no longer exist.
    cache.retain(|k, _| seen.contains(k));
    Ok(out)
}

fn enumerate_inner(
    wmi: &Wmi,
    simulation_disks: &[u32],
    cache: &mut SupportedCache,
    seen: &mut HashSet<SupportedKey>,
    log: Option<&Logger>,
) -> Result<Vec<Disk>, StorageError> {
    let winpe = is_winpe();
    // Optional: enumeration must work (as `None`) without BitLocker access. One provider
    // query serves every partition.
    let bl = match bitlocker::Provider::open() {
        Ok(p) => Some(p),
        Err(e) => {
            if let Some(l) = log {
                l.warn(&format!(
                    "BitLocker status unavailable, reporting no protection: {e}"
                ));
            }
            None
        }
    };
    let mut out = Vec::new();
    for row in query_disks(wmi)? {
        let d = &row.obj;
        let mut partitions = Vec::new();
        let mut is_boot_media = false;
        for p in query_partitions(wmi, row.number)? {
            let (part, boot_wim) = read_partition(wmi, bl.as_ref(), log, &p, &row.id, cache, seen)?;
            is_boot_media |= boot_wim;
            partitions.push(part);
        }
        partitions.sort_by_key(|p| p.offset);
        let flagged =
            d.get_bool("IsBoot").unwrap_or(false) || d.get_bool("IsSystem").unwrap_or(false);
        let bus = BusType::from_storage_bus_type(d.get_u16("BusType").unwrap_or(0));
        out.push(Disk {
            id: row.id.clone(),
            number: row.number,
            model: d
                .get_str("FriendlyName")
                .unwrap_or_default()
                .trim()
                .to_string(),
            serial: d
                .get_str("SerialNumber")
                .unwrap_or_default()
                .trim()
                .to_string(),
            size: d.get_u64("Size").unwrap_or(0),
            bus,
            style: partition_style_from(d.get_u16("PartitionStyle").unwrap_or(0)),
            sector_size: d.get_u32("LogicalSectorSize").unwrap_or(512),
            is_boot_media,
            is_running_os: flagged && !winpe,
            is_simulation: is_simulation_disk(simulation_disks, row.number, bus),
            partitions,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------------------

/// Real partition operations over Storage WMI.
///
/// Two explicit modes, never inferred from a set being empty:
/// - [`StorageBackend::new`]: real mode. Any disk may be modified (after `validate`).
/// - [`StorageBackend::new_simulation`]: simulation mode. Only virtual disks whose number
///   was passed in may be modified; an empty list therefore allows nothing at all. This is
///   checked twice in [`Backend::execute`], both before any change.
///
/// The simulation disk list is fixed at construction. Attaching a VHDX afterwards (a new
/// disk number) requires building a new backend.
///
/// `snapshot` is cheap after the first call: the NTFS `supported` range is cached per
/// `(disk, offset, size, used)` and the affected disk's entries are dropped on `execute`.
///
/// # Thread affinity
/// Holds a [`Wmi`] and is therefore `!Send`; construct it on the thread that uses it.
pub struct StorageBackend {
    wmi: Wmi,
    log: Logger,
    simulation: bool,
    simulation_disks: Vec<u32>,
    supported_cache: SupportedCache,
    /// Ids of simulation disks seen by the last `snapshot`. `execute` refuses any other
    /// disk without touching WMI.
    sim_ids: HashSet<DiskId>,
}

impl StorageBackend {
    /// Real mode: connects to Storage WMI on the calling thread. No simulation guard.
    pub fn new(log: Logger) -> Result<Self, StorageError> {
        Self::connect(log, false, Vec::new())
    }

    /// Simulation mode: only the virtual disks in `simulation_disks` can be modified (an
    /// empty list allows nothing). Connects to Storage WMI on the calling thread.
    pub fn new_simulation(log: Logger, simulation_disks: Vec<u32>) -> Result<Self, StorageError> {
        Self::connect(log, true, simulation_disks)
    }

    fn connect(
        log: Logger,
        simulation: bool,
        simulation_disks: Vec<u32>,
    ) -> Result<Self, StorageError> {
        let wmi = Wmi::connect(STORAGE_NS)?;
        Ok(StorageBackend {
            wmi,
            log,
            simulation,
            simulation_disks,
            supported_cache: SupportedCache::new(),
            sim_ids: HashSet::new(),
        })
    }

    fn simulation_on(&self) -> bool {
        self.simulation
    }

    /// First guard: pure, no WMI.
    fn check_known_simulation_disk(&self, id: &DiskId) -> Result<(), StorageError> {
        if self.simulation_on() && !self.sim_ids.contains(id) {
            return Err(StorageError::Refused(format!(
                "disk {} is not a simulation disk; simulation mode only allows changes to \
                 disks listed as simulation disks",
                id.0
            )));
        }
        Ok(())
    }

    fn run(&mut self, op: &Op) -> Result<(), StorageError> {
        self.check_known_simulation_disk(op.disk())?;
        let disk = find_disk_by_id(&self.wmi, op.disk())?;
        // Second guard: the resolved disk must be a virtual disk with a listed number
        // (disk numbers are reused, so the number alone is not enough).
        if self.simulation_on() {
            let bus = BusType::from_storage_bus_type(disk.obj.get_u16("BusType").unwrap_or(0));
            if !is_simulation_disk(&self.simulation_disks, disk.number, bus) {
                return Err(StorageError::Refused(format!(
                    "disk {} (number {}) is not a simulation disk",
                    disk.id.0, disk.number
                )));
            }
        }
        // The disk is about to change: forget its cached resize ranges.
        let id = disk.id.clone();
        self.supported_cache.retain(|k, _| k.0 != id);
        self.log.info(&format!("execute: {op:?}"));
        let wmi = &self.wmi;
        let log = &self.log;
        match op {
            Op::Create {
                offset,
                size,
                fs,
                label,
                ..
            } => {
                fs_format_name(fs)?;
                let style = partition_style_from(disk.obj.get_u16("PartitionStyle").unwrap_or(0));
                let start = align_up(*offset, MIB);
                let part =
                    create_partition(wmi, log, &disk, style, Some(start), *size, GPT_BASIC, true)?;
                format_partition(wmi, log, disk.number, &part, fs, label, false)
            }
            Op::Delete { offset, .. } => {
                let part = require_partition(wmi, disk.number, *offset)?;
                call_logged(wmi, log, &rel_path(&part)?, "DeleteObject", &[])?;
                update_cache(wmi);
                Ok(())
            }
            Op::Format {
                offset, fs, label, ..
            } => {
                let part = require_partition(wmi, disk.number, *offset)?;
                format_partition(wmi, log, disk.number, &part, fs, label, true)
            }
            Op::Resize {
                offset, new_size, ..
            } => {
                let part = require_partition(wmi, disk.number, *offset)?;
                call_logged(
                    wmi,
                    log,
                    &rel_path(&part)?,
                    "Resize",
                    &[("Size", Value::U64(*new_size))],
                )?;
                update_cache(wmi);
                Ok(())
            }
        }
    }
}

fn require_partition(wmi: &Wmi, disk: u32, offset: u64) -> Result<WmiObject, StorageError> {
    find_partition(wmi, disk, offset)?.ok_or_else(|| {
        StorageError::NotFound(format!("no partition at offset {offset} on disk {disk}"))
    })
}

impl Backend for StorageBackend {
    fn snapshot(&mut self) -> Result<Vec<Disk>, BackendError> {
        let disks = enumerate_cached(
            &self.wmi,
            &self.simulation_disks,
            &mut self.supported_cache,
            Some(&self.log),
        )
        .map_err(|e| BackendError::Storage(e.to_string()))?;
        self.sim_ids = disks
            .iter()
            .filter(|d| d.is_simulation)
            .map(|d| d.id.clone())
            .collect();
        Ok(disks)
    }

    fn execute(&mut self, op: &Op) -> Result<(), BackendError> {
        self.run(op).map_err(|e| {
            self.log.error(&format!("execute failed: {e}"));
            match e {
                StorageError::Refused(m) => BackendError::Other(m),
                other => BackendError::Storage(other.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_type_mapping() {
        assert_eq!(BusType::from_storage_bus_type(15), BusType::Virtual);
        assert_eq!(BusType::from_storage_bus_type(14), BusType::Virtual);
        assert_eq!(BusType::from_storage_bus_type(7), BusType::Usb);
        assert_eq!(BusType::from_storage_bus_type(17), BusType::Nvme);
    }

    #[test]
    fn fs_mapping() {
        assert_eq!(fs_kind_from_str("NTFS"), FsKind::Ntfs);
        assert_eq!(fs_kind_from_str("FAT32"), FsKind::Fat32);
        assert_eq!(fs_kind_from_str("exFAT"), FsKind::ExFat);
        assert_eq!(fs_kind_from_str("ReFS"), FsKind::Refs);
        assert_eq!(fs_kind_from_str(""), FsKind::Raw);
        assert_eq!(fs_kind_from_str("RAW"), FsKind::Raw);
        assert_eq!(fs_kind_from_str("UDF"), FsKind::Other("UDF".into()));
        assert_eq!(fs_format_name(&FsKind::ExFat).unwrap(), "exFAT");
        assert!(fs_format_name(&FsKind::Raw).is_err());
    }

    #[test]
    fn style_and_letter_mapping() {
        assert_eq!(partition_style_from(2), PartitionStyle::Gpt);
        assert_eq!(partition_style_from(1), PartitionStyle::Mbr);
        assert_eq!(partition_style_from(0), PartitionStyle::Raw);
        assert_eq!(letter_from(Some(67)), Some('C'));
        assert_eq!(letter_from(Some(0)), None);
        assert_eq!(letter_from(None), None);
        assert_eq!(part_kind_from_mbr(0x07), PartKind::Basic);
        assert_eq!(part_kind_from_mbr(0xEF), PartKind::Esp);
    }

    /// Read-only: needs no admin. Exercises the real WMI mapping.
    #[test]
    fn enumerate_reads_this_machines_disks() {
        let wmi = Wmi::connect(STORAGE_NS).unwrap();
        let disks = enumerate(&wmi, &[]).unwrap();
        assert!(!disks.is_empty());
        for d in &disks {
            assert!(!d.id.0.is_empty());
            assert!(d.size > 0, "disk {} has size 0", d.number);
            assert!(!d.is_simulation);
            assert!(d.partitions.windows(2).all(|w| w[0].offset <= w[1].offset));
            for p in &d.partitions {
                assert!(p.size > 0);
            }
        }
    }

    #[test]
    fn simulation_requires_listed_number_and_virtual_bus() {
        assert!(is_simulation_disk(&[3], 3, BusType::Virtual));
        // A physical disk that inherited a detached VHDX's number is not a simulation disk.
        assert!(!is_simulation_disk(&[3], 3, BusType::Nvme));
        assert!(!is_simulation_disk(&[3], 3, BusType::Usb));
        assert!(!is_simulation_disk(&[3], 4, BusType::Virtual));
        assert!(!is_simulation_disk(&[], 3, BusType::Virtual));
    }

    #[test]
    fn supported_cache_key_changes_with_used() {
        let id = DiskId("d".into());
        let mut cache = SupportedCache::new();
        cache.insert((id.clone(), 1, 2, Some(3)), Some((1, 2)));
        assert!(cache.contains_key(&(id.clone(), 1, 2, Some(3))));
        assert!(!cache.contains_key(&(id, 1, 2, Some(4))));
        // Real keys bucket `used`, so small drift maps to the same entry.
        assert_eq!(1_000u64 / USED_BUCKET, (1_000u64 + 4096) / USED_BUCKET);
    }

    /// Read-only. The second snapshot must come from the cache, not GetSupportedSize.
    #[test]
    fn snapshot_reuses_cached_supported_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let log = Logger::open(dir.path(), "cache").unwrap();
        let mut be = StorageBackend::new(log).unwrap();
        let first = be.snapshot().unwrap();
        assert!(!be.supported_cache.is_empty(), "no NTFS volume was cached");
        let t = std::time::Instant::now();
        let second = be.snapshot().unwrap();
        let again = t.elapsed();
        let supported = |d: &[Disk]| -> Vec<_> {
            d.iter()
                .flat_map(|d| d.partitions.iter().map(|p| p.supported))
                .collect()
        };
        assert_eq!(supported(&first), supported(&second));
        eprintln!("second snapshot took {again:?}");
    }

    #[test]
    fn empty_simulation_set_refuses_everything() {
        // Simulation mode with no VHDX attached yet must not behave like real mode.
        let dir = tempfile::tempdir().unwrap();
        let log = Logger::open(dir.path(), "guard-empty").unwrap();
        let mut be = StorageBackend::new_simulation(log, Vec::new()).unwrap();
        let op = Op::Delete {
            disk: DiskId("any-physical-disk".into()),
            offset: MIB,
        };
        let err = be.execute(&op).unwrap_err();
        assert!(matches!(err, BackendError::Other(_)), "{err}");
        assert!(err.to_string().contains("not a simulation disk"), "{err}");
    }

    #[test]
    fn simulation_guard_refuses_unknown_disk_before_any_wmi_change() {
        let dir = tempfile::tempdir().unwrap();
        let log = Logger::open(dir.path(), "guard").unwrap();
        let mut be = StorageBackend::new_simulation(log, vec![99]).unwrap();
        // No snapshot has marked any disk as a simulation disk, so everything is refused
        // by the first (pure) guard.
        let op = Op::Delete {
            disk: DiskId("not-a-simulation-disk".into()),
            offset: MIB,
        };
        let err = be.execute(&op).unwrap_err();
        assert!(err.to_string().contains("not a simulation disk"), "{err}");
        assert!(matches!(err, BackendError::Other(_)));
    }
}
