//! Application state shared by the window code (and, later, the operation dialogs).

use std::path::PathBuf;

use diskcore::{
    plan_boot_repair, ApplyReport, Backend, BackendError, BootPlanError, BootRepairPlan, Context,
    Disk, DiskId, Op, Partition,
};

use crate::ui::lists::fmt_size;

/// Shown in the boot-repair preview when the Windows volume is encrypted but unlocked.
pub const BITLOCKER_SUSPEND_WARNING: &str =
    "BitLocker protection will be suspended until the next \
     restart so that the repaired boot files are accepted.";

/// Why a boot repair cannot be planned, in words for the user.
pub fn boot_plan_error_text(e: BootPlanError) -> &'static str {
    match e {
        BootPlanError::NoWindows => "That is not a Windows installation.",
        BootPlanError::NoEsp => {
            "This disk has no EFI System Partition, so there is nowhere to put the boot files."
        }
        BootPlanError::MultipleEsp => {
            "This disk has more than one EFI System Partition. Remove the extra one first so the \
             boot files go to the right place."
        }
        BootPlanError::WindowsLocked => {
            "The Windows volume is locked by BitLocker. Unlock it first (BitLocker, then Unlock)."
        }
    }
}

/// `C: "Windows", 237 GB on Disk 0 (Model)`: how a partition is named in the boot dialogs.
pub fn describe_partition(disk: &Disk, p: &Partition) -> String {
    let name = match p.letter {
        Some(l) => format!("{l}:"),
        None => format!("partition {}", p.number),
    };
    let label = if p.label.trim().is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", p.label.trim())
    };
    format!(
        "{name}{label}, {} on Disk {} ({})",
        fmt_size(p.size),
        disk.number,
        disk.model.trim()
    )
}
/// Shown when every queued operation ran.
pub const APPLY_SUCCESS: &str = "All operations completed successfully.";

/// The simulation disk as the UI knows it. The attached VHDX itself lives on the storage
/// thread, which owns the backend that is bound to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimInfo {
    pub path: PathBuf,
    pub disk_number: u32,
}

pub struct AppState {
    pub disks: Vec<Disk>,
    /// Selected `(disk, offset)`. The offset is that of a partition or of a free extent;
    /// [`AppState::selected_partition`] only resolves partitions.
    pub selected: Option<(DiskId, u64)>,
    /// Pending operations. Change them only through the methods below, which keep
    /// `system_ok` in step.
    queue: Vec<Op>,
    /// Parallel to `queue`: the user approved this operation touching an ESP or MSR.
    system_ok: Vec<bool>,
    pub simulation: Option<SimInfo>,
    /// Validation context. `simulation` follows [`AppState::set_simulation`]; use
    /// [`AppState::run_ctx`] for the context to validate or apply with.
    pub ctx: Context,
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            disks: Vec::new(),
            selected: None,
            queue: Vec::new(),
            system_ok: Vec::new(),
            simulation: None,
            ctx: Context {
                simulation: false,
                allow_system_partitions: false,
            },
        }
    }

    pub fn queue(&self) -> &[Op] {
        &self.queue
    }

    /// Adds an operation. `allow_system` records that the user approved deleting an ESP or
    /// MSR with it (a second, explicit prompt).
    pub fn push_op(&mut self, op: Op, allow_system: bool) {
        self.queue.push(op);
        self.system_ok.push(allow_system);
    }

    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.system_ok.clear();
    }

    /// The context to validate and apply the queue with. diskcore takes one context for the
    /// whole queue, so `allow_system_partitions` is on only while a queued operation was
    /// approved for it; every ESP/MSR delete enters the queue through that approval.
    pub fn run_ctx(&self) -> Context {
        Context {
            simulation: self.ctx.simulation,
            allow_system_partitions: self.system_ok.iter().any(|&a| a),
        }
    }

    /// Enters (`Some`) or leaves (`None`) simulation mode. The queue refers to disks of the
    /// old mode, so it is dropped.
    pub fn set_simulation(&mut self, sim: Option<SimInfo>) {
        self.ctx.simulation = sim.is_some();
        self.simulation = sim;
        self.clear_queue();
    }

    /// Updates the queue after `apply` ran it. Success empties it. After a failure the
    /// completed operations are dropped (re-running them would be wrong) and the failed one
    /// and those not run stay, for the user to check and apply again or clear.
    pub fn finish_apply(&mut self, report: &ApplyReport) {
        if report.failed.is_none() {
            self.clear_queue();
            return;
        }
        let mut i = 0;
        self.queue.retain(|_| {
            let keep = !report.completed.contains(&i);
            i += 1;
            keep
        });
        let mut i = 0;
        self.system_ok.retain(|_| {
            let keep = !report.completed.contains(&i);
            i += 1;
            keep
        });
    }

    /// Re-reads the disks from `b`. The selection is kept only if its disk is still there.
    #[allow(dead_code)] // the window reads on a storage thread and calls `apply_snapshot`
    pub fn refresh(&mut self, b: &mut dyn Backend) -> Result<(), BackendError> {
        self.apply_snapshot(b.snapshot()?);
        Ok(())
    }

    /// Replaces the disk list with `disks`, dropping a selection whose disk has gone.
    pub fn apply_snapshot(&mut self, disks: Vec<Disk>) {
        self.disks = disks;
        if let Some((id, _)) = &self.selected {
            if !self.disks.iter().any(|d| &d.id == id) {
                self.selected = None;
            }
        }
    }

    /// The boot-repair plan for the Windows install at `offset` on `disk`.
    pub fn boot_repair_plan(
        &self,
        disk: &DiskId,
        offset: u64,
    ) -> Result<BootRepairPlan, BootPlanError> {
        let d = self
            .disks
            .iter()
            .find(|d| &d.id == disk)
            .ok_or(BootPlanError::NoWindows)?;
        plan_boot_repair(d, offset)
    }

    /// The preview shown before a repair: the Windows volume, the ESP and what happens to
    /// BitLocker. Returns the plan with it so the caller runs exactly what was shown.
    pub fn boot_repair_preview_for(
        &self,
        disk: &DiskId,
        offset: u64,
    ) -> Result<(BootRepairPlan, String), BootPlanError> {
        let plan = self.boot_repair_plan(disk, offset)?;
        let d = self
            .disks
            .iter()
            .find(|d| d.id == plan.disk)
            .ok_or(BootPlanError::NoWindows)?;
        let part = |off: u64| d.partitions.iter().find(|p| p.offset == off);
        let (Some(win), Some(esp)) = (part(plan.windows_offset), part(plan.esp_offset)) else {
            return Err(BootPlanError::NoWindows);
        };
        let bitlocker = if plan.suspend_bitlocker {
            BITLOCKER_SUSPEND_WARNING
        } else {
            "BitLocker: the Windows volume is not encrypted, so there is nothing to suspend."
        };
        let text = format!(
            "Windows volume: {}\n\
             EFI System Partition: {}, size {}\n\
             The boot files on the EFI System Partition are rewritten from this Windows \
             installation. The EFI System Partition gets a temporary drive letter while this \
             runs; it is removed afterwards.\n\
             {bitlocker}",
            describe_partition(d, win),
            describe_partition(d, esp),
            fmt_size(esp.size),
        );
        Ok((plan, text))
    }

    /// [`AppState::boot_repair_preview_for`] for the Windows install at `offset`, on the first
    /// disk that has one there. The window passes the disk too (offsets repeat across
    /// disks); this is the offset-only form.
    #[allow(dead_code)] // the window uses `boot_repair_preview_for`
    pub fn boot_repair_preview(&self, offset: u64) -> Result<String, BootPlanError> {
        let disk = self
            .disks
            .iter()
            .find(|d| {
                d.partitions
                    .iter()
                    .any(|p| p.offset == offset && p.has_windows)
            })
            .ok_or(BootPlanError::NoWindows)?;
        self.boot_repair_preview_for(&disk.id, offset)
            .map(|(_, text)| text)
    }

    /// The selected partition and its disk (`None` for no selection or a free extent).
    pub fn selected_partition(&self) -> Option<(&Disk, &Partition)> {
        let (id, offset) = self.selected.as_ref()?;
        let disk = self.disks.iter().find(|d| &d.id == id)?;
        let part = disk.partitions.iter().find(|p| p.offset == *offset)?;
        Some((disk, part))
    }
}

/// The message shown after `apply`: [`APPLY_SUCCESS`], or which operation failed and why.
pub fn apply_summary(report: &ApplyReport) -> String {
    match &report.failed {
        None => APPLY_SUCCESS.to_string(),
        Some((index, error)) => format!(
            "Operation {} failed: {error}. {} completed; {} not run.",
            index + 1,
            report.completed.len(),
            report.not_run.len()
        ),
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::lists::fmt_size;
    use diskcore::{apply, disk_fixture, FakeBackend, FsKind, PartKind, Protection, MIB};

    fn shrink_and_create(d: &Disk) -> Vec<Op> {
        let c = &d.partitions[2];
        vec![
            Op::Resize {
                disk: d.id.clone(),
                offset: c.offset,
                new_size: c.size - 1024 * MIB,
            },
            Op::Create {
                disk: d.id.clone(),
                offset: c.offset + c.size - 1024 * MIB,
                size: 512 * MIB,
                fs: FsKind::Ntfs,
                label: "Data".into(),
            },
        ]
    }

    #[test]
    fn queue_cleared_after_successful_apply() {
        let d = disk_fixture();
        let mut b = FakeBackend::new(vec![d.clone()]);
        let mut app = AppState::new();
        app.set_simulation(Some(SimInfo {
            path: "x.vhdx".into(),
            disk_number: 0,
        }));
        app.apply_snapshot(b.snapshot().unwrap());
        for op in shrink_and_create(&d) {
            app.push_op(op, false);
        }
        assert_eq!(app.queue().len(), 2);

        let report = apply(&mut b, app.queue(), &app.run_ctx());
        assert!(report.failed.is_none(), "{report:?}");
        assert_eq!(
            apply_summary(&report),
            "All operations completed successfully."
        );
        app.finish_apply(&report);
        assert!(app.queue().is_empty());
        assert!(!app.run_ctx().allow_system_partitions);
    }

    #[test]
    fn failed_apply_keeps_only_unfinished_operations() {
        let d = disk_fixture();
        let mut b = FakeBackend::new(vec![d.clone()]);
        b.fail_at = Some(1);
        let mut app = AppState::new();
        app.apply_snapshot(b.snapshot().unwrap());
        let ops = shrink_and_create(&d);
        for op in &ops {
            app.push_op(op.clone(), false);
        }
        let report = apply(&mut b, app.queue(), &app.run_ctx());
        assert_eq!(report.completed, vec![0]);
        assert_eq!(
            apply_summary(&report),
            "Operation 2 failed: injected failure at 1. 1 completed; 0 not run."
        );
        app.finish_apply(&report);
        assert_eq!(app.queue(), &ops[1..]);
    }

    #[test]
    fn system_partition_allowance_is_per_queued_operation() {
        let d = disk_fixture();
        let del = |i: usize| Op::Delete {
            disk: d.id.clone(),
            offset: d.partitions[i].offset,
        };
        let mut app = AppState::new();
        assert!(!app.run_ctx().allow_system_partitions);
        app.push_op(del(3), false);
        assert!(!app.run_ctx().allow_system_partitions);
        app.push_op(del(0), true);
        assert!(app.run_ctx().allow_system_partitions);
        // Dropping the approved operation (it completed) withdraws the allowance.
        let report = ApplyReport {
            completed: vec![1],
            failed: Some((0, "x".into())),
            not_run: vec![],
        };
        app.finish_apply(&report);
        assert_eq!(app.queue().len(), 1);
        assert!(!app.run_ctx().allow_system_partitions);
        app.clear_queue();
        assert!(app.queue().is_empty());
    }

    #[test]
    fn simulation_switch_follows_ctx_and_drops_queue() {
        let d = disk_fixture();
        let mut app = AppState::new();
        app.push_op(
            Op::Delete {
                disk: d.id.clone(),
                offset: d.partitions[3].offset,
            },
            false,
        );
        app.set_simulation(Some(SimInfo {
            path: "x.vhdx".into(),
            disk_number: 5,
        }));
        assert!(app.ctx.simulation && app.run_ctx().simulation);
        assert!(app.queue().is_empty());
        app.set_simulation(None);
        assert!(!app.run_ctx().simulation && app.simulation.is_none());
    }

    #[test]
    fn failure_text_uses_the_agreed_copy() {
        let r = ApplyReport {
            completed: vec![0, 1],
            failed: Some((2, "disk is busy".into())),
            not_run: vec![3, 4, 5],
        };
        assert_eq!(
            apply_summary(&r),
            "Operation 3 failed: disk is busy. 2 completed; 3 not run."
        );
    }

    #[test]
    fn refresh_loads_disks_and_resolves_selection() {
        let d = disk_fixture();
        let c_off = d.partitions[2].offset;
        let mut b = FakeBackend::new(vec![d.clone()]);
        let mut app = AppState::new();
        app.refresh(&mut b).unwrap();
        assert_eq!(app.disks.len(), 1);
        assert!(app.selected_partition().is_none());

        app.selected = Some((d.id.clone(), c_off));
        let (disk, part) = app.selected_partition().unwrap();
        assert_eq!(disk.id, d.id);
        assert_eq!(part.letter, Some('C'));

        // A free-space offset selects no partition.
        app.selected = Some((d.id.clone(), 1));
        assert!(app.selected_partition().is_none());
    }

    fn windows_offset(d: &Disk) -> u64 {
        d.partitions.iter().find(|p| p.has_windows).unwrap().offset
    }

    fn with_protection(p: Protection) -> (AppState, u64, u64) {
        let mut d = disk_fixture();
        d.partitions
            .iter_mut()
            .find(|p| p.has_windows)
            .unwrap()
            .protection = p;
        let esp = d
            .partitions
            .iter()
            .find(|p| p.kind == PartKind::Esp)
            .unwrap()
            .size;
        let off = windows_offset(&d);
        let mut app = AppState::new();
        app.apply_snapshot(vec![d]);
        (app, off, esp)
    }

    #[test]
    fn boot_preview_names_esp_size_and_the_suspend_warning() {
        let (app, off, esp) = with_protection(Protection::Unlocked);
        let text = app.boot_repair_preview(off).unwrap();
        assert!(text.contains(&fmt_size(esp)), "{text}");
        assert!(text.contains("suspended until the next restart"), "{text}");
        assert!(text.contains(BITLOCKER_SUSPEND_WARNING), "{text}");
    }

    #[test]
    fn boot_preview_without_bitlocker_has_no_suspend_phrase() {
        let (app, off, esp) = with_protection(Protection::None);
        let text = app.boot_repair_preview(off).unwrap();
        assert!(text.contains(&fmt_size(esp)), "{text}");
        assert!(!text.contains("suspended until the next restart"), "{text}");
    }

    #[test]
    fn boot_preview_reports_plan_errors() {
        let (app, _, _) = with_protection(Protection::None);
        assert_eq!(
            app.boot_repair_preview(12345),
            Err(BootPlanError::NoWindows)
        );
        let (app, off, _) = with_protection(Protection::Locked);
        assert_eq!(
            app.boot_repair_preview(off),
            Err(BootPlanError::WindowsLocked)
        );
    }

    #[test]
    fn refresh_drops_selection_of_vanished_disk() {
        let d = disk_fixture();
        let mut b = FakeBackend::new(vec![d.clone()]);
        let mut app = AppState::new();
        app.selected = Some((DiskId("gone".into()), 0));
        app.refresh(&mut b).unwrap();
        assert!(app.selected.is_none());
        app.selected = Some((d.id.clone(), d.partitions[0].offset));
        app.refresh(&mut b).unwrap();
        assert!(app.selected.is_some());
    }
}
