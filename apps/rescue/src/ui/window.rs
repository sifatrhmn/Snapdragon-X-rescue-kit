//! The main window: menu, disk map, partition list, pending operations and log.
//!
//! The UI runs on the UI thread. State lives in thread-locals so that window procedures
//! can reach it; no `RefCell` borrow is ever held across a `SendMessage` or a dialog.
//!
//! Everything that touches a disk runs on one dedicated storage thread, which owns the
//! `!Send` WMI backend (and the attached simulation VHDX). The UI sends it a [`Job`] and
//! gets an [`Outcome`] back through [`OUTCOMES`] and a `WM_APP_DONE` post. While a job is
//! in flight `Ui::busy` is set and every command that would start another is refused.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{mpsc, Mutex};

use diskcore::{
    free_extents, validate, windows_installs, ApplyReport, Backend, BackendError, BootRepairPlan,
    Context, Disk, DiskId, Extent, Op, Partition, Rule, Severity,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;
use winstorage::bootrepair::run_boot_repair;
use winstorage::{
    bitlocker, create_vhdx, default_log_dir, is_winpe, AttachedVhd, Logger, StorageBackend,
    Template,
};

use super::dialogs::{self, Env};
use super::diskmap::{self, WM_MAP_CLICK};
use super::lists::{self, Row};
use crate::app::{apply_summary, AppState, SimInfo};

const TITLE: &str = "WoA Rescue Kit";
const STORAGE_THREAD: &str = "storage";

const WM_APP_LOG: u32 = WM_APP + 1;
const WM_APP_REFRESH: u32 = WM_APP + 3;
/// The storage thread finished a job; its [`Outcome`] is waiting in [`OUTCOMES`].
const WM_APP_DONE: u32 = WM_APP + 4;

const IDM_REFRESH: usize = 1001;
const IDM_EXIT: usize = 1002;
const IDM_SIM_OPEN: usize = 1003;
const IDM_SIM_CLOSE: usize = 1004;
const IDM_CREATE: usize = 1101;
const IDM_DELETE: usize = 1102;
const IDM_FORMAT: usize = 1103;
const IDM_RESIZE: usize = 1104;
const IDM_UNLOCK: usize = 1201;
const IDM_REPAIR_BOOT: usize = 1301;
const IDM_CMD: usize = 1401;
const IDM_NOTEPAD: usize = 1402;
const IDC_MAP: isize = 2001;
const IDC_LIST: isize = 2002;
const IDC_PENDING_LABEL: isize = 2003;
const IDC_QUEUE: isize = 2004;
const IDC_VALIDATE: isize = 2005;
const IDC_APPLY: isize = 2006;
const IDC_CLEAR: isize = 2007;
const IDC_LOG: isize = 2008;
const IDC_BANNER: isize = 2009;

/// Text of the banner shown while a simulation disk is open.
const SIMULATION_BANNER: &str = "Simulation mode \u{2014} changes affect the virtual disk only";

/// Lines logged from any thread wait here until the UI thread appends them to the edit box.
static LOG_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
/// Results of storage-thread jobs, handed to the UI thread (see `WM_APP_DONE`).
static OUTCOMES: Mutex<VecDeque<Outcome>> = Mutex::new(VecDeque::new());

#[derive(Clone, Copy)]
struct Handles {
    main: HWND,
    banner: HWND,
    map: HWND,
    list: HWND,
    pending_label: HWND,
    queue: HWND,
    validate: HWND,
    apply: HWND,
    clear: HWND,
    log: HWND,
}

/// Work for the storage thread.
enum Job {
    Refresh,
    /// Run the queue. `ctx` is the context the UI validated with.
    Apply {
        queue: Vec<Op>,
        ctx: Context,
    },
    /// Create and attach a VHDX, then rebuild the backend bound to it.
    EnterSimulation {
        path: PathBuf,
        template: Template,
        size: u64,
    },
    /// Detach the VHDX and rebuild the real backend.
    LeaveSimulation,
    /// Unlock a BitLocker volume with a recovery key. The key is a secret: nothing here
    /// prints or logs this job.
    Unlock {
        volume_path: String,
        key: String,
    },
    /// Repair the boot files for `plan`; `disks` is the snapshot the plan was made from.
    BootRepair {
        plan: BootRepairPlan,
        disks: Vec<Disk>,
    },
}

/// Written by hand, not derived: `Job::Unlock` carries the recovery key, which must never
/// reach a log or a panic message. A derive here would print it.
impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Job::Refresh => write!(f, "Job::Refresh"),
            Job::Apply { queue, ctx } => f
                .debug_struct("Job::Apply")
                .field("queue", queue)
                .field("ctx", ctx)
                .finish(),
            Job::EnterSimulation {
                path,
                template,
                size,
            } => f
                .debug_struct("Job::EnterSimulation")
                .field("path", path)
                .field("template", &format_args!("{template:?}"))
                .field("size", size)
                .finish(),
            Job::LeaveSimulation => write!(f, "Job::LeaveSimulation"),
            Job::Unlock { volume_path, .. } => f
                .debug_struct("Job::Unlock")
                .field("volume", volume_path)
                .field("key", &format_args!("<redacted>"))
                .finish(),
            Job::BootRepair { plan, disks } => f
                .debug_struct("Job::BootRepair")
                .field("plan", plan)
                .field("disks", &disks.len())
                .finish(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JobKind {
    Refresh,
    Apply,
    EnterSimulation,
    LeaveSimulation,
    Unlock,
    BootRepair,
}

impl Job {
    fn kind(&self) -> JobKind {
        match self {
            Job::Refresh => JobKind::Refresh,
            Job::Apply { .. } => JobKind::Apply,
            Job::EnterSimulation { .. } => JobKind::EnterSimulation,
            Job::LeaveSimulation => JobKind::LeaveSimulation,
            Job::Unlock { .. } => JobKind::Unlock,
            Job::BootRepair { .. } => JobKind::BootRepair,
        }
    }
}

/// What a job produced, as plain data (nothing `!Send` crosses back).
enum Outcome {
    Disks(Result<Vec<Disk>, String>),
    Applied(ApplyReport),
    /// The apply job panicked: it is not known which operations ran.
    ApplyAborted(String),
    SimulationEntered(Result<SimInfo, String>),
    /// `Some` carries a problem to report; the storage thread is in real mode either way.
    SimulationLeft(Option<String>),
    /// The unlock finished (or failed, with the message to show).
    Unlocked(Result<(), String>),
    /// The boot repair finished (or failed, with the message to show).
    BootRepaired(Result<(), String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    /// Reading the disks.
    Reading,
    /// Changing disks: Apply, or creating or detaching the simulation disk. Closing the
    /// window is refused meanwhile.
    Changing,
}

struct Ui {
    app: AppState,
    /// Jobs for the storage thread, which owns the `!Send` WMI backend.
    jobs: mpsc::Sender<Job>,
    busy: Busy,
    /// After the next disk read, show this disk (a new simulation disk).
    focus_disk: Option<u32>,
    log: Logger,
    /// The disk whose partitions the list shows.
    current: Option<DiskId>,
    rows: Vec<Row>,
    /// Shown in the disk map when there are no disks.
    message: String,
}

const SS_CENTER: u32 = 0x0001;
const SS_CENTERIMAGE: u32 = 0x0200;
const BANNER_BG: u32 = 0x00B3_ECFF; // amber (BGR)

thread_local! {
    static BANNER_BRUSH: Cell<isize> = const { Cell::new(0) };
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    static HANDLES: Cell<Option<Handles>> = const { Cell::new(None) };
    static DPI: Cell<u32> = const { Cell::new(96) };
    static FONTS: Cell<(isize, isize)> = const { Cell::new((0, 0)) };
    /// Set while the code (not the user) changes the list selection.
    static SYNCING: Cell<bool> = const { Cell::new(false) };
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn handles() -> Option<Handles> {
    HANDLES.with(|h| h.get())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn px(v: i32) -> i32 {
    v * DPI.with(|d| d.get()) as i32 / 96
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, msg, WPARAM(w), LPARAM(l)).0 }
}

// ---------------------------------------------------------------- logging

fn install_sink(log: &Logger) {
    log.set_sink(Box::new(|line| {
        if let Ok(mut q) = LOG_QUEUE.lock() {
            q.push(line.to_string());
        }
        let h = MAIN_HWND.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(HWND(h as *mut c_void), WM_APP_LOG, WPARAM(0), LPARAM(0));
            }
        }
    }));
}

fn open_logger() -> Logger {
    Logger::open(&default_log_dir(), "rescue")
        .or_else(|_| Logger::open(&std::env::temp_dir().join("WoARescue"), "rescue"))
        .expect("cannot open a log file")
}

fn drain_log(edit: HWND) {
    let lines: Vec<String> = LOG_QUEUE
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();
    if lines.is_empty() {
        return;
    }
    let len = unsafe { GetWindowTextLengthW(edit) } as usize;
    if len > 400_000 {
        // Keep the box bounded: drop the oldest quarter.
        send(edit, EM_SETSEL, 0, 100_000);
        send(edit, EM_REPLACESEL, 0, wide("").as_ptr() as isize);
    }
    let len = unsafe { GetWindowTextLengthW(edit) } as usize;
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    send(edit, EM_SETSEL, len, len as isize);
    send(edit, EM_REPLACESEL, 0, wide(&text).as_ptr() as isize);
    send(edit, EM_SCROLLCARET, 0, 0);
}

// ---------------------------------------------------------------- fonts and layout

fn make_fonts(dpi: u32) -> (HFONT, HFONT) {
    unsafe {
        let stock = GetStockObject(DEFAULT_GUI_FONT);
        let mut lf = LOGFONTW::default();
        GetObjectW(
            stock,
            std::mem::size_of::<LOGFONTW>() as i32,
            Some(&mut lf as *mut LOGFONTW as *mut c_void),
        );
        // DEFAULT_GUI_FONT is sized for 96 dpi; scale it for this monitor.
        lf.lfHeight = (lf.lfHeight * dpi as i32 + if lf.lfHeight < 0 { -48 } else { 48 }) / 96;
        let regular = CreateFontIndirectW(&lf);
        lf.lfWeight = 700;
        let bold = CreateFontIndirectW(&lf);
        (regular, bold)
    }
}

fn apply_fonts(h: &Handles, dpi: u32) {
    let (regular, bold) = make_fonts(dpi);
    for c in [
        h.list,
        h.pending_label,
        h.queue,
        h.validate,
        h.apply,
        h.clear,
        h.log,
    ] {
        send(c, WM_SETFONT, regular.0 as usize, 1);
    }
    send(h.banner, WM_SETFONT, bold.0 as usize, 1);
    diskmap::set_look(h.map, regular, bold, dpi);
    let (old_r, old_b) = FONTS.with(|f| f.replace((regular.0 as isize, bold.0 as isize)));
    for old in [old_r, old_b] {
        if old != 0 {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(old as *mut c_void));
            }
        }
    }
}

fn layout() {
    let Some(h) = handles() else { return };
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(h.main, &mut rc);
    }
    let (cw, ch) = (rc.right, rc.bottom);
    let m = px(8);
    let gap = px(6);
    let width = (cw - 2 * m).max(50);
    let (n_disks, simulation) =
        with_ui(|u| (u.app.disks.len(), u.app.simulation.is_some())).unwrap_or((0, false));
    let rows = n_disks.clamp(1, 4) as i32;
    let map_h = rows * px(diskmap::ROW_H) + 2 * px(2);
    let banner_h = if simulation { px(26) } else { 0 };
    let btn_h = px(26);
    let btn_w = px(84);

    let rest = (ch - map_h - banner_h - btn_h - 4 * m - 2 * gap).max(px(150));
    let list_h = rest * 40 / 100;
    let queue_h = rest * 22 / 100;
    let log_h = rest - list_h - queue_h;

    let mut y = m;
    let mv = |c: HWND, x: i32, y: i32, w: i32, h: i32| unsafe {
        let _ = MoveWindow(c, x, y, w, h, true);
    };
    if banner_h > 0 {
        mv(h.banner, m, y, width, banner_h);
        y += banner_h + gap;
    }
    mv(h.map, m, y, width, map_h);
    y += map_h + gap;
    mv(h.list, m, y, width, list_h);
    y += list_h + gap;
    mv(
        h.pending_label,
        m,
        y + px(4),
        width - 3 * btn_w - 3 * gap,
        btn_h,
    );
    let bx = m + width - 3 * btn_w - 2 * gap;
    mv(h.validate, bx, y, btn_w, btn_h);
    mv(h.apply, bx + btn_w + gap, y, btn_w, btn_h);
    mv(h.clear, bx + 2 * (btn_w + gap), y, btn_w, btn_h);
    y += btn_h + gap;
    mv(h.queue, m, y, width, queue_h);
    y += queue_h + gap;
    mv(h.log, m, y, width, log_h.max(px(40)));
}

// ---------------------------------------------------------------- views

/// Pushes the application state into every control.
fn refresh_views() {
    let Some(h) = handles() else { return };
    let Some((disks, selected, current, message, queue, simulation, idle)) = with_ui(|u| {
        (
            u.app.disks.clone(),
            u.app.selected.clone(),
            u.current.clone(),
            u.message.clone(),
            u.app.queue().to_vec(),
            u.app.simulation.is_some(),
            u.busy == Busy::Idle,
        )
    }) else {
        return;
    };
    diskmap::set_model(h.map, &disks, selected.as_ref(), current.as_ref(), &message);
    rebuild_list();
    lists::fill_queue_box(h.queue, &queue, &disks);
    let label = format!("Pending operations ({})", queue.len());
    unsafe {
        let _ = SetWindowTextW(h.pending_label, PCWSTR(wide(&label).as_ptr()));
        let runnable = idle && !queue.is_empty();
        let _ = EnableWindow(h.clear, runnable);
        let _ = EnableWindow(h.validate, runnable);
        let _ = EnableWindow(h.apply, runnable);
        let _ = ShowWindow(h.banner, if simulation { SW_SHOW } else { SW_HIDE });
        let title = if simulation {
            format!("{TITLE} \u{2014} [Simulation mode]")
        } else {
            TITLE.to_string()
        };
        let _ = SetWindowTextW(h.main, PCWSTR(wide(&title).as_ptr()));
    }
    update_menu();
    layout();
}

/// Rebuilds the partition list for the current disk and restores the selection.
fn rebuild_list() {
    let Some(h) = handles() else { return };
    let Some((rows, sel_idx)) = with_ui(|u| {
        let disk = u
            .current
            .as_ref()
            .and_then(|id| u.app.disks.iter().find(|d| &d.id == id));
        u.rows = disk.map(lists::rows_for_disk).unwrap_or_default();
        let idx = match (&u.app.selected, &u.current) {
            (Some((sid, off)), Some(cur)) if sid == cur => {
                u.rows.iter().position(|r| r.offset == *off)
            }
            _ => None,
        };
        (u.rows.clone(), idx)
    }) else {
        return;
    };
    SYNCING.with(|s| s.set(true));
    lists::fill_list_view(h.list, &rows);
    lists::select_list_row(h.list, sel_idx);
    SYNCING.with(|s| s.set(false));
}

fn push_map_model() {
    let Some(h) = handles() else { return };
    if let Some((disks, selected, current, message)) = with_ui(|u| {
        (
            u.app.disks.clone(),
            u.app.selected.clone(),
            u.current.clone(),
            u.message.clone(),
        )
    }) {
        diskmap::set_model(h.map, &disks, selected.as_ref(), current.as_ref(), &message);
    }
}

/// Selection made in the disk map: `offset` is a partition / free extent, `None` the header.
fn select_from_map(disk_index: usize, offset: isize) {
    let Some(h) = handles() else { return };
    let changed = with_ui(|u| {
        let id = u.app.disks.get(disk_index)?.id.clone();
        let changed = u.current.as_ref() != Some(&id);
        u.app.selected = (offset >= 0).then(|| (id.clone(), offset as u64));
        u.current = Some(id);
        Some(changed)
    })
    .flatten();
    let Some(changed) = changed else { return };
    if changed {
        rebuild_list();
    } else {
        let idx = with_ui(|u| {
            let off = u.app.selected.as_ref().map(|s| s.1)?;
            u.rows.iter().position(|r| r.offset == off)
        })
        .flatten();
        SYNCING.with(|s| s.set(true));
        lists::select_list_row(h.list, idx);
        SYNCING.with(|s| s.set(false));
    }
    push_map_model();
}

/// Selection made in the ListView.
fn select_from_list(row: usize) {
    if SYNCING.with(|s| s.get()) {
        return;
    }
    with_ui(|u| {
        if let (Some(r), Some(cur)) = (u.rows.get(row), u.current.clone()) {
            u.app.selected = Some((cur, r.offset));
        }
    });
    push_map_model();
}

// ---------------------------------------------------------------- storage thread

/// What the storage thread owns. The backend is `!Send`, so it is built and used on this
/// thread only. Its mode follows `vhd`: with a simulation disk attached it is the
/// simulation backend for exactly that disk number, otherwise the real one. A backend that
/// has to be rebuilt (first use, a failed job) is rebuilt in the *same* mode, never in the
/// other one.
struct Worker {
    log_path: PathBuf,
    backend: Option<StorageBackend>,
    /// The attached simulation disk. Kept alive here; dropping it detaches the disk.
    vhd: Option<AttachedVhd>,
}

const STORAGE_UNAVAILABLE: &str = "Storage is not available. Run Rescue Kit as administrator.";

/// Logs each operation as `apply` reaches it, so the log box shows progress live.
struct Progress<'a> {
    inner: &'a mut StorageBackend,
    log: Logger,
    total: usize,
    started: usize,
    disks: Vec<Disk>,
}

impl Backend for Progress<'_> {
    fn snapshot(&mut self) -> Result<Vec<Disk>, BackendError> {
        let d = self.inner.snapshot()?;
        self.disks = d.clone();
        Ok(d)
    }

    fn execute(&mut self, op: &Op) -> Result<(), BackendError> {
        self.started += 1;
        self.log.info(&format!(
            "operation {} of {}: {}",
            self.started,
            self.total,
            op.describe(&self.disks)
        ));
        self.inner.execute(op)
    }
}

fn failed_report(n: usize, message: String) -> ApplyReport {
    ApplyReport {
        completed: Vec::new(),
        failed: Some((0, message)),
        not_run: (1..n).collect(),
    }
}

impl Worker {
    /// A logger on the shared log file that also feeds the log box.
    fn logger(&self) -> Result<Logger, String> {
        let l = Logger::append(&self.log_path).map_err(|e| e.to_string())?;
        install_sink(&l);
        Ok(l)
    }

    fn backend(&mut self) -> Result<&mut StorageBackend, String> {
        if self.backend.is_none() {
            let log = self.logger().map_err(|e| {
                // No log: there is nowhere to report the detail, so say it in the message.
                format!("{STORAGE_UNAVAILABLE} ({e})")
            })?;
            let built = match &self.vhd {
                Some(v) => StorageBackend::new_simulation(log, vec![v.disk_number]),
                None => StorageBackend::new(log),
            };
            match built {
                Ok(b) => self.backend = Some(b),
                Err(e) => {
                    if let Ok(l) = self.logger() {
                        l.error(&format!("cannot open storage: {e}"));
                    }
                    return Err(STORAGE_UNAVAILABLE.to_string());
                }
            }
        }
        match self.backend.as_mut() {
            Some(b) => Ok(b),
            None => Err(STORAGE_UNAVAILABLE.to_string()),
        }
    }

    fn run(&mut self, job: Job) -> Outcome {
        match job {
            Job::Refresh => Outcome::Disks(match self.backend() {
                Ok(b) => b
                    .snapshot()
                    .map_err(|e| format!("Cannot read the disks: {e}")),
                Err(m) => Err(m),
            }),
            Job::Apply { queue, ctx } => Outcome::Applied(self.apply(&queue, &ctx)),
            Job::EnterSimulation {
                path,
                template,
                size,
            } => Outcome::SimulationEntered(self.enter_simulation(&path, template, size)),
            Job::LeaveSimulation => Outcome::SimulationLeft(self.leave_simulation()),
            Job::Unlock { volume_path, key } => Outcome::Unlocked(self.unlock(&volume_path, &key)),
            Job::BootRepair { plan, disks } => {
                Outcome::BootRepaired(self.boot_repair(&plan, &disks))
            }
        }
    }

    /// BitLocker unlock. `bitlocker::unlock` opens its own WMI connection, which is why this
    /// runs here and not on the UI thread. Neither the log nor the message holds the key.
    fn unlock(&mut self, volume_path: &str, key: &str) -> Result<(), String> {
        let log = self
            .logger()
            .map_err(|e| format!("Cannot open the log: {e}"))?;
        bitlocker::unlock(volume_path, key, &log)
            .map_err(|e| format!("Could not unlock the volume: {e}"))
    }

    /// Boot repair (BitLocker suspend, temporary drive letters, `bcdboot`). Same thread
    /// rule as [`Worker::unlock`].
    fn boot_repair(&mut self, plan: &BootRepairPlan, disks: &[Disk]) -> Result<(), String> {
        let log = self
            .logger()
            .map_err(|e| format!("Cannot open the log: {e}"))?;
        log.info("repairing boot files");
        run_boot_repair(plan, disks, &log).map_err(|e| format!("Boot repair failed: {e}"))
    }

    fn apply(&mut self, queue: &[Op], ctx: &Context) -> ApplyReport {
        let n = queue.len();
        // The UI and this thread must agree on the mode. A UI that believes it is in
        // simulation while this thread is on the real backend (or the reverse) is refused
        // before anything runs.
        if ctx.simulation != self.vhd.is_some() {
            return failed_report(
                n,
                "the simulation state changed; refresh and queue the operations again".into(),
            );
        }
        let log = match self.logger() {
            Ok(l) => l,
            Err(e) => return failed_report(n, format!("cannot open the log: {e}")),
        };
        let backend = match self.backend() {
            Ok(b) => b,
            Err(m) => return failed_report(n, m),
        };
        log.info(&format!("applying {n} operation(s)"));
        let mut progress = Progress {
            inner: backend,
            log,
            total: n,
            started: 0,
            disks: Vec::new(),
        };
        diskcore::apply(&mut progress, queue, ctx)
    }

    /// Creates the VHDX, then rebuilds the backend as the simulation backend for its disk
    /// number. The old backend stays untouched unless everything before the swap worked.
    fn enter_simulation(
        &mut self,
        path: &std::path::Path,
        template: Template,
        size: u64,
    ) -> Result<SimInfo, String> {
        if self.vhd.is_some() {
            return Err("A simulation disk is already open.".into());
        }
        let log = self
            .logger()
            .map_err(|e| format!("Cannot open the log: {e}"))?;
        let vhd = create_vhdx(path, size, template, &log)
            .map_err(|e| format!("Cannot create the simulation disk: {e}"))?;
        // From here an early return drops `vhd`, which detaches the disk again.
        let log = self
            .logger()
            .map_err(|e| format!("Cannot open the log: {e}"))?;
        let backend = StorageBackend::new_simulation(log, vec![vhd.disk_number])
            .map_err(|e| format!("Cannot open the simulation disk: {e}"))?;
        let info = SimInfo {
            path: vhd.path.clone(),
            disk_number: vhd.disk_number,
        };
        self.backend = Some(backend);
        self.vhd = Some(vhd);
        Ok(info)
    }

    /// Drops the simulation backend, detaches the VHDX (the file stays) and goes back to the
    /// real backend. The real backend is rebuilt on the next job if this cannot.
    fn leave_simulation(&mut self) -> Option<String> {
        self.backend = None;
        let vhd = self.vhd.take();
        let note = vhd.as_ref().map(|v| {
            format!(
                "The simulation disk file is still at {}. You can delete it.",
                v.path.display()
            )
        });
        drop(vhd);
        match self.backend() {
            Ok(_) => note,
            Err(m) => Some(m),
        }
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown error".into())
}

/// The outcome to report when a job panicked, so the UI is never left waiting.
fn panic_outcome(kind: JobKind, text: &str) -> Outcome {
    let m = format!("Rescue Kit hit an internal error: {text}");
    match kind {
        JobKind::Refresh => Outcome::Disks(Err(m)),
        JobKind::Apply => Outcome::ApplyAborted(m),
        JobKind::EnterSimulation => Outcome::SimulationEntered(Err(m)),
        JobKind::LeaveSimulation => Outcome::SimulationLeft(Some(m)),
        JobKind::Unlock => Outcome::Unlocked(Err(m)),
        JobKind::BootRepair => Outcome::BootRepaired(Err(m)),
    }
}

fn post_to_main(msg: u32) {
    let h = MAIN_HWND.load(Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut c_void), msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn push_outcome(o: Outcome) {
    OUTCOMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(o);
    post_to_main(WM_APP_DONE);
}

/// Storage thread body. Every job ends in exactly one posted [`Outcome`], even if it
/// panics, so the UI's busy state can never stay stuck. Reading `GetSupportedSize` for
/// large NTFS volumes can take tens of seconds, which is why disks are read here and not on
/// the UI thread. The loop ends when the UI drops its sender; the simulation disk (if any)
/// is detached when the worker is dropped.
fn storage_thread(rx: mpsc::Receiver<Job>, log_path: PathBuf) {
    let mut worker = Worker {
        log_path,
        backend: None,
        vhd: None,
    };
    for job in rx {
        let kind = job.kind();
        let outcome = match catch_unwind(AssertUnwindSafe(|| worker.run(job))) {
            Ok(o) => o,
            Err(p) => {
                // Whatever the backend was doing, do not trust it: rebuild it (same mode).
                worker.backend = None;
                panic_outcome(kind, &panic_text(&*p))
            }
        };
        push_outcome(outcome);
    }
}

// ---------------------------------------------------------------- jobs from the UI

/// Sends `job` to the storage thread and marks the UI busy. Returns false (and says why in
/// the log) if another job is running or the storage thread is gone.
fn submit(job: Job, busy: Busy) -> bool {
    let ok = with_ui(|u| {
        if u.busy != Busy::Idle {
            return false;
        }
        match u.jobs.send(job) {
            Ok(()) => {
                u.busy = busy;
                true
            }
            Err(_) => {
                u.log
                    .error("the storage thread is not running; restart Rescue Kit");
                u.message = "The storage thread stopped. Restart Rescue Kit.".into();
                false
            }
        }
    })
    .unwrap_or(false);
    refresh_views();
    ok
}

/// Asks the storage thread for a fresh disk list.
fn do_refresh() {
    if with_ui(|u| u.busy != Busy::Idle).unwrap_or(true) {
        return;
    }
    with_ui(|u| {
        u.log
            .info("reading disks (large NTFS volumes can take a while the first time)")
    });
    submit(Job::Refresh, Busy::Reading);
}

fn on_done() {
    let outcomes: Vec<Outcome> = OUTCOMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .collect();
    for o in outcomes {
        handle_outcome(o);
    }
}

fn main_hwnd() -> HWND {
    handles().map(|h| h.main).unwrap_or_default()
}

fn handle_outcome(o: Outcome) {
    match o {
        Outcome::Disks(result) => {
            with_ui(|u| {
                u.busy = Busy::Idle;
                match result {
                    Ok(disks) => {
                        u.app.apply_snapshot(disks);
                        u.message.clear();
                        u.log.info(&format!("found {} disk(s)", u.app.disks.len()));
                    }
                    Err(m) => {
                        u.log.error(&m);
                        u.message = m;
                    }
                }
                if let Some(n) = u.focus_disk.take() {
                    if let Some(d) = u.app.disks.iter().find(|d| d.number == n) {
                        u.current = Some(d.id.clone());
                        u.app.selected = None;
                    }
                }
                let keep = u
                    .current
                    .as_ref()
                    .is_some_and(|id| u.app.disks.iter().any(|d| &d.id == id));
                if !keep {
                    u.current = u.app.disks.first().map(|d| d.id.clone());
                }
            });
            refresh_views();
        }
        Outcome::Applied(report) => {
            let text = apply_summary(&report);
            let ok = report.failed.is_none();
            with_ui(|u| {
                u.busy = Busy::Idle;
                u.app.finish_apply(&report);
                if ok {
                    u.log.info(&text);
                } else {
                    u.log.error(&text);
                }
            });
            refresh_views();
            // The disks changed (fully or partly): read them again before the user acts.
            do_refresh();
            if ok {
                dialogs::info(main_hwnd(), &text);
            } else {
                dialogs::warn(main_hwnd(), &text);
            }
        }
        Outcome::ApplyAborted(m) => {
            let text = format!(
                "{m}. The pending operations were cleared because it is not known which of \
                 them ran. Check the disks before queueing anything again."
            );
            with_ui(|u| {
                u.busy = Busy::Idle;
                u.app.clear_queue();
                u.log.error(&text);
            });
            refresh_views();
            do_refresh();
            dialogs::warn(main_hwnd(), &text);
        }
        Outcome::SimulationEntered(result) => {
            match &result {
                Ok(info) => with_ui(|u| {
                    u.busy = Busy::Idle;
                    u.app.set_simulation(Some(info.clone()));
                    u.focus_disk = Some(info.disk_number);
                    u.log.info(&format!(
                        "simulation disk {} is open (disk {}). Changes affect the virtual \
                         disk only",
                        info.path.display(),
                        info.disk_number
                    ));
                }),
                Err(m) => with_ui(|u| {
                    u.busy = Busy::Idle;
                    u.log.error(m);
                }),
            };
            refresh_views();
            if let Err(m) = result {
                dialogs::warn(main_hwnd(), &m);
            } else {
                do_refresh();
            }
        }
        Outcome::SimulationLeft(note) => {
            with_ui(|u| {
                u.busy = Busy::Idle;
                u.app.set_simulation(None);
                u.log.info("simulation mode ended");
                if let Some(n) = &note {
                    u.log.warn(n);
                }
            });
            refresh_views();
            do_refresh();
        }
        Outcome::Unlocked(result) => {
            with_ui(|u| {
                u.busy = Busy::Idle;
                match &result {
                    Ok(()) => u.log.info("the BitLocker volume is unlocked"),
                    Err(m) => u.log.error(m),
                }
            });
            refresh_views();
            // The volume's state (and, once unlocked, its contents) changed: read it again.
            do_refresh();
            match result {
                Ok(()) => dialogs::info(main_hwnd(), "The volume is unlocked."),
                Err(m) => dialogs::warn(main_hwnd(), &m),
            }
        }
        Outcome::BootRepaired(result) => {
            with_ui(|u| {
                u.busy = Busy::Idle;
                match &result {
                    Ok(()) => u.log.info("boot files repaired"),
                    Err(m) => u.log.error(m),
                }
            });
            refresh_views();
            // Drive letters were added and removed; BitLocker may have been suspended.
            do_refresh();
            match result {
                Ok(()) => dialogs::info(main_hwnd(), "The boot files were repaired."),
                Err(m) => dialogs::warn(main_hwnd(), &m),
            }
        }
    }
}

// ---------------------------------------------------------------- commands

fn env() -> Option<Env> {
    let h = handles()?;
    let font = FONTS.with(|f| f.get()).0;
    Some(Env {
        owner: h.main,
        dpi: DPI.with(|d| d.get()),
        font: HFONT(font as *mut c_void),
    })
}

enum Selected {
    Free(Disk, Extent),
    Part(Disk, Partition),
}

fn selection() -> Option<Selected> {
    with_ui(|u| {
        if let Some((disk, p)) = u.app.selected_partition() {
            return Some(Selected::Part(disk.clone(), p.clone()));
        }
        let (id, off) = u.app.selected.as_ref()?;
        let disk = u.app.disks.iter().find(|d| &d.id == id)?;
        let ext = free_extents(disk).into_iter().find(|e| e.offset == *off)?;
        Some(Selected::Free(disk.clone(), ext))
    })
    .flatten()
}

fn is_idle() -> bool {
    with_ui(|u| u.busy == Busy::Idle).unwrap_or(false)
}

/// Adds `op` to the queue and logs it.
fn enqueue(op: Op, allow_system: bool) {
    with_ui(|u| {
        let text = lists::describe_op(&op, &u.app.disks);
        u.log.info(&format!("queued: {text}"));
        u.app.push_op(op, allow_system);
    });
    refresh_views();
}

fn cmd_create() {
    if !is_idle() {
        return;
    }
    let (Some(env), Some(Selected::Free(disk, ext))) = (env(), selection()) else {
        dialogs::info(main_hwnd(), "Select a free space segment first.");
        return;
    };
    if let Some(op) = dialogs::create_partition(env, &disk, ext) {
        enqueue(op, false);
    }
}

fn cmd_delete() {
    if !is_idle() {
        return;
    }
    let (Some(env), Some(Selected::Part(disk, p))) = (env(), selection()) else {
        dialogs::info(main_hwnd(), "Select a partition first.");
        return;
    };
    if let Some((op, allow_system)) = dialogs::delete_partition(env, &disk, &p) {
        enqueue(op, allow_system);
    }
}

fn cmd_format() {
    if !is_idle() {
        return;
    }
    let (Some(env), Some(Selected::Part(disk, p))) = (env(), selection()) else {
        dialogs::info(main_hwnd(), "Select a partition first.");
        return;
    };
    if let Some(op) = dialogs::format_partition(env, &disk, &p) {
        enqueue(op, false);
    }
}

fn cmd_resize() {
    if !is_idle() {
        return;
    }
    let (Some(env), Some(Selected::Part(disk, p))) = (env(), selection()) else {
        dialogs::info(main_hwnd(), "Select a partition first.");
        return;
    };
    if let Some(op) = dialogs::resize_partition(env, &disk, &p) {
        enqueue(op, false);
    }
}

/// Logs the validation of the queue. Returns `None` if the state is unavailable, otherwise
/// whether there is no error (warnings are allowed).
fn run_validation() -> Option<bool> {
    let (disks, queue, ctx) =
        with_ui(|u| (u.app.disks.clone(), u.app.queue().to_vec(), u.app.run_ctx()))?;
    let v = validate(&disks, &queue, &ctx);
    with_ui(|u| {
        for i in &v.issues {
            let line = format!(
                "operation {}: {} [{}]",
                i.op_index + 1,
                i.message,
                rule_name(i.rule)
            );
            match i.severity {
                Severity::Error => u.log.error(&line),
                Severity::Warning => u.log.warn(&line),
            }
        }
        if v.issues.is_empty() {
            u.log.info("validation passed: no problems found");
        } else if v.ok() {
            u.log.info("validation passed with warnings");
        } else {
            u.log
                .error("validation failed: nothing will run until these are fixed");
        }
    });
    Some(v.ok())
}

fn rule_name(r: Rule) -> String {
    format!("{r:?}")
}

fn cmd_validate() {
    if !is_idle() {
        return;
    }
    run_validation();
}

fn cmd_apply() {
    if !is_idle() {
        return;
    }
    // Validate first against what the window shows; `apply` validates again against a
    // fresh read on the storage thread before it changes anything.
    match run_validation() {
        Some(true) => {}
        Some(false) => {
            dialogs::warn(
                main_hwnd(),
                "The pending operations have problems, so nothing was changed. \
                 The log lists them.",
            );
            return;
        }
        None => return,
    }
    let Some((queue, ctx)) = with_ui(|u| (u.app.queue().to_vec(), u.app.run_ctx())) else {
        return;
    };
    if queue.is_empty() {
        return;
    }
    let n = queue.len();
    let question = format!(
        "Apply {n} pending operation{}? The changes are made to the disk straight away \
         and cannot be undone.",
        if n == 1 { "" } else { "s" }
    );
    if !dialogs::ask_yes_no(main_hwnd(), &question) {
        return;
    }
    submit(Job::Apply { queue, ctx }, Busy::Changing);
}

fn cmd_clear() {
    if !is_idle() {
        return;
    }
    with_ui(|u| {
        if !u.app.queue().is_empty() {
            u.app.clear_queue();
            u.log.info("pending operations cleared");
        }
    });
    refresh_views();
}

/// True if the queue is empty or the user agrees to drop it.
fn confirm_discard_queue(what: &str) -> bool {
    let n = with_ui(|u| u.app.queue().len()).unwrap_or(0);
    n == 0
        || dialogs::ask_yes_no(
            main_hwnd(),
            &format!("{what} discards the {n} pending operation(s). Continue?"),
        )
}

fn cmd_open_simulation() {
    if !is_idle() {
        return;
    }
    if with_ui(|u| u.app.simulation.is_some()).unwrap_or(false) {
        dialogs::info(
            main_hwnd(),
            "A simulation disk is already open. Close it first.",
        );
        return;
    }
    let Some(env) = env() else { return };
    if !confirm_discard_queue("Opening a simulation disk") {
        return;
    }
    let default = with_ui(|u| {
        dialogs::default_simulation_folder(&u.app.disks, is_winpe(), &std::env::temp_dir())
    })
    .unwrap_or_else(std::env::temp_dir);
    let Some(req) = dialogs::simulation_request(env, &default) else {
        return;
    };
    with_ui(|u| {
        u.log.info(&format!(
            "creating simulation disk {} ({} MiB); this takes a few seconds",
            req.path.display(),
            req.size / diskcore::MIB
        ))
    });
    submit(
        Job::EnterSimulation {
            path: req.path,
            template: req.template,
            size: req.size,
        },
        Busy::Changing,
    );
}

fn cmd_close_simulation() {
    if !is_idle() || !with_ui(|u| u.app.simulation.is_some()).unwrap_or(false) {
        return;
    }
    if !confirm_discard_queue("Closing the simulation disk") {
        return;
    }
    with_ui(|u| u.log.info("closing the simulation disk"));
    submit(Job::LeaveSimulation, Busy::Changing);
}

/// BitLocker, Unlock: asks for the recovery key of a locked volume and hands both to the
/// storage thread (the WMI call cannot run here). The key leaves this function only inside
/// the job.
fn cmd_unlock() {
    if !is_idle() {
        return;
    }
    let Some(env) = env() else { return };
    let Some((volumes, preselect)) = with_ui(|u| {
        let volumes = dialogs::locked_volumes(&u.app.disks);
        // Offer the selected partition first if it is one of them.
        let pre = u
            .app
            .selected_partition()
            .and_then(|(_, p)| p.volume_path.as_ref())
            .and_then(|v| volumes.iter().position(|x| &x.0 == v))
            .unwrap_or(0);
        (volumes, pre)
    }) else {
        return;
    };
    if volumes.is_empty() {
        dialogs::info(main_hwnd(), "No volume locked by BitLocker was found.");
        return;
    }
    let Some(req) = dialogs::unlock_request(env, &volumes, preselect) else {
        return;
    };
    with_ui(|u| u.log.info(&format!("unlocking volume {}", req.volume_path)));
    submit(
        Job::Unlock {
            volume_path: req.volume_path,
            key: req.key,
        },
        Busy::Changing,
    );
}

/// Boot, Repair boot files: pick an install, show the plan, confirm, then run it on the
/// storage thread.
fn cmd_repair_boot() {
    if !is_idle() {
        return;
    }
    let Some(env) = env() else { return };
    let Some(disks) = with_ui(|u| u.app.disks.clone()) else {
        return;
    };
    let installs: Vec<(DiskId, u64, String)> = windows_installs(&disks)
        .into_iter()
        .filter_map(|(id, off)| {
            let d = disks.iter().find(|d| d.id == id)?;
            let p = d.partitions.iter().find(|p| p.offset == off)?;
            Some((id, off, crate::app::describe_partition(d, p)))
        })
        .collect();
    if installs.is_empty() {
        dialogs::info(
            main_hwnd(),
            "No Windows installation was found on the disks.",
        );
        return;
    }
    let pick = if installs.len() == 1 {
        0
    } else {
        let items = installs.iter().map(|i| i.2.clone()).collect();
        let Some(i) = dialogs::choose_windows_install(env, items) else {
            return;
        };
        i.min(installs.len() - 1)
    };
    let (disk, offset, _) = &installs[pick];
    let Some(preview) = with_ui(|u| u.app.boot_repair_preview_for(disk, *offset)) else {
        return;
    };
    let (plan, text) = match preview {
        Ok(p) => p,
        Err(e) => {
            dialogs::warn(main_hwnd(), crate::app::boot_plan_error_text(e));
            return;
        }
    };
    if !dialogs::confirm_boot_repair(env, &text) {
        return;
    }
    with_ui(|u| u.log.info("boot repair confirmed"));
    submit(Job::BootRepair { plan, disks }, Busy::Changing);
}

fn cmd_tool(program: &str, own_console: bool) {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let mut c = std::process::Command::new(program);
    if own_console {
        c.creation_flags(CREATE_NEW_CONSOLE);
    }
    let result = c.spawn();
    with_ui(|u| match result {
        Ok(_) => u.log.info(&format!("started {program}")),
        Err(e) => u.log.error(&format!("cannot start {program}: {e}")),
    });
}

/// Enables or greys the menu items for the current selection and job state.
fn update_menu() {
    let Some(h) = handles() else { return };
    let (idle, simulation) =
        with_ui(|u| (u.busy == Busy::Idle, u.app.simulation.is_some())).unwrap_or((false, false));
    let (free, part) = match selection() {
        Some(Selected::Free(..)) => (true, false),
        Some(Selected::Part(..)) => (false, true),
        None => (false, false),
    };
    let set = |id: usize, on: bool| unsafe {
        let menu = GetMenu(h.main);
        let flag = if on { MF_ENABLED } else { MF_GRAYED };
        let _ = EnableMenuItem(menu, id as u32, MF_BYCOMMAND | flag);
    };
    set(IDM_REFRESH, idle);
    set(IDM_SIM_OPEN, idle && !simulation);
    set(IDM_SIM_CLOSE, idle && simulation);
    set(IDM_CREATE, idle && free);
    set(IDM_DELETE, idle && part);
    set(IDM_FORMAT, idle && part);
    set(IDM_RESIZE, idle && part);
    set(IDM_UNLOCK, idle);
    set(IDM_REPAIR_BOOT, idle);
}

/// Closing is refused while a job is changing disks; a plain disk read may be abandoned
/// (the process end waits for the storage thread, see `run`).
fn on_close(hwnd: HWND) {
    let changing = with_ui(|u| u.busy == Busy::Changing).unwrap_or(false);
    if changing {
        dialogs::warn(
            hwnd,
            "An operation is still running. Closing now could leave a disk half changed. \
             Wait for it to finish.",
        );
        return;
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

// ---------------------------------------------------------------- window procedure

fn child(
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    ex: WINDOW_EX_STYLE,
    parent: HWND,
    id: isize,
    hinst: HINSTANCE,
) -> HWND {
    unsafe {
        CreateWindowExW(
            ex,
            class,
            PCWSTR(wide(text).as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            10,
            10,
            parent,
            HMENU(id as *mut c_void),
            hinst,
            None,
        )
    }
    .unwrap_or_default()
}

fn on_create(hwnd: HWND) {
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    DPI.with(|d| d.set(dpi));

    let map = diskmap::create(hwnd, IDC_MAP, hinst);
    let list = lists::create_list_view(hwnd, IDC_LIST, hinst, dpi);
    let pending_label = child(
        w!("STATIC"),
        "Pending operations (0)",
        WINDOW_STYLE(0),
        WINDOW_EX_STYLE(0),
        hwnd,
        IDC_PENDING_LABEL,
        hinst,
    );
    let queue = child(
        w!("LISTBOX"),
        "",
        WS_BORDER
            | WS_VSCROLL
            | WS_TABSTOP
            | WINDOW_STYLE((LBS_NOINTEGRALHEIGHT | LBS_NOTIFY) as u32),
        WINDOW_EX_STYLE(0),
        hwnd,
        IDC_QUEUE,
        hinst,
    );
    let button = |text: &str, id: isize| {
        child(
            w!("BUTTON"),
            text,
            WS_TABSTOP,
            WINDOW_EX_STYLE(0),
            hwnd,
            id,
            hinst,
        )
    };
    let validate = button("Validate", IDC_VALIDATE);
    let apply = button("Apply", IDC_APPLY);
    let clear = button("Clear", IDC_CLEAR);
    let log = child(
        w!("EDIT"),
        "",
        WS_BORDER | WS_VSCROLL | WINDOW_STYLE((ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32),
        WINDOW_EX_STYLE(0),
        hwnd,
        IDC_LOG,
        hinst,
    );
    send(log, EM_SETLIMITTEXT, 1_000_000, 0);
    let banner = child(
        w!("STATIC"),
        SIMULATION_BANNER,
        WINDOW_STYLE(SS_CENTER | SS_CENTERIMAGE),
        WINDOW_EX_STYLE(0),
        hwnd,
        IDC_BANNER,
        hinst,
    );
    unsafe {
        let _ = ShowWindow(banner, SW_HIDE);
    }

    let h = Handles {
        main: hwnd,
        banner,
        map,
        list,
        pending_label,
        queue,
        validate,
        apply,
        clear,
        log,
    };
    HANDLES.with(|c| c.set(Some(h)));
    apply_fonts(&h, dpi);
    layout();
    drain_log(log);
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_REFRESH, WPARAM(0), LPARAM(0));
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            MAIN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
            on_create(hwnd);
            LRESULT(0)
        }
        WM_SIZE => {
            layout();
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = lp.0 as *mut MINMAXINFO;
            unsafe {
                (*mmi).ptMinTrackSize.x = px(720);
                (*mmi).ptMinTrackSize.y = px(560);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wp.0 & 0xFFFF) as u32;
            DPI.with(|d| d.set(dpi));
            let r = unsafe { &*(lp.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if let Some(h) = handles() {
                apply_fonts(&h, dpi);
                for (i, (_, width, _)) in lists::COLUMNS.iter().enumerate() {
                    send(
                        h.list,
                        LVM_SETCOLUMNWIDTH,
                        i,
                        (width * dpi as i32 / 96) as isize,
                    );
                }
            }
            layout();
            LRESULT(0)
        }
        WM_APP_LOG => {
            if let Some(h) = handles() {
                drain_log(h.log);
            }
            LRESULT(0)
        }
        WM_APP_REFRESH => {
            do_refresh();
            LRESULT(0)
        }
        WM_APP_DONE => {
            on_done();
            LRESULT(0)
        }
        WM_CLOSE => {
            on_close(hwnd);
            LRESULT(0)
        }
        WM_INITMENUPOPUP => {
            update_menu();
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            if handles().is_some_and(|h| h.banner.0 as isize == lp.0) {
                let hdc = HDC(wp.0 as *mut c_void);
                unsafe {
                    SetTextColor(hdc, COLORREF(0x0000_3A66)); // dark brown (BGR)
                    SetBkColor(hdc, COLORREF(BANNER_BG));
                }
                return LRESULT(BANNER_BRUSH.with(|b| {
                    if b.get() == 0 {
                        b.set(unsafe { CreateSolidBrush(COLORREF(BANNER_BG)) }.0 as isize);
                    }
                    b.get()
                }));
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_MAP_CLICK => {
            select_from_map(wp.0, lp.0);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let nm = unsafe { &*(lp.0 as *const NMHDR) };
            if nm.idFrom == IDC_LIST as usize && nm.code == LVN_ITEMCHANGED {
                let lv = unsafe { &*(lp.0 as *const NMLISTVIEW) };
                if lv.uChanged.0 & LVIF_STATE.0 != 0
                    && lv.uNewState & LVIS_SELECTED.0 != 0
                    && lv.iItem >= 0
                {
                    select_from_list(lv.iItem as usize);
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wp.0 & 0xFFFF, wp.0 >> 16) {
                (IDM_REFRESH, _) => do_refresh(),
                (IDM_EXIT, _) => unsafe {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                },
                (IDM_SIM_OPEN, _) => cmd_open_simulation(),
                (IDM_SIM_CLOSE, _) => cmd_close_simulation(),
                (IDM_CREATE, _) => cmd_create(),
                (IDM_DELETE, _) => cmd_delete(),
                (IDM_FORMAT, _) => cmd_format(),
                (IDM_RESIZE, _) => cmd_resize(),
                (IDM_UNLOCK, _) => cmd_unlock(),
                (IDM_REPAIR_BOOT, _) => cmd_repair_boot(),
                (IDM_CMD, _) => cmd_tool("cmd.exe", true),
                (IDM_NOTEPAD, _) => cmd_tool("notepad.exe", false),
                (id, n) if n == BN_CLICKED as usize && id == IDC_CLEAR as usize => cmd_clear(),
                (id, n) if n == BN_CLICKED as usize && id == IDC_VALIDATE as usize => {
                    cmd_validate()
                }
                (id, n) if n == BN_CLICKED as usize && id == IDC_APPLY as usize => cmd_apply(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            MAIN_HWND.store(0, Ordering::SeqCst);
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn build_menu() -> HMENU {
    unsafe {
        let bar = CreateMenu().unwrap_or_default();
        let add = |menu: HMENU, id: usize, text: &str| {
            let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide(text).as_ptr()));
        };
        let popup = |title: &str, menu: HMENU| {
            let _ = AppendMenuW(bar, MF_POPUP, menu.0 as usize, PCWSTR(wide(title).as_ptr()));
        };

        let disk = CreatePopupMenu().unwrap_or_default();
        add(disk, IDM_REFRESH, "&Refresh\tF5");
        let _ = AppendMenuW(disk, MF_SEPARATOR, 0, PCWSTR::null());
        add(disk, IDM_SIM_OPEN, "Open &simulation disk\u{2026}");
        add(disk, IDM_SIM_CLOSE, "&Close simulation disk");
        let _ = AppendMenuW(disk, MF_SEPARATOR, 0, PCWSTR::null());
        add(disk, IDM_EXIT, "E&xit");
        popup("&Disk", disk);

        let part = CreatePopupMenu().unwrap_or_default();
        add(part, IDM_CREATE, "&Create\u{2026}");
        add(part, IDM_DELETE, "&Delete\u{2026}");
        add(part, IDM_FORMAT, "&Format\u{2026}");
        add(part, IDM_RESIZE, "&Shrink or extend\u{2026}");
        popup("&Partition", part);

        let bl = CreatePopupMenu().unwrap_or_default();
        add(bl, IDM_UNLOCK, "&Unlock\u{2026}");
        popup("&BitLocker", bl);

        let boot = CreatePopupMenu().unwrap_or_default();
        add(boot, IDM_REPAIR_BOOT, "&Repair boot files\u{2026}");
        popup("B&oot", boot);

        let tools = CreatePopupMenu().unwrap_or_default();
        add(tools, IDM_CMD, "&Command prompt");
        add(tools, IDM_NOTEPAD, "&Notepad");
        popup("&Tools", tools);
        bar
    }
}

fn message_box(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide(text).as_ptr()),
            w!("WoA Rescue Kit"),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Creates the window and runs the message loop. Returns the process exit code.
pub fn run() -> i32 {
    std::panic::set_hook(Box::new(|info| {
        // A panic on the storage thread is caught there and reported through the log, so
        // the window stays usable; only a panic on the UI thread ends the program.
        if std::thread::current().name() == Some(STORAGE_THREAD) {
            return;
        }
        message_box(&format!("Rescue Kit stopped unexpectedly:\n{info}"));
    }));

    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();

    let log = open_logger();
    install_sink(&log);
    log.info(&format!(
        "{TITLE} started ({})",
        if is_winpe() { "WinPE" } else { "desktop" }
    ));
    let (jobs, rx) = mpsc::channel();
    let log_path = log.path().to_path_buf();
    UI.with(|u| {
        *u.borrow_mut() = Some(Ui {
            app: AppState::new(),
            jobs,
            busy: Busy::Idle,
            focus_disk: None,
            log,
            current: None,
            rows: Vec::new(),
            message: "Reading disks\u{2026}".into(),
        })
    });

    diskmap::register_class(hinst);
    let class = w!("WoARescueMain");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut c_void),
        lpszClassName: class,
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&wc) } == 0 {
        message_box("Cannot register the main window class.");
        return 1;
    }

    let scale = unsafe { GetDpiForSystem() }.max(96) as i32;
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(wide(TITLE).as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1000 * scale / 96,
            760 * scale / 96,
            None,
            build_menu(),
            hinst,
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        message_box("Cannot create the main window.");
        return 1;
    };

    // The worker reports back through `MAIN_HWND`, set by WM_CREATE above.
    let worker = std::thread::Builder::new()
        .name(STORAGE_THREAD.into())
        .spawn(move || storage_thread(rx, log_path));
    let Ok(worker) = worker else {
        message_box("Cannot start the storage thread.");
        return 1;
    };

    let accel = unsafe {
        CreateAcceleratorTableW(&[ACCEL {
            fVirt: FVIRTKEY,
            key: 0x74, // VK_F5
            cmd: IDM_REFRESH as u16,
        }])
    }
    .unwrap_or_default();

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);
        let _ = UpdateWindow(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if TranslateAcceleratorW(hwnd, accel, &msg) == 0 {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = DestroyAcceleratorTable(accel);
        // Dropping the state closes the job channel. Wait for the storage thread to finish
        // the job it is on (closing is refused while a job is changing disks, so this is at
        // most a disk read) and to detach the simulation disk, instead of exiting mid-job.
        UI.with(|u| u.borrow_mut().take());
        let _ = worker.join();
        msg.wParam.0 as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskcore::{FsKind, MIB};

    #[test]
    fn job_debug_never_shows_the_recovery_key() {
        let job = Job::Unlock {
            volume_path: r"\?Volume{abc}\".into(),
            key: "110011-220022-330033-440044-550055-660066-000011-000022".into(),
        };
        let text = format!("{job:?}");
        assert!(
            !text.contains("110011") && !text.contains("000022"),
            "{text}"
        );
        assert!(
            text.contains("<redacted>") && text.contains("Volume{abc}"),
            "{text}"
        );
        assert_eq!(format!("{:?}", Job::Refresh), "Job::Refresh");
    }

    fn disks_of(w: &mut Worker) -> Vec<Disk> {
        match w.run(Job::Refresh) {
            Outcome::Disks(Ok(d)) => d,
            Outcome::Disks(Err(e)) => panic!("refresh failed: {e}"),
            _ => panic!("unexpected outcome"),
        }
    }

    /// Runs the storage thread's job code for real: creates a VHDX, applies a shrink and a
    /// create on it, checks the safety refusals and leaves simulation mode. Needs
    /// administrator rights, so it is ignored by default:
    /// `cargo test -p rescue -- --ignored --nocapture`
    #[test]
    #[ignore = "needs administrator rights; attaches a temporary VHDX"]
    fn worker_simulation_round_trip() {
        let dir = tempfile_dir();
        let mut w = Worker {
            log_path: dir.join("worker.log"),
            backend: None,
            vhd: None,
        };
        let sim = w
            .enter_simulation(
                &dir.join("sim.vhdx"),
                Template::SnapdragonLike,
                3 * 1024 * MIB,
            )
            .expect("enter simulation");
        assert!(w.vhd.is_some() && w.backend.is_some());

        let disks = disks_of(&mut w);
        let d = disks
            .iter()
            .find(|d| d.number == sim.disk_number)
            .expect("simulation disk is listed");
        assert!(d.is_simulation);
        let win = d.partitions.iter().find(|p| p.label == "Windows").unwrap();
        let ops = vec![
            Op::Resize {
                disk: d.id.clone(),
                offset: win.offset,
                new_size: win.size - 1024 * MIB,
            },
            Op::Create {
                disk: d.id.clone(),
                offset: win.offset + win.size - 1024 * MIB,
                size: 512 * MIB,
                fs: FsKind::Ntfs,
                label: "Data".into(),
            },
        ];
        let sim_ctx = Context {
            simulation: true,
            allow_system_partitions: false,
        };

        // A UI that thinks it is not in simulation must be refused before anything runs.
        let real_ctx = Context {
            simulation: false,
            ..sim_ctx
        };
        let r = w.apply(&ops, &real_ctx);
        assert!(r.failed.is_some() && r.completed.is_empty(), "{r:?}");

        // Operations on a physical disk are flagged (validated only, never executed).
        if let Some(phys) = disks.iter().find(|d| d.is_running_os) {
            let del = Op::Delete {
                disk: phys.id.clone(),
                offset: phys.partitions[0].offset,
            };
            let v = validate(&disks, &[del], &sim_ctx);
            let rules: Vec<Rule> = v.issues.iter().map(|i| i.rule).collect();
            assert!(rules.contains(&Rule::RunningOs), "{rules:?}");
            assert!(rules.contains(&Rule::PhysicalInSimulation), "{rules:?}");
        }

        let report = w.apply(&ops, &sim_ctx);
        assert_eq!(report.failed, None, "{report:?}");
        assert_eq!(report.completed, vec![0, 1]);
        let after = disks_of(&mut w);
        let d = after.iter().find(|d| d.number == sim.disk_number).unwrap();
        assert!(d
            .partitions
            .iter()
            .any(|p| p.label == "Data" && p.fs == FsKind::Ntfs));

        let note = w.leave_simulation();
        println!("leave note: {note:?}");
        assert!(w.vhd.is_none() && w.backend.is_some());
        let real = disks_of(&mut w);
        assert!(real
            .iter()
            .all(|d| d.number != sim.disk_number || !d.is_simulation));
        assert!(!real.iter().any(|d| d.is_simulation));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("woa-rescue-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
