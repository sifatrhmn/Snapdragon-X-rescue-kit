//! Rehearses `woa-install prepare` on a throw-away VHDX shaped like the PZ13 SSD.
//!
//! Run elevated on the dev PC (never in WinPE):
//!   cargo run --release -p woa-install --example vhd_rehearsal -- <scratch-dir>
//!
//! 1. Creates a dynamic VHDX of exactly the PZ13 SSD size and attaches it.
//! 2. Writes markers: "FIRMWARE" inside where NHLOS will be, and an NTFS-looking boot
//!    sector at 1121 MiB (the old Windows partition).
//! 3. Gives it the 11 PZ13 firmware partitions with IOCTL_DISK_SET_DRIVE_LAYOUT_EX.
//! 4. Runs the real `prepare` (answer INSTALL), then again (resume path).
//! 5. Checks: 15 partitions, firmware entries unchanged, NHLOS marker intact, the stale
//!    NTFS sector cleared, S:/W:/R: assigned. Then removes the letters and detaches.

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
    dev.ioctl_out(GET_LAYOUT, &[], &mut buf)
        .expect("get layout");
    parse_layout(&buf).expect("parse layout")
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
    let log = winstorage::Logger::open(&dir, "rehearsal").unwrap();
    let vhdx = dir.join("pz13-rehearsal.vhdx");
    let _ = std::fs::remove_file(&vhdx);
    let vhd = winstorage::create_vhdx(&vhdx, PZ13_DISK_BYTES, winstorage::Template::Empty, &log)
        .unwrap_or_else(|e| fail(&format!("create VHDX: {e}")));
    let path = format!(r"\\.\PhysicalDrive{}", vhd.disk_number);
    println!("rehearsal disk: {path}");

    // 2. Markers, written while the disk has no volumes.
    {
        let dev = Device::open(&path, GENERIC_READ | GENERIC_WRITE, FILE_FLAG_NO_BUFFERING)
            .unwrap_or_else(|e| fail(&format!("open rw: {}", describe(e))));
        let mut s = Box::new(Sector([0; 4096]));
        s.0[..8].copy_from_slice(b"FIRMWARE");
        dev.write_at(90 * MIB, &s.0).expect("marker in NHLOS");
        let mut s = Box::new(Sector([0; 4096]));
        s.0[3..11].copy_from_slice(b"NTFS    ");
        s.0[510] = 0x55;
        s.0[511] = 0xAA;
        dev.write_at(1121 * MIB, &s.0).expect("stale NTFS");
    }

    // 3. The 11 firmware partitions.
    let before = layout_of(&path);
    let mut fw = before.clone();
    fw.partitions.clear();
    let mut buf = fw.header.clone();
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
    let fw = layout_of(&path);
    if check_firmware(&fw) != Ok(0) {
        fail(&format!(
            "firmware layout not accepted: {:?}",
            check_firmware(&fw)
        ));
    }
    println!("rehearsal disk now has the 11 PZ13 firmware partitions");

    // 4. The real thing, twice.
    let opts = woa_install::Options {
        backup_dir: dir.join("backup"),
        dry_run: false,
        allow_virtual_test_disk: true,
        answer: Some("INSTALL".into()),
    };
    let rc = woa_install::prepare(&opts);
    if rc != 0 {
        fail(&format!("prepare returned {rc}"));
    }
    let rc2 = woa_install::prepare(&opts);
    if rc2 != 0 {
        fail(&format!("second prepare (resume) returned {rc2}"));
    }

    // 5. Checks.
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
    let planned = plan(&fw).unwrap();
    if !matches_plan(&after, &planned) {
        fail("new partitions differ from the plan");
    }
    {
        let dev = Device::open(&path, GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        if &read_sector(&dev, 90 * MIB)[..8] != b"FIRMWARE" {
            fail("the marker inside NHLOS was changed");
        }
    }
    for l in ['S', 'W', 'R'] {
        if !letter_in_use(l) {
            fail(&format!("{l}: was not assigned"));
        }
    }
    // The stale NTFS sector at 1121 MiB must be gone (read through the W: volume).
    {
        let w = Device::open(r"\\.\W:", GENERIC_READ, FILE_FLAG_NO_BUFFERING).unwrap();
        if read_sector(&w, 0).iter().any(|&b| b != 0) {
            fail("the old boot sector at the Windows partition start was not cleared");
        }
    }
    // Optional: run the stick's format script on the new partitions and check the result.
    if let Some(script) = std::env::args().nth(2) {
        let flog = dir.join("format.log");
        let st = std::process::Command::new("cmd.exe")
            .args(["/c", &script, &flog.display().to_string()])
            .status()
            .expect("run format script");
        if !st.success() {
            fail(&format!(
                "format script failed ({st}); see {}",
                flog.display()
            ));
        }
        for (l, fs) in [('S', "FAT32"), ('W', "NTFS"), ('R', "NTFS")] {
            let out = std::process::Command::new("fsutil")
                .args(["fsinfo", "volumeinfo", &format!("{l}:")])
                .output()
                .expect("fsutil");
            let text = String::from_utf8_lossy(&out.stdout);
            if !text.contains(&format!(": {fs}")) {
                fail(&format!("{l}: is not {fs}:\n{text}"));
            }
            println!("{l}: formatted {fs}");
        }
    }
    // Optional: apply a Windows image to W: exactly as install-windows.cmd does.
    if let Some(image) = std::env::args().nth(3) {
        println!("applying {image} index 1 to W: (this takes a while)...");
        let st = std::process::Command::new("dism.exe")
            .args([
                "/English",
                "/Apply-Image",
                &format!("/ImageFile:{image}"),
                "/Index:1",
                r"/ApplyDir:W:\",
                &format!("/LogPath:{}", dir.join("dism-apply.log").display()),
            ])
            .status()
            .expect("run dism");
        if !st.success() {
            fail(&format!("DISM /Apply-Image failed ({st})"));
        }
        for f in [
            r"W:\Windows\System32\ntoskrnl.exe",
            r"W:\Windows\System32\bcdboot.exe",
            r"W:\Windows\System32\Reagentc.exe",
            r"W:\Windows\System32\Recovery\Winre.wim",
        ] {
            if !std::path::Path::new(f).exists() {
                fail(&format!("{f} missing after apply"));
            }
            println!("present: {f}");
        }
    }
    for l in ['S', 'W', 'R'] {
        let _ = delete_mount_point(&format!("{l}:\\"));
    }
    println!("REHEARSAL PASSED: firmware kept, 4 partitions added, stale sector cleared, letters assigned, resume OK");
    drop(vhd);
    let _ = std::fs::remove_file(&vhdx);
}
