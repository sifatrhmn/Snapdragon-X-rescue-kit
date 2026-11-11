//! Pure builders and parsers for the structures nvme-probe exchanges with Windows and
//! the NVMe controller. Layouts follow winioctl.h, ntddscsi.h and the NVMe 1.4 spec.

/// Size of `STORAGE_PROTOCOL_SPECIFIC_DATA` (ten ULONGs).
pub const PROTOCOL_DATA_LEN: usize = 40;
/// `FIELD_OFFSET(STORAGE_PROPERTY_QUERY, AdditionalParameters)`.
const QUERY_HEADER_LEN: usize = 8;

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}
fn u128_at(b: &[u8], o: usize) -> u128 {
    b.get(o..o + 16)
        .map(|s| u128::from_le_bytes(s.try_into().unwrap()))
        .unwrap_or(0)
}
fn text_at(b: &[u8], from: usize, to: usize) -> String {
    b.get(from..to)
        .map(|s| {
            String::from_utf8_lossy(s)
                .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string()
        })
        .unwrap_or_default()
}
fn cstr_at(b: &[u8], o: usize) -> String {
    if o == 0 || o >= b.len() {
        return String::new();
    }
    let end = b[o..]
        .iter()
        .position(|&c| c == 0)
        .map_or(b.len(), |p| o + p);
    text_at(b, o, end)
}
fn gb(bytes: u128) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// `STORAGE_PROPERTY_QUERY` + `STORAGE_PROTOCOL_SPECIFIC_DATA` + `len` bytes of room.
pub fn build_protocol_query(
    property: u32,
    protocol: u32,
    data_type: u32,
    value: u32,
    sub_value: u32,
    len: usize,
) -> Vec<u8> {
    let mut b = vec![0u8; QUERY_HEADER_LEN + PROTOCOL_DATA_LEN + len];
    let put = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    put(&mut b, 0, property);
    put(&mut b, 4, 0); // PropertyStandardQuery
    let p = QUERY_HEADER_LEN;
    put(&mut b, p, protocol);
    put(&mut b, p + 4, data_type);
    put(&mut b, p + 8, value);
    put(&mut b, p + 12, sub_value);
    put(&mut b, p + 16, PROTOCOL_DATA_LEN as u32); // ProtocolDataOffset
    put(&mut b, p + 20, len as u32); // ProtocolDataLength
    b
}

/// The returned data of a `STORAGE_PROTOCOL_DATA_DESCRIPTOR` (offset is relative to its
/// `ProtocolSpecificData`, which starts 8 bytes in). `None` if it does not fit.
pub fn protocol_payload(b: &[u8]) -> Option<&[u8]> {
    let base = 8;
    let off = u32_at(b, base + 16) as usize;
    let len = u32_at(b, base + 20) as usize;
    if off == 0 || len == 0 {
        return None;
    }
    b.get(base + off..base + off + len)
}

/// Identify Controller (CNS 01h).
pub fn describe_controller(d: &[u8]) -> Vec<String> {
    let oacs = u16_at(d, 256);
    vec![
        format!(
            "vendor 0x{:04X}, model '{}', serial '{}', firmware '{}'",
            u16_at(d, 0),
            text_at(d, 24, 64),
            text_at(d, 4, 24),
            text_at(d, 64, 72)
        ),
        format!(
            "total capacity {} ({} bytes), unallocated {}",
            gb(u128_at(d, 280)),
            u128_at(d, 280),
            gb(u128_at(d, 296))
        ),
        format!("namespaces supported: {}", u32_at(d, 516)),
        format!(
            "security send/receive (self-encrypting / TCG): {}; format NVM: {}; namespace management: {}",
            yes(oacs & 0x1 != 0),
            yes(oacs & 0x2 != 0),
            yes(oacs & 0x8 != 0)
        ),
    ]
}

fn yes(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Active Namespace ID list (CNS 02h): non-zero IDs up to the first zero.
pub fn active_namespaces(d: &[u8]) -> Vec<u32> {
    (0..d.len() / 4)
        .map(|i| u32_at(d, i * 4))
        .take_while(|&id| id != 0)
        .collect()
}

/// Identify Namespace (CNS 00h).
pub fn describe_namespace(d: &[u8]) -> Vec<String> {
    let nsze = u64_at(d, 0);
    let ncap = u64_at(d, 8);
    let nuse = u64_at(d, 16);
    let fmt = (d.get(26).copied().unwrap_or(0) & 0x0F) as usize;
    let lbads = d.get(128 + fmt * 4 + 2).copied().unwrap_or(0);
    let block: u128 = if (9..=16).contains(&lbads) {
        1u128 << lbads
    } else {
        0
    };
    let mut v = vec![
        format!("size {nsze} blocks, capacity {ncap}, in use {nuse}"),
        format!(
            "block size {} bytes (LBA format {fmt}); namespace size {}",
            block,
            gb(nsze as u128 * block)
        ),
    ];
    if nsze == 0 {
        v.push(
            "WARNING: namespace 1 has zero size (not created, or controller in a failed state)"
                .into(),
        );
    }
    v
}

/// SMART / Health Information log (log page 02h).
pub fn describe_health(d: &[u8]) -> Vec<String> {
    let cw = d.first().copied().unwrap_or(0);
    let mut warn = Vec::new();
    for (bit, what) in [
        (0, "spare below threshold"),
        (1, "temperature"),
        (2, "reliability degraded"),
        (3, "MEDIA READ-ONLY"),
        (4, "volatile backup failed"),
        (5, "persistent memory read-only"),
    ] {
        if cw & (1 << bit) != 0 {
            warn.push(what);
        }
    }
    let kelvin = u16_at(d, 1);
    vec![
        format!(
            "critical warning 0x{cw:02X}{}",
            if warn.is_empty() {
                " (none)".to_string()
            } else {
                format!(" ({})", warn.join(", "))
            }
        ),
        format!(
            "temperature {} C, spare {}% (threshold {}%), wear used {}%",
            kelvin as i32 - 273,
            d.get(3).copied().unwrap_or(0),
            d.get(4).copied().unwrap_or(0),
            d.get(5).copied().unwrap_or(0)
        ),
        format!(
            "data read {}, written {}",
            gb(u128_at(d, 32) * 512_000),
            gb(u128_at(d, 48) * 512_000)
        ),
        format!(
            "power cycles {}, power-on hours {}, unsafe shutdowns {}",
            u128_at(d, 112),
            u128_at(d, 128),
            u128_at(d, 144)
        ),
        format!(
            "media errors {}, error log entries {}",
            u128_at(d, 160),
            u128_at(d, 176)
        ),
    ]
}

/// One LUN from `IOCTL_SCSI_GET_INQUIRY_DATA`.
#[derive(Debug, PartialEq, Eq)]
pub struct Lun {
    pub path: u8,
    pub target: u8,
    pub lun: u8,
    pub claimed: bool,
    pub device_type: u8,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

/// Walks `SCSI_ADAPTER_BUS_INFO` / `SCSI_INQUIRY_DATA`.
pub fn parse_inquiry_data(b: &[u8]) -> Vec<Lun> {
    let mut out = Vec::new();
    let buses = b.first().copied().unwrap_or(0) as usize;
    for i in 0..buses {
        let mut off = u32_at(b, 4 + i * 8 + 4) as usize;
        let mut guard = 0;
        while off != 0 && off + 12 <= b.len() && guard < 64 {
            let len = u32_at(b, off + 4) as usize;
            let inq = b.get(off + 12..off + 12 + len).unwrap_or(&[]);
            out.push(Lun {
                path: b[off],
                target: b[off + 1],
                lun: b[off + 2],
                claimed: b[off + 3] != 0,
                device_type: inq.first().copied().unwrap_or(0) & 0x1F,
                vendor: text_at(inq, 8, 16),
                product: text_at(inq, 16, 32),
                revision: text_at(inq, 32, 36),
            });
            off = u32_at(b, off + 8) as usize;
            guard += 1;
        }
    }
    out
}

/// `STORAGE_DEVICE_DESCRIPTOR`.
pub fn describe_device_descriptor(q: &[u8]) -> String {
    let bus = match u32_at(q, 28) {
        7 => "USB".to_string(),
        11 => "SATA".to_string(),
        17 => "NVMe".to_string(),
        n => format!("bus type {n}"),
    };
    format!(
        "{bus}, vendor '{}', product '{}', revision '{}', serial '{}', removable {}",
        cstr_at(q, u32_at(q, 12) as usize),
        cstr_at(q, u32_at(q, 16) as usize),
        cstr_at(q, u32_at(q, 20) as usize),
        cstr_at(q, u32_at(q, 24) as usize),
        yes(q.get(10).copied().unwrap_or(0) != 0)
    )
}

/// What the first 4 KiB of a disk hold.
pub fn describe_first_sectors(s: &[u8]) -> String {
    if s.iter().all(|&b| b == 0) {
        return "all zeros (blank disk, or encrypted/locked reads returning zeros)".into();
    }
    let mut v = Vec::new();
    if s.get(510..512) == Some(&[0x55, 0xAA]) {
        v.push("MBR/protective-MBR signature");
    }
    if s.get(512..520) == Some(b"EFI PART") {
        v.push("GPT header (512-byte sectors)");
    }
    if v.is_empty() {
        "data present, no MBR or GPT signature".into()
    } else {
        v.join(", ")
    }
}

/// Formats a GUID stored in its on-disk (mixed-endian) layout.
pub fn guid_at(b: &[u8], o: usize) -> String {
    let Some(g) = b.get(o..o + 16) else {
        return String::new();
    };
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32::from_le_bytes(g[0..4].try_into().unwrap()),
        u16::from_le_bytes(g[4..6].try_into().unwrap()),
        u16::from_le_bytes(g[6..8].try_into().unwrap()),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    )
}

/// Friendly name for a GPT partition type GUID.
pub fn gpt_type_name(guid: &str) -> &'static str {
    match guid {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Basic data (Windows)",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "LDM metadata (DYNAMIC DISK)",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "LDM data (DYNAMIC DISK)",
        "E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D" => "STORAGE SPACES pool member",
        "558D43C5-A1AC-43C0-AAC8-D1472B2923D1" => "Storage Replica",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "CA7D7CCB-63ED-4C53-861C-1742536059CC" => "LUKS encrypted",
        _ => "other (vendor/firmware or unknown)",
    }
}

/// One GPT partition entry.
#[derive(Debug, PartialEq, Eq)]
pub struct GptEntry {
    pub index: usize,
    pub type_guid: String,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: String,
}

/// Primary GPT: header at byte 512 of `disk` (bytes read from offset 0).
pub fn parse_gpt(disk: &[u8]) -> Result<(String, Vec<GptEntry>), String> {
    parse_gpt_in(disk, 512, 0)
}

/// GPT header at byte `h` of `buf`, where `buf` starts at LBA `base_lba` (512-byte
/// sectors); its entries must lie inside `buf`. `Err` explains what is wrong.
pub fn parse_gpt_in(
    buf: &[u8],
    h: usize,
    base_lba: u64,
) -> Result<(String, Vec<GptEntry>), String> {
    let disk = buf;
    if disk.get(h..h + 8) != Some(b"EFI PART") {
        return Err("no GPT header there".into());
    }
    let entries_lba = u64_at(disk, h + 72) as usize;
    let count = u32_at(disk, h + 80) as usize;
    let size = u32_at(disk, h + 84) as usize;
    let summary = format!(
        "GPT header: disk GUID {}, usable LBA {}..{}, {} entries of {} bytes at LBA {}",
        guid_at(disk, h + 56),
        u64_at(disk, h + 40),
        u64_at(disk, h + 48),
        count,
        size,
        entries_lba
    );
    if size < 128 || count == 0 || count > 1024 {
        return Err(format!("{summary}; unusual entry count/size"));
    }
    let Some(rel) = (entries_lba as u64).checked_sub(base_lba) else {
        return Err(format!("{summary}; entries lie before the bytes read"));
    };
    let mut v = Vec::new();
    for i in 0..count {
        let o = rel as usize * 512 + i * size;
        let Some(e) = disk.get(o..o + 128) else {
            return Err(format!("{summary}; entry {i} lies beyond the bytes read"));
        };
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        let name: Vec<u16> = (0..36)
            .map(|k| u16::from_le_bytes([e[56 + 2 * k], e[57 + 2 * k]]))
            .take_while(|&c| c != 0)
            .collect();
        v.push(GptEntry {
            index: i + 1,
            type_guid: guid_at(e, 0),
            first_lba: u64_at(e, 32),
            last_lba: u64_at(e, 40),
            attributes: u64_at(e, 48),
            name: String::from_utf16_lossy(&name),
        });
    }
    Ok((summary, v))
}

/// What kind of boot sector (if any) starts `s` (at least 512 bytes).
pub fn boot_sector_kind(s: &[u8]) -> Option<&'static str> {
    let sig = s.get(510..512) == Some(&[0x55, 0xAA]);
    match s.get(3..11)? {
        b"NTFS    " if sig => Some("NTFS"),
        b"-FVE-FS-" => Some("BitLocker (encrypted volume)"),
        _ if sig && s.get(82..90) == Some(b"FAT32   ") => Some("FAT32"),
        _ if sig && s.get(54..62) == Some(b"FAT16   ") => Some("FAT16"),
        b"EXFAT   " => Some("exFAT"),
        _ if s.get(0..8) == Some(b"EFI PART") => Some("GPT header"),
        _ if s.get(3..7) == Some(b"ReFS") => Some("ReFS"),
        _ => None,
    }
}

/// One line per GPT entry, with offset and size in MB.
pub fn describe_gpt_entry(e: &GptEntry) -> String {
    let start = e.first_lba * 512;
    let size = (e.last_lba.saturating_sub(e.first_lba) + 1) * 512;
    format!(
        "#{:<3} {:<36} {:<34} start {:>10.1} MB  size {:>12.1} MB  attr 0x{:016X}  '{}'",
        e.index,
        e.type_guid,
        gpt_type_name(&e.type_guid),
        start as f64 / 1_048_576.0,
        size as f64 / 1_048_576.0,
        e.attributes,
        e.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_query_layout_matches_winioctl() {
        let b = build_protocol_query(49, 3, 1, 1, 0, 4096);
        assert_eq!(b.len(), 8 + 40 + 4096);
        assert_eq!(u32_at(&b, 0), 49);
        assert_eq!(u32_at(&b, 8), 3);
        assert_eq!(u32_at(&b, 12), 1);
        assert_eq!(u32_at(&b, 16), 1);
        assert_eq!(u32_at(&b, 24), 40);
        assert_eq!(u32_at(&b, 28), 4096);
    }

    #[test]
    fn payload_uses_offset_relative_to_protocol_data() {
        let mut b = build_protocol_query(49, 3, 1, 1, 0, 8);
        b[48..56].copy_from_slice(b"ABCDEFGH");
        assert_eq!(protocol_payload(&b), Some(&b"ABCDEFGH"[..]));
        let mut short = b.clone();
        short.truncate(50);
        assert_eq!(protocol_payload(&short), None);
    }

    #[test]
    fn controller_fields() {
        let mut d = vec![0u8; 4096];
        d[0..2].copy_from_slice(&0x15B7u16.to_le_bytes());
        d[4..10].copy_from_slice(b"SER123");
        d[24..44].copy_from_slice(b"WD PC SN740 SDDPTQD-");
        d[64..70].copy_from_slice(b"73120W");
        d[256] = 0x01;
        d[280..296].copy_from_slice(&1_024_209_543_168u128.to_le_bytes());
        d[516..520].copy_from_slice(&1u32.to_le_bytes());
        let v = describe_controller(&d).join("\n");
        assert!(v.contains("vendor 0x15B7"), "{v}");
        assert!(v.contains("WD PC SN740"), "{v}");
        assert!(v.contains("1024.2 GB"), "{v}");
        assert!(v.contains("self-encrypting / TCG): yes"), "{v}");
    }

    #[test]
    fn namespace_size_and_zero_warning() {
        let mut d = vec![0u8; 4096];
        d[0..8].copy_from_slice(&2_000_409_264u64.to_le_bytes());
        d[128 + 2] = 9; // LBA format 0: 512-byte blocks
        let v = describe_namespace(&d).join("\n");
        assert!(v.contains("block size 512"), "{v}");
        assert!(v.contains("1024.2 GB"), "{v}");
        assert!(!v.contains("WARNING"));
        let z = describe_namespace(&vec![0u8; 4096]).join("\n");
        assert!(z.contains("zero size"), "{z}");
    }

    #[test]
    fn health_decodes_read_only_warning() {
        let mut d = vec![0u8; 512];
        d[0] = 0x08;
        d[1..3].copy_from_slice(&313u16.to_le_bytes());
        let v = describe_health(&d).join("\n");
        assert!(v.contains("MEDIA READ-ONLY"), "{v}");
        assert!(v.contains("temperature 40 C"), "{v}");
    }

    #[test]
    fn active_namespace_list_stops_at_zero() {
        let mut d = vec![0u8; 16];
        d[0..4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(active_namespaces(&d), vec![1]);
        assert!(active_namespaces(&[0u8; 16]).is_empty());
    }

    #[test]
    fn inquiry_walk() {
        let mut b = vec![0u8; 128];
        b[0] = 1; // one bus
        b[8..12].copy_from_slice(&16u32.to_le_bytes()); // InquiryDataOffset
        let off = 16;
        b[off + 3] = 1; // claimed
        b[off + 4..off + 8].copy_from_slice(&36u32.to_le_bytes());
        let inq = off + 12;
        b[inq + 8..inq + 12].copy_from_slice(b"NVMe");
        b[inq + 16..inq + 27].copy_from_slice(b"WD PC SN740");
        let l = parse_inquiry_data(&b);
        assert_eq!(l.len(), 1);
        assert!(l[0].claimed);
        assert_eq!(l[0].vendor, "NVMe");
        assert_eq!(l[0].product, "WD PC SN740");
    }

    #[test]
    fn gpt_entries_and_type_names() {
        let mut d = vec![0u8; 64 * 1024];
        d[512..520].copy_from_slice(b"EFI PART");
        d[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
        d[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes());
        d[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
        // Entry 1: EFI system partition C12A7328-F81F-11D2-BA4B-00A0C93EC93B, LBA 2048..534527.
        let e = 1024;
        d[e..e + 16].copy_from_slice(&[
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ]);
        d[e + 32..e + 40].copy_from_slice(&2048u64.to_le_bytes());
        d[e + 40..e + 48].copy_from_slice(&534_527u64.to_le_bytes());
        for (k, c) in "EFI".encode_utf16().enumerate() {
            d[e + 56 + 2 * k..e + 58 + 2 * k].copy_from_slice(&c.to_le_bytes());
        }
        let (_, v) = parse_gpt(&d).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].type_guid, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(gpt_type_name(&v[0].type_guid), "EFI system");
        let line = describe_gpt_entry(&v[0]);
        assert!(line.contains("start        1.0 MB"), "{line}");
        assert!(line.contains("size        260.0 MB"), "{line}");
        assert!(line.contains("'EFI'"), "{line}");
        assert!(parse_gpt(&vec![0u8; 4096]).is_err());
    }

    #[test]
    fn backup_gpt_with_base_lba() {
        // Buffer covers LBA 1000..1128; entries at LBA 1000, header at LBA 1127.
        let mut b = vec![0u8; 128 * 512];
        let h = 127 * 512;
        b[h..h + 8].copy_from_slice(b"EFI PART");
        b[h + 72..h + 80].copy_from_slice(&1000u64.to_le_bytes());
        b[h + 80..h + 84].copy_from_slice(&128u32.to_le_bytes());
        b[h + 84..h + 88].copy_from_slice(&128u32.to_le_bytes());
        b[0] = 0xAA; // a non-empty type GUID in entry 1
        let (_, v) = parse_gpt_in(&b, h, 1000).unwrap();
        assert_eq!(v.len(), 1);
        assert!(parse_gpt_in(&b, h, 1001).is_err(), "entries before buffer");
    }

    #[test]
    fn boot_sector_kinds() {
        let mut s = vec![0u8; 512];
        s[3..11].copy_from_slice(b"NTFS    ");
        s[510] = 0x55;
        s[511] = 0xAA;
        assert_eq!(boot_sector_kind(&s), Some("NTFS"));
        s[3..11].copy_from_slice(b"-FVE-FS-");
        assert_eq!(boot_sector_kind(&s), Some("BitLocker (encrypted volume)"));
        s[3..11].copy_from_slice(b"MSDOS5.0");
        s[82..90].copy_from_slice(b"FAT32   ");
        assert_eq!(boot_sector_kind(&s), Some("FAT32"));
        assert_eq!(boot_sector_kind(&[0u8; 512]), None);
    }

    #[test]
    fn first_sectors() {
        assert!(describe_first_sectors(&[0u8; 4096]).contains("all zeros"));
        let mut s = vec![0u8; 4096];
        s[510] = 0x55;
        s[511] = 0xAA;
        s[512..520].copy_from_slice(b"EFI PART");
        let d = describe_first_sectors(&s);
        assert!(d.contains("MBR") && d.contains("GPT"), "{d}");
    }
}
