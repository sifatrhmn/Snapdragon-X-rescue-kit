use crate::align::align_up;
use crate::model::{Disk, FsKind, PartKind, Partition, PartitionStyle, Protection, FAT32_MAX, MIB};
use crate::ops::Op;

#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub simulation: bool,
    pub allow_system_partitions: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    UnknownTarget,
    Overlap,
    OutOfDisk,
    LockedVolume,
    BootMedia,
    RunningOs,
    PhysicalInSimulation,
    SystemPartition,
    BelowSupportedMin,
    AboveSupportedMax,
    ResizeNotNtfs,
    ExtendNotAdjacent,
    Fat32TooLarge,
    Misaligned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub op_index: usize,
    pub rule: Rule,
    pub severity: Severity,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Validation {
    pub issues: Vec<Issue>,
    pub projected: Vec<Disk>,
}

impl Validation {
    /// True when there is no `Error` issue (warnings are allowed).
    pub fn ok(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == Severity::Error)
    }
}

fn issue(rule: Rule, severity: Severity, message: impl Into<String>) -> Issue {
    Issue {
        op_index: 0,
        rule,
        severity,
        message: message.into(),
    }
}

fn error(rule: Rule, message: impl Into<String>) -> Issue {
    issue(rule, Severity::Error, message)
}

/// Highest byte usable for partitions: GPT and raw disks reserve the last
/// MiB for the backup table.
fn usable_end(d: &Disk) -> u64 {
    if d.style == PartitionStyle::Mbr {
        d.size
    } else {
        d.size.saturating_sub(MIB)
    }
}

/// Applies `op` to a copy of `disks` and returns the result. Fails on
/// structural problems: unknown target, overlap, running off the disk, or
/// growing into space that is not free. A Create's offset is aligned up to
/// 1 MiB here.
pub fn project(disks: &[Disk], op: &Op) -> Result<Vec<Disk>, Issue> {
    let mut out = disks.to_vec();
    let disk = out.iter_mut().find(|d| d.id == *op.disk()).ok_or_else(|| {
        error(
            Rule::UnknownTarget,
            format!("disk {} not found", op.disk().0),
        )
    })?;
    let limit = usable_end(disk);
    let find = |disk: &Disk, offset: u64| {
        disk.partitions
            .iter()
            .position(|p| p.offset == offset)
            .ok_or_else(|| {
                error(
                    Rule::UnknownTarget,
                    format!("no partition at offset {offset}"),
                )
            })
    };
    match op {
        Op::Create {
            offset,
            size,
            fs,
            label,
            ..
        } => {
            let start = align_up(*offset, MIB);
            let end = start
                .checked_add(*size)
                .filter(|_| *size > 0)
                .ok_or_else(|| error(Rule::OutOfDisk, "partition size is invalid"))?;
            if start < MIB || end > limit {
                return Err(error(
                    Rule::OutOfDisk,
                    "new partition does not fit on the disk",
                ));
            }
            if disk
                .partitions
                .iter()
                .any(|p| start < p.offset + p.size && p.offset < end)
            {
                return Err(error(
                    Rule::Overlap,
                    "new partition overlaps an existing partition",
                ));
            }
            let number = disk.partitions.iter().map(|p| p.number).max().unwrap_or(0) + 1;
            let part = Partition {
                number,
                offset: start,
                size: *size,
                kind: PartKind::Basic,
                letter: None,
                label: label.clone(),
                fs: fs.clone(),
                used: None,
                protection: Protection::None,
                supported: None,
                volume_path: None,
                has_windows: false,
            };
            let at = disk.partitions.partition_point(|p| p.offset < start);
            disk.partitions.insert(at, part);
        }
        Op::Delete { offset, .. } => {
            let i = find(disk, *offset)?;
            disk.partitions.remove(i);
        }
        Op::Format {
            offset, fs, label, ..
        } => {
            let i = find(disk, *offset)?;
            let p = &mut disk.partitions[i];
            p.fs = fs.clone();
            p.label = label.clone();
            p.used = None;
        }
        Op::Resize {
            offset, new_size, ..
        } => {
            let i = find(disk, *offset)?;
            let p = &disk.partitions[i];
            if *new_size == 0 {
                return Err(error(Rule::OutOfDisk, "new size is invalid"));
            }
            if *new_size > p.size {
                let end = p.offset.saturating_add(*new_size);
                let next = disk
                    .partitions
                    .iter()
                    .map(|q| q.offset)
                    .filter(|o| *o > p.offset)
                    .min();
                match next {
                    Some(n) if end > n => {
                        return Err(error(
                            Rule::ExtendNotAdjacent,
                            "there is not enough free space directly after the partition",
                        ))
                    }
                    None if end > limit => {
                        return Err(error(
                            Rule::OutOfDisk,
                            "the partition would extend past the end of the disk",
                        ))
                    }
                    _ => {}
                }
            }
            let p = &mut disk.partitions[i];
            p.size = *new_size;
            // Keep the minimum so a later Resize in the same queue is still
            // range-checked; the maximum is stale once the layout changes.
            p.supported = p.supported.map(|(min, _)| (min, u64::MAX));
        }
    }
    Ok(out)
}

/// Policy rules for `op` against the state it will run in. Structural
/// problems are left to `project`.
fn check_policy(state: &[Disk], op: &Op, ctx: &Context) -> Vec<Issue> {
    let mut out = Vec::new();
    let Some(disk) = state.iter().find(|d| d.id == *op.disk()) else {
        return out;
    };
    let what = op.describe(state);
    if ctx.simulation && !disk.is_simulation {
        out.push(error(
            Rule::PhysicalInSimulation,
            format!("{what}: simulation mode must not touch a physical disk"),
        ));
    }
    if disk.is_boot_media {
        out.push(error(
            Rule::BootMedia,
            format!("{what}: this disk is the boot media"),
        ));
    }
    if disk.is_running_os {
        out.push(error(
            Rule::RunningOs,
            format!("{what}: this disk holds the running operating system"),
        ));
    }
    let target = match op {
        Op::Create { .. } => None,
        Op::Delete { offset, .. } | Op::Format { offset, .. } | Op::Resize { offset, .. } => {
            disk.partitions.iter().find(|p| p.offset == *offset)
        }
    };
    if let Some(p) = target {
        if p.protection == Protection::Locked {
            out.push(error(
                Rule::LockedVolume,
                format!("{what}: the volume is locked by BitLocker"),
            ));
        }
        if matches!(op, Op::Delete { .. })
            && matches!(p.kind, PartKind::Esp | PartKind::Msr)
            && !ctx.allow_system_partitions
        {
            out.push(error(
                Rule::SystemPartition,
                format!("{what}: this is a system partition"),
            ));
        }
        if matches!(op, Op::Format { .. } | Op::Resize { .. }) && p.offset % MIB != 0 {
            out.push(issue(
                Rule::Misaligned,
                Severity::Warning,
                format!("{what}: the partition is not aligned to 1 MiB"),
            ));
        }
    }
    match op {
        Op::Create { size, fs, .. } if *fs == FsKind::Fat32 && *size > FAT32_MAX => {
            out.push(error(
                Rule::Fat32TooLarge,
                format!("{what}: FAT32 volumes are limited to 32 GiB"),
            ));
        }
        Op::Format { fs, .. } if *fs == FsKind::Fat32 => {
            if target.is_some_and(|p| p.size > FAT32_MAX) {
                out.push(error(
                    Rule::Fat32TooLarge,
                    format!("{what}: FAT32 volumes are limited to 32 GiB"),
                ));
            }
        }
        Op::Resize { new_size, .. } => {
            if let Some(p) = target {
                if p.fs != FsKind::Ntfs {
                    out.push(error(
                        Rule::ResizeNotNtfs,
                        format!("{what}: only NTFS volumes can be resized"),
                    ));
                }
                if p.used.is_some_and(|u| *new_size < u) {
                    out.push(error(
                        Rule::BelowSupportedMin,
                        format!("{what}: smaller than the data already on the volume"),
                    ));
                }
                if let Some((min, max)) = p.supported {
                    if *new_size < min {
                        out.push(error(
                            Rule::BelowSupportedMin,
                            format!("{what}: below the smallest supported size"),
                        ));
                    }
                    if *new_size > max {
                        out.push(error(
                            Rule::AboveSupportedMax,
                            format!("{what}: above the largest supported size"),
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// Validates the whole queue, projecting each operation onto the result of
/// the previous one. Stops projecting at the first structural error.
pub fn validate(disks: &[Disk], queue: &[Op], ctx: &Context) -> Validation {
    let mut state = disks.to_vec();
    let mut issues = Vec::new();
    for (i, op) in queue.iter().enumerate() {
        for mut is in check_policy(&state, op, ctx) {
            is.op_index = i;
            issues.push(is);
        }
        match project(&state, op) {
            Ok(next) => state = next,
            Err(mut is) => {
                is.op_index = i;
                issues.push(is);
                break;
            }
        }
    }
    Validation {
        issues,
        projected: state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::free_extents;
    use crate::model::*;

    const GIB: u64 = 1024 * MIB;

    fn ctx() -> Context {
        Context {
            simulation: true,
            allow_system_partitions: false,
        }
    }

    /// The fixture is fully packed; shrink C: by 100 GiB to leave a gap.
    fn gap_disk() -> Disk {
        let mut d = disk_fixture();
        d.partitions[2].size -= 100 * GIB;
        d
    }

    fn has(v: &Validation, rule: Rule, sev: Severity) -> bool {
        v.issues.iter().any(|i| i.rule == rule && i.severity == sev)
    }

    fn create(d: &Disk, offset: u64, size: u64, fs: FsKind) -> Op {
        Op::Create {
            disk: d.id.clone(),
            offset,
            size,
            fs,
            label: "New".into(),
        }
    }

    fn resize_c(d: &Disk, new_size: u64) -> Op {
        Op::Resize {
            disk: d.id.clone(),
            offset: d.partitions[2].offset,
            new_size,
        }
    }

    #[test]
    fn create_in_free_space_aligns_offset() {
        let d = gap_disk();
        let free = free_extents(&d)[0];
        let op = create(&d, free.offset + 4096, 10 * GIB, FsKind::Ntfs);
        let out = project(std::slice::from_ref(&d), &op).unwrap();
        let p = out[0]
            .partitions
            .iter()
            .find(|p| p.label == "New")
            .expect("created");
        assert_eq!(p.offset % MIB, 0);
        assert_eq!(p.offset, free.offset + MIB);
    }

    #[test]
    fn overlap_rejected() {
        let d = disk_fixture();
        let op = create(&d, d.partitions[2].offset + MIB, GIB, FsKind::Ntfs);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::Overlap, Severity::Error));
        assert!(!v.ok());
    }

    #[test]
    fn out_of_disk_rejected() {
        let d = gap_disk();
        let op = create(&d, d.size - GIB, 2 * GIB, FsKind::Ntfs);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::OutOfDisk, Severity::Error));
    }

    #[test]
    fn unknown_target_rejected() {
        let d = disk_fixture();
        let op = Op::Delete {
            disk: d.id.clone(),
            offset: 12345,
        };
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::UnknownTarget, Severity::Error));
    }

    #[test]
    fn locked_volume_rejected() {
        let mut d = disk_fixture();
        d.partitions[2].protection = Protection::Locked;
        let op = resize_c(&d, d.partitions[2].size - GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::LockedVolume, Severity::Error));
    }

    #[test]
    fn boot_media_rejected() {
        let mut d = disk_fixture();
        d.is_boot_media = true;
        let op = resize_c(&d, d.partitions[2].size - GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::BootMedia, Severity::Error));
    }

    #[test]
    fn running_os_rejected() {
        let mut d = disk_fixture();
        d.is_running_os = true;
        let op = resize_c(&d, d.partitions[2].size - GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::RunningOs, Severity::Error));
    }

    #[test]
    fn physical_in_simulation_rejected() {
        let mut d = disk_fixture();
        d.is_simulation = false;
        let op = resize_c(&d, d.partitions[2].size - GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::PhysicalInSimulation, Severity::Error));
    }

    #[test]
    fn physical_disk_fine_outside_simulation() {
        let mut d = disk_fixture();
        d.is_simulation = false;
        let op = resize_c(&d, d.partitions[2].size - GIB);
        let c = Context {
            simulation: false,
            allow_system_partitions: false,
        };
        assert!(validate(&[d], &[op], &c).ok());
    }

    #[test]
    fn delete_esp_needs_flag() {
        let d = disk_fixture();
        let op = Op::Delete {
            disk: d.id.clone(),
            offset: d.partitions[0].offset,
        };
        let v = validate(std::slice::from_ref(&d), std::slice::from_ref(&op), &ctx());
        assert!(has(&v, Rule::SystemPartition, Severity::Error));
        let allow = Context {
            simulation: true,
            allow_system_partitions: true,
        };
        assert!(validate(&[d], &[op], &allow).ok());
    }

    #[test]
    fn delete_msr_needs_flag() {
        let d = disk_fixture();
        let op = Op::Delete {
            disk: d.id.clone(),
            offset: d.partitions[1].offset,
        };
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::SystemPartition, Severity::Error));
    }

    #[test]
    fn delete_recovery_ok_without_flag() {
        let d = disk_fixture();
        let op = Op::Delete {
            disk: d.id.clone(),
            offset: d.partitions[3].offset,
        };
        let v = validate(&[d], &[op], &ctx());
        assert!(v.ok(), "{:?}", v.issues);
    }

    #[test]
    fn format_esp_is_not_a_system_partition_error() {
        let d = disk_fixture();
        let op = Op::Format {
            disk: d.id.clone(),
            offset: d.partitions[0].offset,
            fs: FsKind::Fat32,
            label: "SYSTEM".into(),
        };
        let v = validate(&[d], &[op], &ctx());
        assert!(!has(&v, Rule::SystemPartition, Severity::Error));
        assert!(v.ok(), "{:?}", v.issues);
    }

    #[test]
    fn resize_below_used_rejected() {
        let d = disk_fixture();
        assert_eq!(d.partitions[2].supported, None);
        assert!(d.partitions[2].used.is_some());
        let op = resize_c(&d, 60 * GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::BelowSupportedMin, Severity::Error));
    }

    #[test]
    fn double_resize_keeps_supported_minimum() {
        let mut d = disk_fixture();
        d.partitions[2].used = None;
        d.partitions[2].supported = Some((50 * GIB, 400 * GIB));
        let q = vec![resize_c(&d, 100 * GIB), resize_c(&d, 40 * GIB)];
        let v = validate(&[d], &q, &ctx());
        assert!(!v.ok());
        assert!(v
            .issues
            .iter()
            .any(|i| i.op_index == 1 && i.rule == Rule::BelowSupportedMin));
        assert!(!v.issues.iter().any(|i| i.op_index == 0));
    }

    #[test]
    fn delete_partition_without_letter() {
        let d = disk_fixture();
        let rec = &d.partitions[3];
        assert!(rec.letter.is_none());
        let op = Op::Delete {
            disk: d.id.clone(),
            offset: rec.offset,
        };
        let out = project(std::slice::from_ref(&d), &op).unwrap();
        assert_eq!(out[0].partitions.len(), 3);
        assert!(out[0]
            .partitions
            .iter()
            .all(|p| p.kind != PartKind::Recovery));
    }

    #[test]
    fn shrink_below_min() {
        let mut d = disk_fixture();
        d.partitions[2].supported = Some((50 * GIB, 400 * GIB));
        let op = resize_c(&d, 40 * GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::BelowSupportedMin, Severity::Error));
    }

    #[test]
    fn resize_above_max() {
        let mut d = disk_fixture();
        d.partitions[2].supported = Some((50 * GIB, 100 * GIB));
        let op = resize_c(&d, 150 * GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::AboveSupportedMax, Severity::Error));
    }

    #[test]
    fn resize_fat32_rejected() {
        let mut d = disk_fixture();
        d.partitions[2].fs = FsKind::Fat32;
        let op = resize_c(&d, d.partitions[2].size - GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::ResizeNotNtfs, Severity::Error));
    }

    #[test]
    fn extend_needs_adjacent_free() {
        let d = disk_fixture();
        let op = resize_c(&d, d.partitions[2].size + 10 * GIB);
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::ExtendNotAdjacent, Severity::Error));
    }

    #[test]
    fn extend_into_adjacent_free_ok() {
        let d = gap_disk();
        let op = resize_c(&d, d.partitions[2].size + 10 * GIB);
        assert!(validate(&[d], &[op], &ctx()).ok());
    }

    #[test]
    fn fat32_over_32gib_rejected() {
        let d = gap_disk();
        let free = free_extents(&d)[0];
        let op = create(&d, free.offset, FAT32_MAX + MIB, FsKind::Fat32);
        let v = validate(std::slice::from_ref(&d), &[op], &ctx());
        assert!(has(&v, Rule::Fat32TooLarge, Severity::Error));
        // Format of an existing large partition to FAT32 is rejected too.
        let fmt = Op::Format {
            disk: d.id.clone(),
            offset: d.partitions[2].offset,
            fs: FsKind::Fat32,
            label: String::new(),
        };
        let v = validate(&[d], &[fmt], &ctx());
        assert!(has(&v, Rule::Fat32TooLarge, Severity::Error));
    }

    #[test]
    fn misaligned_existing_warns() {
        let mut d = disk_fixture();
        let mut p = d.partitions[2].clone();
        p.offset = 32_256;
        p.size = 10 * GIB;
        d.partitions = vec![p];
        let op = Op::Format {
            disk: d.id.clone(),
            offset: 32_256,
            fs: FsKind::Ntfs,
            label: String::new(),
        };
        let v = validate(&[d], &[op], &ctx());
        assert!(has(&v, Rule::Misaligned, Severity::Warning));
        assert!(v.ok());
    }

    #[test]
    fn queue_projects_cumulatively() {
        let d = disk_fixture();
        let c = &d.partitions[2];
        let new_size = c.size - 40 * GIB;
        let freed = c.offset + new_size;
        let q = vec![
            resize_c(&d, new_size),
            create(&d, freed, 40 * GIB, FsKind::Ntfs),
        ];
        let v = validate(std::slice::from_ref(&d), &q, &ctx());
        assert!(v.ok(), "{:?}", v.issues);
        assert_eq!(v.projected[0].partitions.len(), 5);
        // The input is untouched.
        assert_eq!(d.partitions.len(), 4);
    }

    #[test]
    fn issue_carries_queue_index() {
        let d = gap_disk();
        let ok_op = resize_c(&d, d.partitions[2].size - GIB);
        let bad = create(&d, d.partitions[0].offset, GIB, FsKind::Ntfs);
        let v = validate(&[d], &[ok_op, bad], &ctx());
        assert!(v
            .issues
            .iter()
            .any(|i| i.op_index == 1 && i.rule == Rule::Overlap));
    }
}
