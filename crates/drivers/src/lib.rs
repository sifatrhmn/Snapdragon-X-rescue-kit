//! Driver-package auditing for the WinPE ARM64 image builder: PE machine
//! detection, INF parsing and package selection. Pure std + serde.

pub mod inf;
pub mod pe;
pub mod select;

pub use inf::{parse_inf, read_inf, DriverVer, InfInfo};
pub use pe::{pe_machine, pe_machine_of, Machine};
pub use select::{
    scan, select, write_selection, DropReason, Package, Verdict, CRITICAL_CLASSES, DENIED_CLASSES,
    DENIED_INFS,
};
