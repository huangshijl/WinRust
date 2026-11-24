//! The look of the file manager window, in the same palette as topbar: the top part (tabs, back / forward / up / reload,
//! address pill) and the status line are painted by hand (double buffered, eased hover, click flash); the file list is the
//! native list themed dark with taller rows. Everything is scaled by the system DPI.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::{cell::Cell, collections::HashMap, path::Path};
use windows_sys::Win32::{Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*}, System::{LibraryLoader::{GetProcAddress, LoadLibraryW}, SystemInformation::GetTickCount64}, UI::Input::KeyboardAndMouse::{GetFocus, GetKeyState, ReleaseCapture, SetCapture, TRACKMOUSEEVENT, TME_LEAVE, TrackMouseEvent, VK_CONTROL}};
use winrust_common::{lang, Lang, t, tf};

// The window follows the Windows 11 File Explorer look: the app light/dark theme from Windows, the accent from DWM.
// It is deliberately independent of the WinRust styles the rest of the suite uses.
#[derive(Clone, Copy)]
pub struct Palette { pub bg: u32, pub card: u32, pub text: u32, pub dim: u32, pub line: u32, pub hover: u32, pub active: u32, pub accent: u32, pub pill: u32, pub pill_line: u32, pub sel: u32, pub sel_off: u32, pub row_hover: u32 }

const LIGHT_PAL: Palette = Palette { bg: 0x00F3F3F3, card: 0x00FFFFFF, text: 0x001A1A1A, dim: 0x005F5F5F, line: 0x00E5E5E5, hover: 0x00EAEAEA, active: 0x00DFDFDF, accent: 0x00D77800, pill: 0x00FFFFFF, pill_line: 0x00D6D6D6, sel: 0x00F7E4CC, sel_off: 0x00E9E9E9, row_hover: 0x00F5F5F5 };
const DARK_PAL: Palette = Palette { bg: 0x00202020, card: 0x00282828, text: 0x00FFFFFF, dim: 0x00A0A0A0, line: 0x00333333, hover: 0x002D2D2D, active: 0x00383838, accent: 0x00D77800, pill: 0x002D2D2D, pill_line: 0x003D3D3D, sel: 0x00664B2D, sel_off: 0x00333333, row_hover: 0x002C2C2C };

fn reg_dword(sub: &str, name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (mut v, mut cb) = (0u32, 4u32);
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, w(sub).as_ptr(), w(name).as_ptr(), RRF_RT_REG_DWORD, ptr::null_mut(), &mut v as *mut _ as *mut _, &mut cb) };
    (rc == 0).then_some(v)
}

fn apps_light() -> bool { reg_dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "AppsUseLightTheme").unwrap_or(1) != 0 }
fn system_accent() -> u32 { reg_dword(r"Software\Microsoft\Windows\DWM", "AccentColor").map_or(0x00D77800, |v| v & 0x00FF_FFFF) }

thread_local! { static PAL11: Cell<(u64, Palette)> = const { Cell::new((0, LIGHT_PAL)) }; }

/// The app theme colours, refreshed at most once a second (theme and accent changes come through WM_SETTINGCHANGE).
pub fn pal() -> Palette {
    let t = now();
    let (at, p) = PAL11.get();
    if at != 0 && t.saturating_sub(at) < 1000 { return p; }
    let mut p = if apps_light() { LIGHT_PAL } else { DARK_PAL };
    p.accent = system_accent();
    PAL11.set((t, p));
    p
}

pub fn light() -> bool { apps_light() }

/// The WinRust style changed or Windows switched theme: the next pal() reads the registry again.
pub fn theme_invalidate() { PAL11.set((0, LIGHT_PAL)); }

pub fn ui_face() -> &'static str { "Segoe UI" }

pub fn listbg() -> u32 { pal().card }
pub fn pill() -> u32 { pal().pill }
pub fn off() -> u32 { mix(pal().dim, pal().bg, 0.45) }

pub const CHROME_H: i32 = 126;
pub const STATUS_H: i32 = 24;
pub const WM_UI: u32 = WM_APP + 4; // chrome -> window: wp = action (see Hit), lp = tab index
const WM_MOUSELEAVE: u32 = 0x02A3; // not exported under the enabled features

// what the pointer is over / what a click does
const NONE: i32 = -1;
const BACK: i32 = 1;
const FWD: i32 = 2;
const UP: i32 = 3;
const RELOAD: i32 = 4;
const PLUS: i32 = 5;
const NEW: i32 = 20;
const NEW_MORE: i32 = 21;
const CUT: i32 = 22;
const COPY: i32 = 23;
const PASTE: i32 = 24;
const PASTE_MORE: i32 = 25;
const RENAME: i32 = 26;
const DELETE: i32 = 27;
const SHARE: i32 = 28;
const SORT: i32 = 29;
const VIEW: i32 = 31;
const MORE: i32 = 33;
const SEARCH_BOX: i32 = 34;
const SEARCH_CLEAR: i32 = 35;
const WMIN: i32 = 41; // window buttons, in the tab row (which is the title bar: the window has no caption)
const WMAX: i32 = 42;
const WCLOSE: i32 = 43;
/// The tab row doubles as the title bar: an empty spot drags the window, the buttons sit at its right.
const TITLE_H: i32 = 40;
const TAB: i32 = 100; // + index
const CLOSE: i32 = 200; // + index
const CRUMB: i32 = 400; // + index: a part of the path in the address pill

thread_local! {
    static CRUMBS: RefCell<Vec<(RECT, PathBuf)>> = const { RefCell::new(Vec::new()) }; // where each path part was painted
    static SEARCHING: RefCell<Option<String>> = const { RefCell::new(None) }; // the list shows hits for this query
    static PENDING_MENU: Cell<i32> = const { Cell::new(NONE) }; // a command-bar menu opens on the button release
    static TAB_DRAG: Cell<Option<(usize, i32)>> = const { Cell::new(None) }; // (index, start x) of a tab being dragged
    static TAB_DROP: Cell<usize> = const { Cell::new(0) }; // insertion index while dragging
    static TAB_MOVED: Cell<bool> = const { Cell::new(false) };
}

/// The list shows search hits for `q` (None: a folder again); the pill says so instead of showing the path.
pub fn set_search(q: Option<String>) { SEARCHING.set(q); }

pub fn searching() -> bool { SEARCHING.with_borrow(|s| s.is_some()) }

/// Folder of the path part `i` of the address pill.
pub fn crumb_path(i: usize) -> Option<PathBuf> { CRUMBS.with_borrow(|c| c.get(i).map(|x| x.1.clone())) }

fn crumb_at(x: i32, y: i32) -> Option<usize> { CRUMBS.with_borrow(|c| c.iter().position(|(r, _)| inside(r, x, y))) }

/// Path parts with the folder each one opens: C:\Users\me -> [("C:", C:\), ("Users", C:\Users), ("me", C:\Users\me)].
fn crumbs_of(dir: &Path) -> Vec<(String, PathBuf)> {
    if dir.to_string_lossy().starts_with("::") { return vec![(crate::label(&dir.to_path_buf()), dir.to_path_buf())]; }
    let mut acc = PathBuf::new();
    dir.components().filter_map(|c| {
        acc.push(c.as_os_str());
        match c {
            std::path::Component::RootDir => None, // folded into the drive part
            std::path::Component::Prefix(p) => { acc.push("\\"); Some((p.as_os_str().to_string_lossy().into_owned(), acc.clone())) }
            _ => Some((c.as_os_str().to_string_lossy().into_owned(), acc.clone())),
        }
    }).collect()
}

thread_local! {
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
    static HV: RefCell<HashMap<i32, (f32, u64)>> = RefCell::new(HashMap::new()); // eased hover per element: (0..1, last update)
    static BUSY: Cell<bool> = const { Cell::new(false) }; // something is still easing: paint another frame
    static HOT: Cell<i32> = const { Cell::new(NONE) };
    static PRESS: Cell<(i32, u64)> = const { Cell::new((i32::MIN, 0)) };
    static BRUSH: Cell<isize> = const { Cell::new(0) }; // address box background
}

pub fn init_scale() {
    unsafe {
        let dc = GetDC(ptr::null_mut());
        SCALE.set(GetDeviceCaps(dc, LOGPIXELSX as i32) as f32 / 96.0);
        ReleaseDC(ptr::null_mut(), dc);
    }
}

pub fn sc(v: i32) -> i32 { (v as f32 * SCALE.get()).round() as i32 }

fn now() -> u64 { unsafe { GetTickCount64() } }

fn ease(t: f32) -> f32 { t * t * (3.0 - 2.0 * t) }

pub(crate) fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
        (x + (y - x) * t) as u32
    };
    ch(0) | ch(8) << 8 | ch(16) << 16
}

/// Eased 0..1 hover value of one element; keeps the painter running while it moves.
fn hv(key: i32, on: bool) -> f32 {
    let t = now();
    let target = if on { 1.0 } else { 0.0 };
    let v = HV.with_borrow_mut(|m| {
        let e = m.entry(key).or_insert((0.0, t));
        let dt = t.saturating_sub(e.1).min(50) as f32;
        e.1 = t;
        e.0 = if e.0 < target { (e.0 + dt / 120.0).min(target) } else { (e.0 - dt / 200.0).max(target) };
        e.0
    });
    if v != target { BUSY.set(true); }
    ease(v)
}

/// 1.0 right after a click, easing to 0 over 320 ms.
fn press_t(key: i32) -> f32 {
    let (k, t0) = PRESS.get();
    let t = now().saturating_sub(t0) as f32 / 320.0;
    if k != key || t >= 1.0 { return 0.0; }
    BUSY.set(true);
    (1.0 - t) * (1.0 - t)
}

pub fn font(px: i32, weight: i32, face: &str) -> isize {
    unsafe { CreateFontW(-sc(px), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, w(face).as_ptr()) as isize }
}

// ---------- window frame ----------

/// Dark title bar in the bar's colours, rounded corners.
pub fn dark_title(h: HWND) {
    unsafe {
        let set = |attr: u32, v: i32| { DwmSetWindowAttribute(h, attr, &v as *const _ as _, 4); };
        set(20, !light() as i32); // immersive dark mode when Windows is in the dark app theme
        set(33, 2); // round corners, like Explorer
        set(35, pal().bg as i32); // caption
        set(36, pal().text as i32); // caption text
        set(34, pal().line as i32); // border
    }
}

/// Dark context menus / scrollbars for the whole process (uxtheme's unnamed exports 135 and 136). Missing = stays light.
pub fn dark_mode() {
    unsafe {
        let m = LoadLibraryW(w("uxtheme.dll").as_ptr());
        if m.is_null() { return; }
        if let Some(f) = GetProcAddress(m, 135usize as *const u8) { std::mem::transmute::<_, extern "system" fn(i32) -> i32>(f)(2); } // SetPreferredAppMode(ForceDark)
        if let Some(f) = GetProcAddress(m, 136usize as *const u8) { std::mem::transmute::<_, extern "system" fn()>(f)(); } // FlushMenuThemes
    }
}

/// The WinRust style changed (the topbar's broadcast): the lists take the new colours and font, everything repaints.
pub unsafe fn restyle(h: HWND) {
    unsafe extern "system" fn each(c: HWND, _: LPARAM) -> i32 {
        let mut cls = [0u16; 32];
        let n = GetClassNameW(c, cls.as_mut_ptr(), 32).max(0) as usize;
        if String::from_utf16_lossy(&cls[..n]) == "SysListView32" {
            style_list(c);
        }
        1
    }
    EnumChildWindows(h, Some(each), 0);
    // the list rows' cached fonts and the title bar follow too
    let (a, b) = FONTS.replace((0, 0));
    for f in [a, b].into_iter().filter(|&f| f != 0) {
        DeleteObject(f as HGDIOBJ);
    }
    dark_title(h);
    // the window class was registered with the old background brush (shows while a pane resizes)
    let old = SetClassLongPtrW(h, GCLP_HBRBACKGROUND, CreateSolidBrush(pal().bg) as isize);
    DeleteObject(old as HGDIOBJ);
    RedrawWindow(h, ptr::null(), ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME);
}

/// Dark list: themed scrollbars and header, own colours and font, taller rows.
pub fn style_list(list: HWND) {
    unsafe {
        // the header's unused strip follows the list theme: a dark theme on a light window leaves it black
        SetWindowTheme(list, w(if light() { "Explorer" } else { "DarkMode_Explorer" }).as_ptr(), ptr::null());
        let hdr = SendMessageW(list, LVM_GETHEADER, 0, 0) as HWND;
        SetWindowTheme(hdr, w(if light() { "ItemsView" } else { "DarkMode_ItemsView" }).as_ptr(), ptr::null());
        for (m, c) in [(LVM_SETBKCOLOR, listbg()), (LVM_SETTEXTBKCOLOR, listbg()), (LVM_SETTEXTCOLOR, pal().text)] { SendMessageW(list, m, 0, c as isize); }
        SendMessageW(list, WM_SETFONT, font(14, 400, ui_face()) as usize, 1);
        row_height(list, true);
        SetWindowSubclass(list, Some(list_sub), 1, 0); // paints the column header (the theme alone leaves it dim)
    }
}

/// A blank last column fills the rest of the list width, so the header has no empty strip to paint.
pub unsafe fn fit_filler(list: HWND) {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(list, &mut cr);
    let rest: i32 = (0..4).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).sum();
    SendMessageW(list, LVM_SETCOLUMNWIDTH, 4, (cr.right - rest).max(0) as isize);
}

pub const ROW_H: i32 = 28;

/// Details rows get ROW_H tall through an empty 1 px wide state image list (a row is as tall as its tallest image).
/// Off for the thumbnail view, where a state image would take room next to every thumbnail.
pub fn row_height(list: HWND, on: bool) {
    unsafe {
        let h = if COMPACT.load(Relaxed) { 22 } else { ROW_H };
        let il = if on { ImageList_Create(1, sc(h), ILC_COLOR32, 1, 1) as isize } else { 0 };
        let old = SendMessageW(list, LVM_SETIMAGELIST, LVSIL_STATE as usize, il);
        if old != 0 { ImageList_Destroy(old as _); }
    }
}

thread_local! {
    static ROWHOT: Cell<isize> = const { Cell::new(-1) }; // list row under the pointer
    static FONTS: Cell<(isize, isize)> = const { Cell::new((0, 0)) }; // (name, details) fonts of the list rows
}

/// "Today, 09:48" / "Yesterday, 21:50" for recent files, the plain date otherwise.
/// "3 items" with the right plural: the status line counts rows and the selection.
fn items_text(k: usize) -> String {
    match lang!() {
        Lang::Ru => match (k % 10, k % 100) {
            (1, x) if x != 11 => format!("{k} объект"),
            (2..=4, x) if !(12..=14).contains(&x) => format!("{k} объекта"),
            _ => format!("{k} объектов"),
        },
        Lang::Ja => format!("{k} 個の項目"),
        Lang::Pt => format!("{k} ite{}", if k == 1 { "m" } else { "ns" }),
        Lang::Zh => format!("{k} 个项目"),
        Lang::En => format!("{k} item{}", if k == 1 { "" } else { "s" }),
    }
}

/// The details rows, painted by hand (NM_CUSTOMDRAW of the list): a rounded accent pill with a marker for the selection,
/// a soft one under the pointer, the name bright and the rest dimmed. The thumbnail view keeps the list's own drawing.
pub unsafe fn list_draw(lp: LPARAM) -> LRESULT {
    let cd = &*(lp as *const NMLVCUSTOMDRAW);
    let list = cd.nmcd.hdr.hwndFrom;
    if ST.with(|c| c.try_borrow().map_or(true, |s| s.thumbs)) { return CDRF_DODEFAULT as isize; }
    let dc = cd.nmcd.hdc;
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(list, &mut cr);
    match cd.nmcd.dwDrawStage {
        CDDS_PREPAINT => (CDRF_NOTIFYITEMDRAW | CDRF_NOTIFYPOSTPAINT) as isize,
        CDDS_POSTPAINT => {
            if SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) == 0 {
                // an empty folder says so instead of showing a blank list
                let (f_big, f_txt) = (font(40, 400, winrust_common::style::icon_face()) as HFONT, font(14, 400, ui_face()) as HFONT);
                SetBkMode(dc, TRANSPARENT as i32);
                let mid = cr.bottom / 3;
                let of = SelectObject(dc, f_big);
                text(dc, "\u{E8B7}", rc(0, mid - sc(40), cr.right, mid + sc(10)), mix(listbg(), pal().dim, 0.5), DT_CENTER);
                SelectObject(dc, f_txt);
                let msg = if crate::cur_dir().to_string_lossy() == "::recycle" {
                    t!("Корзина пуста", "Recycle Bin is empty", ja: "ごみ箱は空です", pt: "A Lixeira está vazia", zh: "回收站为空")
                } else if SEARCHING.with_borrow(|s| s.is_some()) {
                    t!("Ничего не найдено", "Nothing found", ja: "何も見つかりません", pt: "Nada encontrado", zh: "未找到任何内容")
                } else {
                    t!("Эта папка пуста", "This folder is empty", ja: "このフォルダーは空です", pt: "Esta pasta está vazia", zh: "此文件夹为空")
                };
                text(dc, msg, rc(0, mid + sc(14), cr.right, mid + sc(40)), pal().dim, DT_CENTER);
                SelectObject(dc, of);
                DeleteObject(f_big);
                DeleteObject(f_txt);
            }
            CDRF_DODEFAULT as isize
        }
        CDDS_ITEMPREPAINT => {
            let i = cd.nmcd.dwItemSpec;
            let row = ST.with(|c| {
                let mut st = c.try_borrow_mut().ok()?;
                let State { dir, rows, .. } = &mut *st;
                let r = rows.get_mut(i)?;
                let actual = r.origin.clone().or_else(|| r.target.clone()).unwrap_or_else(|| dir.join(&r.name));
                if r.icon < 0 && !r.hdr { r.icon = icon_of(&actual, r.dir); }
                Some((r.name.clone(), r.target.is_some(), r.kind.clone(), r.dir, r.time, r.size, r.icon, r.hdr))
            });
            let Some((name, virtual_row, row_kind, is_dir, time, size, icon, hdr)) = row else { return CDRF_DODEFAULT as isize };
            let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
            SendMessageW(list, LVM_GETITEMRECT, i, &mut r as *mut _ as isize);
            let row_r = rc(0, r.top, cr.right, r.bottom);
            let b = CreateSolidBrush(listbg());
            FillRect(dc, &row_r, b);
            DeleteObject(b);
            if hdr {
                // a group heading with the fold chevron, never highlighted
                let x0 = r.left;
                if FONTS.get().0 == 0 { FONTS.set((font(13, 400, ui_face()), font(12, 400, ui_face()))); }
                SetBkMode(dc, TRANSPARENT as i32);
                let label = &name[1..];
                let collapsed = crate::group_collapsed(label);
                let (f_head, f_ch) = (font(13, 600, ui_face()) as HFONT, font(11, 400, winrust_common::style::icon_face()) as HFONT);
                let of = SelectObject(dc, f_ch);
                text(dc, if collapsed { "\u{E76C}" } else { "\u{E70D}" }, rc(x0 + sc(6), r.top, x0 + sc(24), r.bottom), pal().dim, DT_CENTER);
                SelectObject(dc, f_head);
                text(dc, label, rc(x0 + sc(26), r.top + sc(4), cr.right, r.bottom), pal().text, DT_LEFT);
                SelectObject(dc, of);
                DeleteObject(f_head);
                DeleteObject(f_ch);
                return CDRF_SKIPDEFAULT as isize;
            }
            let sel = SendMessageW(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
            let hot = ROWHOT.get() == i as isize;
            let focus = GetFocus() == list;
            let bg = match (sel, focus) {
                (true, true) => pal().sel,
                (true, false) => pal().sel_off,
                _ if hot => pal().row_hover,
                _ => listbg(),
            };
            if bg != listbg() { fill(dc, row_r, bg, 0); }
            // columns from the header widths (left of the row = minus the horizontal scroll)
            let cw: Vec<i32> = (0..4).map(|k| SendMessageW(list, LVM_GETCOLUMNWIDTH, k, 0) as i32).collect();
            let x: Vec<i32> = (0..4).map(|k| r.left + cw[..k].iter().sum::<i32>()).collect();
            let himl = SendMessageW(list, LVM_GETIMAGELIST, LVSIL_SMALL as usize, 0);
            let (mut iw, mut ih) = (0, 0);
            ImageList_GetIconSize(himl as _, &mut iw, &mut ih);
            if icon >= 0 { ImageList_Draw(himl as _, icon, dc, x[0] + sc(16), (r.top + r.bottom - ih) / 2, ILD_TRANSPARENT); }
            if FONTS.get().0 == 0 { FONTS.set((font(13, 400, ui_face()), font(12, 400, ui_face()))); }
            let (f_name, f_det) = FONTS.get();
            SetBkMode(dc, TRANSPARENT as i32);
            let of = SelectObject(dc, f_name as HFONT);
            let p = Path::new(&name);
            let hit_of_search = !virtual_row && p.is_absolute(); // search hits are absolute paths: the name column shows just the file name
            let shown = if virtual_row { name.clone() } else if hit_of_search { p.file_name().map_or(name.clone(), |n| n.to_string_lossy().into_owned()) } else { crate::shown_name(&name, false, is_dir) };
            let shown = if hit_of_search { crate::shown_name(&shown, false, is_dir) } else { shown };
            text(dc, &shown, rc(x[0] + sc(16) + iw + sc(12), r.top, x[1] - sc(8), r.bottom), pal().text, DT_LEFT);
            SelectObject(dc, f_det as HFONT);
            let dim = if sel { mix(pal().dim, pal().text, 0.45) } else { pal().dim };
            text(dc, &date_text(time), rc(x[1] + sc(10), r.top, x[2] - sc(8), r.bottom), dim, DT_LEFT);
            let kind = row_kind.unwrap_or_else(|| type_text(&name, is_dir));
            text(dc, &kind, rc(x[2] + sc(10), r.top, x[3] - sc(8), r.bottom), dim, DT_LEFT | DT_PATH_ELLIPSIS);
            if !is_dir || size > 0 { text(dc, &size_text(size), rc(x[3], r.top, x[3] + cw[3] - sc(14), r.bottom), dim, DT_RIGHT); }
            SelectObject(dc, of);
            CDRF_SKIPDEFAULT as isize
        }
        _ => CDRF_DODEFAULT as isize,
    }
}

/// Repaints one row (hover moved on or off it).
unsafe fn redraw_row(list: HWND, i: isize) {
    if i < 0 { return; }
    let mut r = RECT { left: LVIR_BOUNDS as i32, ..std::mem::zeroed() };
    if SendMessageW(list, LVM_GETITEMRECT, i as usize, &mut r as *mut _ as isize) != 0 {
        let mut cr: RECT = std::mem::zeroed();
        GetClientRect(list, &mut cr);
        InvalidateRect(list, &rc(0, r.top, cr.right, r.bottom), 0);
    }
}

/// Column header, drawn by hand: bar colour, white / dim title, a sort arrow on the sorted column. The header reports its
/// custom draw to its parent, the list, so the hook sits on the list.
unsafe extern "system" fn list_sub(h: HWND, m: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if m == WM_MOUSEWHEEL && GetKeyState(VK_CONTROL as i32) < 0 {
        let d = ((wp >> 16) & 0xFFFF) as i16;
        if d != 0 { PostMessageW(GetAncestor(h, GA_ROOT), WM_UI, 87, if d > 0 { 1 } else { -1 }); }
        return 0;
    }
    if m == WM_MOUSEMOVE {
        // hover row for list_draw
        let mut ht: LVHITTESTINFO = std::mem::zeroed();
        ht.pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
        let i = SendMessageW(h, LVM_HITTEST, 0, &mut ht as *mut _ as isize);
        let old = ROWHOT.replace(i);
        if old != i {
            redraw_row(h, old);
            redraw_row(h, i);
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
        }
    } else if m == WM_MOUSELEAVE {
        redraw_row(h, ROWHOT.replace(-1));
    } else if m == WM_SETFOCUS || m == WM_KILLFOCUS {
        InvalidateRect(h, ptr::null(), 0); // the selection is accent-coloured only while the list has the focus
    }
    if m == WM_NOTIFY {
        let nm = &*(lp as *const NMCUSTOMDRAW);
        let hdr = SendMessageW(h, LVM_GETHEADER, 0, 0) as HWND;
        if nm.hdr.hwndFrom == hdr {
            use windows_sys::Win32::UI::Controls::{HDN_DIVIDERDBLCLICKW, HDN_ENDTRACKW, NMHEADERW};
            if nm.hdr.code == HDN_ENDTRACKW {
                fit_filler(h);
                crate::save_current_view(); // the folder remembers its column widths
            } else if nm.hdr.code == HDN_DIVIDERDBLCLICKW {
                let i = (*(lp as *const NMHEADERW)).iItem as usize;
                SendMessageW(h, LVM_SETCOLUMNWIDTH, i, LVSCW_AUTOSIZE_USEHEADER as isize);
                fit_filler(h);
                crate::save_current_view();
                return 0;
            }
        }
        if nm.hdr.hwndFrom == hdr && nm.hdr.code == NM_CUSTOMDRAW {
            return match nm.dwDrawStage {
                CDDS_PREPAINT => {
                    let b = CreateSolidBrush(listbg()); // the bar behind the columns too
                    FillRect(nm.hdc, &nm.rc, b);
                    DeleteObject(b);
                    CDRF_NOTIFYITEMDRAW as isize
                }
                CDDS_ITEMPREPAINT => {
                    let (dc, r, i) = (nm.hdc, nm.rc, nm.dwItemSpec);
                    let b = CreateSolidBrush(listbg());
                    FillRect(dc, &r, b);
                    DeleteObject(b);
                    if i == 4 { return CDRF_SKIPDEFAULT as isize; } // the blank strip after the real columns
                    let mut buf = [0u16; 128];
                    let mut it: HDITEMW = std::mem::zeroed();
                    it.mask = HDI_TEXT;
                    it.pszText = buf.as_mut_ptr();
                    it.cchTextMax = 128;
                    SendMessageW(hdr, HDM_GETITEMW, i, &mut it as *mut _ as isize);
                    let sorted = SORT_COL.load(Relaxed) == i;
                    let arrow = if !sorted { "" } else if SORT_ASC.load(Relaxed) { "  \u{2191}" } else { "  \u{2193}" };
                    let f = font(12, if sorted { 600 } else { 400 }, ui_face()) as HFONT;
                    let of = SelectObject(dc, f);
                    SetBkMode(dc, TRANSPARENT as i32);
                    let align = if i == 3 { DT_RIGHT } else { DT_LEFT }; // the size column is right-aligned like its numbers
                    text(dc, &format!("{}{arrow}", wstr(buf.as_ptr())), rc(r.left + sc(10), r.top, r.right - sc(12), r.bottom), if sorted { pal().text } else { pal().dim }, align);
                    SelectObject(dc, of);
                    DeleteObject(f);
                    CDRF_SKIPDEFAULT as isize
                }
                _ => 0,
            };
        }
    }
    DefSubclassProc(h, m, wp, lp)
}

// ---------- layout and hit testing ----------

struct Layout { tabs: Vec<RECT>, close: Vec<RECT>, plus: RECT, back: RECT, fwd: RECT, up: RECT, reload: RECT, pill: RECT, search: RECT, search_clear: RECT, new: RECT, new_more: RECT, cut: RECT, copy: RECT, paste: RECT, paste_more: RECT, rename: RECT, delete: RECT, share: RECT, sort: RECT, view: RECT, more: RECT, wmin: RECT, wmax: RECT, wclose: RECT, labels: bool, seps: Vec<i32> }

fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT { RECT { left: l, top: t, right: r, bottom: b } }

thread_local! { static LABELW: RefCell<HashMap<String, i32>> = RefCell::new(HashMap::new()); }

/// Width of a command-bar label in the small UI font (cached, so layout is cheap on every paint).
fn label_w(s: &str) -> i32 {
    LABELW.with_borrow_mut(|m| *m.entry(s.to_owned()).or_insert_with(|| unsafe {
        let dc = GetDC(ptr::null_mut());
        let f = font(12, 400, ui_face());
        let old = SelectObject(dc, f as HGDIOBJ);
        let u: Vec<u16> = s.encode_utf16().collect();
        let mut sz: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(dc, u.as_ptr(), u.len() as i32, &mut sz);
        SelectObject(dc, old);
        DeleteObject(f as HGDIOBJ);
        ReleaseDC(ptr::null_mut(), dc);
        sz.cx
    }))
}

/// The Win11 three-row chrome: the tab row (also the title bar, with the window buttons), navigation + address + search,
/// command bar.
fn layout(cw: i32, n: usize) -> Layout {
    let pad = sc(8);
    let tbtn = sc(46);
    let (wmin, wmax, wclose) = (rc((cw - tbtn * 3).max(0), 0, (cw - tbtn * 2).max(0), sc(TITLE_H)), rc((cw - tbtn * 2).max(0), 0, (cw - tbtn).max(0), sc(TITLE_H)), rc((cw - tbtn).max(0), 0, cw, sc(TITLE_H)));
    let (ty0, ty1) = (sc(5), sc(37));
    let plus_w = sc(28);
    let avail = (cw - pad * 2 - plus_w - sc(6) - tbtn * 3 - sc(6)).max(sc(120));
    let tw = (avail / n.max(1) as i32).clamp(sc(110), sc(240));
    let tabs: Vec<RECT> = (0..n as i32).map(|i| { let l = pad + i * (tw + sc(4)); rc(l, ty0, l + tw, ty1) }).collect();
    let close = tabs.iter().map(|t| rc(t.right - sc(26), (t.top + t.bottom) / 2 - sc(9), t.right - sc(8), (t.top + t.bottom) / 2 + sc(9))).collect();
    let px = tabs.last().map_or(pad, |t| t.right + sc(4));
    let plus = rc(px, ty0 + sc(3), px + plus_w, ty1 - sc(3));

    let (ny0, ny1) = (sc(43), sc(79));
    let bs = sc(32);
    let nav = |i: i32| rc(pad + i * (bs + sc(2)), ny0, pad + i * (bs + sc(2)) + bs, ny1);
    let search_w = (cw / 4).clamp(sc(150), sc(280));
    let search = rc(cw - sc(12) - search_w, ny0, cw - sc(12), ny1);
    let search_clear = rc(search.right - sc(28), search.top + sc(4), search.right - sc(4), search.bottom - sc(4));
    let reload = nav(3);
    let pill = rc(reload.right + sc(8), ny0, (search.left - sc(10)).max(reload.right + sc(140)), ny1);

    let (cy0, cy1) = (sc(85), sc(119));
    let labels = cw >= sc(1060);
    let btn = |x: i32, w: i32| rc(x, cy0, x + w, cy1);
    let [l_new, l_cut, l_copy, l_paste, l_rename, l_delete, l_share, l_sort, l_view] = [
        t!("Создать", "New", ja: "新規作成", pt: "Novo", zh: "新建"),
        t!("Вырезать", "Cut", ja: "切り取り", pt: "Recortar", zh: "剪切"),
        t!("Копировать", "Copy", ja: "コピー", pt: "Copiar", zh: "复制"),
        t!("Вставить", "Paste", ja: "貼り付け", pt: "Colar", zh: "粘贴"),
        t!("Переименовать", "Rename", ja: "名前の変更", pt: "Renomear", zh: "重命名"),
        t!("Удалить", "Delete", ja: "削除", pt: "Excluir", zh: "删除"),
        t!("Общий доступ", "Share", ja: "共有", pt: "Compartilhar", zh: "共享"),
        t!("Сортировка", "Sort", ja: "並べ替え", pt: "Classificar", zh: "排序"),
        t!("Просмотр", "View", ja: "表示", pt: "Exibir", zh: "查看"),
    ];
    let item = |label: &str| if labels { sc(10) + sc(16) + sc(7) + label_w(label) + sc(10) } else { sc(40) };
    let mut x = pad;
    let new_main = if labels { sc(10) + sc(16) + sc(7) + label_w(l_new) + sc(6) } else { sc(34) };
    let new_chev = sc(20);
    let new = btn(x, new_main);
    let new_more = rc(x + new_main, cy0, x + new_main + new_chev, cy1);
    x += new_main + new_chev;
    let mut seps = vec![];
    x += sc(4); seps.push(x); x += sc(5);
    let cut = btn(x, item(l_cut)); x += cut.right - cut.left + sc(2);
    let copy = btn(x, item(l_copy)); x += copy.right - copy.left + sc(2);
    let paste_w = item(l_paste);
    let paste = btn(x, paste_w);
    let paste_more = rc(x + paste_w, cy0, x + paste_w + new_chev, cy1);
    x += paste_w + new_chev + sc(2);
    let rename = btn(x, item(l_rename)); x += rename.right - rename.left + sc(2);
    let delete = btn(x, item(l_delete)); x += delete.right - delete.left + sc(2);
    let share = if cw >= sc(620) { let r = btn(x, item(l_share)); x = r.right + sc(2); r } else { rc(0, 0, 0, 0) };
    seps.push(x + sc(2)); x += sc(9);
    let more = rc(cw - pad - sc(40), cy0, cw - pad, cy1);
    let view_w = if labels { sc(10) + sc(16) + sc(7) + label_w(l_view) + sc(8) + sc(12) } else { sc(54) };
    let view = rc(more.left - sc(4) - view_w, cy0, more.left - sc(4), cy1);
    let sort_w = if labels { sc(10) + sc(16) + sc(7) + label_w(l_sort) + sc(8) + sc(12) } else { sc(54) };
    let sort = rc(view.left - sc(2) - sort_w, cy0, view.left - sc(2), cy1);
    let _ = x;

    Layout { tabs, close, plus, back: nav(0), fwd: nav(1), up: nav(2), reload, pill, search, search_clear, new, new_more, cut, copy, paste, paste_more, rename, delete, share, sort, view, more, wmin, wmax, wclose, labels, seps }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

fn hit(l: &Layout, x: i32, y: i32) -> i32 {
    for (k, r) in [(WMIN, &l.wmin), (WMAX, &l.wmax), (WCLOSE, &l.wclose)] {
        if inside(r, x, y) { return k; }
    }
    for (i, c) in l.close.iter().enumerate() { if inside(c, x, y) { return CLOSE + i as i32; } }
    for (i, t) in l.tabs.iter().enumerate() { if inside(t, x, y) { return TAB + i as i32; } }
    for (k, r) in [(BACK, &l.back), (FWD, &l.fwd), (UP, &l.up), (RELOAD, &l.reload), (PLUS, &l.plus)] {
        if inside(r, x, y) { return k; }
    }
    if inside(&l.new_more, x, y) { return NEW_MORE; }
    if inside(&l.new, x, y) { return NEW; }
    if inside(&l.paste_more, x, y) { return PASTE_MORE; }
    if inside(&l.paste, x, y) { return PASTE; }
    for (k, r) in [(CUT, &l.cut), (COPY, &l.copy), (RENAME, &l.rename), (DELETE, &l.delete), (SHARE, &l.share), (SORT, &l.sort), (VIEW, &l.view), (MORE, &l.more)] {
        if r.right > r.left && inside(r, x, y) { return k; }
    }
    if inside(&l.search_clear, x, y) { return SEARCH_CLEAR; }
    if inside(&l.search, x, y) { return SEARCH_BOX; }
    NONE
}

/// Puts the address and search boxes inside their pills.
pub fn place_edit(chrome: HWND) {
    unsafe {
        let (edit, search) = ST.with(|c| c.try_borrow().map_or((ptr::null_mut(), ptr::null_mut()), |s| (s.edit, s.search)));
        if edit.is_null() { return; }
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(chrome, &mut r);
        let l = layout(r.right, 1);
        let eh = sc(20);
        MoveWindow(edit, l.pill.left + sc(36), (l.pill.top + l.pill.bottom - eh) / 2, (l.pill.right - l.pill.left - sc(36) - sc(14)).max(0), eh, 1);
        if GetFocus() != edit { ShowWindow(edit, SW_HIDE); } // the path parts are painted instead until the pill is clicked
        if !search.is_null() {
            let sx = l.search.left + sc(32);
            MoveWindow(search, sx, (l.search.top + l.search.bottom - eh) / 2, (l.search.right - sc(32) - sx).max(0), eh, 1);
            ShowWindow(search, SW_SHOW);
        }
    }
}

// ---------- painting ----------

/// GDI+ for anti-aliased rounded shapes (started once in main); fill() falls back to plain GDI without it.
pub fn gdiplus_start() {
    use windows_sys::Win32::Graphics::GdiPlus::{GdiplusStartup, GdiplusStartupInput};
    unsafe {
        let (mut tok, mut inp): (usize, GdiplusStartupInput) = (0, std::mem::zeroed());
        inp.GdiplusVersion = 1;
        GdiplusStartup(&mut tok, &inp, ptr::null_mut());
    }
}

/// Fills `r` (right / bottom exclusive) in `c`; `rad` > 0 rounds the corners (rad = corner diameter), anti-aliased.
pub(crate) unsafe fn fill(dc: HDC, r: RECT, c: u32, rad: i32) {
    use windows_sys::Win32::Graphics::GdiPlus::*;
    let (wd, ht) = ((r.right - r.left) as f32, (r.bottom - r.top) as f32);
    if wd <= 0.0 || ht <= 0.0 { return; }
    let mut g: *mut GpGraphics = ptr::null_mut();
    if rad <= 0 || GdipCreateFromHDC(dc, &mut g) != 0 || g.is_null() {
        let b = CreateSolidBrush(c);
        if rad <= 0 {
            FillRect(dc, &r, b);
        } else {
            let (ob, op) = (SelectObject(dc, b), SelectObject(dc, GetStockObject(NULL_PEN)));
            RoundRect(dc, r.left, r.top, r.right + 1, r.bottom + 1, rad, rad);
            SelectObject(dc, ob);
            SelectObject(dc, op);
        }
        DeleteObject(b);
        return;
    }
    GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    GdipSetPixelOffsetMode(g, PixelOffsetModeHalf); // pixel i spans [i, i+1): straight edges stay crisp
    let (x, y, d) = (r.left as f32, r.top as f32, (rad as f32).min(wd).min(ht));
    let mut path: *mut GpPath = ptr::null_mut();
    GdipCreatePath(FillModeAlternate, &mut path);
    GdipAddPathArc(path, x, y, d, d, 180.0, 90.0);
    GdipAddPathArc(path, x + wd - d, y, d, d, 270.0, 90.0);
    GdipAddPathArc(path, x + wd - d, y + ht - d, d, d, 0.0, 90.0);
    GdipAddPathArc(path, x, y + ht - d, d, d, 90.0, 90.0);
    GdipClosePathFigure(path);
    let mut br: *mut GpSolidFill = ptr::null_mut();
    GdipCreateSolidFill(0xFF00_0000 | (c & 255) << 16 | (c & 0xFF00) | (c >> 16 & 255), &mut br); // COLORREF -> ARGB
    GdipFillPath(g, br as *mut GpBrush, path);
    GdipDeleteBrush(br as *mut GpBrush);
    GdipDeletePath(path);
    GdipDeleteGraphics(g);
}

pub(crate) unsafe fn text(dc: HDC, s: &str, mut r: RECT, c: u32, flags: u32) {
    SetTextColor(dc, c);
    DrawTextW(dc, w(s).as_ptr(), -1, &mut r, flags | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
}

/// A rounded highlight that eases in on hover and flashes in the accent colour on click; `base` is what it sits on.
unsafe fn glow(dc: HDC, key: i32, r: RECT, base: u32, to: u32, rad: i32, on: bool) {
    let (t, p) = (hv(key, on), press_t(key));
    if t > 0.01 || p > 0.0 { fill(dc, r, mix(mix(base, to, t), pal().accent, 0.45 * p), rad); }
}

fn selected_count() -> usize {
    ST.with_borrow(|st| {
        let (mut n, mut i) = (0usize, -1isize);
        loop {
            i = unsafe { SendMessageW(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize) };
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            if !r.hdr { n += 1; }
        }
        n
    })
}

fn tab_bg() -> u32 { mix(pal().bg, pal().card, 0.6) }

unsafe fn paint_chrome(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut cr);
    // everything painting needs from the window state, copied out so nothing is borrowed while drawing
    let snap = ST.with(|c| c.try_borrow().ok().map(|s| (s.tabs.iter().map(label).collect::<Vec<_>>(), s.active, s.hist.pos > 0, s.hist.pos + 1 < s.hist.v.len(), s.edit, s.search, s.dir.clone(), s.dir.to_string_lossy().starts_with("::"))));
    let Some((tab_labels, active, can_back, can_fwd, edit, search, dir, virt)) = snap else { EndPaint(hwnd, &ps); return };
    BUSY.set(false);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right, cr.bottom));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let l = layout(cr.right, tab_labels.len());
    let hot = HOT.get();
    let (f_txt, f_cmd, f_glyph, f_small) = (font(13, 400, ui_face()) as HFONT, font(12, 400, ui_face()) as HFONT, font(16, 400, winrust_common::style::icon_face()) as HFONT, font(11, 400, winrust_common::style::icon_face()) as HFONT);
    let of = SelectObject(dc, f_txt);
    let sel = selected_count();
    let zip = crate::in_zip();

    // tabs
    for (i, t) in l.tabs.iter().enumerate() {
        let on = i == active;
        let th = hv(TAB + i as i32, hot == TAB + i as i32 || hot == CLOSE + i as i32);
        if on {
            fill(dc, *t, tab_bg(), sc(6));
            fill(dc, rc(t.left + (t.right - t.left) / 2 - sc(10), t.bottom - sc(2), t.left + (t.right - t.left) / 2 + sc(10), t.bottom), pal().accent, sc(2));
        } else if th > 0.01 {
            fill(dc, *t, mix(pal().bg, pal().hover, th), sc(6));
        }
        SelectObject(dc, f_txt);
        text(dc, &tab_labels[i], rc(t.left + sc(12), t.top, t.right - sc(if tab_labels.len() > 1 { 28 } else { 10 }), t.bottom), if on { pal().text } else { mix(pal().dim, pal().text, th) }, DT_LEFT);
        if on || th > 0.05 {
            let c = l.close[i];
            glow(dc, CLOSE + i as i32, c, if on { tab_bg() } else { mix(pal().bg, pal().hover, th) }, pal().hover, sc(4), hot == CLOSE + i as i32);
            SelectObject(dc, f_small);
            text(dc, "\u{E711}", c, if hot == CLOSE + i as i32 { pal().text } else { pal().dim }, DT_CENTER);
        }
    }
    glow(dc, PLUS, l.plus, pal().bg, pal().hover, sc(4), hot == PLUS);
    SelectObject(dc, f_small);
    text(dc, "\u{E710}", l.plus, pal().dim, DT_CENTER);
    if TAB_DRAG.get().is_some() && TAB_MOVED.get() {
        let to = TAB_DROP.get().min(l.tabs.len());
        let x = if to < l.tabs.len() { l.tabs[to].left - sc(2) } else { l.tabs.last().map_or(sc(8), |t| t.right + sc(2)) };
        fill(dc, rc(x, sc(4), x + sc(2), sc(38)), pal().accent, 1); // where the tab will land
    }

    // window buttons: the tab row is the title bar
    let zoomed = IsZoomed(GetParent(hwnd)) != 0;
    for (k, r, g) in [(WMIN, l.wmin, "\u{E921}"), (WMAX, l.wmax, if zoomed { "\u{E923}" } else { "\u{E922}" }), (WCLOSE, l.wclose, "\u{E8BB}")] {
        let e = hv(k, hot == k);
        SelectObject(dc, f_small);
        if k == WCLOSE {
            if e > 0.01 { fill(dc, r, mix(pal().bg, 0x00C42B1C, e), 0); }
            text(dc, g, r, mix(pal().text, 0x00FFFFFF, e), DT_CENTER);
        } else {
            if e > 0.01 { fill(dc, r, mix(pal().bg, pal().hover, e), 0); }
            text(dc, g, r, pal().text, DT_CENTER);
        }
    }

    // navigation buttons
    SelectObject(dc, f_glyph);
    for (k, r, g, enabled) in [(BACK, l.back, "\u{E72B}", can_back), (FWD, l.fwd, "\u{E72A}", can_fwd), (UP, l.up, "\u{E74A}", true), (RELOAD, l.reload, "\u{E72C}", true)] {
        if enabled { glow(dc, k, r, pal().bg, pal().hover, sc(4), hot == k); }
        text(dc, g, r, if enabled { pal().text } else { off() }, DT_CENTER);
    }

    // address pill: accent outline while it is being edited
    let focused = !edit.is_null() && GetFocus() == edit;
    let f = hv(2000, focused);
    fill(dc, l.pill, mix(pal().pill_line, pal().accent, f), sc(6));
    fill(dc, rc(l.pill.left + 1, l.pill.top + 1, l.pill.right - 1, l.pill.bottom - 1), pal().pill, sc(5));
    SelectObject(dc, f_glyph);
    text(dc, "\u{E8B7}", rc(l.pill.left + sc(8), l.pill.top, l.pill.left + sc(32), l.pill.bottom), pal().dim, DT_CENTER);
    // not being edited: the path as clickable parts, like Windows 11 Explorer ("C: > Users > me"); the edit box is hidden
    let mut crumbs = vec![];
    if !focused {
        let (x0, x1) = (l.pill.left + sc(36), l.pill.right - sc(12));
        let (t, b) = (l.pill.top + sc(5), l.pill.bottom - sc(5));
        let parts = crumbs_of(&dir);
        let width = |s: &str| { let mut r = rc(0, 0, 0, 0); DrawTextW(dc, w(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX); r.right + sc(14) };
        let chev = sc(16);
        SelectObject(dc, f_txt);
        let ws: Vec<i32> = parts.iter().map(|p| width(&p.0)).collect();
        // too long: drop parts from the left; a "..." part then opens the last one dropped
        let mut first = 0;
        while first + 1 < parts.len() && ws[first..].iter().sum::<i32>() + chev * (parts.len() - first) as i32 + width("\u{2026}") > x1 - x0 { first += 1; }
        let mut shown: Vec<(String, PathBuf, i32)> = vec![];
        if first > 0 { shown.push(("\u{2026}".into(), parts[first - 1].1.clone(), width("\u{2026}"))); }
        shown.extend(parts[first..].iter().zip(&ws[first..]).map(|(p, &wd)| (p.0.clone(), p.1.clone(), wd)));
        let mut x = x0;
        let last = shown.len().saturating_sub(1);
        for (i, (part, path, wd)) in shown.into_iter().enumerate() {
            let r = rc(x, t, (x + wd).min(x1), b);
            glow(dc, CRUMB + i as i32, r, pal().pill, pal().hover, sc(4), hot == CRUMB + i as i32);
            SelectObject(dc, f_txt);
            text(dc, &part, r, if i == last { pal().text } else { mix(pal().dim, pal().text, if hot == CRUMB + i as i32 { 1.0 } else { 0.35 }) }, DT_CENTER);
            crumbs.push((r, path));
            x = r.right;
            if i < last {
                SelectObject(dc, f_small);
                text(dc, "\u{E76C}", rc(x, t, x + chev, b), pal().dim, DT_CENTER);
                x += chev;
            }
        }
    }
    CRUMBS.set(crumbs);

    // search box
    let s_focus = !search.is_null() && GetFocus() == search;
    let sf = hv(2100, s_focus);
    fill(dc, l.search, mix(pal().pill_line, pal().accent, sf), sc(6));
    fill(dc, rc(l.search.left + 1, l.search.top + 1, l.search.right - 1, l.search.bottom - 1), pal().pill, sc(5));
    SelectObject(dc, f_small);
    text(dc, "\u{E721}", rc(l.search.left + sc(8), l.search.top, l.search.left + sc(30), l.search.bottom), if s_focus { pal().accent } else { pal().dim }, DT_CENTER);
    if !search.is_null() && GetWindowTextLengthW(search) > 0 {
        glow(dc, SEARCH_CLEAR, l.search_clear, pal().pill, pal().hover, sc(4), hot == SEARCH_CLEAR);
        text(dc, "\u{E711}", l.search_clear, pal().dim, DT_CENTER);
    }

    // command bar: New, cut / copy / paste / rename / delete / share, then sort / view / more
    let (cut_on, copy_on, paste_on, rename_on, delete_on, share_on, new_on) = (sel > 0 && !zip && !virt, sel > 0 && !virt, !zip && !virt, sel == 1 && !zip && !virt, sel > 0 && !zip && !virt, false, !zip && !virt);
    let [c_new, c_cut, c_copy, c_paste, c_rename, c_delete, c_share, c_sort, c_view] = [
        t!("Создать", "New", ja: "新規作成", pt: "Novo", zh: "新建"),
        t!("Вырезать", "Cut", ja: "切り取り", pt: "Recortar", zh: "剪切"),
        t!("Копировать", "Copy", ja: "コピー", pt: "Copiar", zh: "复制"),
        t!("Вставить", "Paste", ja: "貼り付け", pt: "Colar", zh: "粘贴"),
        t!("Переименовать", "Rename", ja: "名前の変更", pt: "Renomear", zh: "重命名"),
        t!("Удалить", "Delete", ja: "削除", pt: "Excluir", zh: "删除"),
        t!("Общий доступ", "Share", ja: "共有", pt: "Compartilhar", zh: "共享"),
        t!("Сортировка", "Sort", ja: "並べ替え", pt: "Classificar", zh: "排序"),
        t!("Просмотр", "View", ja: "表示", pt: "Exibir", zh: "查看"),
    ];
    unsafe fn cmd(dc: HDC, key: i32, r: RECT, glyph: &str, label: &str, enabled: bool, hot: i32, labels: bool, f_glyph: HFONT, f_cmd: HFONT, off: u32) {
        if enabled { glow(dc, key, r, pal().bg, pal().hover, sc(4), hot == key); }
        let (gx, gx1) = (r.left + sc(if labels { 10 } else { 0 }), r.left + sc(if labels { 26 } else { 40 }));
        SelectObject(dc, f_glyph);
        text(dc, glyph, rc(gx, r.top, gx1, r.bottom), if enabled { pal().text } else { off }, DT_CENTER);
        if labels {
            SelectObject(dc, f_cmd);
            text(dc, label, rc(r.left + sc(32), r.top, r.right - sc(6), r.bottom), if enabled { pal().text } else { off }, DT_LEFT);
        }
    }
    cmd(dc, NEW, l.new, "\u{E710}", c_new, new_on, hot, l.labels, f_glyph, f_cmd, off());
    glow(dc, NEW_MORE, l.new_more, pal().bg, pal().hover, sc(4), hot == NEW_MORE);
    SelectObject(dc, f_small);
    text(dc, "\u{E70D}", l.new_more, pal().dim, DT_CENTER);
    cmd(dc, CUT, l.cut, "\u{E8C6}", c_cut, cut_on, hot, l.labels, f_glyph, f_cmd, off());
    cmd(dc, COPY, l.copy, "\u{E8C8}", c_copy, copy_on, hot, l.labels, f_glyph, f_cmd, off());
    cmd(dc, PASTE, l.paste, "\u{E77F}", c_paste, paste_on, hot, l.labels, f_glyph, f_cmd, off());
    glow(dc, PASTE_MORE, l.paste_more, pal().bg, pal().hover, sc(4), hot == PASTE_MORE);
    SelectObject(dc, f_small);
    text(dc, "\u{E70D}", l.paste_more, pal().dim, DT_CENTER);
    cmd(dc, RENAME, l.rename, "\u{E8AC}", c_rename, rename_on, hot, l.labels, f_glyph, f_cmd, off());
    cmd(dc, DELETE, l.delete, "\u{E74D}", c_delete, delete_on, hot, l.labels, f_glyph, f_cmd, off());
    if l.share.right > l.share.left { cmd(dc, SHARE, l.share, "\u{E72D}", c_share, share_on, hot, l.labels, f_glyph, f_cmd, off()); }
    for x in &l.seps { fill(dc, rc(*x, sc(91), *x + 1, sc(113)), pal().line, 0); }
    cmd(dc, SORT, l.sort, "\u{E8CB}", c_sort, true, hot, l.labels, f_glyph, f_cmd, off());
    SelectObject(dc, f_small);
    text(dc, "\u{E70D}", rc(l.sort.right - sc(16), l.sort.top, l.sort.right - sc(2), l.sort.bottom), pal().dim, DT_CENTER);
    cmd(dc, VIEW, l.view, "\u{E890}", c_view, true, hot, l.labels, f_glyph, f_cmd, off());
    SelectObject(dc, f_small);
    text(dc, "\u{E70D}", rc(l.view.right - sc(16), l.view.top, l.view.right - sc(2), l.view.bottom), pal().dim, DT_CENTER);
    glow(dc, MORE, l.more, pal().bg, pal().hover, sc(4), hot == MORE);
    SelectObject(dc, f_glyph);
    text(dc, "\u{E712}", l.more, pal().text, DT_CENTER);

    fill(dc, rc(0, cr.bottom - 1, cr.right, cr.bottom), pal().line, 0); // hairline under the chrome
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, of);
    for f in [f_txt, f_cmd, f_glyph, f_small] { DeleteObject(f); }
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    if BUSY.get() { SetTimer(hwnd, 4, 16, None); } else { KillTimer(hwnd, 4); }
    EndPaint(hwnd, &ps);
}

/// Item / selection counts and the sort order, along the bottom edge.
pub unsafe fn paint_status(h: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let strip = rc(0, cr.bottom - sc(STATUS_H), cr.right, cr.bottom);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &strip, bg);
    DeleteObject(bg);
    fill(dc, rc(0, strip.top, cr.right, strip.top + 1), pal().line, 0); // hairline above the status line
    let snap = ST.with(|c| {
        c.try_borrow().ok().map(|s| {
            // selected count: the status line counts rows the way Explorer does
            let (mut n, mut i) = (0usize, -1isize);
            loop {
                i = SendMessageW(s.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
                let Some(r) = usize::try_from(i).ok().and_then(|i| s.rows.get(i)) else { break };
                if r.hdr { continue; }
                n += 1;
            }
            (s.rows.iter().filter(|r| !r.hdr).count(), n)
        })
    });
    if let Some((n, sel)) = snap {
        let selected = |s: usize| tf!("выбрано: {}", "{} selected", ja: "{} 個選択", pt: "{} selecionado(s)", zh: "已选择 {} 项", s);
        let left = if sel == 0 { items_text(n) } else { format!("{}  \u{00B7}  {}", items_text(n), selected(sel)) };
        let left = crate::jobs::status_text().unwrap_or(left); // a copy, packing or unpacking in progress
        let f = font(12, 400, ui_face()) as HFONT;
        let of = SelectObject(dc, f);
        SetBkMode(dc, TRANSPARENT as i32);
        text(dc, &left, rc(strip.left + sc(14), strip.top, strip.right - sc(14), strip.bottom), pal().dim, DT_LEFT);
        SelectObject(dc, of);
        DeleteObject(f);
    }
    EndPaint(h, &ps);
}

pub unsafe fn status_dirty(h: HWND) {
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let strip = rc(0, cr.bottom - sc(STATUS_H), cr.right, cr.bottom);
    InvalidateRect(h, &strip, 0);
}

// ---------- chrome window ----------

// ---------- chrome dropdowns (the same Win11 menu as the right click) ----------

fn mi(id: u32, label: &str) -> ctxmenu::Item { ctxmenu::Item { id, text: label.into(), ..Default::default() } }
fn moff(id: u32, label: &str) -> ctxmenu::Item { ctxmenu::Item { id, text: label.into(), disabled: true, ..Default::default() } }
fn mcheck(id: u32, label: &str, on: bool) -> ctxmenu::Item { ctxmenu::Item { id, text: label.into(), checked: on, ..Default::default() } }
fn mradio(id: u32, label: &str, on: bool) -> ctxmenu::Item { ctxmenu::Item { id, text: label.into(), radio: true, checked: on, ..Default::default() } }

/// Shows a dropdown just under `at` (chrome coordinates); the picked command reaches the window as WM_UI.
unsafe fn dropdown(h: HWND, at: RECT, items: Vec<ctxmenu::Item>) -> u32 {
    let mut pt = POINT { x: at.left, y: at.bottom + sc(2) };
    ClientToScreen(h, &mut pt);
    match ctxmenu::show(GetParent(h), items, (pt.x, pt.y)) { ctxmenu::Pick::Id(id) => id, ctxmenu::Pick::None => 0 }
}

unsafe fn new_menu(h: HWND, at: RECT) {
    let items = vec![
        mi(15, t!("Папка", "Folder", ja: "フォルダー", pt: "Pasta", zh: "文件夹")),
        moff(42, t!("Ярлык", "Shortcut", ja: "ショートカット", pt: "Atalho", zh: "快捷方式")),
        mi(18, t!("Текстовый документ", "Text document", ja: "テキスト ドキュメント", pt: "Documento de texto", zh: "文本文档")),
        moff(43, t!("Точечный рисунок", "Bitmap image", ja: "ビットマップ イメージ", pt: "Imagem de bitmap", zh: "位图图像")),
        mi(19, t!("Сжатая ZIP-папка", "Compressed (zipped) folder", ja: "圧縮 (zip 形式) フォルダー", pt: "Pasta compactada (zip)", zh: "压缩(zipped)文件夹")),
    ];
    let id = dropdown(h, at, items);
    if id != 0 { PostMessageW(GetParent(h), WM_UI, id as usize, 0); }
}

unsafe fn paste_menu(h: HWND, at: RECT) {
    let mut paste = mi(12, t!("Вставить", "Paste", ja: "貼り付け", pt: "Colar", zh: "粘贴"));
    paste.glyph = "\u{E77F}";
    let items = vec![paste, moff(41, t!("Вставить ярлык", "Paste shortcut", ja: "ショートカットの貼り付け", pt: "Colar atalho", zh: "粘贴快捷方式"))];
    let id = dropdown(h, at, items);
    if id != 0 { PostMessageW(GetParent(h), WM_UI, id as usize, 0); }
}

unsafe fn sort_menu(h: HWND, at: RECT) {
    let col = SORT_COL.load(Relaxed);
    let asc = SORT_ASC.load(Relaxed);
    let items = vec![
        mradio(60, t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"), col == 0),
        mradio(62, t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"), col == 1),
        mradio(64, t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"), col == 2),
        mradio(66, t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"), col == 3),
        ctxmenu::Item::separator(),
        mradio(75, t!("По возрастанию", "Ascending", ja: "昇順", pt: "Crescente", zh: "递增"), asc),
        mradio(76, t!("По убыванию", "Descending", ja: "降順", pt: "Decrescente", zh: "递减"), !asc),
    ];
    let id = dropdown(h, at, items);
    if id == 0 { return; }
    let id = match id { 75 => 60 + col as u32 * 2, 76 => 61 + col as u32 * 2, x => x };
    PostMessageW(GetParent(h), WM_UI, id as usize, 0);
}

unsafe fn view_menu(h: HWND, at: RECT) {
    let view = ST.with_borrow(|s| s.view);
    let g = GROUP.load(Relaxed);
    let compact = COMPACT.load(Relaxed);
    let ext = SHOW_EXT.load(Relaxed);
    let checks = {
        let list = ST.with_borrow(|s| s.list);
        SendMessageW(list, LVM_GETEXTENDEDLISTVIEWSTYLE, 0, 0) as isize & LVS_EX_CHECKBOXES as isize != 0
    };
    let mut items = vec![
        mradio(80, t!("Очень крупные значки", "Extra large icons", ja: "特大アイコン", pt: "Ícones extragrandes", zh: "超大图标"), view == ViewMode::ExtraLarge),
        mradio(81, t!("Крупные значки", "Large icons", ja: "大アイコン", pt: "Ícones grandes", zh: "大图标"), view == ViewMode::Large),
        mradio(82, t!("Обычные значки", "Medium icons", ja: "中アイコン", pt: "Ícones médios", zh: "中等图标"), view == ViewMode::Medium),
        mradio(83, t!("Мелкие значки", "Small icons", ja: "小アイコン", pt: "Ícones pequenos", zh: "小图标"), view == ViewMode::Small),
        mradio(84, t!("Список", "List", ja: "一覧", pt: "Lista", zh: "列表"), view == ViewMode::List),
        mradio(85, t!("Таблица", "Details", ja: "詳細", pt: "Detalhes", zh: "详细信息"), view == ViewMode::Details),
        mradio(86, t!("Плитка", "Tiles", ja: "タイル", pt: "Blocos", zh: "平铺"), view == ViewMode::Tiles),
        ctxmenu::Item::separator(),
    ];
    let show = vec![
        mcheck(93, t!("Флажки элементов", "Item check boxes", ja: "項目のチェック ボックス", pt: "Caixas de seleção de itens", zh: "项目复选框"), checks),
        mcheck(94, t!("Расширения имён файлов", "File name extensions", ja: "ファイル名拡張子", pt: "Extensões de nome de arquivo", zh: "文件扩展名"), ext),
        mcheck(95, t!("Скрытые элементы", "Hidden items", ja: "隠しファイル", pt: "Itens ocultos", zh: "隐藏的项目"), SHOW_HIDDEN.load(Relaxed)),
        mcheck(96, t!("Компактный вид", "Compact view", ja: "コンパクト表示", pt: "Exibição compacta", zh: "紧凑视图"), compact),
    ];
    let show_entry = ctxmenu::Item { text: t!("Показать", "Show", ja: "表示", pt: "Mostrar", zh: "显示").into(), glyph: "\u{E7B3}", sub: show, ..Default::default() };
    items.push(show_entry);
    let group = vec![
        mradio(70, t!("Нет", "(None)", ja: "なし", pt: "(Nenhum)", zh: "(无)"), g == 0),
        mradio(71, t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"), g == 2),
        mradio(72, t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"), g == 1),
        mradio(73, t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"), g == 3),
        mradio(74, t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"), g == 4),
    ];
    items.push(ctxmenu::Item { text: t!("Группировать", "Group by", ja: "グループ化", pt: "Agrupar por", zh: "分组依据").into(), glyph: "\u{E8FD}", sub: group, ..Default::default() });
    let id = dropdown(h, at, items);
    if id != 0 { PostMessageW(GetParent(h), WM_UI, id as usize, 0); }
}

unsafe fn more_menu(h: HWND, at: RECT) {
    let sel = selected_count();
    let items = vec![
        mi(44, t!("Выделить все", "Select all", ja: "すべて選択", pt: "Selecionar tudo", zh: "全选")),
        mi(45, t!("Инвертировать выделение", "Invert selection", ja: "選択を反転", pt: "Inverter seleção", zh: "反向选择")),
        mi(46, t!("Копировать путь", "Copy path", ja: "パスのコピー", pt: "Copiar caminho", zh: "复制路径")),
        mi(47, t!("Открыть в новом окне", "Open in new window", ja: "新しいウィンドウで開く", pt: "Abrir em nova janela", zh: "在新窗口中打开")),
        if sel > 0 { mi(48, t!("Свойства", "Properties", ja: "プロパティ", pt: "Propriedades", zh: "属性")) } else { moff(48, t!("Свойства", "Properties", ja: "プロパティ", pt: "Propriedades", zh: "属性")) },
        moff(49, t!("Параметры", "Options", ja: "オプション", pt: "Opções", zh: "选项")),
    ];
    let id = dropdown(h, at, items);
    if id != 0 { PostMessageW(GetParent(h), WM_UI, id as usize, 0); }
}

unsafe fn tab_menu(h: HWND, i: usize, at: RECT) {
    let items = vec![
        mi(1, t!("Дублировать вкладку", "Duplicate tab", ja: "タブを複製", pt: "Duplicar guia", zh: "复制标签页")),
        mi(2, t!("Закрыть вкладку", "Close tab", ja: "タブを閉じる", pt: "Fechar guia", zh: "关闭标签页")),
        mi(3, t!("Закрыть остальные", "Close other tabs", ja: "他のタブを閉じる", pt: "Fechar outras guias", zh: "关闭其他标签页")),
        ctxmenu::Item::separator(),
        mi(4, t!("Копировать путь", "Copy path", ja: "パスのコピー", pt: "Copiar caminho", zh: "复制路径")),
    ];
    let mut pt = POINT { x: at.left, y: at.bottom };
    ClientToScreen(h, &mut pt);
    let id = match ctxmenu::show(GetParent(h), items, (pt.x, pt.y)) { ctxmenu::Pick::Id(id) => id, ctxmenu::Pick::None => 0 };
    let main = GetParent(h);
    match id {
        1 => { PostMessageW(main, WM_UI, 29, i as isize); }
        2 => { PostMessageW(main, WM_UI, 2, i as isize); }
        3 => { PostMessageW(main, WM_UI, 30, i as isize); }
        4 => { PostMessageW(main, WM_UI, 31, i as isize); }
        _ => {}
    }
}

/// Reorders the tabs and keeps the same tab active.
fn reorder_tab(from: usize, to: usize) {
    ST.with_borrow_mut(|st| {
        if from >= st.tabs.len() { return; }
        let old_active = st.active;
        let at = if to > from { to - 1 } else { to }.min(st.tabs.len() - 1);
        let p = st.tabs.remove(from);
        st.tabs.insert(at, p);
        let mut a = old_active;
        if from < a { a -= 1; }
        if at <= a { a += 1; }
        st.active = if from == old_active { at } else { a.min(st.tabs.len() - 1) };
    });
    if let Some(h) = ST.with_borrow(|st| (st.tab != 0 as HWND).then_some(st.tab)) { unsafe { InvalidateRect(h, ptr::null(), 0) }; }
}

unsafe fn history_menu(h: HWND, at: RECT, back: bool) {
    let (v, pos) = ST.with_borrow(|st| (st.hist.v.clone(), st.hist.pos));
    let mut items = vec![];
    if back { for i in (0..pos).rev() { items.push(mi(i as u32 + 1, &v[i].to_string_lossy())); } }
    else { for i in pos + 1..v.len() { items.push(mi(i as u32 + 1, &v[i].to_string_lossy())); } }
    if items.is_empty() { return; }
    let id = dropdown(h, at, items);
    if id > 0 { PostMessageW(GetParent(h), WM_UI, 23, (id - 1) as isize); }
}

/// The empty part of the tab row: it acts as the window's title bar.
fn ground(y: i32) -> bool { y < sc(TITLE_H) }

/// Dragging that empty spot moves, maximizes (to the top edge) and snaps the window as a caption does.
unsafe fn drag_window(h: HWND, x: i32, y: i32) {
    let mut pt = POINT { x, y };
    ClientToScreen(h, &mut pt);
    ReleaseCapture();
    SendMessageW(GetParent(h), WM_NCLBUTTONDOWN, HTCAPTION as usize, (pt.x as isize & 0xFFFF) | ((pt.y as isize & 0xFFFF) << 16));
}

/// Right click on that spot: the window's own menu (Move, Size, Close...), as on a real title bar.
unsafe fn system_menu(h: HWND, x: i32, y: i32) {
    let p = GetParent(h);
    let m = GetSystemMenu(p, 0);
    if m.is_null() { return; }
    let mut pt = POINT { x, y };
    ClientToScreen(h, &mut pt);
    SetForegroundWindow(p);
    let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, p, ptr::null());
    if id != 0 { PostMessageW(p, WM_SYSCOMMAND, id as usize, 0); }
}

pub unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    let layout_now = || {
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(h, &mut r);
        layout(r.right, ST.with(|c| c.try_borrow().map_or(1, |s| s.tabs.len())))
    };
    match m {
        WM_PAINT => paint_chrome(h),
        WM_ERASEBKGND => return 1,
        WM_SIZE => { place_edit(h); InvalidateRect(h, ptr::null(), 0); } // the pill and the tabs re-flow: repaint it all
        WM_TIMER if wp == 4 => { InvalidateRect(h, ptr::null(), 0); }
        WM_MOUSEMOVE => {
            if let Some((_, x0)) = TAB_DRAG.get() {
                if (x - x0).abs() > sc(6) { TAB_MOVED.set(true); }
                if TAB_MOVED.get() {
                    let l = layout_now();
                    let to = l.tabs.iter().position(|t| x < (t.left + t.right) / 2).unwrap_or(l.tabs.len());
                    if TAB_DROP.replace(to) != to { InvalidateRect(h, ptr::null(), 0); }
                }
            }
            let k = match hit(&layout_now(), x, y) { NONE => crumb_at(x, y).map_or(NONE, |i| CRUMB + i as i32), k => k };
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if HOT.replace(k) != k { InvalidateRect(h, ptr::null(), 0); }
        }
        WM_MOUSELEAVE => {
            HOT.set(NONE);
            InvalidateRect(h, ptr::null(), 0);
        }
        WM_LBUTTONDOWN | WM_MBUTTONDOWN => {
            let l = layout_now();
            let k = hit(&l, x, y);
            match k {
                WMIN => { PostMessageW(GetParent(h), WM_SYSCOMMAND, SC_MINIMIZE as usize, 0); return 0; }
                WMAX => {
                    let p = GetParent(h);
                    let c = if IsZoomed(p) != 0 { SC_RESTORE } else { SC_MAXIMIZE };
                    PostMessageW(p, WM_SYSCOMMAND, c as usize, 0);
                    return 0;
                }
                WCLOSE => { PostMessageW(GetParent(h), WM_CLOSE, 0, 0); return 0; }
                _ => {}
            }
            if k == NONE {
                if ground(y) { drag_window(h, x, y); return 0; }
                if let Some(i) = crumb_at(x, y) {
                    PRESS.set((CRUMB + i as i32, now()));
                    PostMessageW(GetParent(h), WM_UI, 21, i as isize); // a path part: go there
                } else if inside(&l.pill, x, y) {
                    // the rest of the pill: type a path
                    let edit = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.edit));
                    SetFocus(edit);
                    SendMessageW(edit, EM_SETSEL, 0, -1);
                }
                InvalidateRect(h, ptr::null(), 0);
                return 0;
            }
            match k {
                NEW | NEW_MORE | PASTE_MORE | SORT | VIEW | MORE => { PENDING_MENU.set(k); PRESS.set((k, now())); InvalidateRect(h, ptr::null(), 0); return 0; }
                SEARCH_BOX => {
                    let search = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.search));
                    SetFocus(search);
                    let n = GetWindowTextLengthW(search);
                    SendMessageW(search, EM_SETSEL, n as usize, n as isize);
                    return 0;
                }
                SEARCH_CLEAR => {
                    let search = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.search));
                    if GetWindowTextLengthW(search) == 0 {
                        SetFocus(search);
                    } else {
                        PostMessageW(GetParent(h), super::WM_SEARCH, 27, 0);
                    }
                    return 0;
                }
                _ => {}
            }
            if (TAB..CLOSE).contains(&k) {
                let i = (k - TAB) as usize;
                TAB_DRAG.set(Some((i, x)));
                TAB_DROP.set(i);
                TAB_MOVED.set(false);
                SetCapture(h);
            }
            PRESS.set((k, now()));
            InvalidateRect(h, ptr::null(), 0);
            let (act, idx) = match k {
                CUT => (10, 0), COPY => (11, 0), PASTE => (12, 0), RENAME => (13, 0), DELETE => (14, 0), SHARE => (50, 0),
                BACK => (4, 0), FWD => (5, 0), UP => (6, 0), RELOAD => (7, 0), PLUS => (3, 0),
                k if k >= CLOSE => (2, (k - CLOSE) as isize),
                k => (if m == WM_MBUTTONDOWN { 2 } else { 1 }, (k - TAB) as isize), // middle click on a tab closes it
            };
            PostMessageW(GetParent(h), WM_UI, act, idx);
        }
        WM_LBUTTONUP => {
            if let Some((from, _)) = TAB_DRAG.take() {
                ReleaseCapture();
                let moved = TAB_MOVED.replace(false);
                let to = TAB_DROP.replace(0);
                if moved && to != from && to != from + 1 { reorder_tab(from, to); }
                InvalidateRect(h, ptr::null(), 0);
                return 0;
            }
            let k = PENDING_MENU.replace(NONE);
            if k != NONE {
                let l = layout_now();
                if hit(&l, x, y) == k {
                    match k {
                        NEW | NEW_MORE => new_menu(h, rc(l.new.left, l.new.top, l.new_more.right, l.new.bottom)),
                        PASTE_MORE => paste_menu(h, l.paste_more),
                        SORT => sort_menu(h, l.sort),
                        VIEW => view_menu(h, l.view),
                        MORE => more_menu(h, l.more),
                        _ => {}
                    }
                }
            }
        }
        WM_RBUTTONUP => {
            let l = layout_now();
            if ground(y) && hit(&l, x, y) == NONE { system_menu(h, x, y); return 0; }
            if inside(&l.back, x, y) { history_menu(h, l.back, true); return 0; }
            if inside(&l.fwd, x, y) { history_menu(h, l.fwd, false); return 0; }
            if let Some(i) = l.tabs.iter().position(|t| inside(t, x, y)) {
                let main = GetParent(h);
                crate::tab_select(main, i);
                tab_menu(h, i, l.tabs[i]);
            }
        }
        WM_LBUTTONDBLCLK => {
            if ground(y) && hit(&layout_now(), x, y) == NONE {
                let p = GetParent(h);
                let c = if IsZoomed(p) != 0 { SC_RESTORE } else { SC_MAXIMIZE };
                PostMessageW(p, WM_SYSCOMMAND, c as usize, 0);
            }
        }
        WM_NCHITTEST => {
            // the top strip is the window's edge (its caption is ours): let the parent report a resize border there
            if y < sc(5) && IsZoomed(GetParent(h)) == 0 { return HTTRANSPARENT as LRESULT; }
            return DefWindowProcW(h, m, wp, lp);
        }
        WM_COMMAND if matches!((wp >> 16) as u32, 0x0100 | 0x0200) => {
            // the address box is shown only while typing (the path parts otherwise); the search box stays visible
            let edit = ST.with(|c| c.try_borrow().map_or(ptr::null_mut(), |s| s.edit));
            if lp as HWND == edit {
                ShowWindow(edit, if (wp >> 16) == 0x0100 { SW_SHOW } else { SW_HIDE });
            }
            InvalidateRect(h, ptr::null(), 0);
        }
        WM_COMMAND if (wp >> 16) as u32 == 0x0300 => { InvalidateRect(h, ptr::null(), 0); } // search text changed: the clear button
        WM_CTLCOLOREDIT => {
            // the address box: white text on the pill's colour
            let dc = wp as HDC;
            SetTextColor(dc, pal().text);
            SetBkColor(dc, pill());
            if BRUSH.get() == 0 { BRUSH.set(CreateSolidBrush(pill()) as isize); }
            return BRUSH.get();
        }
        _ => return DefWindowProcW(h, m, wp, lp),
    }
    0
}


// ---------- navigation pane (left): sections, quick access folders, drives and an expandable folder tree ----------

const NAV_W_DEFAULT: i32 = 240;
const NAV_ROW: i32 = 32;
const NAV_HEAD: i32 = 30;
const NAV_DRIVE: i32 = 48; // a drive row also shows how full it is

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum NavKind { Section, Home, Folder, Drive, Recycle, ThisPC }

struct NavItem { title: String, path: PathBuf, glyph: &'static str, cap: Option<(u64, u64)>, depth: i32, kind: NavKind, expandable: bool, expanded: bool }

fn nav_h(it: &NavItem) -> i32 {
    sc(if matches!(it.kind, NavKind::Section | NavKind::ThisPC) { NAV_HEAD } else if it.cap.is_some() { NAV_DRIVE } else { NAV_ROW })
}

thread_local! {
    static NAV: RefCell<Vec<NavItem>> = const { RefCell::new(Vec::new()) };
    static NAVWND: Cell<isize> = const { Cell::new(0) };
    static NAVHOT: Cell<i32> = const { Cell::new(-1) };
    static NAV_OPEN: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new()); // expanded folder paths
    static NAV_SCROLL: Cell<i32> = const { Cell::new(0) };
    static NAV_WIDTH: Cell<i32> = const { Cell::new(NAV_W_DEFAULT) };
    static NAV_DRAG: Cell<Option<(i32, i32)>> = const { Cell::new(None) }; // (start x, start width)
}

fn nav_width_file() -> Option<PathBuf> { env::var_os("APPDATA").map(|a| PathBuf::from(a).join("rust-explorer").join("nav-width.txt")) }

pub fn nav_width() -> i32 { NAV_WIDTH.get() }

/// Subfolders of `dir` for an expanded node, hidden and system ones left out.
fn nav_children(dir: &Path, depth: i32) -> Vec<NavItem> {
    let mut out: Vec<(String, NavItem)> = vec![];
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if !m.is_dir() || m.file_attributes() & (2 | 4) != 0 { continue; } // hidden / system
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('$') { continue; } // $Recycle.Bin, System Volume Information
        out.push((name.to_lowercase(), NavItem { title: name, path: e.path(), glyph: "\u{E8B7}", cap: None, depth, kind: NavKind::Folder, expandable: true, expanded: false }));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.truncate(500);
    out.into_iter().map(|(_, it)| it).collect()
}

/// Inserts the subfolders of node `i` right after it (one level deeper).
fn expand_vec(v: &mut Vec<NavItem>, i: usize) {
    if i >= v.len() || v[i].expanded || !v[i].expandable { return; }
    let (path, depth) = (v[i].path.clone(), v[i].depth);
    v[i].expanded = true;
    if path.to_string_lossy().starts_with("::") { return; }
    let kids = nav_children(&path, depth + 1);
    if kids.is_empty() { v[i].expandable = false; return; }
    NAV_OPEN.with_borrow_mut(|s| { s.insert(path.to_string_lossy().to_lowercase()); });
    v.splice(i + 1..i + 1, kids);
}

/// Removes the subfolders under node `i`.
fn collapse_vec(v: &mut Vec<NavItem>, i: usize) {
    if i >= v.len() || !v[i].expanded { return; }
    let depth = v[i].depth;
    let mut end = i + 1;
    while end < v.len() && v[end].depth > depth { 
        let p = v[end].path.to_string_lossy().to_lowercase();
        NAV_OPEN.with_borrow_mut(|s| { s.remove(&p); });
        end += 1;
    }
    v.drain(i + 1..end);
    v[i].expanded = false;
    NAV_OPEN.with_borrow_mut(|s| { s.remove(&v[i].path.to_string_lossy().to_lowercase()); });
}

fn expand_node(h: HWND, i: usize) {
    NAV.with_borrow_mut(|v| expand_vec(v, i));
    unsafe { InvalidateRect(h, ptr::null(), 0) };
}

fn collapse_node(h: HWND, i: usize) {
    NAV.with_borrow_mut(|v| collapse_vec(v, i));
    unsafe { InvalidateRect(h, ptr::null(), 0) };
}

/// Folders the user pinned to Quick access: %APPDATA%\WinRust\rust-explorer-pins.txt, one path per line.
fn pins_file() -> Option<PathBuf> { env::var_os("APPDATA").map(|a| PathBuf::from(a).join("WinRust").join("rust-explorer-pins.txt")) }

pub fn pins() -> Vec<PathBuf> {
    pins_file().and_then(|f| fs::read_to_string(f).ok()).map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect()).unwrap_or_default()
}

/// Pins `p` (or unpins it when already pinned) and rebuilds the left pane.
pub fn toggle_pin(p: &Path) {
    let mut v = pins();
    if let Some(i) = v.iter().position(|x| x == p) { v.remove(i); } else { v.push(p.to_path_buf()); }
    if let Some(f) = pins_file() {
        if let Some(d) = f.parent() { let _ = fs::create_dir_all(d); }
        let _ = fs::write(f, v.iter().map(|x| x.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n"));
    }
    let h = NAVWND.get() as HWND;
    if !h.is_null() { nav_init(h); nav_dirty(); }
}

pub struct DriveInfo { pub title: String, pub root: PathBuf, pub cap: Option<(u64, u64)>, pub network: bool }

/// Fixed, removable and mapped network drives, with the volume label and free / total bytes.
pub fn drive_list() -> Vec<DriveInfo> {
    let mut out = vec![];
    unsafe {
        use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 { continue; }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            // fixed and removable disks only: asking a dead network share for its label would stall the window
            let ty = GetDriveTypeW(w(&root).as_ptr());
            if ty == 4 {
                out.push(DriveInfo { title: tf!("Сетевой диск ({}:)", "Network Drive ({}:)", ja: "ネットワーク ドライブ ({}:)", pt: "Unidade de rede ({}:)", zh: "网络驱动器（{}:）", letter), root: PathBuf::from(root), cap: None, network: true });
                continue;
            }
            if !matches!(ty, 2 | 3) { continue; }
            let mut label = [0u16; 64];
            let ok = GetVolumeInformationW(w(&root).as_ptr(), label.as_mut_ptr(), 64, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), 0);
            let name = if ok != 0 { wstr(label.as_ptr()) } else { String::new() };
            let (mut free, mut total) = (0u64, 0u64);
            let cap = (GetDiskFreeSpaceExW(w(&root).as_ptr(), &mut free, &mut total, ptr::null_mut()) != 0 && total > 0).then_some((free, total));
            let base = if name.is_empty() { t!("Локальный диск", "Local Disk", ja: "ローカル ディスク", pt: "Disco local", zh: "本地磁盘") } else { &name };
            out.push(DriveInfo { title: format!("{base} ({letter}:)"), root: PathBuf::from(root), cap, network: false });
        }
    }
    out
}

/// Quick access folders: the known user folders that exist, then the pinned ones.
pub fn quick_folders() -> Vec<(String, PathBuf, &'static str)> {
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos};
    let known = [
        (t!("Рабочий стол", "Desktop", ja: "デスクトップ", pt: "Área de Trabalho", zh: "桌面"), &FOLDERID_Desktop, "\u{E7F4}"),
        (t!("Загрузки", "Downloads", ja: "ダウンロード", pt: "Downloads", zh: "下载"), &FOLDERID_Downloads, "\u{E896}"),
        (t!("Документы", "Documents", ja: "ドキュメント", pt: "Documentos", zh: "文档"), &FOLDERID_Documents, "\u{E8A5}"),
        (t!("Изображения", "Pictures", ja: "ピクチャ", pt: "Imagens", zh: "图片"), &FOLDERID_Pictures, "\u{E91B}"),
        (t!("Музыка", "Music", ja: "ミュージック", pt: "Música", zh: "音乐"), &FOLDERID_Music, "\u{E8D6}"),
        (t!("Видео", "Videos", ja: "ビデオ", pt: "Vídeos", zh: "视频"), &FOLDERID_Videos, "\u{E714}"),
    ];
    let mut out: Vec<(String, PathBuf, &'static str)> = known.into_iter().filter_map(|(lbl, id, g)| crate::sh::known_dir(id).filter(|p| p.is_dir()).map(|p| (lbl.to_string(), p, g))).collect();
    for p in pins() {
        if p.is_dir() { out.push((p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned()), p, "\u{E718}")); }
    }
    out
}

pub fn nav_init(h: HWND) {
    NAVWND.set(h as isize);
    if let Some(f) = nav_width_file() {
        if let Ok(s) = fs::read_to_string(f) { if let Ok(w) = s.trim().parse::<i32>() { NAV_WIDTH.set(w.clamp(180, 520)); } }
    }
    let section = |lbl: &str| NavItem { title: lbl.into(), path: PathBuf::new(), glyph: "", cap: None, depth: 0, kind: NavKind::Section, expandable: false, expanded: false };
    let node = |title: String, path: PathBuf, glyph: &'static str, kind: NavKind, expandable: bool| NavItem { title, path, glyph, cap: None, depth: 1, kind, expandable, expanded: false };
    let mut v = vec![section(t!("Быстрый доступ", "Quick access", ja: "クイック アクセス", pt: "Acesso rápido", zh: "快速访问"))];
    v.push(node(t!("Главная", "Home", ja: "ホーム", pt: "Início", zh: "主页").into(), PathBuf::from("::home"), "\u{E80F}", NavKind::Home, false));
    for (lbl, p, g) in quick_folders() { v.push(node(lbl, p, g, NavKind::Folder, true)); }
    let mut thispc = section(t!("Этот компьютер", "This PC", ja: "PC", pt: "Este Computador", zh: "此电脑"));
    thispc.path = PathBuf::from("::thispc");
    thispc.kind = NavKind::ThisPC;
    v.push(thispc);
    for d in drive_list() {
        let mut it = node(d.title, d.root, if d.network { "\u{E8CE}" } else { "\u{EDA2}" }, NavKind::Drive, true);
        it.cap = d.cap;
        v.push(it);
    }
    v.push(node(t!("Корзина", "Recycle Bin", ja: "ごみ箱", pt: "Lixeira", zh: "回收站").into(), PathBuf::from("::recycle"), "\u{E74D}", NavKind::Recycle, false));
    // expand the folders that were open before (the current folder's chain is opened again by nav_sync)
    let mut i = 0;
    while i < v.len() {
        if v[i].expandable && !v[i].expanded && NAV_OPEN.with_borrow(|s| s.contains(&v[i].path.to_string_lossy().to_lowercase())) {
            expand_vec(&mut v, i);
        }
        i += 1;
    }
    NAV.with_borrow_mut(|n| *n = v);
    let cur = crate::cur_dir();
    if !cur.as_os_str().is_empty() { nav_sync(&cur); }
}

/// Scrolls the pane so the row of the current folder is in view (called after expanding and after a resize).
fn nav_ensure_current() {
    let h = NAVWND.get() as HWND;
    if h.is_null() { return; }
    let mut cr: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(h, &mut cr) };
    if cr.bottom <= 0 { NAV_SCROLL.set(0); return; }
    let cur = ST.with(|c| c.try_borrow().ok().map(|s| s.dir.to_string_lossy().to_lowercase())).unwrap_or_default();
    if cur.is_empty() { return; }
    NAV.with_borrow(|v| {
        let Some(idx) = v.iter().enumerate().filter(|(_, it)| it.kind != NavKind::Section && !it.path.as_os_str().is_empty() && cur.starts_with(&it.path.to_string_lossy().to_lowercase())).max_by_key(|(_, it)| it.path.as_os_str().len()).map(|(i, _)| i) else { return };
        let mut y = sc(8);
        for it in v.iter().take(idx) { y += nav_h(it); }
        let hgt = nav_h(&v[idx]);
        let scroll = NAV_SCROLL.get();
        let next = if y < scroll { y } else if y + hgt > scroll + cr.bottom { y + hgt - cr.bottom } else { scroll };
        let content: i32 = v.iter().map(nav_h).sum();
        NAV_SCROLL.set(next.clamp(0, (content + sc(8) * 2 - cr.bottom).max(0)));
    });
}

/// Opens the tree down to `dir` so the current folder is visible in the pane.
pub fn nav_sync(dir: &Path) {
    if dir.as_os_str().is_empty() || dir.to_string_lossy().starts_with("::") { return; }
    let d = dir.to_string_lossy().to_lowercase();
    NAV.with_borrow_mut(|v| {
        let mut best: Option<usize> = None;
        let mut best_len = 0;
        for (i, it) in v.iter().enumerate() {
            if it.kind == NavKind::Section { continue; }
            let p = it.path.to_string_lossy().to_lowercase();
            if !p.is_empty() && d.starts_with(&p) && p.len() > best_len { best = Some(i); best_len = p.len(); }
        }
        let Some(mut idx) = best else { return };
        for _ in 0..8 {
            if !v[idx].expanded { expand_vec(v, idx); }
            let base = v[idx].path.clone();
            let base_s = base.to_string_lossy();
            let d_s = dir.to_string_lossy();
            let rest = if d_s.len() > base_s.len() { d_s[base_s.len()..].trim_start_matches(['\\', '/']) } else { "" };
            let Some(comp) = rest.split(['\\', '/']).find(|s| !s.is_empty()) else { break };
            let want = base.join(comp).to_string_lossy().to_lowercase();
            let Some(child) = (idx + 1..v.len()).take_while(|&j| v[j].depth > v[idx].depth).find(|&j| v[j].path.to_string_lossy().to_lowercase() == want) else { break };
            idx = child;
        }
    });
    nav_ensure_current();
    nav_dirty();
}

pub fn nav_hwnd() -> HWND { NAVWND.with(|c| c.get()) as HWND }

/// The path of the navigation row under (x, y): a real folder or a virtual page.
pub fn nav_target_at(_x: i32, y: i32) -> Option<PathBuf> {
    nav_row_at(y).and_then(nav_path)
}

/// The tab under (x, y) in the chrome, and the path part of the address pill.
pub fn tab_at(cw: i32, n: usize, x: i32, y: i32) -> Option<usize> {
    layout(cw, n).tabs.iter().position(|t| inside(t, x, y))
}

pub fn crumb_path_at(x: i32, y: i32) -> Option<PathBuf> {
    crumb_at(x, y).and_then(crumb_path).filter(|p| p.is_dir() && !p.to_string_lossy().starts_with("::"))
}

pub fn nav_path(i: usize) -> Option<PathBuf> {
    NAV.with_borrow(|n| n.get(i).map(|x| x.path.clone()).filter(|p| !p.as_os_str().is_empty()))
}

pub fn nav_dirty() {
    let h = NAVWND.get() as HWND;
    if !h.is_null() { unsafe { InvalidateRect(h, ptr::null(), 0) }; }
}

/// Chrome on top, the pane on the left (hidden when the window is narrow), the list takes the rest.
pub unsafe fn layout_children(cw: i32, ch: i32) {
    let (chrome, list) = ST.with(|c| c.try_borrow().map_or((ptr::null_mut(), ptr::null_mut()), |s| (s.tab, s.list)));
    let nav = NAVWND.get() as HWND;
    let (top, status) = (sc(CHROME_H), sc(STATUS_H));
    let nw = if cw > sc(760) { sc(nav_width()) } else { 0 };
    let h = (ch - top - status).max(0);
    MoveWindow(chrome, 0, 0, cw, top, 1);
    MoveWindow(nav, 0, top, nw, h, 1);
    MoveWindow(list, nw, top, cw - nw, h, 1);
    fit_filler(list);
    nav_ensure_current();
}

/// Row under y, whatever x (for the context menu).
fn nav_row_at(y: i32) -> Option<usize> {
    let mut top = sc(8) - NAV_SCROLL.get();
    NAV.with_borrow(|n| {
        for (i, it) in n.iter().enumerate() {
            let hgt = nav_h(it);
            if y >= top && y < top + hgt { return (it.kind != NavKind::Section).then_some(i); }
            top += hgt;
        }
        None
    })
}

/// Node under (x, y) and whether the click is on its expand chevron.
fn nav_node_at(x: i32, y: i32) -> Option<(usize, bool)> {
    let mut top = sc(8) - NAV_SCROLL.get();
    NAV.with_borrow(|n| {
        for (i, it) in n.iter().enumerate() {
            let hgt = nav_h(it);
            if y >= top && y < top + hgt {
                if it.kind == NavKind::Section { return None; }
                let indent = sc(8) + it.depth * sc(16);
                return Some((i, it.expandable && x >= indent - sc(2) && x < indent + sc(14)));
            }
            top += hgt;
        }
        None
    })
}

unsafe fn paint_nav(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let wdc = BeginPaint(hwnd, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut cr);
    BUSY.set(false);
    let (dc, bmp) = (CreateCompatibleDC(wdc), CreateCompatibleBitmap(wdc, cr.right.max(1), cr.bottom.max(1)));
    let old = SelectObject(dc, bmp);
    let bg = CreateSolidBrush(pal().bg);
    FillRect(dc, &cr, bg);
    DeleteObject(bg);
    SetBkMode(dc, TRANSPARENT as i32);
    let (f_txt, f_head, f_glyph, f_small, f_chev) = (font(13, 400, ui_face()) as HFONT, font(12, 600, ui_face()) as HFONT, font(16, 400, winrust_common::style::icon_face()) as HFONT, font(11, 400, ui_face()) as HFONT, font(10, 400, winrust_common::style::icon_face()) as HFONT);
    let of = SelectObject(dc, f_txt);
    // where we are = the deepest node that contains the current folder
    let cur = ST.with(|c| c.try_borrow().ok().map(|s| s.dir.to_string_lossy().to_lowercase())).unwrap_or_default();
    let hot = NAVHOT.get();
    let scroll = NAV_SCROLL.get();
    NAV.with_borrow(|n| {
        let here = n.iter().enumerate().filter(|(_, it)| it.kind != NavKind::Section && !it.path.as_os_str().is_empty() && cur.starts_with(&it.path.to_string_lossy().to_lowercase())).max_by_key(|(_, it)| it.path.as_os_str().len()).map(|(i, _)| i);
        let mut y = sc(8) - scroll;
        for (i, it) in n.iter().enumerate() {
            let hgt = nav_h(it);
            if y + hgt < 0 { y += hgt; continue; }
            if y > cr.bottom { break; }
            if it.kind == NavKind::Section || it.kind == NavKind::ThisPC {
                SelectObject(dc, f_head);
                let on = it.kind == NavKind::ThisPC && here == Some(i);
                text(dc, &it.title, rc(sc(16), y + sc(4), cr.right - sc(8), y + hgt), if on { pal().text } else { pal().dim }, DT_LEFT);
                y += hgt;
                continue;
            }
            let indent = sc(8) + it.depth * sc(16);
            let r = rc(sc(8), y, cr.right - sc(8), y + hgt);
            let (th, ta) = (hv(5000 + i as i32, hot == i as i32), hv(6000 + i as i32, here == Some(i)));
            let p = press_t(5000 + i as i32);
            if th.max(ta) > 0.01 || p > 0.0 { fill(dc, r, mix(mix(mix(pal().bg, pal().hover, th), pal().active, ta), pal().accent, 0.3 * p), sc(6)); }
            if ta > 0.01 { fill(dc, rc(r.left, y + sc(8), r.left + sc(3), y + hgt - sc(8)), mix(pal().active, pal().accent, ta), sc(3)); }
            let line = rc(indent, y, cr.right - sc(8), y + sc(NAV_ROW)); // the title line (a drive has its usage bar below it)
            if it.expandable {
                SelectObject(dc, f_chev);
                text(dc, if it.expanded { "\u{E70D}" } else { "\u{E76C}" }, rc(indent - sc(2), line.top, indent + sc(14), line.bottom), mix(pal().dim, pal().text, th * 0.6), DT_CENTER);
            }
            SelectObject(dc, f_glyph);
            text(dc, it.glyph, rc(indent + sc(16), line.top, indent + sc(36), line.bottom), mix(pal().dim, pal().accent, ta), DT_CENTER);
            SelectObject(dc, f_txt);
            let text_x = indent + sc(40);
            text(dc, &it.title, rc(text_x, line.top, cr.right - sc(8), line.bottom), mix(pal().dim, pal().text, ta.max(th)), DT_LEFT);
            if let Some((free, total)) = it.cap {
                // usage bar like Windows' "This PC": accent, red once the drive is more than 90 % full
                let used = 1.0 - free as f32 / total as f32;
                let bar = rc(text_x, y + sc(28), cr.right - sc(14), y + sc(32));
                fill(dc, bar, mix(pal().bg, pal().text, 0.12), sc(4));
                let fw = ((bar.right - bar.left) as f32 * used) as i32;
                fill(dc, rc(bar.left, bar.top, bar.left + fw.max(sc(4)), bar.bottom), if used > 0.9 { winrust_common::style::BAD } else { pal().accent }, sc(4));
                SelectObject(dc, f_small);
                let gb = |b: u64| b as f64 / 1_073_741_824.0;
                let free_total = tf!("{:.0} ГБ свободно из {:.0} ГБ", "{:.0} GB free of {:.0} GB", ja: "{:.0} GB 空き / {:.0} GB", pt: "{:.0} GB livres de {:.0} GB", zh: "{:.0} GB 可用，共 {:.0} GB", gb(free), gb(total));
                text(dc, &free_total, rc(bar.left, bar.bottom + sc(1), cr.right - sc(8), y + hgt - sc(2)), pal().dim, DT_LEFT);
            }
            y += hgt;
        }
    });
    fill(dc, rc(cr.right - 1, 0, cr.right, cr.bottom), pal().line, 0); // hairline between the pane and the list
    BitBlt(wdc, 0, 0, cr.right, cr.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, of);
    for f in [f_txt, f_head, f_glyph, f_small, f_chev] { DeleteObject(f); }
    SelectObject(dc, old);
    DeleteObject(bmp);
    DeleteDC(dc);
    if BUSY.get() { SetTimer(hwnd, 4, 16, None); } else { KillTimer(hwnd, 4); }
    EndPaint(hwnd, &ps);
}

pub unsafe extern "system" fn nav_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (x, y) = ((lp & 0xFFFF) as i16 as i32, ((lp >> 16) & 0xFFFF) as i16 as i32);
    match m {
        WM_PAINT => paint_nav(h),
        WM_ERASEBKGND => return 1,
        WM_TIMER if wp == 4 => { InvalidateRect(h, ptr::null(), 0); }
        WM_SETCURSOR => {
            // the right edge is the splitter between the pane and the list
            let mut p: POINT = std::mem::zeroed();
            let mut cr: RECT = std::mem::zeroed();
            GetCursorPos(&mut p);
            ScreenToClient(h, &mut p);
            GetClientRect(h, &mut cr);
            if p.x >= cr.right - sc(5) {
                SetCursor(LoadCursorW(0 as _, IDC_SIZEWE));
                return 1;
            }
            return DefWindowProcW(h, m, wp, lp);
        }
        WM_MOUSEMOVE => {
            if let Some((sx, sw)) = NAV_DRAG.get() {
                if GetKeyState(0x01) < 0 { // VK_LBUTTON
                    let w = (sw + x - sx).clamp(180, 520);
                    NAV_WIDTH.set(w);
                    PostMessageW(GetParent(h), WM_UI, 56, 0);
                    InvalidateRect(h, ptr::null(), 0);
                    return 0;
                }
            }
            let k = nav_row_at(y).map_or(-1, |i| i as i32);
            let mut tme = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            if NAVHOT.replace(k) != k { InvalidateRect(h, ptr::null(), 0); }
        }
        WM_MOUSELEAVE => { NAVHOT.set(-1); InvalidateRect(h, ptr::null(), 0); }
        WM_MOUSEWHEEL => {
            let d = (((wp >> 16) & 0xFFFF) as i16) as i32;
            let content: i32 = NAV.with_borrow(|n| n.iter().map(nav_h).sum());
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            let max = (content + sc(8) * 2 - cr.bottom).max(0);
            let next = (NAV_SCROLL.get() - d.signum() * sc(48)).clamp(0, max);
            NAV_SCROLL.set(next);
            InvalidateRect(h, ptr::null(), 0);
        }
        WM_RBUTTONUP => {
            let Some(i) = nav_row_at(y) else { return 0 };
            let Some(p) = nav_path(i) else { return 0 };
            let menu = CreatePopupMenu();
            AppendMenuW(menu, MF_STRING, 1, w(t!("Открыть в новой вкладке", "Open in new tab", ja: "新しいタブで開く", pt: "Abrir em nova guia", zh: "在新标签页中打开")).as_ptr());
            AppendMenuW(menu, MF_STRING, 2, w(t!("Открыть в новом окне", "Open in new window", ja: "新しいウィンドウで開く", pt: "Abrir em nova janela", zh: "在新窗口中打开")).as_ptr());
            AppendMenuW(menu, MF_STRING, 3, w(t!("Копировать путь", "Copy path", ja: "パスのコピー", pt: "Copiar caminho", zh: "复制路径")).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            let pin = if pins().contains(&p) { t!("Открепить от быстрого доступа", "Unpin from Quick access", ja: "クイック アクセスからピン留めを解除", pt: "Desafixar do Acesso rápido", zh: "从快速访问取消固定") } else { t!("Закрепить в быстром доступе", "Pin to Quick access", ja: "クイック アクセスにピン留め", pt: "Fixar no Acesso rápido", zh: "固定到快速访问") };
            AppendMenuW(menu, MF_STRING, 4, w(pin).as_ptr());
            let expanded = NAV.with_borrow(|n| n.get(i).is_some_and(|it| it.expandable));
            if expanded {
                let lbl = if NAV.with_borrow(|n| n.get(i).is_some_and(|it| it.expanded)) { t!("Свернуть", "Collapse", ja: "折りたたむ", pt: "Recolher", zh: "折叠") } else { t!("Развернуть", "Expand", ja: "展開", pt: "Expandir", zh: "展开") };
                AppendMenuW(menu, MF_STRING, 5, w(lbl).as_ptr());
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
            AppendMenuW(menu, MF_STRING, 6, w(t!("Свойства", "Properties", ja: "プロパティ", pt: "Propriedades", zh: "属性")).as_ptr());
            let mut pt: POINT = std::mem::zeroed();
            GetCursorPos(&mut pt);
            let id = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, h, ptr::null());
            DestroyMenu(menu);
            let main = GetParent(h);
            match id {
                1 => { PostMessageW(main, WM_UI, 33, i as isize); }
                2 => { PostMessageW(main, WM_UI, 34, i as isize); }
                3 => { PostMessageW(main, WM_UI, 35, i as isize); }
                4 => toggle_pin(&p),
                5 => if NAV.with_borrow(|n| n.get(i).is_some_and(|it| it.expanded)) { collapse_node(h, i) } else { expand_node(h, i) },
                6 => crate::properties(&p),
                _ => {}
            }
        }
        WM_LBUTTONDOWN => {
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(h, &mut cr);
            if x >= cr.right - sc(5) {
                NAV_DRAG.set(Some((x, nav_width())));
                SetCapture(h);
                return 0;
            }
            match nav_node_at(x, y) {
                Some((i, true)) => {
                    if NAV.with_borrow(|n| n.get(i).is_some_and(|it| it.expanded)) { collapse_node(h, i) } else { expand_node(h, i) }
                }
                Some((i, false)) => {
                    PRESS.set((5000 + i as i32, now()));
                    InvalidateRect(h, ptr::null(), 0);
                    if nav_path(i).is_some() { PostMessageW(GetParent(h), WM_UI, 20, i as isize); }
                }
                None => {}
            }
        }
        WM_LBUTTONUP => {
            if NAV_DRAG.take().is_some() {
                ReleaseCapture();
                if let Some(f) = nav_width_file() {
                    if let Some(d) = f.parent() { let _ = fs::create_dir_all(d); }
                    let _ = fs::write(f, nav_width().to_string());
                }
            }
        }
        _ => return DefWindowProcW(h, m, wp, lp),
    }
    0
}
