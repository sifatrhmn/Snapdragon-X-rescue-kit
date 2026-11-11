//! Text for the partition list and disk map, plus the ListView / ListBox helpers.
//!
//! Everything above the `Win32 helpers` marker is pure and unit-tested.

use diskcore::{
    free_extents, Disk, DiskId, Extent, FsKind, Op, PartKind, Partition, PartitionStyle,
    Protection, MIB,
};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const COLUMNS: [(&str, i32, bool); 8] = [
    // (title, width at 96 dpi, right-aligned)
    ("#", 36, true),
    ("Letter", 56, false),
    ("Label", 150, false),
    ("File system", 90, false),
    ("Size", 90, true),
    ("Used", 90, true),
    ("Flags", 240, false),
    ("Offset", 90, true),
];

/// Text drawn for a locked BitLocker volume (text only, no glyph).
pub const LOCKED_TEXT: &str = "[locked]";

/// One row of the partition ListView.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Partition offset, or the free extent's offset.
    pub offset: u64,
    pub is_free: bool,
    pub cells: [String; 8],
}

/// Windows-style size text (1024-based units, as Disk Management shows them).
pub fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;
    let (div, unit) = if bytes >= TB {
        (TB, "TB")
    } else if bytes >= GB {
        (GB, "GB")
    } else if bytes >= MB {
        (MB, "MB")
    } else if bytes >= KB {
        (KB, "KB")
    } else {
        return format!("{bytes} B");
    };
    let v = bytes as f64 / div as f64;
    if v >= 100.0 || (v - v.round()).abs() < 0.05 {
        format!("{} {unit}", v.round() as u64)
    } else {
        format!("{v:.1} {unit}")
    }
}

pub fn fs_name(fs: &FsKind) -> String {
    match fs {
        FsKind::Ntfs => "NTFS".into(),
        FsKind::Fat32 => "FAT32".into(),
        FsKind::ExFat => "exFAT".into(),
        FsKind::Refs => "ReFS".into(),
        FsKind::Raw => "RAW".into(),
        FsKind::Other(s) => s.clone(),
    }
}

fn kind_name(kind: PartKind) -> Option<&'static str> {
    match kind {
        PartKind::Esp => Some("ESP"),
        PartKind::Msr => Some("MSR"),
        PartKind::Recovery => Some("Recovery"),
        PartKind::Basic | PartKind::Other => None,
    }
}

/// Disk-level protections, repeated in the Flags column of every row of that disk.
pub fn disk_flags(disk: &Disk) -> Vec<&'static str> {
    let mut f = Vec::new();
    if disk.is_running_os {
        f.push("running OS");
    }
    if disk.is_boot_media {
        f.push("boot media");
    }
    if disk.is_simulation {
        f.push("simulation");
    }
    f
}

/// Flags column: partition kind, BitLocker state and the disk-level protections.
pub fn partition_flags(disk: &Disk, p: &Partition) -> String {
    let mut f: Vec<&str> = Vec::new();
    if let Some(k) = kind_name(p.kind) {
        f.push(k);
    }
    if p.has_windows {
        f.push("Windows");
    }
    match p.protection {
        Protection::Locked => f.push(LOCKED_TEXT),
        Protection::Unlocked => f.push("BitLocker (unlocked)"),
        Protection::None => {}
    }
    f.extend(disk_flags(disk));
    f.join(", ")
}

pub fn partition_row(disk: &Disk, p: &Partition) -> Row {
    Row {
        offset: p.offset,
        is_free: false,
        cells: [
            p.number.to_string(),
            p.letter.map(|c| format!("{c}:")).unwrap_or_default(),
            p.label.clone(),
            fs_name(&p.fs),
            fmt_size(p.size),
            p.used.map(fmt_size).unwrap_or_else(|| "-".into()),
            partition_flags(disk, p),
            fmt_size(p.offset),
        ],
    }
}

pub fn free_row(disk: &Disk, e: &Extent) -> Row {
    Row {
        offset: e.offset,
        is_free: true,
        cells: [
            String::new(),
            String::new(),
            "(free space)".into(),
            String::new(),
            fmt_size(e.size),
            String::new(),
            disk_flags(disk).join(", "),
            fmt_size(e.offset),
        ],
    }
}

/// Free gaps smaller than this are alignment slivers between partitions; the views hide them.
pub const MIN_FREE_SHOWN: u64 = 8 * MIB;

/// Free extents worth showing (display only; `diskcore::free_extents` stays the truth).
pub fn visible_free_extents(disk: &Disk) -> Vec<Extent> {
    free_extents(disk)
        .into_iter()
        .filter(|e| e.size >= MIN_FREE_SHOWN)
        .collect()
}

/// Partitions and free extents of `disk`, in disk order.
pub fn rows_for_disk(disk: &Disk) -> Vec<Row> {
    let mut rows: Vec<Row> = disk
        .partitions
        .iter()
        .map(|p| partition_row(disk, p))
        .collect();
    rows.extend(visible_free_extents(disk).iter().map(|e| free_row(disk, e)));
    rows.sort_by_key(|r| r.offset);
    rows
}

fn bus_name(disk: &Disk) -> String {
    use diskcore::BusType::*;
    match disk.bus {
        Nvme => "NVMe".into(),
        Ufs => "UFS".into(),
        Usb => "USB".into(),
        Sata => "SATA".into(),
        Virtual => "Virtual".into(),
        Other(n) => format!("bus {n}"),
    }
}

/// Header line above each disk's bar.
pub fn disk_header(disk: &Disk) -> String {
    let style = match disk.style {
        PartitionStyle::Gpt => "GPT",
        PartitionStyle::Mbr => "MBR",
        PartitionStyle::Raw => "uninitialised",
    };
    let mut s = format!(
        "Disk {}  {}  {}  {}  {}",
        disk.number,
        bus_name(disk),
        disk.model.trim(),
        fmt_size(disk.size),
        style
    );
    if disk.is_boot_media {
        s.push_str("  (boot media \u{2014} protected)");
    }
    if disk.is_running_os {
        s.push_str("  (running OS \u{2014} read-only)");
    }
    if disk.is_simulation {
        s.push_str("  (simulation disk)");
    }
    s
}

/// Text drawn inside a partition's bar segment.
pub fn partition_segment_label(p: &Partition) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(l) = p.letter {
        parts.push(format!("{l}:"));
    } else if let Some(k) = kind_name(p.kind) {
        parts.push(k.into());
    } else if !p.label.is_empty() {
        parts.push(p.label.clone());
    } else {
        parts.push(format!("Partition {}", p.number));
    }
    if p.letter.is_some() && p.kind == PartKind::Basic && !matches!(p.fs, FsKind::Raw) {
        parts.push(fs_name(&p.fs));
    }
    parts.push(fmt_size(p.size));
    if p.protection == Protection::Locked {
        parts.push(LOCKED_TEXT.into());
    }
    parts.join(" ")
}

pub fn free_segment_label(size: u64) -> String {
    format!("Free {}", fmt_size(size))
}

fn find_part<'a>(disks: &'a [Disk], id: &DiskId, offset: u64) -> Option<&'a Partition> {
    disks
        .iter()
        .find(|d| &d.id == id)?
        .partitions
        .iter()
        .find(|p| p.offset == offset)
}

fn part_name(disks: &[Disk], id: &DiskId, offset: u64) -> String {
    match find_part(disks, id, offset) {
        Some(p) => match p.letter {
            Some(l) => format!("{l}:"),
            None => format!("partition {}", p.number),
        },
        None => format!("partition at {}", fmt_size(offset)),
    }
}

/// One line for the pending-operations list (without the leading number).
pub fn describe_op(op: &Op, disks: &[Disk]) -> String {
    match op {
        Op::Create {
            disk,
            offset,
            size,
            fs,
            label,
        } => {
            let n = disks.iter().find(|d| &d.id == disk).map(|d| d.number);
            format!(
                "Create {} partition {} \"{}\" at {} on disk {}",
                fs_name(fs),
                fmt_size(*size),
                label,
                fmt_size(*offset),
                n.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
            )
        }
        Op::Delete { disk, offset } => format!("Delete {}", part_name(disks, disk, *offset)),
        Op::Format {
            disk,
            offset,
            fs,
            label,
        } => format!(
            "Format {} as {} \"{}\"",
            part_name(disks, disk, *offset),
            fs_name(fs),
            label
        ),
        Op::Resize {
            disk,
            offset,
            new_size,
        } => {
            let name = part_name(disks, disk, *offset);
            match find_part(disks, disk, *offset) {
                Some(p) if *new_size < p.size => format!(
                    "Shrink {name} by {}",
                    fmt_size((p.size - new_size) / MIB * MIB)
                ),
                Some(p) => format!(
                    "Extend {name} by {}",
                    fmt_size((new_size - p.size) / MIB * MIB)
                ),
                None => format!("Resize {name} to {}", fmt_size(*new_size)),
            }
        }
    }
}

// ---------------------------------------------------------------- Win32 helpers

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, msg, WPARAM(w), LPARAM(l)).0 }
}

/// Creates the report-mode partition ListView with its eight columns.
pub fn create_list_view(parent: HWND, id: isize, hinst: HINSTANCE, dpi: u32) -> HWND {
    let style = WS_CHILD
        | WS_VISIBLE
        | WS_BORDER
        | WS_TABSTOP
        | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WC_LISTVIEWW,
            PCWSTR::null(),
            style,
            0,
            0,
            10,
            10,
            parent,
            HMENU(id as *mut _),
            hinst,
            None,
        )
    }
    .unwrap_or_default();
    send(
        hwnd,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        0,
        (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
    );
    for (i, (title, width, right)) in COLUMNS.iter().enumerate() {
        let mut text = wide(title);
        let col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
            fmt: if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT },
            cx: width * dpi as i32 / 96,
            pszText: PWSTR(text.as_mut_ptr()),
            ..Default::default()
        };
        send(
            hwnd,
            LVM_INSERTCOLUMNW,
            i,
            &col as *const LVCOLUMNW as isize,
        );
    }
    hwnd
}

/// Replaces the list contents with `rows`.
pub fn fill_list_view(list: HWND, rows: &[Row]) {
    send(list, LVM_DELETEALLITEMS, 0, 0);
    for (i, row) in rows.iter().enumerate() {
        for (c, cell) in row.cells.iter().enumerate() {
            let mut text = wide(cell);
            let item = LVITEMW {
                mask: LVIF_TEXT,
                iItem: i as i32,
                iSubItem: c as i32,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            if c == 0 {
                send(list, LVM_INSERTITEMW, 0, &item as *const LVITEMW as isize);
            } else {
                send(list, LVM_SETITEMTEXTW, i, &item as *const LVITEMW as isize);
            }
        }
    }
}

/// Selects (and scrolls to) row `idx`, or clears the selection for `None`.
pub fn select_list_row(list: HWND, idx: Option<usize>) {
    let clear = LVITEMW {
        stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
        state: LIST_VIEW_ITEM_STATE_FLAGS(0),
        ..Default::default()
    };
    send(
        list,
        LVM_SETITEMSTATE,
        usize::MAX,
        &clear as *const LVITEMW as isize,
    );
    if let Some(i) = idx {
        let sel = LVITEMW {
            stateMask: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
            state: LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0),
            ..Default::default()
        };
        send(list, LVM_SETITEMSTATE, i, &sel as *const LVITEMW as isize);
        send(list, LVM_ENSUREVISIBLE, i, 0);
    }
}

/// Replaces the pending-operations ListBox contents.
pub fn fill_queue_box(lb: HWND, ops: &[Op], disks: &[Disk]) {
    send(lb, LB_RESETCONTENT, 0, 0);
    for (i, op) in ops.iter().enumerate() {
        let text = wide(&format!("{}. {}", i + 1, describe_op(op, disks)));
        send(lb, LB_ADDSTRING, 0, text.as_ptr() as isize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskcore::disk_fixture;

    #[test]
    fn sizes_read_like_disk_management() {
        assert_eq!(fmt_size(260 * MIB), "260 MB");
        assert_eq!(fmt_size(16 * MIB), "16 MB");
        assert_eq!(fmt_size(1024 * MIB), "1 GB");
        assert_eq!(fmt_size(476 * 1024 * MIB), "476 GB");
        assert_eq!(fmt_size(1536 * MIB), "1.5 GB");
        assert_eq!(fmt_size(512), "512 B");
    }

    #[test]
    fn locked_partition_uses_text_not_glyphs() {
        let d = disk_fixture();
        let mut p = d.partitions[2].clone();
        p.protection = Protection::Locked;
        assert!(partition_flags(&d, &p).contains("[locked]"));
        let label = partition_segment_label(&p);
        assert!(label.contains("[locked]"));
        assert!(label.is_ascii());
    }

    #[test]
    fn running_os_disk_shows_running_os_flag() {
        let mut d = disk_fixture();
        d.is_running_os = true;
        d.partitions.pop(); // leaves free space after C:
        let rows = rows_for_disk(&d);
        assert!(rows.iter().all(|r| r.cells[6].contains("running OS")));
        assert!(rows.iter().any(|r| r.is_free));
    }

    #[test]
    fn boot_media_header_is_labelled() {
        let mut d = disk_fixture();
        d.is_boot_media = true;
        assert!(disk_header(&d).contains("(boot media \u{2014} protected)"));
    }

    #[test]
    fn alignment_slivers_are_hidden() {
        let mut d = disk_fixture();
        // Leave exactly 1 MiB between ESP and MSR.
        d.partitions[1].offset += MIB;
        d.partitions[1].size -= MIB;
        assert!(free_extents(&d).iter().any(|e| e.size == MIB));
        assert!(rows_for_disk(&d).iter().all(|r| !r.is_free));
    }

    #[test]
    fn rows_are_in_disk_order() {
        let d = disk_fixture();
        let rows = rows_for_disk(&d);
        assert!(rows.windows(2).all(|w| w[0].offset < w[1].offset));
        assert_eq!(rows[0].cells[0], "1");
        assert_eq!(rows[2].cells[1], "C:");
    }

    #[test]
    fn describes_resize_as_shrink_or_extend() {
        let d = disk_fixture();
        let c = &d.partitions[2];
        let shrink = Op::Resize {
            disk: d.id.clone(),
            offset: c.offset,
            new_size: c.size - 40 * 1024 * MIB,
        };
        assert_eq!(
            describe_op(&shrink, std::slice::from_ref(&d)),
            "Shrink C: by 40 GB"
        );
    }
}
