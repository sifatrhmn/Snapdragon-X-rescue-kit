//! Modal dialogs for partition operations and the simulation disk.
//!
//! The dialogs are built in code (no resource file) on one small form framework: a
//! pop-up window, a nested message loop that disables the owner, and a validation callback
//! that keeps the dialog open and shows its message until the input is acceptable. Each
//! operation dialog turns the input into exactly one [`Op`]; nothing here touches a disk.
//!
//! Everything above the `Win32 form` marker is pure and unit-tested.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use diskcore::{Disk, Extent, FsKind, Op, PartKind, Partition, Protection, FAT32_MAX, MIB};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{EM_SETLIMITTEXT, EM_SETSEL};
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use winstorage::bitlocker::parse_recovery_key;
use winstorage::Template;

use super::lists::{describe_op, fmt_size, fs_name};
use crate::confirm::{confirm_prompt, confirmation_matches};

pub const TITLE: &str = "WoA Rescue Kit";

const GIB: u64 = 1024 * MIB;

// ---------------------------------------------------------------- pure helpers

/// Parses a size such as `512`, `512 MB`, `1.5 GB` or `3gb`. A bare number is megabytes.
/// Returns whole MiB worth of bytes (at least 1 MiB).
pub fn parse_size(text: &str) -> Result<u64, String> {
    let t = text.trim().to_ascii_lowercase();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let unit = unit.trim();
    let scale = match unit {
        "" | "mb" | "mib" => MIB,
        "gb" | "gib" => GIB,
        _ => return Err("Enter a size such as 512 MB or 2 GB.".into()),
    };
    let v: f64 = num
        .parse()
        .map_err(|_| "Enter a size such as 512 MB or 2 GB.".to_string())?;
    if !v.is_finite() || v <= 0.0 || v > 1.0e9 {
        return Err("Enter a size greater than zero.".into());
    }
    let mib = ((v * scale as f64) / MIB as f64).round() as u64;
    if mib == 0 {
        return Err("The size must be at least 1 MB.".into());
    }
    Ok(mib * MIB)
}

/// Longest volume label the file system accepts.
pub fn label_limit(fs: &FsKind) -> usize {
    match fs {
        FsKind::Ntfs | FsKind::Refs => 32,
        FsKind::ExFat => 15,
        _ => 11,
    }
}

/// The file systems offered for new and reformatted partitions, in menu order.
pub const FS_CHOICES: [(&str, FsKind); 3] = [
    ("NTFS", FsKind::Ntfs),
    ("FAT32", FsKind::Fat32),
    ("exFAT", FsKind::ExFat),
];

fn fs_choice_index(fs: &FsKind) -> usize {
    FS_CHOICES.iter().position(|(_, f)| f == fs).unwrap_or(0)
}

/// Checks a volume label against the file system's rules.
pub fn check_label(label: &str, fs: &FsKind) -> Result<(), String> {
    let n = label.trim().chars().count();
    let max = label_limit(fs);
    if n > max {
        return Err(format!(
            "A {} label can have at most {max} characters.",
            fs_name(fs)
        ));
    }
    if label.contains(['"', '\r', '\n']) {
        return Err("The label cannot contain quotation marks or line breaks.".into());
    }
    Ok(())
}

/// Where the simulation disk is offered by default: the largest lettered volume of the boot
/// USB in WinPE, otherwise `temp`.
pub fn default_simulation_folder(disks: &[Disk], winpe: bool, temp: &Path) -> PathBuf {
    if winpe {
        let best = disks
            .iter()
            .filter(|d| d.is_boot_media)
            .flat_map(|d| d.partitions.iter())
            .filter(|p| {
                p.kind == PartKind::Basic
                    && matches!(p.fs, FsKind::Ntfs | FsKind::Fat32 | FsKind::ExFat)
                    && p.letter.is_some_and(|l| !l.eq_ignore_ascii_case(&'X'))
            })
            .max_by_key(|p| p.size);
        if let Some(l) = best.and_then(|p| p.letter) {
            return PathBuf::from(format!("{}:\\", l.to_ascii_uppercase()));
        }
    }
    temp.to_path_buf()
}

/// File name for a new simulation disk.
pub fn simulation_file_name(unix_secs: u64) -> String {
    format!("WoA-Rescue-simulation-{unix_secs}.vhdx")
}

/// Smallest simulation disk the template can be laid out on, in GiB.
pub fn min_simulation_gib(template: Template) -> u64 {
    match template {
        Template::SnapdragonLike => 3,
        Template::Empty => 1,
    }
}

/// Template names, in the order of the choice box.
pub const TEMPLATE_CHOICES: [(&str, Template); 2] = [
    ("Snapdragon-like layout", Template::SnapdragonLike),
    ("Empty", Template::Empty),
];

// ---------------------------------------------------------------- Win32 form

/// Window, DPI and font a dialog needs from the main window.
#[derive(Clone, Copy)]
pub struct Env {
    pub owner: HWND,
    pub dpi: u32,
    pub font: HFONT,
}

pub enum Field {
    Text {
        label: String,
        initial: String,
        max_len: usize,
    },
    Choice {
        label: String,
        items: Vec<String>,
        selected: usize,
    },
    /// A path box with a Browse button.
    Folder { label: String, initial: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Index(usize),
}

impl Value {
    pub fn text(&self) -> &str {
        match self {
            Value::Text(s) => s,
            Value::Index(_) => "",
        }
    }
    pub fn index(&self) -> usize {
        match self {
            Value::Index(i) => *i,
            Value::Text(_) => 0,
        }
    }
}

type Validate = Rc<dyn Fn(&[Value]) -> Result<(), String>>;

pub struct Form {
    pub title: String,
    pub intro: String,
    pub fields: Vec<Field>,
    pub ok: String,
    /// Check the fields as they change: show the problem and keep OK off until none is left.
    pub live: bool,
    pub validate: Validate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Choice,
    Folder,
}

const ID_FIELD: isize = 100;
const ID_BROWSE: isize = 300;
const ID_ERROR: isize = 900;
const DM_GETDEFID: u32 = WM_USER;
const DC_HASDEFID: isize = 0x534B;

thread_local! {
    static CLASS_DONE: Cell<bool> = const { Cell::new(false) };
    static CONTROLS: RefCell<Vec<(Kind, HWND)>> = const { RefCell::new(Vec::new()) };
    static CHECK: RefCell<Option<Validate>> = const { RefCell::new(None) };
    static RESULT: RefCell<Option<Vec<Value>>> = const { RefCell::new(None) };
    static DONE: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<bool> = const { Cell::new(false) };
    static ERR_HWND: Cell<isize> = const { Cell::new(0) };
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, msg, WPARAM(w), LPARAM(l)).0 }
}

fn window_text(h: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(h) }.max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    let n = unsafe { GetWindowTextW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn read_values() -> Vec<Value> {
    CONTROLS.with(|c| {
        c.borrow()
            .iter()
            .map(|(kind, h)| match kind {
                Kind::Text | Kind::Folder => Value::Text(window_text(*h)),
                Kind::Choice => Value::Index(send(*h, CB_GETCURSEL, 0, 0).max(0) as usize),
            })
            .collect()
    })
}

fn set_error(text: &str) {
    let h = ERR_HWND.with(|e| e.get());
    if h != 0 {
        unsafe {
            let _ = SetWindowTextW(HWND(h as *mut c_void), PCWSTR(wide(text).as_ptr()));
        }
    }
}

fn on_ok() {
    let values = read_values();
    let check = CHECK.with(|c| c.borrow().clone());
    if let Some(check) = check {
        if let Err(msg) = check(&values) {
            set_error(&msg);
            return;
        }
    }
    RESULT.with(|r| *r.borrow_mut() = Some(values));
    DONE.with(|d| d.set(true));
}

/// Live check: shows the problem under the fields and enables OK only when there is none.
/// Nothing is shown while every field is still empty.
fn live_check(hwnd: HWND) {
    let values = read_values();
    let check = CHECK.with(|c| c.borrow().clone());
    let result = check.map(|c| c(&values)).unwrap_or(Ok(()));
    let all_empty = values.iter().all(|v| v.text().is_empty());
    match &result {
        Ok(()) => set_error(""),
        Err(_) if all_empty => set_error(""),
        Err(msg) => set_error(msg),
    }
    if let Ok(ok) = unsafe { GetDlgItem(hwnd, 1) } {
        unsafe {
            let _ = EnableWindow(ok, result.is_ok());
        }
    }
}
/// Opens the folder picker; `None` if cancelled.
fn browse_for_folder(owner: HWND, title: &str) -> Option<String> {
    // The new-style folder picker needs COM on this (UI) thread. Done on first use so that
    // start-up is unchanged. A failure only leaves the picker unavailable: the path can
    // still be typed.
    if !COM_READY.with(|c| c.replace(true)) {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    }
    let t = wide(title);
    let mut name = [0u16; 260];
    let bi = BROWSEINFOW {
        hwndOwner: owner,
        pszDisplayName: windows::core::PWSTR(name.as_mut_ptr()),
        lpszTitle: PCWSTR(t.as_ptr()),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..Default::default()
    };
    let pidl = unsafe { SHBrowseForFolderW(&bi) };
    if pidl.is_null() {
        return None;
    }
    let mut path = [0u16; 260];
    let ok = unsafe { SHGetPathFromIDListW(pidl, &mut path) }.as_bool();
    unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
    if !ok {
        return None;
    }
    let n = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    Some(String::from_utf16_lossy(&path[..n]))
}

extern "system" fn form_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as isize;
            let code = ((wp.0 >> 16) & 0xFFFF) as u32;
            if code == EN_CHANGE
                && LIVE.with(|l| l.get())
                && (ID_FIELD..ID_FIELD + 50).contains(&id)
            {
                live_check(hwnd);
            } else if id == 1 {
                on_ok();
            } else if id == 2 {
                DONE.with(|d| d.set(true));
            } else if (ID_BROWSE..ID_BROWSE + 50).contains(&id) {
                let idx = (id - ID_BROWSE) as usize;
                let edit = CONTROLS.with(|c| c.borrow().get(idx).map(|x| x.1));
                if let Some(edit) = edit {
                    if let Some(p) =
                        browse_for_folder(hwnd, "Choose the folder for the simulation disk")
                    {
                        unsafe {
                            let _ = SetWindowTextW(edit, PCWSTR(wide(&p).as_ptr()));
                        }
                    }
                }
            }
            LRESULT(0)
        }
        DM_GETDEFID => LRESULT((DC_HASDEFID << 16) | 1),
        WM_CTLCOLORSTATIC => {
            let h = lp.0;
            if h != 0 && h == ERR_HWND.with(|e| e.get()) {
                unsafe {
                    let hdc = HDC(wp.0 as *mut c_void);
                    SetTextColor(hdc, COLORREF(0x0000_00B0)); // dark red (BGR)
                    SetBkMode(hdc, TRANSPARENT);
                    return LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize);
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_CLOSE => {
            DONE.with(|d| d.set(true));
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn register_form_class(hinst: HINSTANCE) {
    if CLASS_DONE.with(|c| c.get()) {
        return;
    }
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(form_proc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as isize as *mut c_void),
        lpszClassName: w!("WoARescueForm"),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
    CLASS_DONE.with(|c| c.set(true));
}

/// Parent, instance and font shared by the controls of one form.
struct Kit {
    parent: HWND,
    hinst: HINSTANCE,
    font: HFONT,
}

impl Kit {
    fn child(
        &self,
        class: PCWSTR,
        text: &str,
        style: u32,
        ex: u32,
        id: isize,
        rect: (i32, i32, i32, i32),
    ) -> HWND {
        let h = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                PCWSTR(wide(text).as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
                rect.0,
                rect.1,
                rect.2,
                rect.3,
                self.parent,
                HMENU(id as *mut c_void),
                self.hinst,
                None,
            )
        }
        .unwrap_or_default();
        send(h, WM_SETFONT, self.font.0 as usize, 1);
        h
    }
}

/// Shows `form` and runs a modal loop. Returns the field values on OK, `None` on cancel.
pub fn show_form(env: Env, form: Form) -> Option<Vec<Value>> {
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();
    register_form_class(hinst);
    let px = |v: i32| v * env.dpi as i32 / 96;

    let margin = px(12);
    let client_w = px(470);
    let label_w = px(130);
    let ctl_x = margin + label_w + px(8);
    let row_h = px(24);
    let gap = px(8);
    let btn_w = px(88);
    let btn_h = px(26);
    let inner_w = client_w - 2 * margin;

    let intro_lines: i32 = form
        .intro
        .split('\n')
        .map(|l| (l.chars().count() as i32 / 62) + 1)
        .sum();
    let intro_h = intro_lines * px(17);
    let err_h = px(36);
    let client_h = margin
        + intro_h
        + gap
        + form.fields.len() as i32 * (row_h + gap)
        + err_h
        + gap
        + btn_h
        + margin;

    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let ex = WS_EX_DLGMODALFRAME;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    unsafe {
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, ex, env.dpi);
    }
    let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
    let mut orc = RECT::default();
    unsafe {
        let _ = GetWindowRect(env.owner, &mut orc);
    }
    let x = orc.left + ((orc.right - orc.left) - ww) / 2;
    let y = orc.top + ((orc.bottom - orc.top) - wh) / 3;

    let hwnd = unsafe {
        CreateWindowExW(
            ex,
            w!("WoARescueForm"),
            PCWSTR(wide(&form.title).as_ptr()),
            style,
            x.max(0),
            y.max(0),
            ww,
            wh,
            env.owner,
            None,
            hinst,
            None,
        )
    };
    let Ok(hwnd) = hwnd else { return None };

    let kit = Kit {
        parent: hwnd,
        hinst,
        font: env.font,
    };
    let mut yy = margin;
    kit.child(
        w!("STATIC"),
        &form.intro,
        0,
        0,
        -1,
        (margin, yy, inner_w, intro_h),
    );
    yy += intro_h + gap;

    let mut controls: Vec<(Kind, HWND)> = Vec::new();
    for (i, f) in form.fields.iter().enumerate() {
        let id = ID_FIELD + i as isize;
        let (label, kind) = match f {
            Field::Text { label, .. } => (label, Kind::Text),
            Field::Choice { label, .. } => (label, Kind::Choice),
            Field::Folder { label, .. } => (label, Kind::Folder),
        };
        kit.child(
            w!("STATIC"),
            label,
            0,
            0,
            -1,
            (margin, yy + px(4), label_w, row_h),
        );
        let ctl_w = client_w - margin - ctl_x;
        let h = match f {
            Field::Text {
                initial, max_len, ..
            } => {
                let h = kit.child(
                    w!("EDIT"),
                    initial,
                    (WS_TABSTOP.0) | ES_AUTOHSCROLL as u32,
                    WS_EX_CLIENTEDGE.0,
                    id,
                    (ctl_x, yy, ctl_w, row_h),
                );
                send(h, EM_SETLIMITTEXT, *max_len, 0);
                h
            }
            Field::Choice {
                items, selected, ..
            } => {
                let h = kit.child(
                    w!("COMBOBOX"),
                    "",
                    WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
                    0,
                    id,
                    (ctl_x, yy, ctl_w, px(160)),
                );
                for it in items {
                    send(h, CB_ADDSTRING, 0, wide(it).as_ptr() as isize);
                }
                send(h, CB_SETCURSEL, *selected, 0);
                h
            }
            Field::Folder { initial, .. } => {
                let bw = px(84);
                let h = kit.child(
                    w!("EDIT"),
                    initial,
                    WS_TABSTOP.0 | ES_AUTOHSCROLL as u32,
                    WS_EX_CLIENTEDGE.0,
                    id,
                    (ctl_x, yy, ctl_w - bw - px(6), row_h),
                );
                send(h, EM_SETLIMITTEXT, 259, 0);
                kit.child(
                    w!("BUTTON"),
                    "Browse\u{2026}",
                    WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
                    0,
                    ID_BROWSE + i as isize,
                    (ctl_x + ctl_w - bw, yy, bw, row_h),
                );
                h
            }
        };
        controls.push((kind, h));
        yy += row_h + gap;
    }
    let err = kit.child(
        w!("STATIC"),
        "",
        0,
        0,
        ID_ERROR,
        (margin, yy, inner_w, err_h),
    );
    yy += err_h + gap;
    let bx = client_w - margin - 2 * btn_w - gap;
    kit.child(
        w!("BUTTON"),
        &form.ok,
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        0,
        1,
        (bx, yy, btn_w, btn_h),
    );
    kit.child(
        w!("BUTTON"),
        "Cancel",
        WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        0,
        2,
        (bx + btn_w + gap, yy, btn_w, btn_h),
    );

    let first = controls.first().map(|c| c.1);
    CONTROLS.with(|c| *c.borrow_mut() = controls);
    CHECK.with(|c| *c.borrow_mut() = Some(form.validate.clone()));
    RESULT.with(|r| *r.borrow_mut() = None);
    DONE.with(|d| d.set(false));
    ERR_HWND.with(|e| e.set(err.0 as isize));
    LIVE.with(|l| l.set(form.live));
    if form.live {
        live_check(hwnd);
    }

    unsafe {
        let _ = EnableWindow(env.owner, false);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        if let Some(f) = first {
            let _ = SetFocus(f);
            send(f, EM_SETSEL, 0, -1);
        }
        let mut msg = MSG::default();
        while !DONE.with(|d| d.get()) {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                if r.0 == 0 {
                    // WM_QUIT: pass it on to the main loop and leave.
                    PostQuitMessage(msg.wParam.0 as i32);
                }
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // Re-enable the owner first so activation returns to it, not to another program.
        let _ = EnableWindow(env.owner, true);
        let _ = DestroyWindow(hwnd);
        let _ = SetForegroundWindow(env.owner);
    }
    ERR_HWND.with(|e| e.set(0));
    LIVE.with(|l| l.set(false));
    CHECK.with(|c| *c.borrow_mut() = None);
    CONTROLS.with(|c| c.borrow_mut().clear());
    RESULT.with(|r| r.borrow_mut().take())
}

// ---------------------------------------------------------------- message boxes

fn message(owner: HWND, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe {
        MessageBoxW(
            owner,
            PCWSTR(wide(text).as_ptr()),
            PCWSTR(wide(TITLE).as_ptr()),
            style,
        )
    }
}

pub fn info(owner: HWND, text: &str) {
    message(owner, text, MB_OK | MB_ICONINFORMATION);
}

pub fn warn(owner: HWND, text: &str) {
    message(owner, text, MB_OK | MB_ICONWARNING);
}

/// Yes/No question; the default button is No.
pub fn ask_yes_no(owner: HWND, text: &str) -> bool {
    message(owner, text, MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) == IDYES
}

// ---------------------------------------------------------------- operation dialogs

/// Typed confirmation (see [`confirmation_matches`]). `what` names the operation, for
/// example `Delete D:`.
fn confirm_typed(env: Env, p: &Partition, what: &str, ok: &str) -> bool {
    let intro = format!("{what}\n{}", confirm_prompt(p));
    let part = p.clone();
    let form = Form {
        title: "Confirm".into(),
        intro,
        fields: vec![Field::Text {
            label: "Confirmation:".into(),
            initial: String::new(),
            max_len: 64,
        }],
        ok: ok.into(),
        live: false,
        validate: Rc::new(move |v| {
            if confirmation_matches(&part, v[0].text()) {
                Ok(())
            } else {
                Err("That does not match. Check what you typed.".into())
            }
        }),
    };
    show_form(env, form).is_some()
}

fn describe_target(disk: &Disk, p: &Partition, verb: &str) -> String {
    let op = if verb == "Delete" {
        Op::Delete {
            disk: disk.id.clone(),
            offset: p.offset,
        }
    } else {
        Op::Format {
            disk: disk.id.clone(),
            offset: p.offset,
            fs: p.fs.clone(),
            label: p.label.clone(),
        }
    };
    let name = describe_op(&op, std::slice::from_ref(disk));
    // "Format D: as NTFS" reads oddly before the file system is chosen.
    let base = name.split(" as ").next().unwrap_or(&name).to_string();
    let label = if p.label.trim().is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", p.label.trim())
    };
    format!(
        "{base}{label}, {} on Disk {} ({})",
        fmt_size(p.size),
        disk.number,
        disk.model.trim()
    )
}

/// New partition in the free extent `ext` of `disk`.
pub fn create_partition(env: Env, disk: &Disk, ext: Extent) -> Option<Op> {
    let intro = format!(
        "Create a partition in {} of free space on Disk {}. \
         Enter its size in MB, or with GB (for example 20 GB).",
        fmt_size(ext.size),
        disk.number
    );
    let max = ext.size;
    let form = Form {
        title: "Create partition".into(),
        intro,
        fields: vec![
            Field::Text {
                label: "Size:".into(),
                initial: format!("{} MB", ext.size / MIB),
                max_len: 20,
            },
            Field::Choice {
                label: "File system:".into(),
                items: FS_CHOICES.iter().map(|c| c.0.to_string()).collect(),
                selected: 0,
            },
            Field::Text {
                label: "Label:".into(),
                initial: String::new(),
                max_len: 40,
            },
        ],
        ok: "Add to queue".into(),
        live: false,
        validate: Rc::new(move |v| {
            let size = parse_size(v[0].text())?;
            if size > max {
                return Err(format!("There is only {} of free space.", fmt_size(max)));
            }
            let fs = &FS_CHOICES[v[1].index().min(FS_CHOICES.len() - 1)].1;
            if *fs == FsKind::Fat32 && size > FAT32_MAX {
                return Err("FAT32 volumes are limited to 32 GB. Choose NTFS or exFAT.".into());
            }
            check_label(v[2].text(), fs)
        }),
    };
    let v = show_form(env, form)?;
    let size = parse_size(v[0].text()).ok()?.min(ext.size);
    Some(Op::Create {
        disk: disk.id.clone(),
        offset: ext.offset,
        size,
        fs: FS_CHOICES[v[1].index().min(FS_CHOICES.len() - 1)].1.clone(),
        label: v[2].text().trim().to_string(),
    })
}

/// The extra Yes/No (default No) after the typed confirmation when `p` is the EFI system or
/// Microsoft reserved partition. `verb` is "Delete" or "Format". True when `p` is not one
/// of them or the user agrees.
fn confirm_system_partition(env: Env, p: &Partition, verb: &str) -> bool {
    let kind = match p.kind {
        PartKind::Esp => "EFI system partition",
        PartKind::Msr => "Microsoft reserved partition",
        _ => return true,
    };
    let q = format!("This is the {kind}. Windows may not start without it.\n\n{verb} it anyway?");
    ask_yes_no(env.owner, &q)
}

/// Delete `p`. Returns the operation and whether the user approved deleting a system
/// partition with it (the ESP or MSR), after a second Yes/No prompt.
pub fn delete_partition(env: Env, disk: &Disk, p: &Partition) -> Option<(Op, bool)> {
    let what = describe_target(disk, p, "Delete");
    if !confirm_typed(env, p, &what, "Delete") {
        return None;
    }
    let system = matches!(p.kind, PartKind::Esp | PartKind::Msr);
    if !confirm_system_partition(env, p, "Delete") {
        return None;
    }
    Some((
        Op::Delete {
            disk: disk.id.clone(),
            offset: p.offset,
        },
        system,
    ))
}

/// Format `p`, after the typed confirmation (and, for the ESP or MSR, a second Yes/No).
pub fn format_partition(env: Env, disk: &Disk, p: &Partition) -> Option<Op> {
    let what = describe_target(disk, p, "Format");
    if !confirm_typed(env, p, &what, "Continue") {
        return None;
    }
    if !confirm_system_partition(env, p, "Format") {
        return None;
    }
    let form = Form {
        title: "Format partition".into(),
        intro: format!("{what}\nAll data on it will be erased when the queue is applied."),
        fields: vec![
            Field::Choice {
                label: "File system:".into(),
                items: FS_CHOICES.iter().map(|c| c.0.to_string()).collect(),
                selected: fs_choice_index(&p.fs),
            },
            Field::Text {
                label: "Label:".into(),
                initial: p.label.trim().to_string(),
                max_len: 40,
            },
        ],
        ok: "Add to queue".into(),
        live: false,
        validate: {
            let size = p.size;
            Rc::new(move |v| {
                let fs = &FS_CHOICES[v[0].index().min(FS_CHOICES.len() - 1)].1;
                if *fs == FsKind::Fat32 && size > FAT32_MAX {
                    return Err("FAT32 volumes are limited to 32 GB. Choose NTFS or exFAT.".into());
                }
                check_label(v[1].text(), fs)
            })
        },
    };
    let v = show_form(env, form)?;
    Some(Op::Format {
        disk: disk.id.clone(),
        offset: p.offset,
        fs: FS_CHOICES[v[0].index().min(FS_CHOICES.len() - 1)].1.clone(),
        label: v[1].text().trim().to_string(),
    })
}

/// Shrink or extend `p` to a new size.
pub fn resize_partition(env: Env, disk: &Disk, p: &Partition) -> Option<Op> {
    let name = describe_op(
        &Op::Delete {
            disk: disk.id.clone(),
            offset: p.offset,
        },
        std::slice::from_ref(disk),
    )
    .replacen("Delete ", "", 1);
    let mut intro = format!(
        "Resize {name}, now {}. Enter the new size in MB, or with GB (for example 60 GB).",
        fmt_size(p.size)
    );
    if let Some((min, max)) = p.supported {
        if max != u64::MAX {
            intro.push_str(&format!(
                "\nWindows allows {} to {}.",
                fmt_size(min),
                fmt_size(max)
            ));
        }
    }
    if p.fs != FsKind::Ntfs {
        intro.push_str("\nOnly NTFS volumes can be resized.");
    }
    let (current, disk_size) = (p.size, disk.size);
    let form = Form {
        title: "Shrink or extend partition".into(),
        intro,
        fields: vec![Field::Text {
            label: "New size:".into(),
            initial: format!("{} MB", p.size / MIB),
            max_len: 20,
        }],
        ok: "Add to queue".into(),
        live: false,
        validate: Rc::new(move |v| {
            let size = parse_size(v[0].text())?;
            if size > disk_size {
                return Err("That is larger than the disk.".into());
            }
            if size == current / MIB * MIB {
                return Err("That is the current size.".into());
            }
            Ok(())
        }),
    };
    let v = show_form(env, form)?;
    Some(Op::Resize {
        disk: disk.id.clone(),
        offset: p.offset,
        new_size: parse_size(v[0].text()).ok()?,
    })
}

/// What the user chose in the "Open simulation disk" dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulationRequest {
    pub path: PathBuf,
    pub template: Template,
    pub size: u64,
}

/// Folder, template and size for a new simulation disk (default 3 GB).
pub fn simulation_request(env: Env, default_folder: &Path) -> Option<SimulationRequest> {
    let form = Form {
        title: "Open simulation disk".into(),
        intro: "Creates a virtual disk (VHDX) to practise on. Changes affect the virtual \
                disk only. Your physical disks are not touched."
            .into(),
        fields: vec![
            Field::Folder {
                label: "Folder:".into(),
                initial: default_folder.display().to_string(),
            },
            Field::Choice {
                label: "Layout:".into(),
                items: TEMPLATE_CHOICES.iter().map(|c| c.0.to_string()).collect(),
                selected: 0,
            },
            Field::Text {
                label: "Size (GB):".into(),
                initial: "3".into(),
                max_len: 6,
            },
        ],
        ok: "Create".into(),
        live: false,
        validate: Rc::new(|v| {
            if !Path::new(v[0].text().trim()).is_dir() {
                return Err("That folder does not exist.".into());
            }
            let template = TEMPLATE_CHOICES[v[1].index().min(1)].1;
            let size = parse_size(&format!("{} GB", v[2].text().trim()))?;
            let min = min_simulation_gib(template);
            if size < min * GIB {
                return Err(format!("This layout needs at least {min} GB."));
            }
            if size > 1024 * GIB {
                return Err("The size is limited to 1024 GB.".into());
            }
            Ok(())
        }),
    };
    let v = show_form(env, form)?;
    let template = TEMPLATE_CHOICES[v[1].index().min(1)].1;
    let size = parse_size(&format!("{} GB", v[2].text().trim())).ok()?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(SimulationRequest {
        path: Path::new(v[0].text().trim()).join(simulation_file_name(secs)),
        template,
        size,
    })
}

// ---------------------------------------------------------------- BitLocker and boot repair

/// The prompt of the BitLocker unlock dialog.
pub const UNLOCK_PROMPT: &str =
    "This volume is locked by BitLocker. Enter the 48-digit recovery key to unlock it.";

/// Checks what was typed as a recovery key. The messages name a group position only, never
/// any digits, so they are safe to show. Returns the key in canonical form.
pub fn recovery_key_check(text: &str) -> Result<String, String> {
    parse_recovery_key(text).map_err(|e| e.to_string())
}

/// The locked BitLocker volumes that can be unlocked: `(volume path, description)`.
pub fn locked_volumes(disks: &[Disk]) -> Vec<(String, String)> {
    disks
        .iter()
        .flat_map(|d| {
            d.partitions
                .iter()
                .filter(|p| p.protection == Protection::Locked)
                .filter_map(move |p| {
                    p.volume_path
                        .as_ref()
                        .map(|v| (v.clone(), crate::app::describe_partition(d, p)))
                })
        })
        .collect()
}

/// What the user entered in the unlock dialog. The key is a secret: it is never logged and
/// the request is dropped as soon as the unlock job has it.
pub struct UnlockRequest {
    pub volume_path: String,
    pub key: String,
}

/// Asks for the recovery key of one of `volumes` (from [`locked_volumes`]; `preselect` is
/// the index to offer first). The key is checked as it is typed and OK stays off until it
/// is valid. Nothing of the key is shown back after OK.
pub fn unlock_request(
    env: Env,
    volumes: &[(String, String)],
    preselect: usize,
) -> Option<UnlockRequest> {
    let form = Form {
        title: "Unlock BitLocker volume".into(),
        intro: UNLOCK_PROMPT.into(),
        fields: vec![
            Field::Choice {
                label: "Volume:".into(),
                items: volumes.iter().map(|v| v.1.clone()).collect(),
                selected: preselect.min(volumes.len().saturating_sub(1)),
            },
            Field::Text {
                label: "Recovery key:".into(),
                initial: String::new(),
                max_len: 64,
            },
        ],
        ok: "Unlock".into(),
        live: true,
        validate: Rc::new(|v| recovery_key_check(v[1].text()).map(|_| ())),
    };
    let v = show_form(env, form)?;
    let key = recovery_key_check(v[1].text()).ok()?;
    let volume_path = volumes.get(v[0].index())?.0.clone();
    Some(UnlockRequest { volume_path, key })
}

/// Picks one of several Windows installations; `None` if cancelled.
pub fn choose_windows_install(env: Env, items: Vec<String>) -> Option<usize> {
    let form = Form {
        title: "Repair boot files".into(),
        intro: "Choose the Windows installation whose boot files should be repaired.".into(),
        fields: vec![Field::Choice {
            label: "Windows:".into(),
            items,
            selected: 0,
        }],
        ok: "Next".into(),
        live: false,
        validate: Rc::new(|_| Ok(())),
    };
    show_form(env, form).map(|v| v[0].index())
}

/// Shows what a boot repair will do (`preview`, from `AppState::boot_repair_preview_for`)
/// and asks for the go-ahead.
pub fn confirm_boot_repair(env: Env, preview: &str) -> bool {
    let form = Form {
        title: "Repair boot files".into(),
        intro: format!("{preview}\n\nRepair the boot files now?"),
        fields: Vec::new(),
        ok: "Repair".into(),
        live: false,
        validate: Rc::new(|_| Ok(())),
    };
    show_form(env, form).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskcore::{disk_fixture, Protection};

    const GB1: u64 = GIB;

    #[test]
    fn sizes_parse_with_units() {
        assert_eq!(parse_size("512"), Ok(512 * MIB));
        assert_eq!(parse_size(" 512 MB "), Ok(512 * MIB));
        assert_eq!(parse_size("1gb"), Ok(GB1));
        assert_eq!(parse_size("1.5 GB"), Ok(1536 * MIB));
        assert_eq!(parse_size("2 GiB"), Ok(2 * GB1));
    }

    #[test]
    fn bad_sizes_are_refused() {
        for bad in ["", "abc", "0", "-5", "1 TB", "1..2", "0.0001 MB", "MB"] {
            assert!(parse_size(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn label_limits_follow_the_file_system() {
        assert!(check_label(&"x".repeat(32), &FsKind::Ntfs).is_ok());
        assert!(check_label(&"x".repeat(33), &FsKind::Ntfs).is_err());
        assert!(check_label(&"x".repeat(11), &FsKind::Fat32).is_ok());
        assert!(check_label(&"x".repeat(12), &FsKind::Fat32).is_err());
        assert!(check_label(&"x".repeat(16), &FsKind::ExFat).is_err());
        assert!(check_label("say \"hi\"", &FsKind::Ntfs).is_err());
        assert!(check_label("", &FsKind::Fat32).is_ok());
    }

    #[test]
    fn simulation_folder_is_the_boot_usb_in_winpe_and_temp_elsewhere() {
        let temp = PathBuf::from(r"C:\Temp");
        let mut usb = disk_fixture();
        usb.is_boot_media = true;
        usb.partitions[0].letter = Some('E'); // small ESP-like volume
        usb.partitions[0].kind = PartKind::Basic;
        usb.partitions[2].letter = Some('d'); // the larger data volume
        let disks = vec![disk_fixture(), usb];
        assert_eq!(
            default_simulation_folder(&disks, true, &temp),
            PathBuf::from(r"D:\")
        );
        assert_eq!(default_simulation_folder(&disks, false, &temp), temp);
        // No usable volume on the boot media: fall back to the temp folder.
        let none = vec![disk_fixture()];
        assert_eq!(default_simulation_folder(&none, true, &temp), temp);
    }

    #[test]
    fn x_drive_is_never_offered() {
        let temp = PathBuf::from(r"C:\Temp");
        let mut usb = disk_fixture();
        usb.is_boot_media = true;
        usb.partitions[2].letter = Some('X');
        usb.partitions[2].protection = Protection::None;
        assert_eq!(default_simulation_folder(&[usb], true, &temp), temp);
    }

    #[test]
    fn templates_have_minimum_sizes() {
        assert_eq!(min_simulation_gib(Template::SnapdragonLike), 3);
        assert_eq!(min_simulation_gib(Template::Empty), 1);
        assert_eq!(
            simulation_file_name(42),
            "WoA-Rescue-simulation-42.vhdx".to_string()
        );
    }

    const KEY: &str = "110011 220022 330033 440044 550055 660066 000011 000022";

    #[test]
    fn recovery_key_is_checked_as_typed_and_never_echoed() {
        assert_eq!(
            recovery_key_check(KEY).unwrap(),
            "110011-220022-330033-440044-550055-660066-000011-000022"
        );
        let partial = recovery_key_check("110011-2200").unwrap_err();
        assert!(partial.contains("8 groups"), "{partial}");
        let bad = recovery_key_check(&KEY.replace("330033", "330034")).unwrap_err();
        assert!(!bad.contains("330034"), "{bad}");
        assert!(recovery_key_check("").is_err());
    }

    #[test]
    fn only_locked_volumes_with_a_path_are_offered() {
        let mut d = disk_fixture();
        for p in &mut d.partitions {
            p.protection = Protection::None;
            p.volume_path = Some(format!(r"\?\Volume{{{}}}\", p.number));
        }
        assert!(locked_volumes(std::slice::from_ref(&d)).is_empty());
        d.partitions[2].protection = Protection::Locked;
        let got = locked_volumes(std::slice::from_ref(&d));
        assert_eq!(got.len(), 1);
        assert!(got[0].0.contains(&d.partitions[2].number.to_string()));
        d.partitions[2].volume_path = None;
        assert!(locked_volumes(&[d]).is_empty());
    }
}
