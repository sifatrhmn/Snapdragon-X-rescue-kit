pub mod align;
pub mod apply;
pub mod boot;
pub mod model;
pub mod ops;
pub mod validate;

pub use align::{align_down, align_up, free_extents, Extent};
#[cfg(any(test, feature = "fixtures"))]
pub use apply::FakeBackend;
pub use apply::{apply, ApplyReport, Backend, BackendError};
pub use boot::{plan_boot_repair, windows_installs, BootPlanError, BootRepairPlan};
#[cfg(any(test, feature = "fixtures"))]
pub use model::disk_fixture;
pub use model::{
    part_kind_from_gpt, BusType, Disk, DiskId, FsKind, PartKind, Partition, PartitionStyle,
    Protection, FAT32_MAX, MIB,
};
pub use ops::Op;
pub use validate::{project, validate, Context, Issue, Rule, Severity, Validation};
