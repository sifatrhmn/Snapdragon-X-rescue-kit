//! Rehearses the UEFI gptfix flow on a throw-away VHDX shaped like the PZ13 SSD.
//!
//! Run elevated on the dev PC:
//!   cargo run --release -p woa-install --example gptfix_rehearsal -- <scratch-dir> [format-script]
//!
//! 1. VHDX of the exact PZ13 size, given the 11 firmware partitions; markers written
//!    inside NHLOS ("FIRMWARE") and a stale NTFS boot sector at 1121 MiB.
//! 2. Reads LBA 0..=33 and the last 33 sectors raw, runs `gptfix::check` + `build`, and
//!    writes exactly those bytes raw (backup first, primary header last) after clearing
//!    the stale areas — the same sequence the UEFI app performs.
//! 3. Windows re-reads the disk: 15 partitions, firmware entries unchanged, the four new
//!    ones matching woa-install's own plan; NHLOS marker intact.
//! 4. `woa_install::prepare` must take its "already exist" path and assign S:/W:/R:;
//!    W: must start with zeros; optionally runs the stick's format script.

use std::path::PathBuf;

use woa_install::ffi::*;
use woa_install::layout::*;

const GET_LAYOUT: u32 = 0x0007_0050;
const SET_LAYOUT: u32 = 0x0007_C054;
const UPDATE_PROPERTIES: u32 = 0x0007_0140;

#[repr(C, align(4096))]
struct Big([u8; 1024 * 1024]);

fn big() -> Box<Big> {
    let l = std::alloc::Layout::new::<Big>();
    // SAFETY: non-zero layout; zeroed bytes are a valid Big.
    unsafe { Box::from_raw(std::alloc::alloc_zeroed(l) as *mut Big) }
}

fn fail(msg: &str) -> ! {
    eprintln!("GPTFIX REHEARSAL FAILED: {msg}");
    std::process::exit(1);
}

fn layout_of(path: &str) -> Layout {
    let dev = Device::open(path, GENERIC_READ, 0).expect("open disk");
    let mut buf = vec![0u8; HEADER_LEN + 256 * ENTRY_LEN];
    dev.ioctl_out(GET_LAYOUT, &[], &mut buf)
        .expect("get layout");
    parse_layout(&buf).expect("parse layout")
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("scratch dir"));
    std::fs::create_dir_all(&dir).unwrap();
    for l in ['S', 'W', 'R'] {
        if letter_in_use(l) {
            fail(&format!("{l}: is in use on this PC"));
        }
    }
    let log = winstorage::Logger::open(&dir, "gptfix-rehearsal").unwrap();
    let vhdx = dir.join("pz13-gptfix.vhdx");
    let _ = std::fs::remove_file(&vhdx);
    let vhd = winstorage::create_vhdx(&vhdx, PZ13_DISK_BYTES, winstorage::Template::Empty, &log)
        .unwrap_or_else(|e| fail(&format!("create VHDX: {e}")));
    let path = format!(r"\\.\PhysicalDrive{}", vhd.disk_number);
    println!("rehearsal disk: {path}");

    // 1. Markers, then the 11 firmware partitions.
    {
        let dev =
            Device::open(&path, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_NO_BUFFERING).unwrap();
        let mut b = big();
        b.0[..8].copy_from_slice(b"FIRMWARE");
        dev.write_at(90 * MIB, &b.0[..4096]).unwrap();
        let mut b = big();
        b.0[3..11].copy_from_slice(b"NTFS    ");
        b.0[510] = 0x55;
        b.0[511] = 0xAA;
        dev.write_at(1121 * MIB, &b.0[..4096]).unwrap();
    }
    let empty = layout_of(&path);
    let mut buf = empty.header.clone();
    buf[4..8].copy_from_slice(&(PZ13_FIRMWARE.len() as u32).to_le_bytes());
    for (i, (name, start, size)) in PZ13_FIRMWARE.iter().enumerate() {
        let n = NewPart {
            label: "fw",
            type_guid: "97745ABA-135A-44C3-9ADC-05616173C24C",
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
            .unwrap_or_else(|e| fail(&describe(e)));
        let _ = dev.ioctl_in(UPDATE_PROPERTIES, &[]);
    }
    let fw = layout_of(&path);
    if check_firmware(&fw) != Ok(0) {
        fail("firmware layout not accepted");
    }
    println!("rehearsal disk has the 11 PZ13 firmware partitions");

    // 2. Exactly what the UEFI app does.
    let last_lba = PZ13_DISK_BYTES / 512 - 1;
    let dev = Device::open(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
    )
    .unwrap();
    let mut head = big();
    dev.read_at(0, &mut head.0[..64 * 1024]).unwrap();
    let mut tail = big();
    let tail_off = PZ13_DISK_BYTES - 64 * 1024;
    dev.read_at(tail_off, &mut tail.0[..64 * 1024]).unwrap();
    let gpt = gptfix::check(&head.0[..64 * 1024], &tail.0[..64 * 1024], last_lba)
        .unwrap_or_else(|e| fail(&format!("gptfix::check: {e}")));
    let gptfix::State::FirmwareOnly { plan: fix_plan } = gpt.state.clone() else {
        fail("expected firmware-only")
    };
    let ids = [
        v4_guid(random16()),
        v4_guid(random16()),
        v4_guid(random16()),
        v4_guid(random16()),
    ];
    let w = gptfix::build(&gpt, ids).unwrap_or_else(|e| fail(e));
    let zero = big();
    for lba in gptfix::areas_to_clear(&fix_plan) {
        dev.write_at(lba * 512, &zero.0)
            .unwrap_or_else(|e| fail(&describe(e)));
    }
    let write = |lba: u64, data: &[u8]| {
        dev.write_at(lba * 512, data)
            .unwrap_or_else(|e| fail(&format!("write LBA {lba}: {}", describe(e))))
    };
    write(w.entries_lba_backup, &w.entries);
    write(w.header_lba_backup, &w.backup_header);
    write(w.entries_lba_primary, &w.entries);
    write(w.header_lba_primary, &w.primary_header);
    let _ = dev.ioctl_in(UPDATE_PROPERTIES, &[]);
    drop(dev);
    println!("gptfix bytes written raw (backup first, primary header last)");

    // 3. Windows' view.
    let after = layout_of(&path);
    if after.partitions.len() != 15 {
        fail(&format!(
            "{} partitions, expected 15",
            after.partitions.len()
        ));
    }
    for (a, b) in after.partitions.iter().zip(&fw.partitions) {
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
            fail(&format!("firmware entry '{}' changed", b.name));
        }
    }
    let woa_plan = plan(&fw).unwrap();
    if !matches_plan(&after, &woa_plan) {
        fail("gptfix partitions do not match woa-install's plan");
    }
    {
        let dev = Device::open(&path, GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        let mut b = big();
        dev.read_at(90 * MIB, &mut b.0[..4096]).unwrap();
        if &b.0[..8] != b"FIRMWARE" {
            fail("NHLOS marker changed");
        }
    }
    println!("Windows sees 15 partitions; firmware unchanged; matches woa-install plan");
    // Re-check with gptfix itself: both copies valid, state AlreadyFixed.
    {
        let dev = Device::open(&path, GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        let (mut h, mut t) = (big(), big());
        dev.read_at(0, &mut h.0[..64 * 1024]).unwrap();
        dev.read_at(tail_off, &mut t.0[..64 * 1024]).unwrap();
        match gptfix::check(&h.0[..64 * 1024], &t.0[..64 * 1024], last_lba) {
            Ok(g) if g.state == gptfix::State::AlreadyFixed => {
                println!("gptfix re-check: AlreadyFixed")
            }
            other => fail(&format!("gptfix re-check: {other:?}")),
        }
    }

    // 4. The WinPE installer continues from here.
    let rc = woa_install::prepare(&woa_install::Options {
        backup_dir: dir.join("backup"),
        dry_run: false,
        allow_virtual_test_disk: true,
        answer: Some("INSTALL".into()),
    });
    if rc != 0 {
        fail(&format!("prepare returned {rc}"));
    }
    {
        let w = Device::open(r"\\.\W:", GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        let mut b = big();
        w.read_at(0, &mut b.0[..4096]).unwrap();
        if b.0[..4096].iter().any(|&x| x != 0) {
            fail("stale boot sector at the Windows partition start was not cleared");
        }
    }
    if let Some(script) = std::env::args().nth(2) {
        let st = std::process::Command::new("cmd.exe")
            .args(["/c", &script, &dir.join("format.log").display().to_string()])
            .status()
            .unwrap();
        if !st.success() {
            fail("format script failed");
        }
        println!("format script OK");
    }
    for l in ['S', 'W', 'R'] {
        let _ = delete_mount_point(&format!("{l}:\\"));
    }
    println!("GPTFIX REHEARSAL PASSED");
    drop(vhd);
    let _ = std::fs::remove_file(&vhdx);
}
