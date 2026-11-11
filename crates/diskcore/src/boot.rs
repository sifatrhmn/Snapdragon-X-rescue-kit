//! Boot-repair planning: decides, from a disk model alone, how to repair UEFI boot files.

use crate::model::{Disk, DiskId, PartKind, Protection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootRepairPlan {
    pub disk: DiskId,
    pub windows_offset: u64,
    pub esp_offset: u64,
    pub suspend_bitlocker: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPlanError {
    NoWindows,
    NoEsp,
    WindowsLocked,
    MultipleEsp,
}

/// Plans a UEFI boot-file repair for the Windows install at `windows_offset` on `disk`.
///
/// Checks run in order: the offset must name a Windows partition, the disk must have
/// exactly one ESP, and the Windows volume must not be locked. BitLocker is suspended
/// only when the volume is protected but currently unlocked.
pub fn plan_boot_repair(disk: &Disk, windows_offset: u64) -> Result<BootRepairPlan, BootPlanError> {
    let windows = disk
        .partitions
        .iter()
        .find(|p| p.offset == windows_offset && p.has_windows)
        .ok_or(BootPlanError::NoWindows)?;

    let mut esps = disk.partitions.iter().filter(|p| p.kind == PartKind::Esp);
    let esp = esps.next().ok_or(BootPlanError::NoEsp)?;
    if esps.next().is_some() {
        return Err(BootPlanError::MultipleEsp);
    }

    let suspend_bitlocker = match windows.protection {
        Protection::None => false,
        Protection::Unlocked => true,
        Protection::Locked => return Err(BootPlanError::WindowsLocked),
    };

    Ok(BootRepairPlan {
        disk: disk.id.clone(),
        windows_offset,
        esp_offset: esp.offset,
        suspend_bitlocker,
    })
}

/// Every partition across all disks marked as holding a Windows install.
pub fn windows_installs(disks: &[Disk]) -> Vec<(DiskId, u64)> {
    disks
        .iter()
        .flat_map(|d| {
            d.partitions
                .iter()
                .filter(|p| p.has_windows)
                .map(|p| (d.id.clone(), p.offset))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{disk_fixture, Protection};

    fn windows_offset(d: &Disk) -> u64 {
        d.partitions.iter().find(|p| p.has_windows).unwrap().offset
    }

    fn set_windows_protection(d: &mut Disk, p: Protection) {
        let c = d.partitions.iter_mut().find(|p| p.has_windows).unwrap();
        c.has_windows = true;
        c.protection = p;
    }

    #[test]
    fn unlocked_windows_suspends_bitlocker() {
        let mut d = disk_fixture();
        set_windows_protection(&mut d, Protection::Unlocked);
        let esp = d
            .partitions
            .iter()
            .find(|p| p.kind == PartKind::Esp)
            .unwrap()
            .offset;
        let plan = plan_boot_repair(&d, windows_offset(&d)).unwrap();
        assert!(plan.suspend_bitlocker);
        assert_eq!(plan.esp_offset, esp);
        assert_eq!(plan.windows_offset, windows_offset(&d));
        assert_eq!(plan.disk, d.id);
    }

    #[test]
    fn unprotected_windows_does_not_suspend() {
        let mut d = disk_fixture();
        set_windows_protection(&mut d, Protection::None);
        let plan = plan_boot_repair(&d, windows_offset(&d)).unwrap();
        assert!(!plan.suspend_bitlocker);
    }

    #[test]
    fn locked_windows_is_error() {
        let mut d = disk_fixture();
        set_windows_protection(&mut d, Protection::Locked);
        assert_eq!(
            plan_boot_repair(&d, windows_offset(&d)),
            Err(BootPlanError::WindowsLocked)
        );
    }

    #[test]
    fn missing_esp_is_error() {
        let mut d = disk_fixture();
        d.partitions.retain(|p| p.kind != PartKind::Esp);
        assert_eq!(
            plan_boot_repair(&d, windows_offset(&d)),
            Err(BootPlanError::NoEsp)
        );
    }

    #[test]
    fn two_esps_is_error() {
        let mut d = disk_fixture();
        assert_eq!(d.partitions[0].kind, PartKind::Esp);
        let mut extra = d.partitions[0].clone();
        extra.number = 9;
        extra.offset = d.size - 2 * crate::model::MIB;
        d.partitions.push(extra);
        assert_eq!(
            plan_boot_repair(&d, windows_offset(&d)),
            Err(BootPlanError::MultipleEsp)
        );
    }

    #[test]
    fn unknown_or_non_windows_offset_is_no_windows() {
        let d = disk_fixture();
        assert_eq!(plan_boot_repair(&d, 12345), Err(BootPlanError::NoWindows));
        assert_eq!(d.partitions[0].kind, PartKind::Esp);
        let esp = d.partitions[0].offset;
        assert_eq!(plan_boot_repair(&d, esp), Err(BootPlanError::NoWindows));
    }

    #[test]
    fn windows_installs_lists_all_disks() {
        let a = disk_fixture();
        let mut b = disk_fixture();
        b.id = DiskId("other".into());
        for p in &mut b.partitions {
            p.has_windows = false;
        }
        let mut c = disk_fixture();
        c.id = DiskId("third".into());
        let got = windows_installs(&[a.clone(), b, c.clone()]);
        assert_eq!(
            got,
            vec![
                (a.id.clone(), windows_offset(&a)),
                (c.id.clone(), windows_offset(&c))
            ]
        );
    }

    #[test]
    fn windows_installs_empty_slice_is_empty() {
        assert_eq!(windows_installs(&[]), vec![]);
    }

    #[test]
    fn windows_installs_single_disk_single_install() {
        let d = disk_fixture();
        assert_eq!(
            windows_installs(std::slice::from_ref(&d)),
            vec![(d.id.clone(), windows_offset(&d))]
        );
    }

    #[test]
    fn windows_installs_two_installs_on_one_disk_in_order() {
        let mut d = disk_fixture();
        let first = windows_offset(&d);
        // Mark the Recovery partition (last by offset) as a second Windows install.
        let last = d.partitions.last_mut().unwrap();
        last.has_windows = true;
        let second = last.offset;
        assert!(first < second);
        assert_eq!(
            windows_installs(std::slice::from_ref(&d)),
            vec![(d.id.clone(), first), (d.id.clone(), second)]
        );
    }
}
