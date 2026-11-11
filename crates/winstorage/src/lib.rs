//! Windows storage helpers: logging, child-process runner, WMI access, disk enumeration,
//! partition operations, VHDX simulation disks and rescue-USB preparation.

pub mod bitlocker;
pub mod bootrepair;
pub mod log;
pub mod process;
pub mod storage;
pub mod usbprep;
pub mod vhd;
pub mod wmi;

pub use log::{default_log_dir, is_winpe, Logger};
pub use process::{run_tool, ToolOutput};
pub use storage::{enumerate, StorageBackend, StorageError};
pub use usbprep::prepare_usb;
pub use vhd::{create_vhdx, AttachedVhd, Template};
pub use wmi::{Value, Wmi, WmiError, WmiObject};
