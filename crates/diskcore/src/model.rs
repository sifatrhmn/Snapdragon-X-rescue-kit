use serde::{Deserialize, Serialize};

pub const MIB: u64 = 1_048_576;
pub const FAT32_MAX: u64 = 32 * 1024 * MIB;

/// Stable disk identity (MSFT_Disk `UniqueId`). Never the disk number.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DiskId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionStyle {
    Raw,
    Mbr,
    Gpt,
}

/// From `STORAGE_BUS_TYPE`: 17 NVMe, 19 UFS, 7 USB, 11 SATA, 14/15 virtual.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusType {
    Nvme,
    Ufs,
    Usb,
    Sata,
    Virtual,
    Other(u16),
}

impl BusType {
    pub fn from_storage_bus_type(v: u16) -> Self {
        match v {
            17 => BusType::Nvme,
            19 => BusType::Ufs,
            7 => BusType::Usb,
            11 => BusType::Sata,
            14 | 15 => BusType::Virtual,
            other => BusType::Other(other),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsKind {
    Ntfs,
    Fat32,
    ExFat,
    Refs,
    Raw,
    Other(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKind {
    Esp,
    Msr,
    Basic,
    Recovery,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    None,
    Unlocked,
    Locked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    pub number: u32,
    pub offset: u64,
    pub size: u64,
    pub kind: PartKind,
    pub letter: Option<char>,
    pub label: String,
    pub fs: FsKind,
    pub used: Option<u64>,
    pub protection: Protection,
    /// Supported resize range (min, max) in bytes, when known.
    pub supported: Option<(u64, u64)>,
    pub volume_path: Option<String>,
    pub has_windows: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub id: DiskId,
    pub number: u32,
    pub model: String,
    pub serial: String,
    pub size: u64,
    pub bus: BusType,
    pub style: PartitionStyle,
    pub sector_size: u32,
    pub is_boot_media: bool,
    pub is_running_os: bool,
    pub is_simulation: bool,
    /// Sorted by offset.
    pub partitions: Vec<Partition>,
}

const GUID_ESP: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const GUID_MSR: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";
const GUID_BASIC: &str = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
const GUID_RECOVERY: &str = "de94bba4-06d1-4d40-a16a-bfd50179d6ac";

/// Maps a GPT partition-type GUID (any case, braces optional) to a `PartKind`.
pub fn part_kind_from_gpt(guid: &str) -> PartKind {
    let g = guid
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .to_ascii_lowercase();
    match g.as_str() {
        GUID_ESP => PartKind::Esp,
        GUID_MSR => PartKind::Msr,
        GUID_BASIC => PartKind::Basic,
        GUID_RECOVERY => PartKind::Recovery,
        _ => PartKind::Other,
    }
}

/// A "Snapdragon-like" 256 GiB NVMe disk: ESP 260 MiB, MSR 16 MiB, C: NTFS, Recovery 1 GiB.
#[cfg(any(test, feature = "fixtures"))]
pub fn disk_fixture() -> Disk {
    let size = 256 * 1024 * MIB;
    let esp_off = MIB;
    let msr_off = esp_off + 260 * MIB;
    let c_off = msr_off + 16 * MIB;
    let rec_size = 1024 * MIB;
    let rec_off = size - MIB - rec_size;
    let part = |number, offset, size, kind, letter, label: &str, fs| Partition {
        number,
        offset,
        size,
        kind,
        letter,
        label: label.to_string(),
        fs,
        used: None,
        protection: Protection::None,
        supported: None,
        volume_path: None,
        has_windows: false,
    };
    let mut c = part(
        3,
        c_off,
        rec_off - c_off,
        PartKind::Basic,
        Some('C'),
        "Windows",
        FsKind::Ntfs,
    );
    c.used = Some(64 * 1024 * MIB);
    c.has_windows = true;
    Disk {
        id: DiskId("fixture-disk-0".into()),
        number: 0,
        model: "Snapdragon NVMe".into(),
        serial: "FIXTURE0001".into(),
        size,
        bus: BusType::Nvme,
        style: PartitionStyle::Gpt,
        sector_size: 512,
        is_boot_media: false,
        is_running_os: false,
        is_simulation: true,
        partitions: vec![
            part(
                1,
                esp_off,
                260 * MIB,
                PartKind::Esp,
                None,
                "SYSTEM",
                FsKind::Fat32,
            ),
            part(2, msr_off, 16 * MIB, PartKind::Msr, None, "", FsKind::Raw),
            c,
            part(
                4,
                rec_off,
                rec_size,
                PartKind::Recovery,
                None,
                "Recovery",
                FsKind::Ntfs,
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_guid_maps_to_kind() {
        assert_eq!(
            part_kind_from_gpt("{C12A7328-F81F-11D2-BA4B-00A0C93EC93B}"),
            PartKind::Esp
        );
        assert_eq!(
            part_kind_from_gpt("e3c9e316-0b5c-4db8-817d-f92df00215ae"),
            PartKind::Msr
        );
        assert_eq!(
            part_kind_from_gpt("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"),
            PartKind::Basic
        );
        assert_eq!(
            part_kind_from_gpt("{de94bba4-06d1-4d40-a16a-bfd50179d6ac}"),
            PartKind::Recovery
        );
        assert_eq!(
            part_kind_from_gpt("00000000-0000-0000-0000-000000000000"),
            PartKind::Other
        );
    }

    #[test]
    fn fixture_is_sorted_snapdragon_layout() {
        let d = disk_fixture();
        assert_eq!(d.partitions.len(), 4);
        assert!(d.partitions.windows(2).all(|w| w[0].offset < w[1].offset));
        assert_eq!(d.partitions[0].kind, PartKind::Esp);
        assert_eq!(d.partitions[0].size, 260 * MIB);
        assert_eq!(d.partitions[3].kind, PartKind::Recovery);
    }
}
