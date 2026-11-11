use crate::model::{Disk, PartitionStyle, MIB};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub size: u64,
}

/// Rounds `v` up to a multiple of `a` (`a` must be non-zero).
pub fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Rounds `v` down to a multiple of `a` (`a` must be non-zero).
pub fn align_down(v: u64, a: u64) -> u64 {
    v / a * a
}

/// Free space on the disk usable for new partitions: gaps of at least 1 MiB,
/// 1 MiB-aligned, starting after the first MiB (partition-table area). On
/// GPT and raw disks the last MiB is reserved for the backup GPT.
pub fn free_extents(disk: &Disk) -> Vec<Extent> {
    let reserved_tail = if disk.style == PartitionStyle::Mbr {
        0
    } else {
        MIB
    };
    let limit = disk.size.saturating_sub(reserved_tail);
    let mut out = Vec::new();
    let mut gap = |from: u64, to: u64| {
        let start = align_up(from, MIB);
        let end = align_down(to, MIB);
        if end > start && end - start >= MIB {
            out.push(Extent {
                offset: start,
                size: end - start,
            });
        }
    };
    let mut cursor = MIB;
    for p in &disk.partitions {
        if p.offset > cursor {
            gap(cursor, p.offset.min(limit));
        }
        cursor = cursor.max(p.offset.saturating_add(p.size));
    }
    if limit > cursor {
        gap(cursor, limit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn part(offset: u64, size: u64) -> Partition {
        Partition {
            number: 1,
            offset,
            size,
            kind: PartKind::Basic,
            letter: None,
            label: String::new(),
            fs: FsKind::Ntfs,
            used: None,
            protection: Protection::None,
            supported: None,
            volume_path: None,
            has_windows: false,
        }
    }

    fn disk(size: u64, partitions: Vec<Partition>) -> Disk {
        Disk {
            id: DiskId("x".into()),
            number: 0,
            model: String::new(),
            serial: String::new(),
            size,
            bus: BusType::Nvme,
            style: PartitionStyle::Gpt,
            sector_size: 512,
            is_boot_media: false,
            is_running_os: false,
            is_simulation: true,
            partitions,
        }
    }

    #[test]
    fn align_helpers() {
        assert_eq!(align_up(1, MIB), MIB);
        assert_eq!(align_up(MIB, MIB), MIB);
        assert_eq!(align_down(MIB + 5, MIB), MIB);
    }

    #[test]
    fn free_extents_between_and_after() {
        let end = 10 * 1024 * MIB;
        let d = disk(end, vec![part(MIB, 100 * MIB), part(200 * MIB, 1024 * MIB)]);
        assert_eq!(
            free_extents(&d),
            vec![
                Extent {
                    offset: 101 * MIB,
                    size: 99 * MIB
                },
                Extent {
                    offset: 1224 * MIB,
                    size: end - MIB - 1224 * MIB
                },
            ]
        );
    }

    #[test]
    fn small_gap_not_reported() {
        let end = 10 * 1024 * MIB;
        let d = disk(
            end,
            vec![
                part(MIB, 100 * MIB),
                part(101 * MIB + MIB / 2, end - MIB - (101 * MIB + MIB / 2)),
            ],
        );
        assert!(free_extents(&d).is_empty());
    }

    #[test]
    fn empty_disk_is_one_extent() {
        let d = disk(100 * MIB, vec![]);
        assert_eq!(
            free_extents(&d),
            vec![Extent {
                offset: MIB,
                size: 98 * MIB
            }]
        );
    }
}
