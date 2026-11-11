//! Rehearses `woa-install backup` then `restore` on a throw-away VHDX shaped
//! like the PZ13 SSD, proving the raw partition imaging and GPT restore survive
//! a total wipe.
//!
//! Run elevated on the dev PC (never in WinPE):
//!   cargo run --release -p woa-install --example backup_restore_rehearsal -- <scratch-dir>
//!
//! 1. Creates a dynamic VHDX of exactly the PZ13 SSD size and attaches it.
//! 2. Gives it the 11 firmware partitions, then runs `prepare` to add EFI/MSR/
//!    Windows/Recovery (a full 15-partition disk).
//! 3. Writes distinctive markers inside NHLOS, EFI and Recovery.
//! 4. Runs `backup_system` (raw images + GPT + manifest) to the scratch dir.
//! 5. WIPES the disk: zeroes both GPT copies and all three markers.
//! 6. Runs `restore_system` (answer RESTORE).
//! 7. Checks: 15 partitions parse again, firmware entries match, and all three
//!    markers are byte-identical to what was backed up.

use std::path::PathBuf;

use woa_install::ffi::*;
use woa_install::layout::*;

const GET_LAYOUT: u32 = 0x0007_0050;
const SET_LAYOUT: u32 = 0x0007_C054;
const UPDATE_PROPERTIES: u32 = 0x0007_0140;

#[repr(C, align(4096))]
struct Sector([u8; 4096]);

fn layout_of(path: &str) -> Layout {
    let dev = Device::open(path, GENERIC_READ, 0).expect("open disk");
    let mut buf = vec![0u8; HEADER_LEN + 256 * ENTRY_LEN];
    dev.ioctl_out(GET_LAYOUT, &[], &mut buf).expect("get layout");
    parse_layout(&buf).expect("parse layout")
}

fn write_marker(dev: &Device, off: u64, tag: &[u8]) {
    let mut s = Box::new(Sector([0; 4096]));
    s.0[..tag.len()].copy_from_slice(tag);
    dev.write_at(off, &s.0).expect("write marker");
}

fn read_sector(dev: &Device, off: u64) -> Vec<u8> {
    let mut s = Box::new(Sector([0; 4096]));
    dev.read_at(off, &mut s.0).expect("read");
    s.0.to_vec()
}

fn fail(msg: &str) -> ! {
    eprintln!("REHEARSAL FAILED: {msg}");
    std::process::exit(1);
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("scratch dir"));
    std::fs::create_dir_all(&dir).unwrap();
    for l in ['S', 'W', 'R'] {
        if letter_in_use(l) {
            fail(&format!("{l}: is in use on this PC; free it first"));
        }
    }
    let log = winstorage::Logger::open(&dir, "backup-rehearsal").unwrap();
    let vhdx = dir.join("pz13-backup-rehearsal.vhdx");
    let _ = std::fs::remove_file(&vhdx);
    let vhd = winstorage::create_vhdx(&vhdx, PZ13_DISK_BYTES, winstorage::Template::Empty, &log)
        .unwrap_or_else(|e| fail(&format!("create VHDX: {e}")));
    let path = format!(r"\\.\PhysicalDrive{}", vhd.disk_number);
    println!("rehearsal disk: {path}");

    // Firmware partitions.
    let before = layout_of(&path);
    let mut buf = before.header.clone();
    buf[4..8].copy_from_slice(&(PZ13_FIRMWARE.len() as u32).to_le_bytes());
    for (i, (name, start, size)) in PZ13_FIRMWARE.iter().enumerate() {
        let n = NewPart {
            label: "fw",
            type_guid: TYPE_BASIC,
            start: start * MIB,
            length: size * MIB,
            attributes: 0x8000_0000_0000_0001,
            name,
        };
        let mut r = random16();
        r[0] = i as u8;
        buf.extend_from_slice(&new_entry(&n, v4_guid(r)));
    }
    {
        let dev = Device::open(&path, GENERIC_READ | GENERIC_WRITE, 0).unwrap();
        dev.ioctl_in(SET_LAYOUT, &buf)
            .unwrap_or_else(|e| fail(&format!("set firmware layout: {}", describe(e))));
        let _ = dev.ioctl_in(UPDATE_PROPERTIES, &[]);
    }

    // Add EFI/MSR/Windows/Recovery with the real prepare.
    let opts = woa_install::Options {
        backup_dir: dir.join("prep-backup"),
        dry_run: false,
        allow_virtual_test_disk: true,
        answer: Some("INSTALL".into()),
    };
    if woa_install::prepare(&opts) != 0 {
        fail("prepare failed to create the Windows partitions");
    }
    let full = layout_of(&path);
    if full.partitions.len() != 15 {
        fail(&format!("expected 15 partitions, got {}", full.partitions.len()));
    }
    let efi_start = full
        .partitions
        .iter()
        .find(|p| p.type_guid.eq_ignore_ascii_case(TYPE_EFI))
        .map(|p| p.start)
        .unwrap();
    let rec_start = full
        .partitions
        .iter()
        .find(|p| p.type_guid.eq_ignore_ascii_case(TYPE_RECOVERY))
        .map(|p| p.start)
        .unwrap();
    let nhlos_off = 90 * MIB;

    // Drop the letters prepare assigned (we image raw, by PhysicalDrive).
    for l in ['S', 'W', 'R'] {
        let _ = delete_mount_point(&format!("{l}:\\"));
    }

    // Markers.
    {
        let dev = Device::open(&path, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_NO_BUFFERING).unwrap();
        write_marker(&dev, nhlos_off, b"FIRMWARE-NHLOS-marker");
        write_marker(&dev, efi_start, b"EFI-boot-marker-12345");
        write_marker(&dev, rec_start, b"RECOVERY-marker-67890");
    }

    // Backup.
    let out = dir.join("portable");
    let _ = std::fs::create_dir_all(&out);
    if woa_install::backup_system(&out, false, true) != 0 {
        fail("backup_system returned non-zero");
    }
    // Locate the backup sub-folder it created.
    let bdir = std::fs::read_dir(&out)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("pz13-image-"))
        })
        .unwrap_or_else(|| fail("backup sub-folder not found"));
    println!("backup folder: {}", bdir.display());

    // WIPE: zero both GPT copies and all three markers.
    {
        let dev = Device::open(&path, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_NO_BUFFERING).unwrap();
        let zero = Box::new(Sector([0; 4096]));
        // 64 KiB at head and at tail = both GPT copies.
        for base in [0u64, PZ13_DISK_BYTES - 64 * 1024] {
            let mut o = 0u64;
            while o < 64 * 1024 {
                dev.write_at(base + o, &zero.0).unwrap();
                o += 4096;
            }
        }
        write_marker(&dev, nhlos_off, &[0u8; 0]);
        write_marker(&dev, efi_start, &[0u8; 0]);
        write_marker(&dev, rec_start, &[0u8; 0]);
        let _ = dev.ioctl_in(UPDATE_PROPERTIES, &[]);
    }
    // Confirm the wipe really destroyed the markers.
    {
        let dev = Device::open(&path, GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        if read_sector(&dev, nhlos_off).iter().any(|&b| b != 0) {
            fail("wipe did not clear the NHLOS marker");
        }
    }
    println!("disk wiped (GPT + markers zeroed)");

    // Restore.
    if woa_install::restore_system(&bdir, Some("RESTORE"), true) != 0 {
        fail("restore_system returned non-zero");
    }

    // Checks.
    let after = layout_of(&path);
    if after.partitions.len() != 15 {
        fail(&format!(
            "after restore: {} partitions, expected 15",
            after.partitions.len()
        ));
    }
    for (a, b) in after.partitions.iter().take(11).zip(full.partitions.iter().take(11)) {
        if (a.start, a.length, &a.name) != (b.start, b.length, &b.name) {
            fail(&format!("firmware entry '{}' differs after restore", b.name));
        }
    }
    {
        let dev = Device::open(&path, GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        let checks = [
            (nhlos_off, &b"FIRMWARE-NHLOS-marker"[..]),
            (efi_start, &b"EFI-boot-marker-12345"[..]),
            (rec_start, &b"RECOVERY-marker-67890"[..]),
        ];
        for (off, tag) in checks {
            let got = read_sector(&dev, off);
            if &got[..tag.len()] != tag {
                fail(&format!(
                    "marker at {} MiB not restored: {:?}",
                    off / MIB,
                    &got[..tag.len()]
                ));
            }
            println!("restored marker at {} MiB OK", off / MIB);
        }
    }
    for l in ['S', 'W', 'R'] {
        let _ = delete_mount_point(&format!("{l}:\\"));
    }
    println!("REHEARSAL PASSED: GPT and all raw partitions restored after a full wipe");
    drop(vhd);
    let _ = std::fs::remove_file(&vhdx);
}
