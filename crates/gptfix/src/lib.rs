//! gptfix: adds the four Windows partitions (EFI, MSR, Windows, Recovery) to the GPT of
//! the ASUS ProArt PZ13 SSD, outside Windows.
//!
//! Why: on a Qualcomm platform Windows' partmgr hides a disk that holds only firmware
//! (unknown-type) partitions and refuses every write-type open of it, so no Windows tool
//! can repair it. Writing the GPT before Windows starts (from a UEFI app) breaks that
//! deadlock; afterwards the disk has known-type partitions and Windows treats it normally.
//!
//! Safety rules this library enforces:
//! * both GPT copies must be valid (signatures, header and entry CRC32s, LBAs) and equal;
//! * the first 11 entries must be exactly the recorded PZ13 firmware partitions (name,
//!   first and last LBA), and every other slot must be empty — or hold exactly the four
//!   entries this library would add (then nothing needs doing);
//! * the 11 firmware entries are copied byte-for-byte; only empty slots 12–15 are filled;
//! * the disk GUID and usable range are kept; only the entry-array and header CRCs change.
//!
//! `no_std` + `alloc`, so the UEFI app and the host tests share exactly this code.

#![no_std]
extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::vec;
use alloc::vec::Vec;

pub const SECTOR: usize = 512;
/// 1 MiB in 512-byte sectors.
pub const MIB: u64 = 2048;
pub const ENTRY_SIZE: usize = 128;
pub const ENTRY_COUNT: usize = 128;
/// Sectors holding the 128 × 128-byte entry array.
pub const ENTRY_SECTORS: usize = ENTRY_COUNT * ENTRY_SIZE / SECTOR;
/// Sectors needed from the start of the disk: protective MBR, header, entries.
pub const HEAD_SECTORS: usize = 2 + ENTRY_SECTORS;
/// Sectors needed from the end of the disk: backup entries, backup header.
pub const TAIL_SECTORS: usize = ENTRY_SECTORS + 1;

/// Last LBA of the WD SN740 in the PZ13 (1,024,209,543,168 bytes / 512 − 1).
pub const PZ13_LAST_LBA: u64 = 2_000_409_263;

/// The PZ13 firmware partitions recorded on 2026-11-06: (name, start MiB, size MiB).
pub const PZ13_FIRMWARE: [(&str, u64, u64); 11] = [
    ("ToolsFv", 1, 1),
    ("LogFs", 2, 16),
    ("EnterpriseFV", 18, 1),
    ("ToolsFv_BACKUP", 19, 1),
    ("UpdateCapsule", 20, 70),
    ("NHLOS", 90, 120),
    ("core_nhlos_a", 210, 120),
    ("quantumsdk", 330, 1),
    ("quantumfv", 331, 1),
    ("catecontentfv", 332, 1),
    ("logdump", 333, 512),
];

pub const TYPE_EFI: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const TYPE_MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
pub const TYPE_BASIC: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
pub const TYPE_RECOVERY: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";
/// GPT_ATTRIBUTE_PLATFORM_REQUIRED | GPT_BASIC_DATA_ATTRIBUTE_NO_DRIVE_LETTER (WinRE).
pub const ATTR_RECOVERY: u64 = 0x8000_0000_0000_0001;
pub const WINRE_MIB: u64 = 1024;

/// CRC-32 (IEEE 802.3, reflected, as used by GPT).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}
fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// In-memory (mixed-endian) bytes of a GUID given as text. Panics on malformed text;
/// only used with the constants above.
pub fn guid_bytes(text: &str) -> [u8; 16] {
    fn hex(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("bad hex digit"),
        }
    }
    let digits: Vec<u8> = text.bytes().filter(|&c| c != b'-').collect();
    assert_eq!(digits.len(), 32, "GUID must have 32 hex digits");
    let mut raw = [0u8; 16];
    for i in 0..16 {
        raw[i] = hex(digits[2 * i]) << 4 | hex(digits[2 * i + 1]);
    }
    [
        raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10],
        raw[11], raw[12], raw[13], raw[14], raw[15],
    ]
}

/// A GPT header's fields that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    pub entries_lba: u64,
    pub entry_count: u32,
    pub entry_size: u32,
    pub entries_crc: u32,
}

/// Parses and validates a GPT header sector (signature, revision, size, header CRC).
pub fn parse_header(s: &[u8]) -> Result<Header, &'static str> {
    if s.len() < SECTOR {
        return Err("header sector too short");
    }
    if &s[0..8] != b"EFI PART" {
        return Err("no 'EFI PART' signature");
    }
    if u32_at(s, 8) != 0x0001_0000 {
        return Err("unexpected GPT revision");
    }
    let size = u32_at(s, 12) as usize;
    if size != 92 {
        return Err("unexpected GPT header size");
    }
    let mut h = [0u8; 92];
    h.copy_from_slice(&s[..92]);
    let stored = u32_at(&h, 16);
    put_u32(&mut h, 16, 0);
    if crc32(&h) != stored {
        return Err("GPT header CRC mismatch");
    }
    let mut disk_guid = [0u8; 16];
    disk_guid.copy_from_slice(&s[56..72]);
    Ok(Header {
        my_lba: u64_at(s, 24),
        alternate_lba: u64_at(s, 32),
        first_usable: u64_at(s, 40),
        last_usable: u64_at(s, 48),
        disk_guid,
        entries_lba: u64_at(s, 72),
        entry_count: u32_at(s, 80),
        entry_size: u32_at(s, 84),
        entries_crc: u32_at(s, 88),
    })
}

/// One GPT entry's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub type_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: Vec<u16>,
}

fn entry(e: &[u8]) -> Option<Entry> {
    if e[0..16].iter().all(|&b| b == 0) {
        return None;
    }
    let mut type_guid = [0u8; 16];
    type_guid.copy_from_slice(&e[0..16]);
    let name = (0..36)
        .map(|k| u16::from_le_bytes([e[56 + 2 * k], e[57 + 2 * k]]))
        .take_while(|&c| c != 0)
        .collect();
    Some(Entry {
        type_guid,
        first_lba: u64_at(e, 32),
        last_lba: u64_at(e, 40),
        attributes: u64_at(e, 48),
        name,
    })
}

fn name_is(name: &[u16], s: &str) -> bool {
    name.iter().copied().eq(s.encode_utf16())
}

/// One partition to add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewPart {
    pub label: &'static str,
    pub type_guid: &'static str,
    pub first_lba: u64,
    /// Inclusive.
    pub last_lba: u64,
    pub attributes: u64,
    pub name: &'static str,
}

impl NewPart {
    pub fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// The four partitions, placed exactly as woa-install plans them: EFI 260 MiB right
/// after the firmware partitions, MSR 16 MiB, Windows (the rest), Recovery 1 GiB at
/// the end of the usable space (MiB-aligned).
pub fn plan(fw_end_lba: u64, last_usable: u64) -> [NewPart; 4] {
    let efi = fw_end_lba.div_ceil(MIB) * MIB;
    let msr = efi + 260 * MIB;
    let win = msr + 16 * MIB;
    let end = ((last_usable + 1) / MIB) * MIB; // exclusive
    let winre = end - WINRE_MIB * MIB;
    [
        NewPart {
            label: "EFI",
            type_guid: TYPE_EFI,
            first_lba: efi,
            last_lba: msr - 1,
            attributes: 0,
            name: "EFI system partition",
        },
        NewPart {
            label: "MSR",
            type_guid: TYPE_MSR,
            first_lba: msr,
            last_lba: win - 1,
            attributes: 0,
            name: "Microsoft reserved partition",
        },
        NewPart {
            label: "Windows",
            type_guid: TYPE_BASIC,
            first_lba: win,
            last_lba: winre - 1,
            attributes: 0,
            name: "Basic data partition",
        },
        NewPart {
            label: "Recovery",
            type_guid: TYPE_RECOVERY,
            first_lba: winre,
            last_lba: end - 1,
            attributes: ATTR_RECOVERY,
            name: "",
        },
    ]
}

/// What the disk holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // created once per run
pub enum State {
    /// Only the 11 firmware partitions: the Windows partitions can be added.
    FirmwareOnly { plan: [NewPart; 4] },
    /// The four Windows partitions are already there exactly as planned.
    AlreadyFixed,
}

/// The validated GPT of the PZ13 SSD.
#[derive(Debug, Clone)]
pub struct Gpt {
    pub last_lba: u64,
    pub primary: Header,
    pub backup: Header,
    /// The 16 KiB primary entry array (identical to the backup one).
    pub entries: Vec<u8>,
    /// The raw primary and backup header sectors.
    pub primary_sector: Vec<u8>,
    pub backup_sector: Vec<u8>,
    pub state: State,
}

/// Validates the two GPT copies of the PZ13 SSD.
///
/// `head` = the first `HEAD_SECTORS` sectors (LBA 0..=33), `tail` = the last
/// `TAIL_SECTORS` sectors (LBA last−32..=last), `last_lba` = the disk's last LBA.
pub fn check(head: &[u8], tail: &[u8], last_lba: u64) -> Result<Gpt, &'static str> {
    if head.len() < HEAD_SECTORS * SECTOR || tail.len() < TAIL_SECTORS * SECTOR {
        return Err("not enough sectors read");
    }
    if last_lba != PZ13_LAST_LBA {
        return Err("disk size is not the PZ13 SSD size");
    }
    if head[510] != 0x55 || head[511] != 0xAA || head[446 + 4] != 0xEE {
        return Err("no protective MBR");
    }
    let tail = &tail[tail.len() - TAIL_SECTORS * SECTOR..];
    let primary_sector = &head[SECTOR..2 * SECTOR];
    let backup_sector = &tail[ENTRY_SECTORS * SECTOR..];
    let p = parse_header(primary_sector).map_err(|_| "primary GPT header invalid")?;
    let b = parse_header(backup_sector).map_err(|_| "backup GPT header invalid")?;
    if p.my_lba != 1 || p.alternate_lba != last_lba || p.entries_lba != 2 {
        return Err("primary GPT header LBAs unexpected");
    }
    if b.my_lba != last_lba
        || b.alternate_lba != 1
        || b.entries_lba != last_lba - ENTRY_SECTORS as u64
    {
        return Err("backup GPT header LBAs unexpected");
    }
    for h in [&p, &b] {
        if h.entry_count as usize != ENTRY_COUNT || h.entry_size as usize != ENTRY_SIZE {
            return Err("unexpected GPT entry count or size");
        }
    }
    if p.disk_guid != b.disk_guid
        || p.first_usable != b.first_usable
        || p.last_usable != b.last_usable
    {
        return Err("primary and backup GPT headers disagree");
    }
    let pe = &head[2 * SECTOR..HEAD_SECTORS * SECTOR];
    let be = &tail[..ENTRY_SECTORS * SECTOR];
    if crc32(pe) != p.entries_crc {
        return Err("primary GPT entry array CRC mismatch");
    }
    if crc32(be) != b.entries_crc {
        return Err("backup GPT entry array CRC mismatch");
    }
    if pe != be {
        return Err("primary and backup GPT entry arrays differ");
    }
    // The first 11 entries must be exactly the PZ13 firmware partitions.
    for (i, (name, start, size)) in PZ13_FIRMWARE.iter().enumerate() {
        let e =
            entry(&pe[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE]).ok_or("a firmware slot is empty")?;
        if !name_is(&e.name, name)
            || e.first_lba != start * MIB
            || e.last_lba != (start + size) * MIB - 1
        {
            return Err("firmware partitions differ from the recorded PZ13 layout");
        }
    }
    let fw_end = PZ13_FIRMWARE
        .iter()
        .map(|(_, s, z)| (s + z) * MIB)
        .max()
        .unwrap();
    let planned = plan(fw_end, p.last_usable);
    let rest: Vec<Option<Entry>> = (PZ13_FIRMWARE.len()..ENTRY_COUNT)
        .map(|i| entry(&pe[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE]))
        .collect();
    let state = if rest.iter().all(|e| e.is_none()) {
        State::FirmwareOnly { plan: planned }
    } else {
        let ok = rest[..4].iter().zip(&planned).all(|(e, n)| match e {
            Some(e) => {
                e.type_guid == guid_bytes(n.type_guid)
                    && e.first_lba == n.first_lba
                    && e.last_lba == n.last_lba
            }
            None => false,
        }) && rest[4..].iter().all(|e| e.is_none());
        if !ok {
            return Err(
                "the disk has partitions after the firmware ones that this tool did not plan",
            );
        }
        State::AlreadyFixed
    };
    Ok(Gpt {
        last_lba,
        primary: p,
        backup: b,
        entries: pe.to_vec(),
        primary_sector: primary_sector.to_vec(),
        backup_sector: backup_sector.to_vec(),
        state,
    })
}

/// What to write: the new entry array (at LBA 2 and at last−32) and the two headers
/// (at LBA 1 and at last).
#[derive(Debug, Clone)]
pub struct Write {
    pub entries: Vec<u8>,
    pub primary_header: Vec<u8>,
    pub backup_header: Vec<u8>,
    pub entries_lba_primary: u64,
    pub entries_lba_backup: u64,
    pub header_lba_primary: u64,
    pub header_lba_backup: u64,
}

fn reseal_header(sector: &[u8], entries_crc: u32) -> Vec<u8> {
    let mut s = sector.to_vec();
    put_u32(&mut s, 88, entries_crc);
    put_u32(&mut s, 16, 0);
    let c = crc32(&s[..92]);
    put_u32(&mut s, 16, c);
    s
}

/// A version-4 GUID from 16 bytes of entropy.
pub fn v4(mut r: [u8; 16]) -> [u8; 16] {
    r[7] = (r[7] & 0x0F) | 0x40;
    r[8] = (r[8] & 0x3F) | 0x80;
    r
}

/// Builds the new GPT: firmware entries copied verbatim, the four planned entries in the
/// next empty slots with the given unique GUIDs, CRCs recomputed. `gpt` must be
/// `FirmwareOnly`.
pub fn build(gpt: &Gpt, unique: [[u8; 16]; 4]) -> Result<Write, &'static str> {
    let State::FirmwareOnly { plan } = &gpt.state else {
        return Err("the disk does not need fixing");
    };
    let mut entries = gpt.entries.clone();
    for (k, (n, id)) in plan.iter().zip(unique).enumerate() {
        let o = (PZ13_FIRMWARE.len() + k) * ENTRY_SIZE;
        let e = &mut entries[o..o + ENTRY_SIZE];
        if e.iter().any(|&b| b != 0) {
            return Err("target slot is not empty");
        }
        e[0..16].copy_from_slice(&guid_bytes(n.type_guid));
        e[16..32].copy_from_slice(&id);
        e[32..40].copy_from_slice(&n.first_lba.to_le_bytes());
        e[40..48].copy_from_slice(&n.last_lba.to_le_bytes());
        e[48..56].copy_from_slice(&n.attributes.to_le_bytes());
        for (i, c) in n.name.encode_utf16().take(36).enumerate() {
            e[56 + 2 * i..58 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
    }
    let crc = crc32(&entries);
    Ok(Write {
        primary_header: reseal_header(&gpt.primary_sector, crc),
        backup_header: reseal_header(&gpt.backup_sector, crc),
        entries,
        entries_lba_primary: 2,
        entries_lba_backup: gpt.last_lba - ENTRY_SECTORS as u64,
        header_lba_primary: 1,
        header_lba_backup: gpt.last_lba,
    })
}

/// 1 MiB areas to zero before the new partitions are used, so no stale file system from
/// the deleted Windows install is mounted: the first MiB of each new partition and the
/// last MiB of Windows and Recovery (NTFS keeps a backup boot sector there). Returns
/// starting LBAs of 2048-sector runs.
pub fn areas_to_clear(plan: &[NewPart; 4]) -> Vec<u64> {
    let mut v = vec![];
    for p in plan {
        v.push(p.first_lba);
        if p.label == "Windows" || p.label == "Recovery" {
            v.push(p.last_lba + 1 - MIB);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn guid_layout() {
        let g = guid_bytes(TYPE_EFI);
        assert_eq!(&g[..4], &[0x28, 0x73, 0x2A, 0xC1]);
        assert_eq!(&g[8..], &[0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
    }

    /// A synthetic firmware-only PZ13 disk: (head, tail).
    pub fn synthetic() -> (Vec<u8>, Vec<u8>) {
        let last = PZ13_LAST_LBA;
        let mut entries = vec![0u8; ENTRY_SECTORS * SECTOR];
        for (i, (name, start, size)) in PZ13_FIRMWARE.iter().enumerate() {
            let e = &mut entries[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
            e[0] = 0x11 + i as u8; // some unknown type
            e[16] = 0x22 + i as u8;
            e[32..40].copy_from_slice(&(start * MIB).to_le_bytes());
            e[40..48].copy_from_slice(&((start + size) * MIB - 1).to_le_bytes());
            e[48..56].copy_from_slice(&ATTR_RECOVERY.to_le_bytes());
            for (k, c) in name.encode_utf16().enumerate() {
                e[56 + 2 * k..58 + 2 * k].copy_from_slice(&c.to_le_bytes());
            }
        }
        let crc = crc32(&entries);
        let header = |my: u64, alt: u64, elba: u64| {
            let mut s = vec![0u8; SECTOR];
            s[0..8].copy_from_slice(b"EFI PART");
            put_u32(&mut s, 8, 0x0001_0000);
            put_u32(&mut s, 12, 92);
            s[24..32].copy_from_slice(&my.to_le_bytes());
            s[32..40].copy_from_slice(&alt.to_le_bytes());
            s[40..48].copy_from_slice(&34u64.to_le_bytes());
            s[48..56].copy_from_slice(&(last - 33).to_le_bytes());
            s[56..72].copy_from_slice(&[0x41; 16]);
            s[72..80].copy_from_slice(&elba.to_le_bytes());
            put_u32(&mut s, 80, 128);
            put_u32(&mut s, 84, 128);
            put_u32(&mut s, 88, crc);
            let c = crc32(&s[..92]);
            put_u32(&mut s, 16, c);
            s
        };
        let mut head = vec![0u8; HEAD_SECTORS * SECTOR];
        head[446 + 4] = 0xEE;
        head[510] = 0x55;
        head[511] = 0xAA;
        head[SECTOR..2 * SECTOR].copy_from_slice(&header(1, last, 2));
        head[2 * SECTOR..].copy_from_slice(&entries);
        let mut tail = vec![0u8; TAIL_SECTORS * SECTOR];
        tail[..ENTRY_SECTORS * SECTOR].copy_from_slice(&entries);
        tail[ENTRY_SECTORS * SECTOR..].copy_from_slice(&header(last, 1, last - 32));
        (head, tail)
    }

    fn apply(head: &mut [u8], tail: &mut [u8], w: &Write) {
        head[SECTOR..2 * SECTOR].copy_from_slice(&w.primary_header);
        head[2 * SECTOR..].copy_from_slice(&w.entries);
        tail[..ENTRY_SECTORS * SECTOR].copy_from_slice(&w.entries);
        tail[ENTRY_SECTORS * SECTOR..].copy_from_slice(&w.backup_header);
    }

    #[test]
    fn plan_matches_woa_install() {
        let p = plan(845 * MIB, PZ13_LAST_LBA - 33);
        assert_eq!(p[0].first_lba, 845 * MIB);
        assert_eq!(p[1].first_lba, 1105 * MIB);
        assert_eq!(p[2].first_lba, 1121 * MIB);
        assert_eq!(p[2].sectors(), 974_617 * MIB);
        assert_eq!(p[3].first_lba, 975_738 * MIB);
        assert_eq!(p[3].sectors(), 1024 * MIB);
        assert!(p[3].last_lba <= PZ13_LAST_LBA - 33);
    }

    #[test]
    fn build_round_trip_keeps_firmware_and_validates() {
        let (mut head, mut tail) = synthetic();
        let before = check(&head, &tail, PZ13_LAST_LBA).unwrap();
        assert!(matches!(before.state, State::FirmwareOnly { .. }));
        let w = build(&before, [[1; 16], [2; 16], [3; 16], [4; 16]]).unwrap();
        // Firmware entries byte-identical.
        let fw = PZ13_FIRMWARE.len() * ENTRY_SIZE;
        assert_eq!(&w.entries[..fw], &before.entries[..fw]);
        // Disk GUID and usable range unchanged.
        assert_eq!(&w.primary_header[56..72], &before.primary_sector[56..72]);
        assert_eq!(&w.primary_header[24..56], &before.primary_sector[24..56]);
        apply(&mut head, &mut tail, &w);
        let after = check(&head, &tail, PZ13_LAST_LBA).unwrap();
        assert_eq!(after.state, State::AlreadyFixed);
        assert!(build(&after, [[0; 16]; 4]).is_err(), "nothing to do twice");
    }

    #[test]
    fn refuses_tampered_or_foreign_disks() {
        let (head, tail) = synthetic();
        let mut h = head.clone();
        h[2 * SECTOR + 40] ^= 1; // change NHLOS... entry 0 end LBA
        assert!(
            check(&h, &tail, PZ13_LAST_LBA).is_err(),
            "CRC must catch it"
        );
        assert!(
            check(&head, &tail, PZ13_LAST_LBA - 1).is_err(),
            "wrong size"
        );
        let mut t = tail.clone();
        t[ENTRY_SECTORS * SECTOR] = b'X';
        assert!(
            check(&head, &t, PZ13_LAST_LBA).is_err(),
            "bad backup signature"
        );
    }

    #[test]
    fn clear_areas_inside_new_partitions_only() {
        let p = plan(845 * MIB, PZ13_LAST_LBA - 33);
        for a in areas_to_clear(&p) {
            assert!(p
                .iter()
                .any(|n| a >= n.first_lba && a + MIB - 1 <= n.last_lba));
            assert!(a >= 845 * MIB);
        }
    }

    /// The real PZ13 GPT backed up by woa-install on 2026-11-06, if present.
    #[test]
    fn accepts_the_real_pz13_backup() {
        let dir = std::env::var("PZ13_GPT_BACKUP").unwrap_or_else(|_| {
            std::string::String::from(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata"))
        });
        let (Ok(head), Ok(tail)) = (
            std::fs::read(std::format!("{dir}/head-64k.bin")),
            std::fs::read(std::format!("{dir}/tail-64k.bin")),
        ) else {
            std::eprintln!("no real PZ13 backup in {dir}; skipped");
            return;
        };
        let g = check(&head, &tail, PZ13_LAST_LBA).expect("real PZ13 GPT must validate");
        let State::FirmwareOnly { plan } = &g.state else {
            panic!("expected firmware-only")
        };
        assert_eq!(plan[0].first_lba, 845 * MIB);
        assert_eq!(plan[2].first_lba, 1121 * MIB);
        let w = build(&g, [[9; 16]; 4]).unwrap();
        let (mut h, mut t) = (head.clone(), tail.clone());
        let tl = t.len();
        h[SECTOR..2 * SECTOR].copy_from_slice(&w.primary_header);
        h[2 * SECTOR..HEAD_SECTORS * SECTOR].copy_from_slice(&w.entries);
        t[tl - TAIL_SECTORS * SECTOR..tl - SECTOR].copy_from_slice(&w.entries);
        t[tl - SECTOR..].copy_from_slice(&w.backup_header);
        assert_eq!(
            check(&h, &t, PZ13_LAST_LBA).unwrap().state,
            State::AlreadyFixed
        );
    }
}
