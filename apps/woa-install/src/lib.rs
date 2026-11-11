//! woa-install: prepares the ProArt PZ13 internal SSD for a clean Windows install.
//!
//! The SSD holds 11 Qualcomm/ASUS firmware partitions that must never be touched, and its
//! Windows partitions were deleted, so Windows' disk management (diskpart, Setup, ASUS
//! WinRE) hides the disk. This tool works below that layer:
//!
//! `woa-install inspect` — read-only: print every disk's layout as Windows reports it.
//! `woa-install prepare --backup-dir <dir> [--dry-run]` —
//!   1. find exactly one NVMe disk of the PZ13 size whose first partitions are the 11
//!      recorded firmware partitions (refuse anything else);
//!   2. save the first and last 64 KiB of the disk (both GPT copies) to `<dir>`;
//!   3. ask the operator to type INSTALL;
//!   4. clear the first and last MiB of each new partition's area (free space only, so no
//!      stale file system is mounted), then add EFI 260 MiB, MSR 16 MiB, Windows and
//!      Recovery 1 GiB after the firmware partitions, keeping those entries verbatim;
//!   5. re-read the layout and both on-disk GPTs and check the firmware entries are
//!      unchanged;
//!   6. give the new volumes the letters S: (EFI), W: (Windows) and R: (Recovery).
//!
//! A disk that already has exactly these four partitions skips steps 2–5.
//!
//! Exit codes: 0 done, 1 refused or failed before anything was written, 2 failed after
//! writing started (the backup in `<dir>` holds the original GPTs).

pub mod ffi;
pub mod layout;

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use ffi::*;
use layout::*;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
const IOCTL_DISK_GET_DRIVE_LAYOUT_EX: u32 = 0x0007_0050;
const IOCTL_DISK_SET_DRIVE_LAYOUT_EX: u32 = 0x0007_C054;
const IOCTL_DISK_UPDATE_PROPERTIES: u32 = 0x0007_0140;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
const IOCTL_DISK_GET_DISK_ATTRIBUTES: u32 = 0x0007_00F0;
const IOCTL_DISK_SET_DISK_ATTRIBUTES: u32 = 0x0007_C0F4;
/// Access rights that touch only a handle's attributes, never its data. On a Snapdragon
/// machine partmgr refuses CreateFile for FILE_WRITE_DATA on the protected firmware disk,
/// but a handle opened for attributes alone is allowed, which is enough for
/// IOCTL_DISK_SET_DISK_ATTRIBUTES.
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
/// `DISK_ATTRIBUTE_HIDDEN` (ntdddisk.h). A hidden disk is skipped by diskpart/VDS/Setup.
const DISK_ATTRIBUTE_HIDDEN: u64 = 0x4;
const BUS_TYPE_NVME: u32 = 17;
/// `BusTypeFileBackedVirtual`: an attached VHD(X). Only the rehearsal may target one.
const BUS_TYPE_VIRTUAL: u32 = 15;

/// How [`prepare`] runs. The command line only sets `backup_dir` and `dry_run`; the other
/// fields exist for the VHDX rehearsal (`examples/vhd_rehearsal.rs`).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub backup_dir: PathBuf,
    pub dry_run: bool,
    /// Also accept a file-backed virtual disk (never set on the command line).
    pub allow_virtual_test_disk: bool,
    /// Use this as the operator's answer instead of reading the console.
    pub answer: Option<String>,
}
const HEAD: usize = 64 * 1024;

/// A sector-aligned 1 MiB buffer (unbuffered I/O needs aligned memory).
#[repr(C, align(4096))]
struct Block([u8; MIB as usize]);

fn block() -> Box<Block> {
    // Allocate on the heap directly; 1 MiB is too large for the stack.
    let layout = std::alloc::Layout::new::<Block>();
    // SAFETY: the layout is non-zero; zeroed memory is a valid `Block`.
    unsafe {
        let p = std::alloc::alloc_zeroed(layout) as *mut Block;
        if p.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(p)
    }
}

/// Prints to the console and appends to the log file.
struct Log(Option<File>);

impl Log {
    fn say(&mut self, s: &str) {
        println!("{s}");
        if let Some(f) = &mut self.0 {
            let _ = writeln!(f, "{s}");
            let _ = f.flush();
        }
    }
}

macro_rules! say {
    ($log:expr, $($t:tt)*) => { $log.say(&format!($($t)*)) };
}

struct Disk {
    number: u32,
    path: String,
    model: String,
    length: u64,
    bus: u32,
    layout: Result<Layout, String>,
}

fn cstr(b: &[u8], o: usize) -> String {
    if o == 0 || o >= b.len() {
        return String::new();
    }
    let end = b[o..]
        .iter()
        .position(|&c| c == 0)
        .map_or(b.len(), |p| o + p);
    String::from_utf8_lossy(&b[o..end]).trim().to_string()
}

fn read_disk(n: u32) -> Option<Disk> {
    let path = format!(r"\\.\PhysicalDrive{n}");
    let dev = Device::open(&path, GENERIC_READ, 0).ok()?;
    let mut q = vec![0u8; 1024];
    let (mut model, mut bus) = (String::new(), 0);
    if dev
        .ioctl_out(IOCTL_STORAGE_QUERY_PROPERTY, &[0u8; 12], &mut q)
        .is_ok()
    {
        model = cstr(
            &q,
            u32::from_le_bytes(q[16..20].try_into().unwrap()) as usize,
        );
        bus = u32::from_le_bytes(q[28..32].try_into().unwrap());
    }
    let mut len = [0u8; 8];
    let length = dev
        .ioctl_out(IOCTL_DISK_GET_LENGTH_INFO, &[], &mut len)
        .map(|_| u64::from_le_bytes(len))
        .unwrap_or(0);
    let mut buf = vec![0u8; HEADER_LEN + 256 * ENTRY_LEN];
    let layout = match dev.ioctl_out(IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], &mut buf) {
        Ok(_) => parse_layout(&buf),
        Err(e) => Err(describe(e)),
    };
    Some(Disk {
        number: n,
        path,
        model,
        length,
        bus,
        layout,
    })
}

fn disks() -> Vec<Disk> {
    (0..32).filter_map(read_disk).collect()
}

fn print_layout(log: &mut Log, l: &Layout) {
    say!(
        log,
        "  GPT disk {}, usable {} .. {} MiB",
        l.disk_id,
        l.usable_start / MIB,
        l.usable_end / MIB
    );
    for (i, p) in l.partitions.iter().enumerate() {
        say!(
            log,
            "    {:>2}. {:<16} start {:>8} MiB  size {:>8} MiB  type {}  attr 0x{:016X}",
            i + 1,
            if p.name.is_empty() {
                "(no name)"
            } else {
                &p.name
            },
            p.start / MIB,
            p.length / MIB,
            p.type_guid,
            p.attributes
        );
    }
}

/// Read-only: prints every disk's layout as Windows reports it.
pub fn inspect() -> i32 {
    let mut log = Log(None);
    for d in disks() {
        say!(
            log,
            "{}: '{}' bus {} {:.1} GB",
            d.path,
            d.model,
            d.bus,
            d.length as f64 / 1e9
        );
        match &d.layout {
            Ok(l) => {
                print_layout(&mut log, l);
                match check_firmware(l) {
                    Ok(extra) => say!(
                        log,
                        "  => PZ13 firmware layout ({extra} partition(s) after it)"
                    ),
                    Err(why) => say!(log, "  => not the PZ13 disk: {why}"),
                }
            }
            Err(e) => say!(log, "  layout: {e}"),
        }
    }
    0
}

/// Finds the one disk this tool may change.
fn find_target(log: &mut Log, allow_virtual: bool) -> Result<(Disk, Layout, usize), String> {
    let mut found = Vec::new();
    for d in disks() {
        let bus_ok = d.bus == BUS_TYPE_NVME || (allow_virtual && d.bus == BUS_TYPE_VIRTUAL);
        let verdict = if !bus_ok {
            Err("not NVMe".to_string())
        } else if d.length != PZ13_DISK_BYTES {
            Err(format!("size {} is not the PZ13 SSD size", d.length))
        } else {
            match &d.layout {
                Err(e) => Err(format!("layout: {e}")),
                Ok(l) => check_firmware(l).map(|extra| (l.clone(), extra)),
            }
        };
        match verdict {
            Ok((l, extra)) => found.push((d, l, extra)),
            Err(why) => say!(log, "skipping {} ('{}'): {why}", d.path, d.model),
        }
    }
    match found.len() {
        1 => Ok(found.pop().unwrap()),
        0 => Err("no disk matches the PZ13 SSD with its 11 firmware partitions".into()),
        n => Err(format!("{n} disks match; refusing to guess")),
    }
}

/// Reads the 64-bit disk attributes (GET_DISK_ATTRIBUTES is 16 bytes: Version, Reserved,
/// Attributes). The handle needs no more than FILE_READ_ATTRIBUTES.
fn read_attributes(dev: &Device) -> Result<u64, u32> {
    let mut buf = [0u8; 16];
    dev.ioctl_out(IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut buf)?;
    Ok(u64::from_le_bytes(buf[8..16].try_into().unwrap()))
}

/// Clears (or, with `set`, sets) `DISK_ATTRIBUTE_HIDDEN` on the PZ13 SSD so the normal
/// Windows stack (diskpart, Setup, the Rescue Kit) will show it. This changes only the
/// disk's attribute flag; it writes nothing to any partition and is fully reversible.
///
/// It opens the disk for attributes only (not data), because on a Snapdragon machine
/// partmgr refuses a data-write handle to this protected disk but allows an attribute one.
pub fn set_hidden(backup_dir: &Path, set: bool) -> i32 {
    let _ = fs::create_dir_all(backup_dir);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(backup_dir.join("woa-install.log"))
        .ok();
    let mut log = Log(log_file);
    say!(
        log,
        "woa-install {} {}",
        env!("CARGO_PKG_VERSION"),
        if set { "rehide" } else { "unhide" }
    );
    let (d, _l, _extra) = match find_target(&mut log, false) {
        Ok(t) => t,
        Err(e) => {
            say!(log, "REFUSED: {e}. Nothing was changed.");
            return 1;
        }
    };
    say!(log, "target: {} '{}'", d.path, d.model);
    // A read-only open is enough to report the current state.
    match Device::open(&d.path, FILE_READ_ATTRIBUTES, 0).and_then(|dev| read_attributes(&dev)) {
        Ok(a) => say!(
            log,
            "current disk attributes 0x{a:X} (hidden: {})",
            a & DISK_ATTRIBUTE_HIDDEN != 0
        ),
        Err(e) => say!(log, "could not read attributes: {}", describe(e)),
    }
    let dev = match Device::open(&d.path, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, 0) {
        Ok(x) => x,
        Err(e) => {
            say!(
                log,
                "FAILED: cannot open {} for attribute change: {}. Nothing was changed.",
                d.path,
                describe(e)
            );
            return 2;
        }
    };
    // SET_DISK_ATTRIBUTES (40 bytes): Version, Persist, Reserved1[3], Attributes,
    // AttributesMask, Reserved2[4]. Mask limits the change to the HIDDEN bit alone.
    let mut buf = vec![0u8; 40];
    buf[0..4].copy_from_slice(&40u32.to_le_bytes());
    buf[4] = 1; // Persist
    let attrs: u64 = if set { DISK_ATTRIBUTE_HIDDEN } else { 0 };
    buf[8..16].copy_from_slice(&attrs.to_le_bytes());
    buf[16..24].copy_from_slice(&DISK_ATTRIBUTE_HIDDEN.to_le_bytes());
    if let Err(e) = dev.ioctl_in(IOCTL_DISK_SET_DISK_ATTRIBUTES, &buf) {
        say!(
            log,
            "FAILED: could not {} the disk: {}. Nothing was changed.",
            if set { "hide" } else { "unhide" },
            describe(e)
        );
        return 2;
    }
    let _ = dev.ioctl_in(IOCTL_DISK_UPDATE_PROPERTIES, &[]);
    drop(dev);
    // Report the new state from a fresh read.
    match Device::open(&d.path, FILE_READ_ATTRIBUTES, 0).and_then(|dev| read_attributes(&dev)) {
        Ok(a) => {
            let hidden = a & DISK_ATTRIBUTE_HIDDEN != 0;
            say!(log, "new disk attributes 0x{a:X} (hidden: {hidden})");
            if hidden == set {
                say!(
                    log,
                    "{}",
                    if set {
                        "DONE: the disk is hidden again."
                    } else {
                        "DONE: the disk is no longer hidden."
                    }
                );
                0
            } else {
                say!(log, "FAILED: the attribute did not change (the firmware may re-apply it). Nothing else was done.");
                2
            }
        }
        Err(e) => {
            say!(
                log,
                "changed, but could not re-read attributes: {}",
                describe(e)
            );
            0
        }
    }
}

fn backup(
    dev: &Device,
    d: &Disk,
    l: &Layout,
    dir: &Path,
    log: &mut Log,
) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_secs())
        .unwrap_or(0);
    let out = dir.join(format!("pz13-gpt-{stamp}"));
    fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let mut b = block();
    dev.read_at(0, &mut b.0[..HEAD])
        .map_err(|e| format!("read disk start: {}", describe(e)))?;
    fs::write(out.join("head-64k.bin"), &b.0[..HEAD]).map_err(|e| e.to_string())?;
    let tail = d.length - HEAD as u64;
    dev.read_at(tail, &mut b.0[..HEAD])
        .map_err(|e| format!("read disk end: {}", describe(e)))?;
    fs::write(out.join("tail-64k.bin"), &b.0[..HEAD]).map_err(|e| e.to_string())?;
    let mut text = format!(
        "{} '{}' {} bytes, tail offset {tail}\n",
        d.path, d.model, d.length
    );
    for p in &l.partitions {
        text += &format!(
            "{} start {} length {} type {} id {} attr 0x{:X}\n",
            p.name, p.start, p.length, p.type_guid, p.id_guid, p.attributes
        );
    }
    fs::write(out.join("layout.txt"), text).map_err(|e| e.to_string())?;
    // Read the files back: a backup that cannot be read is no backup.
    for f in ["head-64k.bin", "tail-64k.bin"] {
        let n = fs::metadata(out.join(f)).map(|m| m.len()).unwrap_or(0);
        if n != HEAD as u64 {
            return Err(format!("backup file {f} is {n} bytes, expected {HEAD}"));
        }
    }
    say!(
        log,
        "backup of both partition tables saved in {}",
        out.display()
    );
    Ok(out)
}

fn confirm(log: &mut Log, answer: Option<&str>) -> bool {
    say!(log, "");
    say!(
        log,
        "Next, these Windows partitions are formatted and a clean Windows is installed.\r\nType INSTALL and press Enter to continue (anything else cancels):"
    );
    let mut line = String::new();
    match answer {
        Some(a) => line = a.to_string(),
        None => {
            let _ = std::io::stdin().lock().read_line(&mut line);
        }
    }
    let ok = line.trim() == "INSTALL";
    say!(
        log,
        "operator typed '{}': {}",
        line.trim(),
        if ok { "continuing" } else { "cancelled" }
    );
    ok
}

/// Areas cleared before the partitions are created: the first MiB of each, and the last
/// MiB of the Windows and Recovery partitions (NTFS keeps a backup boot sector there).
fn areas_to_clear(plan: &[NewPart]) -> Vec<u64> {
    let mut v = Vec::new();
    for p in plan {
        v.push(p.start);
        if p.label == "Windows" || p.label == "Recovery" {
            v.push(p.start + p.length - MIB);
        }
    }
    v
}

/// Clears DISK_ATTRIBUTE_HIDDEN if it is set, so the data-write handle in [`partition`] is
/// not refused by partmgr's Snapdragon protection. Attribute-only change; writes no data.
fn ensure_not_hidden(d: &Disk, log: &mut Log) {
    let Ok(dev) = Device::open(&d.path, FILE_READ_ATTRIBUTES, 0) else {
        return;
    };
    if read_attributes(&dev).unwrap_or(0) & DISK_ATTRIBUTE_HIDDEN == 0 {
        return;
    }
    drop(dev);
    let Ok(dev) = Device::open(&d.path, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, 0) else {
        return;
    };
    let mut buf = vec![0u8; 40];
    buf[0..4].copy_from_slice(&40u32.to_le_bytes());
    buf[4] = 1;
    buf[16..24].copy_from_slice(&DISK_ATTRIBUTE_HIDDEN.to_le_bytes());
    if dev.ioctl_in(IOCTL_DISK_SET_DISK_ATTRIBUTES, &buf).is_ok() {
        let _ = dev.ioctl_in(IOCTL_DISK_UPDATE_PROPERTIES, &[]);
        say!(
            log,
            "cleared the hidden attribute so the disk can be written"
        );
    }
}

fn partition(d: &Disk, l: &Layout, planned: &[NewPart], log: &mut Log) -> Result<(), String> {
    ensure_not_hidden(d, log);
    let dev = Device::open(
        &d.path,
        GENERIC_READ | GENERIC_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
    )
    .map_err(|e| format!("open {} for writing: {}", d.path, describe(e)))?;
    let fw_end = l
        .partitions
        .iter()
        .take(PZ13_FIRMWARE.len())
        .map(|p| p.start + p.length)
        .max()
        .unwrap_or(0);
    let zero = block();
    for off in areas_to_clear(planned) {
        if off < fw_end || off + MIB > l.usable_end {
            return Err(format!(
                "internal error: area at {} MiB is outside the free space",
                off / MIB
            ));
        }
        dev.write_at(off, &zero.0)
            .map_err(|e| format!("clear {} MiB: {}", off / MIB, describe(e)))?;
    }
    say!(log, "cleared stale boot sectors in the free space");
    let new: Vec<Vec<u8>> = planned
        .iter()
        .map(|n| new_entry(n, v4_guid(random16())))
        .collect();
    let buf = build_layout(l, &new);
    dev.ioctl_in(IOCTL_DISK_SET_DRIVE_LAYOUT_EX, &buf)
        .map_err(|e| format!("IOCTL_DISK_SET_DRIVE_LAYOUT_EX: {}", describe(e)))?;
    let _ = dev.ioctl_in(IOCTL_DISK_UPDATE_PROPERTIES, &[]);
    say!(log, "partition table written");
    Ok(())
}

/// Re-reads the layout and both on-disk GPT copies and checks the result.
fn verify(d: &Disk, before: &Layout, planned: &[NewPart], log: &mut Log) -> Result<(), String> {
    let now = read_disk(d.number).ok_or("disk vanished")?;
    let l = now.layout.map_err(|e| format!("re-read layout: {e}"))?;
    if check_firmware(&l)? != planned.len() || !matches_plan(&l, planned) {
        return Err("the new layout is not what was planned".into());
    }
    for (a, b) in l.partitions.iter().zip(&before.partitions) {
        if (
            a.start,
            a.length,
            &a.type_guid,
            &a.id_guid,
            a.attributes,
            &a.name,
        ) != (
            b.start,
            b.length,
            &b.type_guid,
            &b.id_guid,
            b.attributes,
            &b.name,
        ) {
            return Err(format!("firmware partition '{}' changed", b.name));
        }
    }
    let dev = Device::open(&d.path, GENERIC_READ, FILE_FLAG_NO_BUFFERING).map_err(describe)?;
    let mut b = block();
    dev.read_at(0, &mut b.0[..HEAD]).map_err(describe)?;
    let (_, primary) = nvme_parse::parse_gpt(&b.0[..HEAD])?;
    let tail = d.length - HEAD as u64;
    dev.read_at(tail, &mut b.0[..HEAD]).map_err(describe)?;
    let (_, backup) = nvme_parse::parse_gpt_in(&b.0[..HEAD], HEAD - 512, tail / 512)?;
    for (which, entries) in [("primary", &primary), ("backup", &backup)] {
        if entries.len() != l.partitions.len() {
            return Err(format!(
                "{which} GPT has {} entries, expected {}",
                entries.len(),
                l.partitions.len()
            ));
        }
        for (e, p) in entries.iter().zip(&l.partitions) {
            if e.first_lba * 512 != p.start || e.type_guid != p.type_guid {
                return Err(format!(
                    "{which} GPT entry {} differs from the layout",
                    e.index
                ));
            }
        }
    }
    say!(
        log,
        "verified: firmware partitions unchanged; both GPT copies hold {} partitions",
        l.partitions.len()
    );
    print_layout(log, &l);
    Ok(())
}

/// The `\\?\Volume{...}\` whose single extent is on `disk` at byte `start`.
fn volume_at(disk: u32, start: u64) -> Option<String> {
    for v in volumes() {
        let dev_path = v.trim_end_matches('\\').replacen(r"\\?\", r"\\.\", 1);
        let Ok(dev) = Device::open(&dev_path, 0, 0) else {
            continue;
        };
        let mut b = [0u8; 256];
        if dev
            .ioctl_out(IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut b)
            .is_err()
        {
            continue;
        }
        let n = u32::from_le_bytes(b[0..4].try_into().unwrap());
        let d = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let off = u64::from_le_bytes(b[16..24].try_into().unwrap());
        if n == 1 && d == disk && off == start {
            return Some(v);
        }
    }
    None
}

fn assign_letters(d: &Disk, planned: &[NewPart], log: &mut Log) -> Result<(), String> {
    for (label, letter) in [("EFI", 'S'), ("Windows", 'W'), ("Recovery", 'R')] {
        let p = planned.iter().find(|p| p.label == label).unwrap();
        let mut vol = None;
        for _ in 0..30 {
            vol = volume_at(d.number, p.start);
            if vol.is_some() {
                break;
            }
            sleep_ms(1000);
        }
        let vol = vol.ok_or(format!("the {label} volume did not appear"))?;
        let target = format!("{letter}:\\");
        let existing = mount_points(&vol);
        if existing.iter().any(|m| m.eq_ignore_ascii_case(&target)) {
            say!(log, "{label}: already {target}");
            continue;
        }
        for m in existing.iter().filter(|m| m.len() == 3) {
            let _ = delete_mount_point(m);
            say!(log, "{label}: removed automatic letter {m}");
        }
        if letter_in_use(letter) {
            return Err(format!("{letter}: is already used by another volume"));
        }
        set_mount_point(&target, &vol)
            .map_err(|e| format!("{label}: cannot assign {target}: {}", describe(e)))?;
        say!(log, "{label}: {target} ({vol})");
    }
    Ok(())
}

/// Runs the guarded preparation; returns the process exit code (see the crate docs).
pub fn prepare(opts: &Options) -> i32 {
    let backup_dir = opts.backup_dir.as_path();
    let dry_run = opts.dry_run;
    let _ = fs::create_dir_all(backup_dir);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(backup_dir.join("woa-install.log"))
        .ok();
    let mut log = Log(log_file);
    say!(
        log,
        "woa-install {} prepare{}",
        env!("CARGO_PKG_VERSION"),
        if dry_run { " (dry run)" } else { "" }
    );
    let (d, l, extra) = match find_target(&mut log, opts.allow_virtual_test_disk) {
        Ok(t) => t,
        Err(e) => {
            say!(log, "REFUSED: {e}. Nothing was written.");
            return 1;
        }
    };
    say!(
        log,
        "target: {} '{}' {:.1} GB",
        d.path,
        d.model,
        d.length as f64 / 1e9
    );
    print_layout(&mut log, &l);
    let planned = match plan(&l) {
        Ok(p) => p,
        Err(e) => {
            say!(log, "REFUSED: {e}. Nothing was written.");
            return 1;
        }
    };
    let already = extra == planned.len() && matches_plan(&l, &planned);
    if extra != 0 && !already {
        say!(log, "REFUSED: the disk has {extra} partition(s) after the firmware ones that this tool did not plan. Nothing was written.");
        return 1;
    }
    say!(log, "");
    say!(
        log,
        "The 11 firmware partitions are kept exactly as they are."
    );
    say!(
        log,
        "{}",
        if already {
            "These Windows partitions already exist in the free space:"
        } else {
            "These Windows partitions will be created in the free space:"
        }
    );
    for p in &planned {
        say!(
            log,
            "  {:<9} start {:>8} MiB  size {:>8} MiB",
            p.label,
            p.start / MIB,
            p.length / MIB
        );
    }
    if dry_run {
        say!(log, "DRY RUN: nothing was written.");
        return 0;
    }
    if !already {
        let dev = match Device::open(&d.path, GENERIC_READ, FILE_FLAG_NO_BUFFERING) {
            Ok(x) => x,
            Err(e) => {
                say!(
                    log,
                    "REFUSED: cannot open {}: {}. Nothing was written.",
                    d.path,
                    describe(e)
                );
                return 1;
            }
        };
        if let Err(e) = backup(&dev, &d, &l, backup_dir, &mut log) {
            say!(log, "REFUSED: backup failed: {e}. Nothing was written.");
            return 1;
        }
        drop(dev);
    }
    if !confirm(&mut log, opts.answer.as_deref()) {
        say!(log, "Cancelled. Nothing was written.");
        return 1;
    }
    if !already {
        if let Err(e) = partition(&d, &l, &planned, &mut log) {
            say!(log, "FAILED: {e}");
            say!(
                log,
                "The original partition tables are saved in {}.",
                backup_dir.display()
            );
            return 2;
        }
        if let Err(e) = verify(&d, &l, &planned, &mut log) {
            say!(log, "FAILED verification: {e}");
            return 2;
        }
    }
    if let Err(e) = assign_letters(&d, &planned, &mut log) {
        say!(log, "FAILED: {e}");
        return 2;
    }
    say!(log, "READY: S: = EFI, W: = Windows, R: = Recovery");
    0
}

// ========================================================================
// System image backup and restore.
//
// `woa-install backup --out <dir> [--with-windows]` saves, into a new
// timestamped sub-folder of <dir> on a portable drive:
//   * the exact GPT (first and last 64 KiB of the disk),
//   * a raw image of every partition EXCEPT the large Windows one
//     (the 11 firmware partitions, EFI, MSR and Recovery — about 2 GiB),
//   * a manifest listing every partition.
// With --with-windows it also assigns W: and prints it so the caller can
// DISM-capture the Windows partition to windows.wim in the same folder.
//
// `woa-install restore --in <dir>` writes the saved GPT back, restores every
// raw partition image to its exact offset, then assigns S:/W:/R: so the caller
// can DISM-apply windows.wim and fix the boot files. It finds the PZ13 SSD by
// size alone, so it works even after the disk has been wiped empty.
// ========================================================================

/// A disk of exactly the PZ13 size, found by size alone (not by its
/// partitions), so restore works on a wiped or empty disk. Refuses if more
/// than one matches.
fn find_by_size(log: &mut Log, allow_virtual: bool) -> Result<Disk, String> {
    // On the real device only an internal NVMe disk is a candidate. The rehearsal
    // targets a file-backed virtual disk and must never touch a real NVMe disk of
    // the same size (the dev PC's own SSD can be identical), so in that mode only
    // virtual disks are considered.
    let want_bus = if allow_virtual {
        BUS_TYPE_VIRTUAL
    } else {
        BUS_TYPE_NVME
    };
    let mut found = Vec::new();
    for d in disks() {
        if d.bus == want_bus && d.length == PZ13_DISK_BYTES {
            found.push(d);
        } else {
            say!(log, "skipping {} ('{}'): bus {} size {}", d.path, d.model, d.bus, d.length);
        }
    }
    match found.len() {
        1 => Ok(found.pop().unwrap()),
        0 => Err("no disk of the PZ13 SSD size was found".into()),
        n => Err(format!("{n} disks match the PZ13 size; refusing to guess")),
    }
}

/// Indices of all large data (TYPE_BASIC) partitions after the firmware region:
/// the Windows volume plus any extra data partitions. None of these is ever
/// raw-imaged (a 400+ GiB raw copy would take hours and overflow the stick) - the
/// main one is DISM-captured for a full image, the rest are left for a clean
/// reinstall. Everything else (EFI, MSR, Recovery, firmware) is small and raw-imaged.
fn data_partitions(l: &Layout) -> Vec<usize> {
    l.partitions
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            *i >= PZ13_FIRMWARE.len()
                && p.type_guid.eq_ignore_ascii_case(TYPE_BASIC)
                && p.length > 4 * 1024 * MIB
        })
        .map(|(i, _)| i)
        .collect()
}

/// Index of the main Windows data partition (the largest one), imaged with DISM.
fn windows_partition(l: &Layout) -> Option<usize> {
    data_partitions(l).into_iter().max_by_key(|&i| l.partitions[i].length)
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if s.is_empty() {
        "part".into()
    } else {
        s
    }
}

/// Prints a single-line, overwriting progress indicator to stdout (not the log file).
fn progress_line(label: &str, done: u64, total: u64) {
    use std::io::Write;
    let pct = if total == 0 { 100 } else { done * 100 / total };
    print!(
        "\r    {:<18} {:>3}%  ({:>5} / {:>5} MiB)   ",
        label,
        pct,
        done / MIB,
        total / MIB
    );
    let _ = std::io::stdout().flush();
}

/// Ends the progress line with a newline so the next message starts cleanly.
fn progress_end() {
    println!();
}

/// Copies `length` bytes of the disk from `start` into `path`, in 1 MiB blocks,
/// printing a live percentage as it goes.
fn image_out(dev: &Device, start: u64, length: u64, path: &Path, label: &str) -> Result<(), String> {
    let mut f = File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    let mut b = block();
    let mut off = 0u64;
    let mut last_pct = u64::MAX;
    while off < length {
        let n = MIB.min(length - off) as usize;
        dev.read_at(start + off, &mut b.0[..n])
            .map_err(|e| format!("read disk at {}: {}", start + off, describe(e)))?;
        f.write_all(&b.0[..n])
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        off += n as u64;
        let pct = off * 100 / length.max(1);
        if pct != last_pct {
            progress_line(label, off, length);
            last_pct = pct;
        }
    }
    f.flush().map_err(|e| e.to_string())?;
    progress_end();
    Ok(())
}

/// Writes image file `path` back to the disk at `start`, in 1 MiB blocks.
fn image_in(dev: &Device, start: u64, max_len: u64, path: &Path, label: &str) -> Result<u64, String> {
    use std::io::Read;
    let mut f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let len = f.metadata().map_err(|e| e.to_string())?.len();
    if len % 512 != 0 {
        return Err(format!("{} is {len} bytes, not a sector multiple", path.display()));
    }
    if len > max_len {
        return Err(format!(
            "{} is {len} bytes, larger than its {max_len}-byte partition",
            path.display()
        ));
    }
    let mut b = block();
    let mut off = 0u64;
    let mut last_pct = u64::MAX;
    while off < len {
        let n = MIB.min(len - off) as usize;
        let mut got = 0usize;
        while got < n {
            let r = f.read(&mut b.0[got..n]).map_err(|e| e.to_string())?;
            if r == 0 {
                return Err(format!("{} ended early", path.display()));
            }
            got += r;
        }
        dev.write_at(start + off, &b.0[..n])
            .map_err(|e| format!("write disk at {}: {}", start + off, describe(e)))?;
        off += n as u64;
        let pct = off * 100 / len.max(1);
        if pct != last_pct {
            progress_line(label, off, len);
            last_pct = pct;
        }
    }
    progress_end();
    Ok(len)
}

/// One partition as recorded in the manifest.
#[derive(Debug, Clone)]
struct PartRec {
    start: u64,
    length: u64,
    type_guid: String,
    id_guid: String,
    attributes: u64,
    name: String,
    /// Image file name, or "wim" (DISM), or "none" (not captured).
    file: String,
}

struct Manifest {
    disk_bytes: u64,
    disk_id: String,
    parts: Vec<PartRec>,
}

impl Manifest {
    fn write(&self, path: &Path) -> Result<(), String> {
        let mut s = String::from("# WoA PZ13 backup manifest v1\n");
        s += &format!("disk_bytes {}\n", self.disk_bytes);
        s += &format!("disk_id {}\n", self.disk_id);
        for p in &self.parts {
            // Fields are space-separated; name is quoted because it may be empty
            // or contain spaces, and comes last on the line.
            s += &format!(
                "part {} {} {} {} {} {} \"{}\"\n",
                p.start, p.length, p.type_guid, p.id_guid, p.attributes, p.file, p.name
            );
        }
        fs::write(path, s).map_err(|e| format!("write {}: {e}", path.display()))
    }

    fn read(path: &Path) -> Result<Manifest, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut disk_bytes = 0u64;
        let mut disk_id = String::new();
        let mut parts = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("disk_bytes ") {
                disk_bytes = v.trim().parse().map_err(|_| "bad disk_bytes")?;
            } else if let Some(v) = line.strip_prefix("disk_id ") {
                disk_id = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("part ") {
                // start length type id attr file "name"
                let (head, name) = match v.split_once('"') {
                    Some((h, rest)) => (h, rest.trim_end_matches('"')),
                    None => (v, ""),
                };
                let f: Vec<&str> = head.split_whitespace().collect();
                if f.len() < 6 {
                    return Err(format!("bad part line: {line}"));
                }
                parts.push(PartRec {
                    start: f[0].parse().map_err(|_| "bad start")?,
                    length: f[1].parse().map_err(|_| "bad length")?,
                    type_guid: f[2].to_string(),
                    id_guid: f[3].to_string(),
                    attributes: parse_u64(f[4])?,
                    file: f[5].to_string(),
                    name: name.to_string(),
                });
            }
        }
        if disk_bytes == 0 || parts.is_empty() {
            return Err("manifest is missing disk_bytes or partitions".into());
        }
        Ok(Manifest {
            disk_bytes,
            disk_id,
            parts,
        })
    }
}

fn parse_u64(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16).map_err(|_| format!("bad number {s}"))
    } else {
        s.parse().map_err(|_| format!("bad number {s}"))
    }
}

/// Gives the volume whose single extent starts at `start` on `disk` the drive
/// letter `letter`, removing any automatic letter first.
fn assign_one(d: &Disk, start: u64, letter: char, log: &mut Log) -> Result<String, String> {
    let mut vol = None;
    for _ in 0..30 {
        vol = volume_at(d.number, start);
        if vol.is_some() {
            break;
        }
        sleep_ms(1000);
    }
    let vol = vol.ok_or(format!("the volume at {} MiB did not appear", start / MIB))?;
    let target = format!("{letter}:\\");
    let existing = mount_points(&vol);
    if existing.iter().any(|m| m.eq_ignore_ascii_case(&target)) {
        say!(log, "{letter}: already assigned");
        return Ok(target);
    }
    for m in existing.iter().filter(|m| m.len() == 3) {
        let _ = delete_mount_point(m);
    }
    if letter_in_use(letter) {
        return Err(format!("{letter}: is already used by another volume"));
    }
    set_mount_point(&target, &vol).map_err(|e| format!("cannot assign {target}: {}", describe(e)))?;
    say!(log, "{letter}: {target} ({vol})");
    Ok(target)
}

/// `woa-install backup --out <dir> [--with-windows]`.
pub fn backup_system(out: &Path, with_windows: bool, allow_virtual: bool) -> i32 {
    let _ = fs::create_dir_all(out);
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("woa-image.log"))
        .ok();
    let mut log = Log(log_file);
    say!(log, "woa-install {} backup", env!("CARGO_PKG_VERSION"));
    let (d, l, _extra) = match find_target(&mut log, allow_virtual) {
        Ok(t) => t,
        Err(e) => {
            say!(log, "REFUSED: {e}. Nothing was read.");
            return 1;
        }
    };
    say!(log, "source: {} '{}'", d.path, d.model);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_secs())
        .unwrap_or(0);
    let dir = out.join(format!("pz13-image-{stamp}"));
    if let Err(e) = fs::create_dir_all(&dir) {
        say!(log, "FAILED: cannot create {}: {e}", dir.display());
        return 1;
    }
    let dev = match Device::open(&d.path, GENERIC_READ, FILE_FLAG_NO_BUFFERING) {
        Ok(x) => x,
        Err(e) => {
            say!(log, "FAILED: open {}: {}", d.path, describe(e));
            return 1;
        }
    };
    // Exact GPT: first and last 64 KiB.
    let mut b = block();
    let tail = d.length - HEAD as u64;
    for (name, at) in [("head-64k.bin", 0u64), ("tail-64k.bin", tail)] {
        if let Err(e) = dev.read_at(at, &mut b.0[..HEAD]) {
            say!(log, "FAILED: read GPT at {at}: {}", describe(e));
            return 1;
        }
        if let Err(e) = fs::write(dir.join(name), &b.0[..HEAD]) {
            say!(log, "FAILED: write {name}: {e}");
            return 1;
        }
    }
    let data = data_partitions(&l);
    let win_idx = windows_partition(&l);
    let mut recs = Vec::new();
    for (i, p) in l.partitions.iter().enumerate() {
        let dname = if p.name.is_empty() { "(data)" } else { &p.name };
        let file = if data.contains(&i) {
            // A large data partition: DISM-captured (main, full image) or skipped.
            if with_windows && Some(i) == win_idx {
                say!(log, "Windows volume {:>6} MiB -> windows.wim (captured compressed)", p.length / MIB);
            } else if with_windows {
                say!(log, "NOTE: extra data partition '{}' ({} MiB) is NOT captured - a full", dname, p.length / MIB);
                say!(log, "      image saves only the main Windows volume; it will be empty after a restore.");
            } else {
                say!(log, "skipping data partition '{}' ({} MiB) - boot+firmware-only backup", dname, p.length / MIB);
            }
            if with_windows && Some(i) == win_idx { "wim" } else { "none" }.to_string()
        } else {
            let fname = format!("part-{:02}-{}.img", i, sanitize(&p.name));
            say!(
                log,
                "imaging {:<16} {:>6} MiB -> {}",
                if p.name.is_empty() { "(Recovery)" } else { &p.name },
                p.length / MIB,
                fname
            );
            let label = if p.name.is_empty() { "Recovery" } else { &p.name };
            if let Err(e) = image_out(&dev, p.start, p.length, &dir.join(&fname), label) {
                say!(log, "FAILED: {e}");
                return 2;
            }
            fname
        };
        recs.push(PartRec {
            start: p.start,
            length: p.length,
            type_guid: p.type_guid.clone(),
            id_guid: p.id_guid.clone(),
            attributes: p.attributes,
            name: p.name.clone(),
            file,
        });
    }
    let manifest = Manifest {
        disk_bytes: d.length,
        disk_id: l.disk_id.clone(),
        parts: recs,
    };
    if let Err(e) = manifest.write(&dir.join("manifest.txt")) {
        say!(log, "FAILED: {e}");
        return 2;
    }
    say!(log, "manifest written; raw images complete in {}", dir.display());
    if with_windows {
        drop(dev);
        match assign_one(&d, l.partitions[win_idx.unwrap()].start, 'W', &mut log) {
            Ok(_) => {
                say!(log, "WINDOWS_VOLUME W:");
                say!(log, "BACKUP_DIR {}", dir.display());
                say!(log, "READY: now capture W: to {}\\windows.wim", dir.display());
            }
            Err(e) => {
                say!(log, "WARNING: could not assign W: for the Windows capture: {e}");
                say!(log, "BACKUP_DIR {}", dir.display());
                return 2;
            }
        }
    } else {
        say!(log, "BACKUP_DIR {}", dir.display());
        say!(log, "DONE: boot + firmware backup complete (Windows not captured).");
    }
    0
}

/// `woa-install restore --in <dir>`.
pub fn restore_system(indir: &Path, answer: Option<&str>, allow_virtual: bool) -> i32 {
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(indir.join("woa-restore.log"))
        .ok();
    let mut log = Log(log_file);
    say!(log, "woa-install {} restore", env!("CARGO_PKG_VERSION"));
    let manifest = match Manifest::read(&indir.join("manifest.txt")) {
        Ok(m) => m,
        Err(e) => {
            say!(log, "REFUSED: {e}");
            return 1;
        }
    };
    let head = indir.join("head-64k.bin");
    let tail = indir.join("tail-64k.bin");
    if !head.exists() || !tail.exists() {
        say!(log, "REFUSED: head-64k.bin or tail-64k.bin is missing from {}", indir.display());
        return 1;
    }
    let d = match find_by_size(&mut log, allow_virtual) {
        Ok(d) => d,
        Err(e) => {
            say!(log, "REFUSED: {e}. Nothing was written.");
            return 1;
        }
    };
    if d.length != manifest.disk_bytes {
        say!(
            log,
            "REFUSED: this disk is {} bytes but the backup is of a {}-byte disk.",
            d.length,
            manifest.disk_bytes
        );
        return 1;
    }
    say!(log, "target: {} '{}'", d.path, d.model);
    say!(log, "");
    say!(log, "This OVERWRITES the internal SSD's partition table and the");
    say!(log, "firmware, EFI, MSR and Recovery partitions from the backup in");
    say!(log, "{}.", indir.display());
    say!(log, "Type RESTORE and press Enter to continue (anything else cancels):");
    let mut line = String::new();
    match answer {
        Some(a) => line = a.to_string(),
        None => {
            let _ = std::io::stdin().lock().read_line(&mut line);
        }
    }
    if line.trim() != "RESTORE" {
        say!(log, "Cancelled ('{}'). Nothing was written.", line.trim());
        return 1;
    }
    ensure_not_hidden(&d, &mut log);
    let dev = match Device::open(
        &d.path,
        GENERIC_READ | GENERIC_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
    ) {
        Ok(x) => x,
        Err(e) => {
            say!(log, "FAILED: cannot open {} for writing: {}.", d.path, describe(e));
            say!(log, "If the disk shows as empty/hidden, run the gptfix stick first, then restore again.");
            return 2;
        }
    };
    // Restore the exact GPT: write the saved first and last 64 KiB.
    let mut b = block();
    for (path, at) in [(&head, 0u64), (&tail, d.length - HEAD as u64)] {
        let bytes = match fs::read(path) {
            Ok(v) => v,
            Err(e) => {
                say!(log, "FAILED: read {}: {e}", path.display());
                return 2;
            }
        };
        if bytes.len() != HEAD {
            say!(log, "FAILED: {} is {} bytes, expected {HEAD}", path.display(), bytes.len());
            return 2;
        }
        b.0[..HEAD].copy_from_slice(&bytes);
        if let Err(e) = dev.write_at(at, &b.0[..HEAD]) {
            say!(log, "FAILED: write GPT at {at}: {}", describe(e));
            return 2;
        }
    }
    let _ = dev.ioctl_in(IOCTL_DISK_UPDATE_PROPERTIES, &[]);
    say!(log, "partition table restored");
    // Restore every raw partition image to its saved offset.
    for p in &manifest.parts {
        if p.file == "wim" || p.file == "none" {
            continue;
        }
        let path = indir.join(&p.file);
        if !path.exists() {
            say!(log, "WARNING: image {} is missing; skipping this partition", p.file);
            continue;
        }
        say!(
            log,
            "restoring {:<16} {:>6} MiB <- {}",
            if p.name.is_empty() { "(Recovery)" } else { &p.name },
            p.length / MIB,
            p.file
        );
        let label = if p.name.is_empty() { "Recovery" } else { &p.name };
        if let Err(e) = image_in(&dev, p.start, p.length, &path, label) {
            say!(log, "FAILED: {e}");
            return 2;
        }
    }
    drop(dev);
    let _ = Device::open(&d.path, GENERIC_READ, 0).map(|x| x.ioctl_in(IOCTL_DISK_UPDATE_PROPERTIES, &[]));
    // Assign letters so the caller can DISM-apply windows.wim and fix boot.
    // Only the DISM-captured partition (file == "wim") gets W: for the Windows apply.
    // A boot+firmware-only backup has no "wim", so W: is simply not assigned.
    let mut win_start = None;
    for p in &manifest.parts {
        if p.file == "wim" {
            win_start = Some(p.start);
        }
    }
    let efi = manifest
        .parts
        .iter()
        .find(|p| p.type_guid.eq_ignore_ascii_case(TYPE_EFI))
        .map(|p| p.start);
    let rec = manifest
        .parts
        .iter()
        .find(|p| p.type_guid.eq_ignore_ascii_case(TYPE_RECOVERY))
        .map(|p| p.start);
    if let Some(s) = efi {
        let _ = assign_one(&d, s, 'S', &mut log);
    }
    if let Some(s) = win_start {
        match assign_one(&d, s, 'W', &mut log) {
            Ok(_) => say!(log, "WINDOWS_VOLUME W:"),
            Err(e) => say!(log, "WARNING: could not assign W:: {e}"),
        }
    }
    if let Some(s) = rec {
        let _ = assign_one(&d, s, 'R', &mut log);
    }
    say!(log, "RESTORE_DIR {}", indir.display());
    if win_start.is_some() {
        say!(log, "DONE: partitions restored. Now apply windows.wim to W: (if present) and fix boot.");
    } else {
        say!(log, "DONE: partitions restored.");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips() {
        let m = Manifest {
            disk_bytes: PZ13_DISK_BYTES,
            disk_id: "41A44457-AD39-4120-A46D-5894F61EB83D".into(),
            parts: vec![
                PartRec {
                    start: MIB,
                    length: MIB,
                    type_guid: TYPE_BASIC.into(),
                    id_guid: "11111111-1111-1111-1111-111111111111".into(),
                    attributes: 0x8000_0000_0000_0001,
                    name: "ToolsFv".into(),
                    file: "part-00-ToolsFv.img".into(),
                },
                PartRec {
                    start: 1121 * MIB,
                    length: 900 * 1024 * MIB,
                    type_guid: TYPE_BASIC.into(),
                    id_guid: "22222222-2222-2222-2222-222222222222".into(),
                    attributes: 0,
                    name: "".into(),
                    file: "wim".into(),
                },
            ],
        };
        let dir = std::env::temp_dir().join(format!("woa-mani-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("manifest.txt");
        m.write(&p).unwrap();
        let r = Manifest::read(&p).unwrap();
        assert_eq!(r.disk_bytes, m.disk_bytes);
        assert_eq!(r.disk_id, m.disk_id);
        assert_eq!(r.parts.len(), 2);
        assert_eq!(r.parts[0].name, "ToolsFv");
        assert_eq!(r.parts[0].attributes, 0x8000_0000_0000_0001);
        assert_eq!(r.parts[0].file, "part-00-ToolsFv.img");
        assert_eq!(r.parts[1].name, "");
        assert_eq!(r.parts[1].file, "wim");
        assert_eq!(r.parts[1].start, 1121 * MIB);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_partition_is_the_big_basic_one() {
        // Build an 11-firmware + Windows + Recovery layout inline.
        let mut b = vec![0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&1u32.to_le_bytes());
        b[24..32].copy_from_slice(&(34u64 * 512).to_le_bytes());
        b[32..40].copy_from_slice(&((2_000_409_230u64 - 33) * 512).to_le_bytes());
        let mut count = 0u32;
        for (name, start, size) in PZ13_FIRMWARE {
            let n = NewPart {
                label: "fw",
                type_guid: TYPE_BASIC,
                start: start * MIB,
                length: size * MIB,
                attributes: 1,
                name,
            };
            b.extend_from_slice(&new_entry(&n, [1; 16]));
            count += 1;
        }
        for (name, start, size, ty) in [
            ("Basic data partition", 1121u64, 900 * 1024u64, TYPE_BASIC),
            ("", 975738, 1024, TYPE_RECOVERY),
        ] {
            let n = NewPart {
                label: "x",
                type_guid: ty,
                start: start * MIB,
                length: size * MIB,
                attributes: 0,
                name,
            };
            b.extend_from_slice(&new_entry(&n, [2; 16]));
            count += 1;
        }
        b[4..8].copy_from_slice(&count.to_le_bytes());
        let parsed = parse_layout(&b).unwrap();
        assert_eq!(windows_partition(&parsed), Some(11), "the 900 GiB basic one");
    }

    #[test]
    fn data_partitions_catches_every_large_basic_one() {
        // PZ13 with TWO large data partitions (as seen on a real disk) + EFI/MSR/Recovery.
        let mut b = vec![0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&1u32.to_le_bytes());
        b[24..32].copy_from_slice(&(34u64 * 512).to_le_bytes());
        b[32..40].copy_from_slice(&((2_000_409_230u64 - 33) * 512).to_le_bytes());
        let mut count = 0u32;
        for (name, start, size) in PZ13_FIRMWARE {
            b.extend_from_slice(&new_entry(
                &NewPart { label: "fw", type_guid: TYPE_BASIC, start: start * MIB, length: size * MIB, attributes: 1, name },
                [1; 16],
            ));
            count += 1;
        }
        // 11 = EFI(260M), 12 = MSR(16M), 13 = Windows(550G), 14 = extra data(400G), 15 = Recovery(1G)
        for (name, start, size, ty) in [
            ("EFI system partition", 845u64, 260u64, TYPE_EFI),
            ("Microsoft reserved partition", 1105, 16, TYPE_MSR),
            ("Basic data partition", 1121, 550 * 1024, TYPE_BASIC),
            ("Basic data partition", 563313, 400 * 1024, TYPE_BASIC),
            ("", 972913, 1024, TYPE_RECOVERY),
        ] {
            b.extend_from_slice(&new_entry(
                &NewPart { label: "x", type_guid: ty, start: start * MIB, length: size * MIB, attributes: 0, name },
                [2; 16],
            ));
            count += 1;
        }
        b[4..8].copy_from_slice(&count.to_le_bytes());
        let parsed = parse_layout(&b).unwrap();
        // Both large data partitions must be recognised (so boot+firmware-only skips BOTH).
        assert_eq!(data_partitions(&parsed), vec![13, 14], "both large basic partitions");
        // EFI(260M), MSR(16M) and Recovery(1G) are small/other-type and stay raw-imaged.
        assert_eq!(windows_partition(&parsed), Some(13), "the 550 GiB one is primary Windows");
    }

    #[test]
    fn clears_only_inside_the_planned_partitions() {
        let l = {
            // Reuse the layout test fixture through the public API.
            let mut b = vec![0u8; HEADER_LEN];
            b[0..4].copy_from_slice(&1u32.to_le_bytes());
            b[24..32].copy_from_slice(&(34u64 * 512).to_le_bytes());
            b[32..40].copy_from_slice(&((2_000_409_230u64 - 33) * 512).to_le_bytes());
            let mut count = 0u32;
            for (name, start, size) in PZ13_FIRMWARE {
                let n = NewPart {
                    label: "fw",
                    type_guid: TYPE_BASIC,
                    start: start * MIB,
                    length: size * MIB,
                    attributes: 1,
                    name,
                };
                b.extend_from_slice(&new_entry(&n, [1; 16]));
                count += 1;
            }
            b[4..8].copy_from_slice(&count.to_le_bytes());
            parse_layout(&b).unwrap()
        };
        let p = plan(&l).unwrap();
        let areas = areas_to_clear(&p);
        assert_eq!(areas.len(), 6);
        let fw_end = 845 * MIB;
        for a in areas {
            assert!(a >= fw_end && a + MIB <= l.usable_end, "{a}");
            assert!(
                p.iter()
                    .any(|n| a >= n.start && a + MIB <= n.start + n.length),
                "{a}"
            );
        }
    }
}
