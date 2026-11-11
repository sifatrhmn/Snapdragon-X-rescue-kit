use crate::model::{Disk, DiskId, FsKind};

/// A queued disk operation. Partitions are addressed by byte offset, never by
/// drive letter (many partitions have none) or partition number (unstable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Create {
        disk: DiskId,
        offset: u64,
        size: u64,
        fs: FsKind,
        label: String,
    },
    Delete {
        disk: DiskId,
        offset: u64,
    },
    Format {
        disk: DiskId,
        offset: u64,
        fs: FsKind,
        label: String,
    },
    Resize {
        disk: DiskId,
        offset: u64,
        new_size: u64,
    },
}

impl Op {
    pub fn disk(&self) -> &DiskId {
        match self {
            Op::Create { disk, .. }
            | Op::Delete { disk, .. }
            | Op::Format { disk, .. }
            | Op::Resize { disk, .. } => disk,
        }
    }

    /// Human-readable one-line description, British English.
    pub fn describe(&self, disks: &[Disk]) -> String {
        let disk = disks.iter().find(|d| d.id == *self.disk());
        match self {
            Op::Create {
                size, fs, label, ..
            } => {
                let on = disk.map_or_else(|| self.disk().0.clone(), |d| d.model.clone());
                let name = if label.is_empty() {
                    String::new()
                } else {
                    format!(" \"{label}\"")
                };
                format!(
                    "Create {} {} partition{name} on {on}",
                    gb(*size),
                    fs_name(fs)
                )
            }
            Op::Delete { offset, .. } => format!("Delete {}", target_name(disk, *offset)),
            Op::Format {
                offset, fs, label, ..
            } => {
                let name = if label.is_empty() {
                    String::new()
                } else {
                    format!(" (label \"{label}\")")
                };
                format!(
                    "Format {} as {}{name}",
                    target_name(disk, *offset),
                    fs_name(fs)
                )
            }
            Op::Resize {
                offset, new_size, ..
            } => {
                let target = target_name(disk, *offset);
                let current = disk
                    .and_then(|d| d.partitions.iter().find(|p| p.offset == *offset))
                    .map(|p| p.size);
                match current {
                    Some(cur) if *new_size < cur => {
                        format!("Shrink {target} by {}", gb(cur - *new_size))
                    }
                    Some(cur) if *new_size > cur => {
                        format!("Extend {target} by {}", gb(*new_size - cur))
                    }
                    Some(_) => format!("Resize {target} (no change)"),
                    None => format!("Resize {target} to {}", gb(*new_size)),
                }
            }
        }
    }
}

/// Decimal gigabytes, one decimal place.
fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn fs_name(fs: &FsKind) -> String {
    match fs {
        FsKind::Ntfs => "NTFS".into(),
        FsKind::Fat32 => "FAT32".into(),
        FsKind::ExFat => "exFAT".into(),
        FsKind::Refs => "ReFS".into(),
        FsKind::Raw => "RAW".into(),
        FsKind::Other(s) => s.clone(),
    }
}

fn target_name(disk: Option<&Disk>, offset: u64) -> String {
    match disk.and_then(|d| d.partitions.iter().find(|p| p.offset == offset)) {
        Some(p) => match p.letter {
            Some(l) => format!("{l}:"),
            None => format!("partition {}", p.number),
        },
        None => format!("partition at offset {offset}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::disk_fixture;

    #[test]
    fn describe_shrink_uses_decimal_gb_and_letter() {
        let d = disk_fixture();
        let c = &d.partitions[2];
        let op = Op::Resize {
            disk: d.id.clone(),
            offset: c.offset,
            new_size: c.size - 40_000_000_000,
        };
        assert_eq!(op.describe(&[d]), "Shrink C: by 40.0 GB");
    }

    #[test]
    fn describe_delete_letterless_partition() {
        let d = disk_fixture();
        let op = Op::Delete {
            disk: d.id.clone(),
            offset: d.partitions[3].offset,
        };
        assert_eq!(op.describe(&[d]), "Delete partition 4");
    }

    #[test]
    fn describe_create_and_format() {
        let d = disk_fixture();
        let c = Op::Create {
            disk: d.id.clone(),
            offset: 0,
            size: 2_500_000_000,
            fs: FsKind::ExFat,
            label: "Data".into(),
        };
        assert_eq!(
            c.describe(std::slice::from_ref(&d)),
            "Create 2.5 GB exFAT partition \"Data\" on Snapdragon NVMe"
        );
        let f = Op::Format {
            disk: d.id.clone(),
            offset: d.partitions[2].offset,
            fs: FsKind::Ntfs,
            label: String::new(),
        };
        assert_eq!(f.describe(&[d]), "Format C: as NTFS");
    }
}
