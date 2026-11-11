//! Host helper for the QEMU test of gptfix.efi, working on a raw PZ13-sized image.
//!
//!   cargo run -p gptfix --example image -- create <image> <head-64k.bin> <tail-64k.bin>
//!   cargo run -p gptfix --example image -- verify <image> <head-64k.bin> <tail-64k.bin>
//!
//! `create` makes a sparse file of exactly the PZ13 SSD size, writes the real GPT backup
//! (first and last 64 KiB), a "FIRMWARE" marker at the start of NHLOS (90 MiB) and a stale
//! NTFS boot sector at 1121 MiB. `verify` checks that afterwards both GPT copies are valid
//! and `AlreadyFixed`, the 11 firmware entries are byte-identical to the backup, the NHLOS
//! marker is intact, and the stale sector (and the other cleared areas) are zero.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;

use gptfix::{State, ENTRY_SIZE, HEAD_SECTORS, MIB, PZ13_FIRMWARE, PZ13_LAST_LBA, SECTOR, TAIL_SECTORS};

const SIZE: u64 = (PZ13_LAST_LBA + 1) * SECTOR as u64;
const K64: u64 = 64 * 1024;

fn read_at(f: &mut File, off: u64, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).unwrap();
    f.read_exact(&mut b).unwrap();
    b
}

fn write_at(f: &mut File, off: u64, data: &[u8]) {
    f.seek(SeekFrom::Start(off)).unwrap();
    f.write_all(data).unwrap();
}

fn fail(msg: &str) -> ! {
    eprintln!("VERIFY FAILED: {msg}");
    std::process::exit(1);
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, image, head_p, tail_p) = (&a[0], &a[1], &a[2], &a[3]);
    let head = std::fs::read(head_p).unwrap();
    let tail = std::fs::read(tail_p).unwrap();
    assert_eq!(head.len() as u64, K64);
    assert_eq!(tail.len() as u64, K64);
    match cmd.as_str() {
        "create" => {
            let _ = std::fs::remove_file(image);
            File::create(image).unwrap();
            // Sparse, so only the written blocks use space.
            let st = Command::new("fsutil").args(["sparse", "setflag", image]).status().unwrap();
            assert!(st.success(), "fsutil sparse setflag");
            let mut f = OpenOptions::new().read(true).write(true).open(image).unwrap();
            f.set_len(SIZE).unwrap();
            write_at(&mut f, 0, &head);
            write_at(&mut f, SIZE - K64, &tail);
            let mut m = vec![0u8; 4096];
            m[..8].copy_from_slice(b"FIRMWARE");
            write_at(&mut f, 90 * MIB * SECTOR as u64, &m);
            let mut n = vec![0u8; 4096];
            n[3..11].copy_from_slice(b"NTFS    ");
            n[510] = 0x55;
            n[511] = 0xAA;
            write_at(&mut f, 1121 * MIB * SECTOR as u64, &n);
            f.flush().unwrap();
            let g = gptfix::check(&head, &tail, PZ13_LAST_LBA).unwrap();
            println!("created {image}: {} bytes, state before = {:?}", SIZE, matches!(g.state, State::FirmwareOnly { .. }).then_some("FirmwareOnly"));
        }
        "verify" => {
            let mut f = File::open(image).unwrap();
            assert_eq!(f.metadata().unwrap().len(), SIZE, "image size changed");
            let h = read_at(&mut f, 0, HEAD_SECTORS * SECTOR);
            let t = read_at(&mut f, SIZE - (TAIL_SECTORS * SECTOR) as u64, TAIL_SECTORS * SECTOR);
            let g = gptfix::check(&h, &t, PZ13_LAST_LBA).unwrap_or_else(|e| fail(e));
            if g.state != State::AlreadyFixed {
                fail("state is not AlreadyFixed");
            }
            println!("both GPT copies valid; state AlreadyFixed");
            let fw = PZ13_FIRMWARE.len() * ENTRY_SIZE;
            if h[2 * SECTOR..2 * SECTOR + fw] != head[2 * SECTOR..2 * SECTOR + fw] {
                fail("firmware entries in the primary array changed");
            }
            let tl = t.len();
            let be = &t[..tl - SECTOR];
            let orig_be = &tail[tail.len() - TAIL_SECTORS * SECTOR..tail.len() - SECTOR];
            if be[..fw] != orig_be[..fw] {
                fail("firmware entries in the backup array changed");
            }
            println!("11 firmware entries byte-identical to the backup (primary and backup)");
            if h[..SECTOR] != head[..SECTOR] {
                fail("protective MBR changed");
            }
            if h[SECTOR + 56..SECTOR + 72] != head[SECTOR + 56..SECTOR + 72] {
                fail("disk GUID changed");
            }
            println!("protective MBR and disk GUID unchanged");
            let m = read_at(&mut f, 90 * MIB * SECTOR as u64, 8);
            if &m != b"FIRMWARE" {
                fail("NHLOS marker changed");
            }
            println!("NHLOS marker intact");
            let State::FirmwareOnly { plan } = gptfix::check(&head, &tail, PZ13_LAST_LBA).unwrap().state else {
                unreachable!()
            };
            for lba in gptfix::areas_to_clear(&plan) {
                let z = read_at(&mut f, lba * SECTOR as u64, (MIB as usize) * SECTOR);
                if z.iter().any(|&b| b != 0) {
                    fail(&format!("area at LBA {lba} not cleared"));
                }
            }
            println!("stale boot sectors cleared (all {} areas zero)", gptfix::areas_to_clear(&plan).len());
            println!("QEMU TEST PASSED");
        }
        _ => panic!("create|verify"),
    }
}
