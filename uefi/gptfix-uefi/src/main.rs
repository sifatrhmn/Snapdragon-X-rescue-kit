//! gptfix.efi — runs from a USB stick before Windows starts (Secure Boot off) and adds
//! the four Windows partitions (EFI, MSR, Windows, Recovery) to the ASUS ProArt PZ13 SSD.
//!
//! Windows' partmgr refuses every write-type open of this SSD on a Qualcomm machine while
//! it holds only firmware partitions, so the repair has to happen here, in UEFI.
//!
//! Safety (all checks live in the shared `gptfix` library, tested on the host against
//! the real PZ13 GPT and rehearsed byte-for-byte on a PZ13-shaped virtual disk):
//! * exactly one non-removable 512-byte-sector disk of the PZ13 size;
//! * both GPT copies valid and identical, first 11 entries = the recorded firmware
//!   partitions, all other slots empty (or already fixed → nothing to do);
//! * the operator types YES;
//! * firmware entries copied byte-for-byte; only empty slots are filled;
//! * write order: clear stale boot sectors in the free space, backup entries, backup
//!   header, primary entries, primary header last; then flush and re-verify.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::time::Duration;

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::prelude::*;
use uefi::proto::console::text::Key;
use uefi::proto::media::block::BlockIO;
use uefi::proto::rng::Rng;
use uefi::runtime::{self, ResetType};
use uefi::{print, println, system, Handle};

use gptfix::{State, HEAD_SECTORS, MIB, PZ13_FIRMWARE, PZ13_LAST_LBA, SECTOR, TAIL_SECTORS};

/// A zeroed buffer with the alignment the block device asks for.
struct Aligned {
    ptr: *mut u8,
    layout: Layout,
}

impl Aligned {
    fn new(len: usize, align: usize) -> Aligned {
        let layout = Layout::from_size_align(len, align.max(8)).expect("buffer layout");
        // SAFETY: non-zero size.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            alloc::alloc::handle_alloc_error(layout);
        }
        Aligned { ptr, layout }
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: valid for layout.size() bytes while self lives.
        unsafe { core::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, uniquely borrowed.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        // SAFETY: allocated with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr, self.layout) }
    }
}

/// Reads a line from the keyboard (Enter ends it, Backspace edits).
fn read_line() -> String {
    let mut s = String::new();
    loop {
        match system::with_stdin(|i| i.read_key()) {
            Ok(Some(Key::Printable(c))) => {
                let ch: char = c.into();
                match ch {
                    '\r' | '\n' => {
                        println!();
                        return s;
                    }
                    '\u{8}' => {
                        if s.pop().is_some() {
                            print!("\u{8} \u{8}");
                        }
                    }
                    c if !c.is_control() && s.len() < 32 => {
                        s.push(c);
                        print!("{c}");
                    }
                    _ => {}
                }
            }
            _ => boot::stall(Duration::from_millis(20)),
        }
    }
}

fn wait_enter_and_restart(msg: &str) -> ! {
    println!();
    println!("{msg}");
    println!("Press Enter to restart.");
    let _ = read_line();
    runtime::reset(ResetType::COLD, Status::SUCCESS, None)
}

fn open_block(h: Handle) -> uefi::Result<ScopedProtocol<BlockIO>> {
    // SAFETY: GetProtocol does not disconnect other drivers; the app only reads and
    // writes whole-disk blocks and restarts afterwards.
    unsafe {
        boot::open_protocol::<BlockIO>(
            OpenProtocolParams {
                handle: h,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
}

/// 16 random bytes for a partition GUID: the firmware RNG if present, else a mix of the
/// clock and a counter (uniqueness is all GPT needs).
fn random16(counter: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    if let Ok(h) = boot::get_handle_for_protocol::<Rng>() {
        if let Ok(mut rng) = boot::open_protocol_exclusive::<Rng>(h) {
            if rng.get_rng(None, &mut b).is_ok() {
                return b;
            }
        }
    }
    let t = runtime::get_time().ok();
    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ counter.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    if let Some(t) = t {
        x ^= (t.nanosecond() as u64) << 20 ^ (t.second() as u64) << 8 ^ t.year() as u64;
    }
    for chunk in b.chunks_mut(8) {
        // splitmix64
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    b
}

fn read(bio: &mut BlockIO, media: u32, align: usize, lba: u64, sectors: usize) -> uefi::Result<Vec<u8>> {
    let mut buf = Aligned::new(sectors * SECTOR, align);
    bio.read_blocks(media, lba, buf.bytes_mut())?;
    Ok(buf.bytes().to_vec())
}

fn write(bio: &mut BlockIO, media: u32, align: usize, lba: u64, data: &[u8]) -> uefi::Result {
    let mut buf = Aligned::new(data.len(), align);
    buf.bytes_mut().copy_from_slice(data);
    bio.write_blocks(media, lba, buf.bytes())
}

fn mib(lba: u64) -> u64 {
    lba / MIB
}

#[entry]
fn main() -> Status {
    let _ = system::with_stdout(|o| o.clear());
    println!("==================================================================");
    println!(" WoA Rescue Kit - gptfix for the ASUS ProArt PZ13 internal SSD");
    println!(" Adds the Windows partitions. The 11 firmware partitions are kept.");
    println!("==================================================================");
    println!();

    // 1. Find exactly one disk of the PZ13 SSD size. Firmware with "fast boot" connects
    //    only the devices it boots from, so first connect every controller recursively
    //    (as the UEFI shell's `connect -r` does) so the NVMe disk gets a BlockIO.
    println!("Connecting all devices...");
    if let Ok(all) = boot::locate_handle_buffer(boot::SearchType::AllHandles) {
        for h in all.iter() {
            let _ = boot::connect_controller(*h, &[None], None, true);
        }
    }
    let handles = match boot::find_handles::<BlockIO>() {
        Ok(h) => h,
        Err(e) => wait_enter_and_restart(&alloc::format!("STOPPED: no block devices ({:?}). Nothing was written.", e.status())),
    };
    let mut found: Vec<Handle> = Vec::new();
    for h in handles {
        let Ok(bio) = open_block(h) else { continue };
        let m = bio.media();
        if !m.is_media_present() || m.is_logical_partition() || m.is_removable_media() {
            continue;
        }
        println!(
            "disk: {} sectors of {} bytes ({} MiB){}",
            m.last_block() + 1,
            m.block_size(),
            (m.last_block() + 1) * m.block_size() as u64 / (1024 * 1024),
            if m.block_size() == 512 && m.last_block() == PZ13_LAST_LBA { "  <- PZ13 SSD size" } else { "" }
        );
        if m.block_size() == 512 && m.last_block() == PZ13_LAST_LBA && !m.is_read_only() {
            found.push(h);
        }
    }
    if found.len() != 1 {
        wait_enter_and_restart(&alloc::format!(
            "STOPPED: found {} disk(s) of the PZ13 SSD size (need exactly 1). Nothing was written.",
            found.len()
        ));
    }
    let mut bio = match open_block(found[0]) {
        Ok(b) => b,
        Err(e) => wait_enter_and_restart(&alloc::format!("STOPPED: cannot open the SSD ({:?}). Nothing was written.", e.status())),
    };
    let media = bio.media().media_id();
    let align = bio.media().io_align().max(1) as usize;
    let last = bio.media().last_block();

    // 2. Read and validate both GPT copies.
    let tail_lba = last + 1 - TAIL_SECTORS as u64;
    let (head, tail) = match (
        read(&mut bio, media, align, 0, HEAD_SECTORS),
        read(&mut bio, media, align, tail_lba, TAIL_SECTORS),
    ) {
        (Ok(h), Ok(t)) => (h, t),
        _ => wait_enter_and_restart("STOPPED: could not read the partition tables. Nothing was written."),
    };
    let gpt = match gptfix::check(&head, &tail, last) {
        Ok(g) => g,
        Err(e) => wait_enter_and_restart(&alloc::format!("STOPPED: {e}. Nothing was written.")),
    };
    println!();
    println!("Firmware partitions found (kept exactly as they are):");
    for (name, start, size) in PZ13_FIRMWARE {
        println!("  {name:<16} start {start:>4} MiB  size {size:>4} MiB");
    }
    let plan = match &gpt.state {
        State::AlreadyFixed => wait_enter_and_restart(
            "The Windows partitions are already there. Nothing to do.\nTurn Secure Boot back ON (F2), then boot the SanDisk to install Windows.",
        ),
        State::FirmwareOnly { plan } => *plan,
    };
    println!();
    println!("These Windows partitions will be added in the free space:");
    for p in &plan {
        println!(
            "  {:<9} start {:>7} MiB  size {:>7} MiB",
            p.label,
            mib(p.first_lba),
            mib(p.sectors())
        );
    }
    println!();
    println!("Type YES and press Enter to write the new partition table");
    print!("(anything else cancels): ");
    let answer = read_line();
    if answer.trim() != "YES" {
        wait_enter_and_restart("Cancelled. Nothing was written.");
    }

    // 3. Build, clear the stale areas, write (backup first, primary header last).
    let ids = [random16(1), random16(2), random16(3), random16(4)].map(gptfix::v4);
    let w = match gptfix::build(&gpt, ids) {
        Ok(w) => w,
        Err(e) => wait_enter_and_restart(&alloc::format!("STOPPED: {e}. Nothing was written.")),
    };
    println!("Clearing old boot sectors in the free space...");
    let zero = alloc::vec![0u8; (MIB as usize) * SECTOR];
    for lba in gptfix::areas_to_clear(&plan) {
        if write(&mut bio, media, align, lba, &zero).is_err() {
            wait_enter_and_restart("STOPPED while clearing free space. The partition table was NOT changed.");
        }
    }
    println!("Writing the new partition table...");
    let steps: [(u64, &[u8], &str); 4] = [
        (w.entries_lba_backup, &w.entries, "backup entries"),
        (w.header_lba_backup, &w.backup_header, "backup header"),
        (w.entries_lba_primary, &w.entries, "primary entries"),
        (w.header_lba_primary, &w.primary_header, "primary header"),
    ];
    for (lba, data, what) in steps {
        if let Err(e) = write(&mut bio, media, align, lba, data) {
            wait_enter_and_restart(&alloc::format!(
                "FAILED writing the {what} ({:?}). Do not boot Windows tools yet; plug the stick into the PC and tell Claude.",
                e.status()
            ));
        }
    }
    let _ = bio.flush_blocks();

    // 4. Re-read and verify.
    let ok = match (
        read(&mut bio, media, align, 0, HEAD_SECTORS),
        read(&mut bio, media, align, tail_lba, TAIL_SECTORS),
    ) {
        (Ok(h), Ok(t)) => matches!(gptfix::check(&h, &t, last).map(|g| g.state), Ok(State::AlreadyFixed)),
        _ => false,
    };
    if ok {
        wait_enter_and_restart(
            "DONE: verified. Both partition table copies are valid and hold the firmware\npartitions plus EFI, MSR, Windows and Recovery.\n\nNext: remove this stick, turn Secure Boot back ON (F2),\nthen boot the SanDisk and type I, then INSTALL.",
        )
    } else {
        wait_enter_and_restart("WARNING: written, but the re-check did not pass. Plug this stick into the PC and tell Claude before doing anything else.")
    }
}
