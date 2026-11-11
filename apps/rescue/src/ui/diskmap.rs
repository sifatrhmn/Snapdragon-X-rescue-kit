//! Disk map: pure bar layout plus the owner-drawn control (added below).

use std::cell::RefCell;

use diskcore::{Disk, DiskId, PartKind, Partition, Protection};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::lists;

/// Minimum width of any bar segment, in pixels.
pub const MIN_SEGMENT_PX: i32 = 24;

/// Lays out one disk as a row of segments: `(x, width, Some(partition offset))` for a
/// partition and `(x, width, None)` for free space.
///
/// Segments are proportional to their size but never narrower than [`MIN_SEGMENT_PX`]
/// (when `width_px` is too small for that, the width is split evenly instead). The widths
/// always sum to exactly `width_px`.
pub fn bar_layout(disk: &Disk, width_px: i32) -> Vec<(i32, i32, Option<u64>)> {
    if width_px <= 0 {
        return Vec::new();
    }
    // (offset, size, partition offset or None for free space), in disk order.
    let mut segs: Vec<(u64, u64, Option<u64>)> = disk
        .partitions
        .iter()
        .map(|p| (p.offset, p.size.max(1), Some(p.offset)))
        .collect();
    segs.extend(
        lists::visible_free_extents(disk)
            .iter()
            .map(|e| (e.offset, e.size, None)),
    );
    segs.sort_by_key(|s| s.0);
    if segs.is_empty() {
        return vec![(0, width_px, None)];
    }

    let widths = distribute(&segs.iter().map(|s| s.1).collect::<Vec<_>>(), width_px);
    let mut x = 0;
    segs.iter()
        .zip(widths)
        .map(|(s, w)| {
            let out = (x, w, s.2);
            x += w;
            out
        })
        .collect()
}

/// Splits `total` pixels over `weights`: proportional, each at least `MIN_SEGMENT_PX`,
/// summing exactly to `total`.
fn distribute(weights: &[u64], total: i32) -> Vec<i32> {
    let n = weights.len();
    let total = total as i64;
    let min = MIN_SEGMENT_PX as i64;
    if (n as i64) * min > total {
        let base = total / n as i64;
        let extra = (total % n as i64) as usize;
        return (0..n)
            .map(|i| (base + i64::from(i < extra)) as i32)
            .collect();
    }

    // Pin every segment whose proportional share is below the minimum, then share what
    // is left among the rest; pinning only ever lowers the others' shares, so repeat.
    let mut pinned = vec![false; n];
    loop {
        let pinned_count = pinned.iter().filter(|p| **p).count() as i64;
        let remaining = (total - min * pinned_count) as u128;
        let free_weight: u128 = (0..n)
            .filter(|i| !pinned[*i])
            .map(|i| weights[i] as u128)
            .sum();
        let mut changed = false;
        for i in 0..n {
            if !pinned[i] && remaining * (weights[i] as u128) < (min as u128) * free_weight {
                pinned[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let pinned_count = pinned.iter().filter(|p| **p).count() as i64;
    let remaining = (total - min * pinned_count) as u128;
    let free_weight: u128 = (0..n)
        .filter(|i| !pinned[*i])
        .map(|i| weights[i] as u128)
        .sum();
    let mut out = vec![MIN_SEGMENT_PX; n];
    // (index, fractional remainder) for largest-remainder rounding of the unpinned ones.
    let mut rem: Vec<(usize, u128)> = Vec::new();
    let mut used: i64 = min * pinned_count;
    for i in (0..n).filter(|i| !pinned[*i]) {
        let num = remaining * weights[i] as u128;
        let floor = (num / free_weight) as i64;
        out[i] = floor as i32;
        used += floor;
        rem.push((i, num % free_weight));
    }
    let mut leftover = total - used;
    if rem.is_empty() {
        // Everything pinned: the spare pixels go to the heaviest segment.
        let heaviest = (0..n).max_by_key(|i| weights[*i]).unwrap_or(0);
        out[heaviest] += leftover as i32;
    } else {
        rem.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (i, _) in rem.iter().cycle() {
            if leftover == 0 {
                break;
            }
            out[*i] += 1;
            leftover -= 1;
        }
    }
    out
}

// ---------------------------------------------------------------- owner-drawn control

/// Row height at 96 dpi: header line plus bar.
pub const ROW_H: i32 = 56;
const HEADER_H: i32 = 20;
const BAR_H: i32 = 30;
const PAD: i32 = 4;

/// Parent notification: a segment or header was clicked.
/// `wParam` = disk index, `lParam` = partition / free-extent offset, or -1 for the header.
pub const WM_MAP_CLICK: u32 = WM_APP + 2;

const CLASS_NAME: PCWSTR = w!("WoARescueDiskMap");

/// What the control draws. Set from the UI thread with [`set_model`].
#[derive(Default)]
struct Model {
    disks: Vec<Disk>,
    selected: Option<(DiskId, u64)>,
    current: Option<DiskId>,
    scroll: usize,
    message: String,
    font: isize,
    bold: isize,
    dpi: u32,
}

thread_local! {
    static MODEL: RefCell<Model> = RefCell::new(Model { dpi: 96, ..Default::default() });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn px(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

pub fn register_class(hinst: HINSTANCE) {
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
}

pub fn create(parent: HWND, id: isize, hinst: HINSTANCE) -> HWND {
    unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            CLASS_NAME,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
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
    .unwrap_or_default()
}

/// Gives the control its data and repaints it.
pub fn set_model(
    hwnd: HWND,
    disks: &[Disk],
    selected: Option<&(DiskId, u64)>,
    current: Option<&DiskId>,
    message: &str,
) {
    MODEL.with(|m| {
        let mut m = m.borrow_mut();
        m.disks = disks.to_vec();
        m.selected = selected.cloned();
        m.current = current.cloned();
        m.message = message.to_string();
        let max = m.disks.len().saturating_sub(1);
        m.scroll = m.scroll.min(max);
    });
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// Fonts (regular and bold) and DPI used for drawing. The control does not own the fonts.
pub fn set_look(hwnd: HWND, font: HFONT, bold: HFONT, dpi: u32) {
    MODEL.with(|m| {
        let mut m = m.borrow_mut();
        m.font = font.0 as isize;
        m.bold = bold.0 as isize;
        m.dpi = dpi;
    });
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (rc.right - rc.left, rc.bottom - rc.top)
}

fn visible_rows(m: &Model, height: i32) -> usize {
    ((height / px(ROW_H, m.dpi).max(1)) as usize).max(1)
}

fn kind_colour(p: &Partition) -> COLORREF {
    if p.protection == Protection::Locked {
        return rgb(90, 90, 90);
    }
    match p.kind {
        PartKind::Esp => rgb(86, 156, 214),
        PartKind::Msr => rgb(150, 150, 160),
        PartKind::Recovery => rgb(222, 150, 70),
        PartKind::Basic | PartKind::Other => rgb(80, 170, 140),
    }
}

fn text_colour_for(bg: COLORREF) -> COLORREF {
    let (r, g, b) = (bg.0 & 0xFF, (bg.0 >> 8) & 0xFF, (bg.0 >> 16) & 0xFF);
    if (r * 299 + g * 587 + b * 114) / 1000 > 140 {
        rgb(0, 0, 0)
    } else {
        rgb(255, 255, 255)
    }
}

unsafe fn fill(dc: HDC, rc: &RECT, colour: COLORREF) {
    let brush = CreateSolidBrush(colour);
    FillRect(dc, rc, brush);
    let _ = DeleteObject(brush);
}

unsafe fn frame(dc: HDC, rc: &RECT, colour: COLORREF, thickness: i32) {
    let brush = CreateSolidBrush(colour);
    for i in 0..thickness {
        let r = RECT {
            left: rc.left + i,
            top: rc.top + i,
            right: rc.right - i,
            bottom: rc.bottom - i,
        };
        FrameRect(dc, &r, brush);
    }
    let _ = DeleteObject(brush);
}

unsafe fn text(dc: HDC, s: &str, rc: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut buf = wide(s);
    DrawTextW(dc, &mut buf, rc, flags);
}

fn paint(hwnd: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let (w, h) = client_size(hwnd);
        if w > 0 && h > 0 {
            // Double-buffer to avoid flicker while resizing.
            let dc = CreateCompatibleDC(hdc);
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old_bmp = SelectObject(dc, bmp);
            MODEL.with(|m| draw(dc, w, h, &m.borrow()));
            let _ = BitBlt(hdc, 0, 0, w, h, dc, 0, 0, SRCCOPY);
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(dc);
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe fn draw(dc: HDC, w: i32, h: i32, m: &Model) {
    let dpi = m.dpi;
    let window_bg = COLORREF(GetSysColor(COLOR_WINDOW));
    let highlight = COLORREF(GetSysColor(COLOR_HIGHLIGHT));
    fill(
        dc,
        &RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        },
        window_bg,
    );
    SetBkMode(dc, TRANSPARENT);
    let font = HFONT(m.font as *mut _);
    let bold = HFONT(m.bold as *mut _);
    let old_font = SelectObject(dc, font);

    if m.disks.is_empty() {
        SetTextColor(dc, COLORREF(GetSysColor(COLOR_GRAYTEXT)));
        let mut rc = RECT {
            left: px(PAD * 2, dpi),
            top: px(PAD * 2, dpi),
            right: w - px(PAD * 2, dpi),
            bottom: h,
        };
        let msg = if m.message.is_empty() {
            "No disks found."
        } else {
            m.message.as_str()
        };
        text(dc, msg, &mut rc, DT_LEFT | DT_WORDBREAK);
        SelectObject(dc, old_font);
        return;
    }

    let row_h = px(ROW_H, dpi);
    let header_h = px(HEADER_H, dpi);
    let bar_h = px(BAR_H, dpi);
    let pad = px(PAD, dpi);
    let bar_w = (w - 2 * pad).max(1);

    for (slot, disk) in m
        .disks
        .iter()
        .skip(m.scroll)
        .take(visible_rows(m, h) + 1)
        .enumerate()
    {
        let top = slot as i32 * row_h;
        let is_current = m.current.as_ref() == Some(&disk.id);

        // Header line; the current disk gets a tinted band.
        let band = RECT {
            left: 0,
            top,
            right: w,
            bottom: top + header_h,
        };
        if is_current {
            let mix = |a: u32, b: u32| ((a * 85 + b * 15) / 100) as u8;
            let (wr, wg, wb) = (
                window_bg.0 & 0xFF,
                (window_bg.0 >> 8) & 0xFF,
                (window_bg.0 >> 16) & 0xFF,
            );
            let (hr, hg, hb) = (
                highlight.0 & 0xFF,
                (highlight.0 >> 8) & 0xFF,
                (highlight.0 >> 16) & 0xFF,
            );
            fill(dc, &band, rgb(mix(wr, hr), mix(wg, hg), mix(wb, hb)));
        }
        SelectObject(
            dc,
            if disk.is_boot_media || disk.is_running_os {
                bold
            } else {
                font
            },
        );
        SetTextColor(dc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
        let mut rc = RECT {
            left: pad,
            top: band.top,
            right: w - pad,
            bottom: band.bottom,
        };
        text(
            dc,
            &lists::disk_header(disk),
            &mut rc,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        SelectObject(dc, font);

        // Bar.
        let bar_top = top + header_h + 2;
        let free = lists::visible_free_extents(disk);
        let mut free_i = 0;
        for (x, sw, offset) in bar_layout(disk, bar_w) {
            let seg = RECT {
                left: pad + x,
                top: bar_top,
                right: pad + x + sw,
                bottom: bar_top + bar_h,
            };
            let part = offset.and_then(|o| disk.partitions.iter().find(|p| p.offset == o));
            let (colour, label, seg_offset) = match part {
                Some(p) => (kind_colour(p), lists::partition_segment_label(p), p.offset),
                None => {
                    let e = free.get(free_i);
                    free_i += 1;
                    (
                        rgb(225, 225, 225),
                        e.map(|e| lists::free_segment_label(e.size))
                            .unwrap_or_default(),
                        e.map(|e| e.offset).unwrap_or(u64::MAX),
                    )
                }
            };
            fill(dc, &seg, colour);
            frame(dc, &seg, rgb(255, 255, 255), 1);
            SetTextColor(dc, text_colour_for(colour));
            let mut inner = RECT {
                left: seg.left + 3,
                top: seg.top,
                right: seg.right - 3,
                bottom: seg.bottom,
            };
            text(
                dc,
                &label,
                &mut inner,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            if let Some((id, o)) = &m.selected {
                if id == &disk.id && *o == seg_offset {
                    frame(dc, &seg, highlight, 3);
                }
            }
        }
    }
    SelectObject(dc, old_font);
}

/// Resolves a click to `(disk slot index in `disks`, Some(offset) | None for the header)`.
fn hit_test(m: &Model, hwnd: HWND, x: i32, y: i32) -> Option<(usize, Option<u64>)> {
    let (w, _) = client_size(hwnd);
    let row_h = px(ROW_H, m.dpi);
    let slot = (y / row_h.max(1)) as usize;
    let index = m.scroll + slot;
    let disk = m.disks.get(index)?;
    let within = y - slot as i32 * row_h;
    let header_h = px(HEADER_H, m.dpi);
    if within < header_h + 2 {
        return Some((index, None));
    }
    let pad = px(PAD, m.dpi);
    let bar_w = (w - 2 * pad).max(1);
    let layout = bar_layout(disk, bar_w);
    let free = lists::visible_free_extents(disk);
    let mut free_i = 0;
    for (sx, sw, offset) in layout {
        let hit = x >= pad + sx && x < pad + sx + sw;
        let off = match offset {
            Some(o) => Some(o),
            None => {
                let e = free.get(free_i).map(|e| e.offset);
                free_i += 1;
                e
            }
        };
        if hit {
            return Some((index, off));
        }
    }
    Some((index, None))
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let hit = MODEL.with(|m| hit_test(&m.borrow(), hwnd, x, y));
            unsafe {
                let _ = SetFocus(hwnd);
                if let (Some((index, off)), Ok(parent)) = (hit, GetParent(hwnd)) {
                    let lparam = off.map(|o| o as isize).unwrap_or(-1);
                    SendMessageW(parent, WM_MAP_CLICK, WPARAM(index), LPARAM(lparam));
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as i32;
            let (_, h) = client_size(hwnd);
            MODEL.with(|m| {
                let mut m = m.borrow_mut();
                let vis = visible_rows(&m, h);
                let max = m.disks.len().saturating_sub(vis);
                if delta > 0 {
                    m.scroll = m.scroll.saturating_sub(1);
                } else if delta < 0 {
                    m.scroll = (m.scroll + 1).min(max);
                }
            });
            unsafe {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskcore::{disk_fixture, Partition, MIB};

    fn sum(v: &[(i32, i32, Option<u64>)]) -> i32 {
        v.iter().map(|s| s.1).sum()
    }

    #[test]
    fn widths_sum_to_width_px() {
        let d = disk_fixture();
        for w in [200, 640, 1000, 1917, 3840] {
            let l = bar_layout(&d, w);
            assert!(!l.is_empty());
            assert_eq!(sum(&l), w, "width {w}");
            // contiguous from x = 0
            let mut x = 0;
            for s in &l {
                assert_eq!(s.0, x);
                x += s.1;
            }
        }
    }

    fn big_disk_with_msr() -> Disk {
        let mut d = disk_fixture();
        d.size = 512 * 1_000_000_000;
        let p = |number, offset, size, like: &Partition| Partition {
            number,
            offset,
            size,
            ..like.clone()
        };
        let like = d.partitions[0].clone();
        d.partitions = vec![
            p(1, MIB, 260 * MIB, &like),
            p(2, 261 * MIB, 16 * MIB, &like),
            p(3, 277 * MIB, 400 * 1024 * MIB, &like),
        ];
        d
    }

    #[test]
    fn tiny_msr_on_big_disk_gets_minimum_width() {
        let d = big_disk_with_msr();
        let l = bar_layout(&d, 1000);
        assert_eq!(sum(&l), 1000);
        let msr = l.iter().find(|s| s.2 == Some(261 * MIB)).unwrap();
        assert!(msr.1 >= MIN_SEGMENT_PX, "msr width {}", msr.1);
        assert!(l.iter().all(|s| s.1 >= MIN_SEGMENT_PX));
    }

    #[test]
    fn free_extent_appears_as_none() {
        let d = big_disk_with_msr();
        let l = bar_layout(&d, 1000);
        let free: Vec<_> = l.iter().filter(|s| s.2.is_none()).collect();
        assert!(!free.is_empty(), "layout: {l:?}");
        // partitions still present as Some(offset), in order
        let offs: Vec<u64> = l.iter().filter_map(|s| s.2).collect();
        assert_eq!(offs, vec![MIB, 261 * MIB, 277 * MIB]);
        // the free space after the last partition comes last
        assert_eq!(l.last().unwrap().2, None);
    }

    #[test]
    fn crowded_bar_still_sums_exactly() {
        let d = disk_fixture();
        // 4 partitions + free gaps cannot all fit at 24 px in 60 px: equal split fallback
        let l = bar_layout(&d, 60);
        assert_eq!(sum(&l), 60);
    }

    #[test]
    fn empty_disk_is_one_free_segment() {
        let mut d = disk_fixture();
        d.partitions.clear();
        let l = bar_layout(&d, 500);
        assert_eq!(l, vec![(0, 500, None)]);
    }
}
