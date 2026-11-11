//! nvme-probe: READ-ONLY diagnostics for disks that Windows detects but does not offer.
//!
//! Written for the ProArt PZ13, where the WD SN740 NVMe controller and its disk device
//! start in WinPE but diskpart lists no disk. For every storage adapter (`\\.\ScsiN:`) it
//! lists the LUNs and asks the NVMe controller for Identify Controller, Identify
//! Namespace 1 and the SMART / Health log. For every `\\.\PhysicalDriveN` it reads the
//! length, geometry and device descriptor, and tries to read the first 4 KiB.
//!
//! Nothing is ever written: handles are opened for query or read access only, and only
//! IOCTLs that return information are sent.

use std::ffi::c_void;
use std::fmt::Write as _;

use nvme_parse::*;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const INVALID_HANDLE: isize = -1;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;
const IOCTL_SCSI_GET_INQUIRY_DATA: u32 = 0x0004_100C;

const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_ADAPTER_PROTOCOL_SPECIFIC_PROPERTY: u32 = 49;
const PROPERTY_STANDARD_QUERY: u32 = 0;
const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_IDENTIFY: u32 = 1;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: isize,
    ) -> isize;
    fn DeviceIoControl(
        handle: isize,
        code: u32,
        input: *const c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn ReadFile(
        handle: isize,
        buffer: *mut c_void,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: isize) -> i32;
    fn SetFilePointerEx(handle: isize, distance: i64, new_pos: *mut i64, method: u32) -> i32;
    fn GetLastError() -> u32;
}

/// An open device handle, closed on drop.
struct Device(isize);

impl Device {
    /// Opens `path` with `access` (0 = query only, or GENERIC_READ). Never write access.
    fn open(path: &str, access: u32, flags: u32) -> Result<Device, u32> {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                flags,
                0,
            )
        };
        if h == INVALID_HANDLE {
            Err(unsafe { GetLastError() })
        } else {
            Ok(Device(h))
        }
    }

    /// Sends an information-returning IOCTL; `buf` is used for input and output.
    fn ioctl(&self, code: u32, input_len: usize, buf: &mut [u8]) -> Result<usize, u32> {
        let mut returned = 0u32;
        // SAFETY: `buf` is valid for `buf.len()` bytes and `input_len <= buf.len()`.
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                code,
                buf.as_ptr() as *const c_void,
                input_len as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(returned as usize)
        }
    }
}

impl Device {
    /// Reads `buf.len()` bytes at byte `offset` (both 4 KiB-aligned). Read-only.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, u32> {
        // SAFETY: plain FFI on an open handle; `buf` is valid for its length.
        unsafe {
            if SetFilePointerEx(self.0, offset as i64, std::ptr::null_mut(), 0) == 0 {
                return Err(GetLastError());
            }
            let mut read = 0u32;
            if ReadFile(
                self.0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(GetLastError());
            }
            Ok(read as usize)
        }
    }
}

/// Reads the backup GPT at the end of the disk and scans free space for leftover
/// boot sectors of deleted partitions. Read-only.
fn probe_lost_partitions(rd: &Device, disk_len: u64, free_from: u64, out: &mut String) {
    let mut buf = Box::new(Sector([0u8; HEAD_BYTES]));
    let tail = disk_len - HEAD_BYTES as u64;
    match rd.read_at(tail, &mut buf.0) {
        Ok(_) => match parse_gpt_in(&buf.0, HEAD_BYTES - 512, tail / 512) {
            Ok((summary, entries)) => {
                let _ = writeln!(out, "  BACKUP {summary}");
                for e in &entries {
                    let _ = writeln!(out, "    {}", describe_gpt_entry(e));
                }
                let _ = writeln!(out, "  {} backup GPT partition(s)", entries.len());
            }
            Err(why) => {
                let _ = writeln!(out, "  backup GPT: {why}");
            }
        },
        Err(e) => {
            let _ = writeln!(out, "  backup GPT: read {}", err(e));
        }
    }
    const MIB: u64 = 1024 * 1024;
    let start = free_from.div_ceil(MIB) * MIB;
    let first_end = (start + 4096 * MIB).min(tail);
    // The second range never overlaps the first (small free areas are scanned once).
    let ranges = [
        (start, first_end),
        (tail.saturating_sub(2048 * MIB).max(first_end), tail),
    ];
    let _ = writeln!(
        out,
        "  scanning free space for old boot sectors: {:.0}..{:.0} MB and {:.0}..{:.0} MB (1 MB steps)",
        ranges[0].0 / MIB,
        ranges[0].1 / MIB,
        ranges[1].0 / MIB,
        ranges[1].1 / MIB
    );
    let mut sector = Box::new(Sector([0u8; HEAD_BYTES]));
    let mut hits = 0;
    let mut errors = 0;
    for (from, to) in ranges {
        let mut off = from;
        while off < to {
            match rd.read_at(off, &mut sector.0[..4096]) {
                Ok(_) => {
                    if let Some(kind) = boot_sector_kind(&sector.0[..512]) {
                        hits += 1;
                        if hits <= 40 {
                            let _ =
                                writeln!(out, "    found {kind} boot sector at {} MB", off / MIB);
                        }
                    }
                }
                Err(_) => errors += 1,
            }
            off += MIB;
        }
    }
    let _ = writeln!(
        out,
        "  scan done: {hits} boot sector(s) found, {errors} read error(s)"
    );
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn err(code: u32) -> String {
    let text = match code {
        2 => "file not found",
        5 => "access denied",
        21 => "device not ready",
        23 => "data error (CRC)",
        31 => "device not functioning",
        50 => "request not supported",
        87 => "parameter incorrect",
        1117 => "I/O device error",
        1167 => "device not connected",
        _ => "",
    };
    if text.is_empty() {
        format!("error {code}")
    } else {
        format!("error {code} ({text})")
    }
}

/// Asks the NVMe controller behind an adapter handle for identify or log data.
fn nvme_query(
    dev: &Device,
    data_type: u32,
    value: u32,
    sub: u32,
    len: usize,
) -> Result<Vec<u8>, u32> {
    let mut buf = build_protocol_query(
        STORAGE_ADAPTER_PROTOCOL_SPECIFIC_PROPERTY,
        PROTOCOL_TYPE_NVME,
        data_type,
        value,
        sub,
        len,
    );
    let in_len = buf.len();
    dev.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, in_len, &mut buf)?;
    protocol_payload(&buf).map(<[u8]>::to_vec).ok_or(87)
}

const IOCTL_SCSI_PASS_THROUGH_DIRECT: u32 = 0x0004_D014;
/// sizeof(SCSI_PASS_THROUGH_DIRECT) on 64-bit Windows.
const SPTD_LEN: usize = 56;

/// SCSI_PASS_THROUGH_DIRECT for READ(16) of `blocks` 512-byte blocks at `lba` into `buf`,
/// followed by 32 bytes of sense data. Pure, apart from storing `buf`'s address.
fn sptd_read16(path: u8, target: u8, lun: u8, lba: u64, blocks: u32, buf: &mut [u8]) -> Vec<u8> {
    let mut s = vec![0u8; SPTD_LEN + 32];
    s[0..2].copy_from_slice(&(SPTD_LEN as u16).to_le_bytes());
    s[3] = path;
    s[4] = target;
    s[5] = lun;
    s[6] = 16; // CdbLength
    s[7] = 32; // SenseInfoLength
    s[8] = 1; // SCSI_IOCTL_DATA_IN
    s[12..16].copy_from_slice(&(blocks * 512).to_le_bytes());
    s[16..20].copy_from_slice(&10u32.to_le_bytes()); // timeout, seconds
    s[24..32].copy_from_slice(&(buf.as_mut_ptr() as u64).to_le_bytes());
    s[32..36].copy_from_slice(&(SPTD_LEN as u32).to_le_bytes()); // SenseInfoOffset
    let cdb = &mut s[36..52];
    cdb[0] = 0x88; // READ(16)
    cdb[2..10].copy_from_slice(&lba.to_be_bytes());
    cdb[10..14].copy_from_slice(&blocks.to_be_bytes());
    s
}

/// READ-ONLY test of the adapter pass-through path: opens the adapter for read/write
/// (open only), then reads LBA 1 with SCSI READ(16) and compares it with `expected`.
fn passthrough_read_test(n: u32, lun: (u8, u8, u8), expected: Option<&[u8]>, out: &mut String) {
    let path = format!(r"\\.\Scsi{n}:");
    let dev = match Device::open(&path, GENERIC_READ | GENERIC_WRITE, 0) {
        Ok(d) => {
            let _ = writeln!(out, "  adapter open for read/write (test only): OK");
            d
        }
        Err(e) => {
            let _ = writeln!(out, "  adapter open for read/write (test only): {}", err(e));
            return;
        }
    };
    let mut data = Box::new(Sector([0u8; HEAD_BYTES]));
    let mut s = sptd_read16(lun.0, lun.1, lun.2, 1, 1, &mut data.0[..512]);
    let in_len = s.len();
    match dev.ioctl(IOCTL_SCSI_PASS_THROUGH_DIRECT, in_len, &mut s) {
        Ok(_) => {
            let status = s[2];
            let got = &data.0[..512];
            let same = expected.map(|e| e.get(512..1024) == Some(got));
            let _ = writeln!(
                out,
                "  pass-through READ(16) LBA 1: SCSI status 0x{status:02X}, starts '{}', identical to normal read: {}",
                String::from_utf8_lossy(&got[..8]),
                match same {
                    Some(true) => "yes",
                    Some(false) => "NO",
                    None => "unknown",
                }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "  pass-through READ(16) LBA 1: {}", err(e));
        }
    }
}

fn probe_adapter(n: u32, out: &mut String) -> bool {
    let path = format!(r"\\.\Scsi{n}:");
    let dev = match Device::open(&path, 0, 0) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let _ = writeln!(out, "=== adapter {path}");
    let mut buf = vec![0u8; 16 * 1024];
    match dev.ioctl(IOCTL_SCSI_GET_INQUIRY_DATA, 0, &mut buf) {
        Ok(_) => {
            let luns = parse_inquiry_data(&buf);
            if let Some(first) = luns.first() {
                // Compare with a normal read of PhysicalDrive0 (the internal NVMe here).
                let mut head = Box::new(Sector([0u8; HEAD_BYTES]));
                let expected =
                    Device::open(r"\\.\PhysicalDrive0", GENERIC_READ, FILE_FLAG_NO_BUFFERING)
                        .ok()
                        .and_then(|d| d.read_at(0, &mut head.0).ok())
                        .map(|_| head.0.to_vec());
                passthrough_read_test(
                    n,
                    (first.path, first.target, first.lun),
                    expected.as_deref(),
                    out,
                );
            }
            for l in luns {
                let _ = writeln!(
                    out,
                    "  LUN path {} target {} lun {}: claimed={} type=0x{:02X} vendor='{}' product='{}' rev='{}'",
                    l.path, l.target, l.lun, l.claimed, l.device_type, l.vendor, l.product, l.revision
                );
            }
        }
        Err(e) => {
            let _ = writeln!(out, "  inquiry data: {}", err(e));
        }
    }
    match nvme_query(&dev, NVME_DATA_TYPE_IDENTIFY, 1, 0, 4096) {
        Ok(d) => {
            let _ = writeln!(out, "  NVMe identify controller:");
            for line in describe_controller(&d) {
                let _ = writeln!(out, "    {line}");
            }
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "  NVMe identify controller: {} (not an NVMe adapter?)",
                err(e)
            );
            return true;
        }
    }
    match nvme_query(&dev, NVME_DATA_TYPE_IDENTIFY, 2, 0, 4096) {
        Ok(d) => {
            let _ = writeln!(
                out,
                "  NVMe active namespace IDs: {:?}",
                active_namespaces(&d)
            );
        }
        Err(e) => {
            let _ = writeln!(out, "  NVMe active namespace list: {}", err(e));
        }
    }
    match nvme_query(&dev, NVME_DATA_TYPE_IDENTIFY, 0, 1, 4096) {
        Ok(d) => {
            let _ = writeln!(out, "  NVMe identify namespace 1:");
            for line in describe_namespace(&d) {
                let _ = writeln!(out, "    {line}");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "  NVMe identify namespace 1: {}", err(e));
        }
    }
    // For log pages the sub-value is the byte offset into the page, not a namespace ID.
    match nvme_query(&dev, NVME_DATA_TYPE_LOG_PAGE, 2, 0, 512) {
        Ok(d) => {
            let _ = writeln!(out, "  NVMe SMART / health log:");
            for line in describe_health(&d) {
                let _ = writeln!(out, "    {line}");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "  NVMe SMART / health log: {}", err(e));
        }
    }
    true
}

/// Bytes read from the start of each disk: MBR, GPT header and up to 128 GPT entries.
const HEAD_BYTES: usize = 64 * 1024;

/// A buffer aligned to 4 KiB, as unbuffered disk reads require.
#[repr(C, align(4096))]
struct Sector([u8; HEAD_BYTES]);

const IOCTL_DISK_GET_DRIVE_LAYOUT_EX: u32 = 0x0007_0050;
const IOCTL_DISK_GET_DISK_ATTRIBUTES: u32 = 0x0007_00F0;

fn probe_disk(n: u32, out: &mut String) -> bool {
    let path = format!(r"\\.\PhysicalDrive{n}");
    let dev = match Device::open(&path, 0, 0) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let _ = writeln!(out, "=== {path}");
    let mut q = vec![0u8; 1024];
    q[0..4].copy_from_slice(&STORAGE_DEVICE_PROPERTY.to_le_bytes());
    q[4..8].copy_from_slice(&PROPERTY_STANDARD_QUERY.to_le_bytes());
    match dev.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, 12, &mut q) {
        Ok(_) => {
            let _ = writeln!(out, "  {}", describe_device_descriptor(&q));
        }
        Err(e) => {
            let _ = writeln!(out, "  device descriptor: {}", err(e));
        }
    }
    let mut geo = vec![0u8; 256];
    match dev.ioctl(IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, 0, &mut geo) {
        Ok(_) => {
            let bps = u32::from_le_bytes(geo[20..24].try_into().unwrap());
            let size = u64::from_le_bytes(geo[24..32].try_into().unwrap());
            let _ = writeln!(
                out,
                "  geometry: {bps} bytes/sector, disk size {size} bytes"
            );
        }
        Err(e) => {
            let _ = writeln!(out, "  geometry: {}", err(e));
        }
    }
    drop(dev);
    // Only OPENS the disk with write access and closes it again; nothing is written.
    match Device::open(&path, GENERIC_READ | GENERIC_WRITE, 0) {
        Ok(_) => {
            let _ = writeln!(out, "  open for writing (test only, nothing written): OK");
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "  open for writing (test only, nothing written): {}",
                err(e)
            );
        }
    }
    match Device::open(&path, GENERIC_READ, FILE_FLAG_NO_BUFFERING) {
        Ok(rd) => {
            // IOCTL_DISK_GET_LENGTH_INFO needs a handle with read access.
            let mut len = vec![0u8; 8];
            let mut disk_len = 0u64;
            match rd.ioctl(IOCTL_DISK_GET_LENGTH_INFO, 0, &mut len) {
                Ok(_) => {
                    let bytes = u64::from_le_bytes(len[0..8].try_into().unwrap());
                    disk_len = bytes;
                    let _ = writeln!(
                        out,
                        "  length: {bytes} bytes ({:.1} GB)",
                        bytes as f64 / 1e9
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "  length: {}", err(e));
                }
            }
            let mut attrs = vec![0u8; 16];
            match rd.ioctl(IOCTL_DISK_GET_DISK_ATTRIBUTES, 0, &mut attrs) {
                Ok(_) => {
                    let a = u64::from_le_bytes(attrs[8..16].try_into().unwrap());
                    let _ = writeln!(
                        out,
                        "  disk attributes 0x{a:X} (offline: {}, read-only: {})",
                        a & 1 != 0,
                        a & 2 != 0
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "  disk attributes: {}", err(e));
                }
            }
            let mut layout = vec![0u8; 64 * 1024];
            match rd.ioctl(IOCTL_DISK_GET_DRIVE_LAYOUT_EX, 0, &mut layout) {
                Ok(_) => {
                    let style = match u32::from_le_bytes(layout[0..4].try_into().unwrap()) {
                        0 => "MBR",
                        1 => "GPT",
                        _ => "RAW",
                    };
                    let count = u32::from_le_bytes(layout[4..8].try_into().unwrap());
                    let _ = writeln!(
                        out,
                        "  Windows drive layout: {style}, {count} partition slot(s)"
                    );
                }
                Err(e) => {
                    let _ = writeln!(out, "  Windows drive layout: {}", err(e));
                }
            }
            let mut sector = Box::new(Sector([0u8; HEAD_BYTES]));
            let mut read = 0u32;
            // SAFETY: `sector` is a valid, aligned HEAD_BYTES buffer.
            let ok = unsafe {
                ReadFile(
                    rd.0,
                    sector.0.as_mut_ptr() as *mut c_void,
                    HEAD_BYTES as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let _ = writeln!(
                    out,
                    "  read first 64 KiB: {}",
                    err(unsafe { GetLastError() })
                );
            } else {
                let _ = writeln!(
                    out,
                    "  read first 64 KiB: OK ({read} bytes); {}",
                    describe_first_sectors(&sector.0[..4096])
                );
                match parse_gpt(&sector.0[..read as usize]) {
                    Ok((summary, entries)) => {
                        let _ = writeln!(out, "  {summary}");
                        for e in &entries {
                            let _ = writeln!(out, "    {}", describe_gpt_entry(e));
                        }
                        let _ = writeln!(out, "  {} GPT partition(s)", entries.len());
                        let used_to = entries.iter().map(|e| (e.last_lba + 1) * 512).max();
                        // Only worth scanning a large, mostly empty internal disk.
                        if disk_len > 64 * 1024 * 1024 * 1024 {
                            probe_lost_partitions(&rd, disk_len, used_to.unwrap_or(0), out);
                        }
                    }
                    Err(why) => {
                        let _ = writeln!(out, "  GPT: {why}");
                    }
                }
            }
        }
        Err(e) => {
            let _ = writeln!(out, "  open for reading: {}", err(e));
        }
    }
    true
}

fn main() {
    let mut out = String::new();
    let _ = writeln!(out, "nvme-probe {} (read-only)", env!("CARGO_PKG_VERSION"));
    let mut adapters = 0;
    for n in 0..16 {
        if probe_adapter(n, &mut out) {
            adapters += 1;
        }
    }
    let mut disks = 0;
    for n in 0..32 {
        if probe_disk(n, &mut out) {
            disks += 1;
        }
    }
    let _ = writeln!(
        out,
        "=== summary: {adapters} storage adapter(s), {disks} PhysicalDrive(s)"
    );
    print!("{out}");
}
