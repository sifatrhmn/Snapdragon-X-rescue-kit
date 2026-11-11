//! Pure model of `DRIVE_LAYOUT_INFORMATION_EX` (GPT) and the clean-install plan.
//!
//! Layouts (winioctl.h, 64-bit): the header is 48 bytes (style, count, then the GPT union:
//! DiskId @8, StartingUsableOffset @24, UsableLength @32, MaxPartitionCount @40), and each
//! `PARTITION_INFORMATION_EX` is 144 bytes (StartingOffset @8, PartitionLength @16,
//! PartitionNumber @24, RewritePartition @28, then the GPT union @32: PartitionType @32,
//! PartitionId @48, Attributes @64, Name[36] UTF-16 @72).

pub const HEADER_LEN: usize = 48;
pub const ENTRY_LEN: usize = 144;
pub const MIB: u64 = 1024 * 1024;
const STYLE_GPT: u32 = 1;

pub const TYPE_EFI: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const TYPE_MSR: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
pub const TYPE_BASIC: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
pub const TYPE_RECOVERY: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";

/// Size of the WD SN740 in the ProArt PZ13 (HT5306QA), in bytes.
pub const PZ13_DISK_BYTES: u64 = 1_024_209_543_168;

/// The firmware partitions found on the PZ13 SSD on 2026-11-06: (name, start MiB,
/// size MiB). They are kept byte-for-byte; the tool refuses any disk that differs.
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

/// The Windows recovery partition at the end of the disk.
pub const WINRE_MIB: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub start: u64,
    pub length: u64,
    pub type_guid: String,
    pub id_guid: String,
    pub attributes: u64,
    pub name: String,
    /// The raw 144-byte entry as Windows returned it (kept verbatim when rewriting).
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub disk_id: String,
    pub usable_start: u64,
    pub usable_end: u64,
    pub max_partitions: u32,
    pub partitions: Vec<Partition>,
    /// The raw 48-byte header (kept when rewriting).
    pub header: Vec<u8>,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// GUID text (`XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`) from its in-memory layout.
pub fn guid_text(b: &[u8]) -> String {
    nvme_parse::guid_at(b, 0)
}

/// In-memory bytes of a GUID given as text. `None` if malformed.
pub fn guid_bytes(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || text.len() != 36 {
        return None;
    }
    let raw: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
    g[4..6].copy_from_slice(&[raw[5], raw[4]]);
    g[6..8].copy_from_slice(&[raw[7], raw[6]]);
    g[8..16].copy_from_slice(&raw[8..16]);
    Some(g)
}

/// Parses a GPT `DRIVE_LAYOUT_INFORMATION_EX`. Empty slots (zero type) are skipped.
pub fn parse_layout(b: &[u8]) -> Result<Layout, String> {
    if b.len() < HEADER_LEN {
        return Err("layout buffer too short".into());
    }
    if u32_at(b, 0) != STYLE_GPT {
        return Err(format!("disk is not GPT (style {})", u32_at(b, 0)));
    }
    let count = u32_at(b, 4) as usize;
    if b.len() < HEADER_LEN + count * ENTRY_LEN {
        return Err(format!(
            "layout says {count} entries but the buffer is too short"
        ));
    }
    let usable_start = u64_at(b, 24);
    let mut partitions = Vec::new();
    for i in 0..count {
        let e = &b[HEADER_LEN + i * ENTRY_LEN..HEADER_LEN + (i + 1) * ENTRY_LEN];
        if e[32..48].iter().all(|&x| x == 0) {
            continue;
        }
        let name: Vec<u16> = (0..36)
            .map(|k| u16::from_le_bytes([e[72 + 2 * k], e[73 + 2 * k]]))
            .take_while(|&c| c != 0)
            .collect();
        partitions.push(Partition {
            start: u64_at(e, 8),
            length: u64_at(e, 16),
            type_guid: guid_text(&e[32..48]),
            id_guid: guid_text(&e[48..64]),
            attributes: u64_at(e, 64),
            name: String::from_utf16_lossy(&name),
            raw: e.to_vec(),
        });
    }
    Ok(Layout {
        disk_id: guid_text(&b[8..24]),
        usable_start,
        usable_end: usable_start + u64_at(b, 32),
        max_partitions: u32_at(b, 40),
        partitions,
        header: b[..HEADER_LEN].to_vec(),
    })
}

/// Checks that `l` holds exactly the PZ13 firmware partitions (same names, offsets
/// and sizes, in order) as its first entries, and returns how many entries follow them.
pub fn check_firmware(l: &Layout) -> Result<usize, String> {
    if l.partitions.len() < PZ13_FIRMWARE.len() {
        return Err(format!(
            "expected the {} PZ13 firmware partitions, found {} partition(s)",
            PZ13_FIRMWARE.len(),
            l.partitions.len()
        ));
    }
    for (p, (name, start, size)) in l.partitions.iter().zip(PZ13_FIRMWARE) {
        if p.name != name || p.start != start * MIB || p.length != size * MIB {
            return Err(format!(
                "firmware partition mismatch: found '{}' at {} MiB ({} MiB), expected '{}' at {} MiB ({} MiB)",
                p.name,
                p.start / MIB,
                p.length / MIB,
                name,
                start,
                size
            ));
        }
    }
    Ok(l.partitions.len() - PZ13_FIRMWARE.len())
}

/// One partition to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPart {
    pub label: &'static str,
    pub type_guid: &'static str,
    pub start: u64,
    pub length: u64,
    pub attributes: u64,
    pub name: &'static str,
}

/// The four Windows partitions, in the free space after the firmware partitions:
/// EFI 260 MiB, MSR 16 MiB, Windows (the rest), Recovery 1 GiB at the end.
pub fn plan(l: &Layout) -> Result<Vec<NewPart>, String> {
    let fw_end = l
        .partitions
        .iter()
        .take(PZ13_FIRMWARE.len())
        .map(|p| p.start + p.length)
        .max()
        .unwrap_or(0);
    let efi = fw_end.div_ceil(MIB) * MIB;
    let msr = efi + 260 * MIB;
    let win = msr + 16 * MIB;
    let end = (l.usable_end / MIB) * MIB;
    let winre = end.checked_sub(WINRE_MIB * MIB).ok_or("disk too small")?;
    if winre <= win + 64 * 1024 * MIB {
        return Err("less than 64 GiB would be left for Windows".into());
    }
    Ok(vec![
        NewPart {
            label: "EFI",
            type_guid: TYPE_EFI,
            start: efi,
            length: 260 * MIB,
            attributes: 0,
            name: "EFI system partition",
        },
        NewPart {
            label: "MSR",
            type_guid: TYPE_MSR,
            start: msr,
            length: 16 * MIB,
            attributes: 0,
            name: "Microsoft reserved partition",
        },
        NewPart {
            label: "Windows",
            type_guid: TYPE_BASIC,
            start: win,
            length: winre - win,
            attributes: 0,
            name: "Basic data partition",
        },
        NewPart {
            label: "Recovery",
            type_guid: TYPE_RECOVERY,
            start: winre,
            length: WINRE_MIB * MIB,
            // GPT_ATTRIBUTE_PLATFORM_REQUIRED | GPT_BASIC_DATA_ATTRIBUTE_NO_DRIVE_LETTER
            attributes: 0x8000_0000_0000_0001,
            name: "",
        },
    ])
}

/// True when the entries after the firmware ones are exactly `planned` (a previous
/// run already created them).
pub fn matches_plan(l: &Layout, planned: &[NewPart]) -> bool {
    let rest = &l.partitions[PZ13_FIRMWARE.len().min(l.partitions.len())..];
    rest.len() == planned.len()
        && rest
            .iter()
            .zip(planned)
            .all(|(p, n)| p.start == n.start && p.length == n.length && p.type_guid == n.type_guid)
}

/// A new 144-byte GPT `PARTITION_INFORMATION_EX` with the given random id.
pub fn new_entry(n: &NewPart, id: [u8; 16]) -> Vec<u8> {
    let mut e = vec![0u8; ENTRY_LEN];
    e[0..4].copy_from_slice(&STYLE_GPT.to_le_bytes());
    e[8..16].copy_from_slice(&n.start.to_le_bytes());
    e[16..24].copy_from_slice(&n.length.to_le_bytes());
    e[28] = 1; // RewritePartition
    e[32..48].copy_from_slice(&guid_bytes(n.type_guid).expect("valid type GUID"));
    e[48..64].copy_from_slice(&id);
    e[64..72].copy_from_slice(&n.attributes.to_le_bytes());
    for (k, c) in n.name.encode_utf16().take(36).enumerate() {
        e[72 + 2 * k..74 + 2 * k].copy_from_slice(&c.to_le_bytes());
    }
    e
}

/// The `DRIVE_LAYOUT_INFORMATION_EX` to set: the original header and the firmware
/// entries verbatim (RewritePartition set), then the new entries.
pub fn build_layout(l: &Layout, new: &[Vec<u8>]) -> Vec<u8> {
    let keep = &l.partitions[..PZ13_FIRMWARE.len()];
    let count = keep.len() + new.len();
    let mut b = l.header.clone();
    b[4..8].copy_from_slice(&(count as u32).to_le_bytes());
    for p in keep {
        let mut e = p.raw.clone();
        e[28] = 1;
        b.extend_from_slice(&e);
    }
    for e in new {
        b.extend_from_slice(e);
    }
    b
}

/// A version-4 random GUID from 16 random bytes.
pub fn v4_guid(mut r: [u8; 16]) -> [u8; 16] {
    r[7] = (r[7] & 0x0F) | 0x40; // version 4 (high nibble of Data3, stored little-endian)
    r[8] = (r[8] & 0x3F) | 0x80; // RFC 4122 variant
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PZ13 layout as Windows reports it (firmware partitions only).
    pub fn pz13_layout_bytes() -> Vec<u8> {
        let mut b = vec![0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&STYLE_GPT.to_le_bytes());
        b[4..8].copy_from_slice(&11u32.to_le_bytes());
        b[8..24].copy_from_slice(&guid_bytes("41A44457-AD39-4120-A46D-5894F61EB83D").unwrap());
        b[24..32].copy_from_slice(&(34u64 * 512).to_le_bytes());
        b[32..40].copy_from_slice(&((2_000_409_230u64 - 34 + 1) * 512).to_le_bytes());
        b[40..44].copy_from_slice(&128u32.to_le_bytes());
        for (i, (name, start, size)) in PZ13_FIRMWARE.iter().enumerate() {
            let n = NewPart {
                label: "fw",
                type_guid: TYPE_BASIC,
                start: start * MIB,
                length: size * MIB,
                attributes: 0x8000_0000_0000_0001,
                name,
            };
            let mut e = new_entry(&n, [i as u8 + 1; 16]);
            e[28] = 0;
            e[24..28].copy_from_slice(&(i as u32 + 1).to_le_bytes());
            b.extend_from_slice(&e);
        }
        b
    }

    #[test]
    fn guid_round_trip() {
        let t = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        let g = guid_bytes(t).unwrap();
        assert_eq!(g[0], 0x28, "Data1 is little-endian");
        assert_eq!(guid_text(&g), t);
        assert!(guid_bytes("not-a-guid").is_none());
    }

    #[test]
    fn parses_pz13_layout_and_accepts_firmware() {
        let l = parse_layout(&pz13_layout_bytes()).unwrap();
        assert_eq!(l.partitions.len(), 11);
        assert_eq!(l.partitions[5].name, "NHLOS");
        assert_eq!(l.partitions[5].start, 90 * MIB);
        assert_eq!(l.disk_id, "41A44457-AD39-4120-A46D-5894F61EB83D");
        assert_eq!(check_firmware(&l), Ok(0));
    }

    #[test]
    fn refuses_a_different_disk() {
        let mut l = parse_layout(&pz13_layout_bytes()).unwrap();
        l.partitions[3].name = "Something".into();
        assert!(check_firmware(&l).is_err());
        let mut l = parse_layout(&pz13_layout_bytes()).unwrap();
        l.partitions[6].start += MIB;
        assert!(check_firmware(&l).is_err());
        let mut l = parse_layout(&pz13_layout_bytes()).unwrap();
        l.partitions.truncate(10);
        assert!(check_firmware(&l).is_err());
    }

    #[test]
    fn plan_matches_the_old_windows_layout_and_fills_the_disk() {
        let l = parse_layout(&pz13_layout_bytes()).unwrap();
        let p = plan(&l).unwrap();
        assert_eq!(p[0].start, 845 * MIB, "EFI where the old one was");
        assert_eq!(p[1].start, 1105 * MIB);
        assert_eq!(p[2].start, 1121 * MIB, "Windows where the old C: was");
        assert_eq!(p[3].length, 1024 * MIB);
        assert_eq!(p[2].start + p[2].length, p[3].start);
        assert!(p[3].start + p[3].length <= l.usable_end);
        assert!(p[2].length > 900 * 1024 * MIB, "≈950 GiB for Windows");
        // Nothing overlaps the firmware partitions.
        let fw_end = 333 * MIB + 512 * MIB;
        assert!(p.iter().all(|n| n.start >= fw_end));
    }

    #[test]
    fn built_layout_keeps_firmware_verbatim_and_adds_four() {
        let src = pz13_layout_bytes();
        let l = parse_layout(&src).unwrap();
        let p = plan(&l).unwrap();
        let new: Vec<_> = p.iter().map(|n| new_entry(n, v4_guid([7; 16]))).collect();
        let b = build_layout(&l, &new);
        assert_eq!(b.len(), HEADER_LEN + 15 * ENTRY_LEN);
        let again = parse_layout(&b).unwrap();
        assert_eq!(again.partitions.len(), 15);
        for (a, o) in again.partitions.iter().zip(&l.partitions) {
            assert_eq!(
                (
                    a.start,
                    a.length,
                    &a.type_guid,
                    &a.id_guid,
                    a.attributes,
                    &a.name
                ),
                (
                    o.start,
                    o.length,
                    &o.type_guid,
                    &o.id_guid,
                    o.attributes,
                    &o.name
                )
            );
        }
        assert_eq!(check_firmware(&again), Ok(4));
        assert!(matches_plan(&again, &p));
        assert!(!matches_plan(&l, &p));
        assert_eq!(again.partitions[11].type_guid, TYPE_EFI);
        assert_eq!(again.partitions[14].attributes, 0x8000_0000_0000_0001);
        assert_eq!(again.disk_id, l.disk_id);
    }

    #[test]
    fn v4_guid_sets_version_and_variant() {
        let g = v4_guid([0xFF; 16]);
        let t = guid_text(&g);
        assert_eq!(&t[14..15], "4", "{t}");
        assert!(matches!(&t[19..20], "8" | "9" | "A" | "B"), "{t}");
    }
}
