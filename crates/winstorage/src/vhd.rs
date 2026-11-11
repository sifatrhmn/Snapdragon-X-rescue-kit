//! Dynamic VHDX creation/attachment (virtdisk) and template layout (Storage WMI).
//!
//! VHDX disks are the simulation targets: real partition operations run against them
//! without risking a physical disk.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use diskcore::{FsKind, PartitionStyle, MIB};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
use windows::Win32::Security::PSECURITY_DESCRIPTOR;
use windows::Win32::Storage::Vhd::{
    AttachVirtualDisk, CreateVirtualDisk, DetachVirtualDisk, GetVirtualDiskPhysicalPath,
    OpenVirtualDisk, ATTACH_VIRTUAL_DISK_FLAG_NONE, ATTACH_VIRTUAL_DISK_PARAMETERS,
    ATTACH_VIRTUAL_DISK_VERSION_1, CREATE_VIRTUAL_DISK_FLAG_NONE, CREATE_VIRTUAL_DISK_PARAMETERS,
    CREATE_VIRTUAL_DISK_VERSION_2, DETACH_VIRTUAL_DISK_FLAG_NONE, OPEN_VIRTUAL_DISK_FLAG_NONE,
    VIRTUAL_DISK_ACCESS_ATTACH_RW, VIRTUAL_DISK_ACCESS_DETACH, VIRTUAL_DISK_ACCESS_GET_INFO,
    VIRTUAL_DISK_ACCESS_MASK, VIRTUAL_DISK_ACCESS_NONE, VIRTUAL_STORAGE_TYPE,
    VIRTUAL_STORAGE_TYPE_DEVICE_VHDX, VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
};

use crate::log::Logger;
use crate::storage::{
    call_logged, create_partition, find_disk_by_number, format_partition, wait_for, StorageError,
    GPT_BASIC, STORAGE_NS,
};
use crate::wmi::{Value, Wmi};

const GPT_ESP: &str = "{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}";
const GPT_RECOVERY: &str = "{de94bba4-06d1-4d40-a16a-bfd50179d6ac}";

const GIB: u64 = 1024 * MIB;

/// Initial partition layout of a new VHDX.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// GPT: MSR 16 MiB (added automatically by the provider when the disk is initialised),
    /// ESP 260 MiB FAT32, NTFS "Windows" (rest minus 1 GiB), Recovery 1 GiB NTFS.
    /// Enumerated order is therefore `[Msr, Esp, Basic, Recovery]`.
    SnapdragonLike,
    /// GPT with no partitions.
    Empty,
}

/// One partition of a template, in layout order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    pub offset: u64,
    pub size: u64,
    pub gpt_type: &'static str,
    /// Filesystem to format with; `None` to leave the partition unformatted.
    pub fs: Option<FsKind>,
    pub label: &'static str,
    pub assign_letter: bool,
}

/// Computes the template's partitions for a disk of `size` bytes. Pure.
pub(crate) fn plan(template: Template, size: u64) -> Result<Vec<Planned>, StorageError> {
    match template {
        Template::Empty => Ok(Vec::new()),
        Template::SnapdragonLike => {
            // `Initialize` on a GPT disk makes the provider create a 16 MiB Microsoft
            // Reserved partition by itself (at ~17 KiB, before anything else), so the plan
            // has no MSR of its own. The first partition we create lands at about 16 MiB
            // (explicit offsets below that are rejected), so the layout is planned from
            // there. The offsets are what the provider is expected to produce (it places
            // partitions contiguously in creation order); they are not passed to it.
            let esp_off = 16 * MIB;
            let win_off = esp_off + 260 * MIB;
            // Last MiB is reserved for the backup GPT.
            let end = size / MIB * MIB - MIB.min(size / MIB * MIB);
            let rec_off = end.checked_sub(GIB).ok_or_else(|| {
                StorageError::Invalid("VHDX is too small for this template".into())
            })?;
            if rec_off <= win_off + GIB {
                return Err(StorageError::Invalid(
                    "VHDX is too small for this template (need at least 2.5 GiB)".into(),
                ));
            }
            Ok(vec![
                Planned {
                    offset: esp_off,
                    size: 260 * MIB,
                    gpt_type: GPT_ESP,
                    fs: Some(FsKind::Fat32),
                    label: "SYSTEM",
                    assign_letter: false,
                },
                Planned {
                    offset: win_off,
                    size: rec_off - win_off,
                    gpt_type: GPT_BASIC,
                    fs: Some(FsKind::Ntfs),
                    label: "Windows",
                    assign_letter: true,
                },
                Planned {
                    offset: rec_off,
                    size: GIB,
                    gpt_type: GPT_RECOVERY,
                    fs: Some(FsKind::Ntfs),
                    label: "Recovery",
                    assign_letter: false,
                },
            ])
        }
    }
}

/// Parses `\\.\PhysicalDrive7` into 7.
pub(crate) fn parse_physical_drive(path: &str) -> Option<u32> {
    let lower = path.trim().to_ascii_lowercase();
    lower.strip_prefix(r"\\.\physicaldrive")?.parse().ok()
}

/// A VHDX attached as a physical disk. Dropping it detaches the disk.
///
/// The attach is not persistent: the underlying handle is kept open for the lifetime of
/// this value, and Windows also detaches the disk if the process dies.
#[derive(Debug)]
pub struct AttachedVhd {
    pub path: PathBuf,
    pub disk_number: u32,
    /// virtdisk handle (stored as an integer so the struct stays `Send`).
    handle: isize,
    log_path: PathBuf,
}

impl Drop for AttachedVhd {
    fn drop(&mut self) {
        if self.handle == 0 {
            return;
        }
        let h = HANDLE(self.handle as *mut c_void);
        self.handle = 0;
        let rc = unsafe { DetachVirtualDisk(h, DETACH_VIRTUAL_DISK_FLAG_NONE, 0) };
        let msg = if rc == ERROR_SUCCESS {
            format!("detached {}", self.path.display())
        } else {
            format!(
                "DetachVirtualDisk failed for {}: error {}",
                self.path.display(),
                rc.0
            )
        };
        let _ = unsafe { CloseHandle(h) };
        match Logger::append(&self.log_path) {
            Ok(l) if rc == ERROR_SUCCESS => l.info(&msg),
            Ok(l) => l.error(&msg),
            Err(_) if rc != ERROR_SUCCESS => eprintln!("{msg}"),
            Err(_) => {}
        }
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn win32(
    what: &'static str,
    rc: windows::Win32::Foundation::WIN32_ERROR,
) -> Result<(), StorageError> {
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(StorageError::Win32 { what, code: rc.0 })
    }
}

fn vhdx_type() -> VIRTUAL_STORAGE_TYPE {
    VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_VHDX,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    }
}

/// Creates the (dynamic) VHDX file at `path`, replacing nothing: fails if it exists.
fn create_vhdx_file(path: &Path, size: u64) -> Result<(), StorageError> {
    let vst = vhdx_type();
    let mut params: CREATE_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
    params.Version = CREATE_VIRTUAL_DISK_VERSION_2;
    params.Anonymous.Version2.MaximumSize = size;
    let w = wide(path);
    let mut handle = HANDLE::default();
    let rc = unsafe {
        CreateVirtualDisk(
            &vst,
            PCWSTR(w.as_ptr()),
            VIRTUAL_DISK_ACCESS_NONE,
            PSECURITY_DESCRIPTOR::default(),
            CREATE_VIRTUAL_DISK_FLAG_NONE,
            0,
            &params,
            None,
            &mut handle,
        )
    };
    win32("CreateVirtualDisk", rc)?;
    let _ = unsafe { CloseHandle(handle) };
    Ok(())
}

/// Opens and attaches the VHDX; the returned handle keeps it attached.
fn attach(path: &Path) -> Result<HANDLE, StorageError> {
    let vst = vhdx_type();
    let w = wide(path);
    let mut handle = HANDLE::default();
    let access = VIRTUAL_DISK_ACCESS_MASK(
        VIRTUAL_DISK_ACCESS_ATTACH_RW.0
            | VIRTUAL_DISK_ACCESS_DETACH.0
            | VIRTUAL_DISK_ACCESS_GET_INFO.0,
    );
    let rc = unsafe {
        OpenVirtualDisk(
            &vst,
            PCWSTR(w.as_ptr()),
            access,
            OPEN_VIRTUAL_DISK_FLAG_NONE,
            None,
            &mut handle,
        )
    };
    win32("OpenVirtualDisk", rc)?;
    let mut params: ATTACH_VIRTUAL_DISK_PARAMETERS = unsafe { std::mem::zeroed() };
    params.Version = ATTACH_VIRTUAL_DISK_VERSION_1;
    let rc = unsafe {
        AttachVirtualDisk(
            handle,
            PSECURITY_DESCRIPTOR::default(),
            ATTACH_VIRTUAL_DISK_FLAG_NONE,
            0,
            Some(&params),
            None,
        )
    };
    if let Err(e) = win32("AttachVirtualDisk", rc) {
        let _ = unsafe { CloseHandle(handle) };
        return Err(e);
    }
    Ok(handle)
}

fn physical_disk_number(handle: HANDLE) -> Result<u32, StorageError> {
    let mut buf = [0u16; 260];
    let mut bytes = (buf.len() * 2) as u32;
    let rc = unsafe {
        GetVirtualDiskPhysicalPath(handle, &mut bytes, windows::core::PWSTR(buf.as_mut_ptr()))
    };
    win32("GetVirtualDiskPhysicalPath", rc)?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..len]);
    parse_physical_drive(&s)
        .ok_or_else(|| StorageError::Invalid(format!("unexpected physical path {s:?}")))
}

/// Creates a dynamic VHDX of `size` bytes at `path`, attaches it, initialises it as GPT and
/// lays out `template`. Needs administrator rights. The disk is detached when the returned
/// value is dropped.
///
/// Connects its own [`Wmi`] on the calling thread for the layout step.
pub fn create_vhdx(
    path: &Path,
    size: u64,
    template: Template,
    log: &Logger,
) -> Result<AttachedVhd, StorageError> {
    // Fail before touching anything if the template cannot fit.
    let planned = plan(template, size)?;
    let abs = std::path::absolute(path)?;
    log.info(&format!(
        "creating VHDX {} ({} MiB, {template:?})",
        abs.display(),
        size / MIB
    ));
    create_vhdx_file(&abs, size)?;
    let handle = attach(&abs)?;
    let mut vhd = AttachedVhd {
        path: abs,
        disk_number: u32::MAX,
        handle: handle.0 as isize,
        log_path: log.path().to_path_buf(),
    };
    vhd.disk_number = physical_disk_number(handle)?;
    log.info(&format!("attached as disk {}", vhd.disk_number));

    let wmi = Wmi::connect(STORAGE_NS)?;
    lay_out(&wmi, log, vhd.disk_number, &planned)?;
    Ok(vhd)
}

fn lay_out(wmi: &Wmi, log: &Logger, number: u32, planned: &[Planned]) -> Result<(), StorageError> {
    // The disk object appears asynchronously after attach.
    let disk = wait_for(&format!("disk {number}"), 40, || {
        find_disk_by_number(wmi, number)
    })?;
    if disk.obj.get_bool("IsOffline") == Some(true) {
        let _ = call_logged(wmi, log, &disk.path, "Online", &[]);
    }
    if disk.obj.get_bool("IsReadOnly") == Some(true) {
        let _ = call_logged(
            wmi,
            log,
            &disk.path,
            "SetAttributes",
            &[("IsReadOnly", Value::Bool(false))],
        );
    }
    call_logged(
        wmi,
        log,
        &disk.path,
        "Initialize",
        &[("PartitionStyle", Value::U16(2))],
    )?;
    for p in planned {
        let part = create_partition(
            wmi,
            log,
            &disk,
            PartitionStyle::Gpt,
            None,
            p.size,
            p.gpt_type,
            p.assign_letter,
        )?;
        if let Some(fs) = &p.fs {
            format_partition(wmi, log, number, &part, fs, p.label, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapdragon_plan_matches_spec() {
        let p = plan(Template::SnapdragonLike, 6 * GIB).unwrap();
        // ESP, Windows, Recovery: the MSR is created by the provider, not by the plan.
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].offset, 16 * MIB);
        assert_eq!(p[0].size, 260 * MIB);
        assert_eq!(p[0].fs, Some(FsKind::Fat32));
        assert_eq!(p[1].label, "Windows");
        assert_eq!(p[1].fs, Some(FsKind::Ntfs));
        assert_eq!(p[2].size, GIB);
        assert_eq!(p[2].offset, 6 * GIB - MIB - GIB);
        for w in p.windows(2) {
            assert_eq!(w[0].offset + w[0].size, w[1].offset, "contiguous");
        }
        for x in &p {
            assert_eq!(x.offset % MIB, 0);
            assert_eq!(x.size % MIB, 0);
        }
        assert_eq!(p[2].offset + p[2].size, 6 * GIB - MIB);
    }

    #[test]
    fn plans_reject_tiny_disks_and_empty_is_empty() {
        assert!(plan(Template::SnapdragonLike, GIB).is_err());
        assert!(plan(Template::SnapdragonLike, 0).is_err());
        assert!(plan(Template::Empty, 0).unwrap().is_empty());
    }

    #[test]
    fn physical_drive_parsing() {
        assert_eq!(parse_physical_drive(r"\\.\PhysicalDrive7"), Some(7));
        assert_eq!(parse_physical_drive(r"\\.\PHYSICALDRIVE12"), Some(12));
        assert_eq!(parse_physical_drive(r"\\.\CdRom0"), None);
        assert_eq!(parse_physical_drive(r"\\.\PhysicalDrive"), None);
    }
}
