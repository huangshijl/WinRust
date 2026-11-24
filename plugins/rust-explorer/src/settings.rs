//! The gear's popup: a small rounded card with the file manager's preferences (each row applies at once).
//! It is an owned, borderless window drawn in the same palette as the rest of the window; Esc, the close button or a
//! click outside (the owner becomes active again) closes it.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::{
    cell::{Cell, RefCell},
    ptr,
};
use windows_sys::Win32::{
    Graphics::{Dwm::DwmSetWindowAttribute, Gdi::*},
    System::LibraryLoader::GetModuleHandleW,
    UI::Input::KeyboardAndMouse::{SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE},
};
use winrust_common::t;

const CLASS: &str = "RxSettings";
const WM_DONE: u32 = WM_APP + 2; // the Win+E toggle finished on its thread
const HEADER: i32 = 54;
const ROWS: usize = 2;

#[derive(Clone, Copy, Default)]
struct Layout { close: RECT, rows: [RECT; ROWS], switch: [RECT; ROWS] }

thread_local! {
    static WND: Cell<isize> = const { Cell::new(0) };
    static HOT: Cell<i32> = const { Cell::new(-1) }; // 0 / 1 = a row, 9 = the close button
    static BUSY: Cell<bool> = const { Cell::new(false) }; // the Win+E switch is being applied on its thread
    static LAY: RefCell<Layout> = const { RefCell::new(Layout { close: RECT { left: 0, top: 0, right: 0, bottom: 0 }, rows: [RECT { left: 0, top: 0, right: 0, bottom: 0 }; ROWS], switch: [RECT { left: 0, top: 0, right: 0, bottom: 0 }; ROWS] }) };
    static REG: Cell<bool> = const { Cell::new(false) };
}

fn row_h(i: usize) -> i32 { ui::sc(if i == 1 { 72 } else { 58 }) }

fn layout(w: i32) -> Layout {
    let pad = ui::sc(20);
    let mut l = Layout::default();
    let cs = ui::sc(32);
    l.close = RECT { left: w - ui::sc(12) - cs, top: (ui::sc(HEADER) - cs) / 2, right: w - ui::sc(12), bottom: (ui::sc(HEADER) + cs) / 2 };
    let mut y = ui::sc(HEADER);
    for i in 0..ROWS {
        let h = row_h(i);
        l.rows[i] = RECT { left: ui::sc(12), top: y, right: w - ui::sc(12), bottom: y + h };
        let sw = ui::sc(40);
        let sh = ui::sc(20);
        let right = w - pad;
        l.switch[i] = RECT { left: right - sw, top: y + (h - sh) / 2, right, bottom: y + (h - sh) / 2 + sh };
        y += h;
    }
    l
}

fn height() -> i32 { ui::sc(HEADER + 12) + row_h(0) + row_h(1) }

fn register() {
    REG.with(|r| {
        if r.get() { return; }
        let inst = unsafe { GetModuleHandleW(ptr::null()) };
        unsafe {
            RegisterClassW(&WNDCLASSW { style: 0x0002_0000 /* CS_DROPSHADOW */, lpfnWndProc: Some(proc_settings), hInstance: inst, lpszClassName: w(CLASS).as_ptr(), hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW), ..std::mem::zeroed() });
        }
        r.set(true);
    });
}

/// Opens the popup over `owner` (the gear's window); a second call brings the open one to the front again.
pub fn show(owner: HWND) {
    unsafe {
        let old = WND.get() as HWND;
        if !old.is_null() && IsWindow(old) != 0 {
            SetForegroundWindow(old);
            return;
        }
        register();
        let (wd, ht) = (ui::sc(460), height());
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(owner, &mut r);
        let x = r.left + ((r.right - r.left) - wd) / 2;
        let y = r.top + ((r.bottom - r.top) - ht) / 2;
        let h = CreateWindowExW(WS_EX_TOOLWINDOW, w(CLASS).as_ptr(), w(t!("Настройки", "Settings", ja: "設定", pt: "Configurações", zh: "设置")).as_ptr(), WS_POPUP, x, y, wd, ht, owner, 0 as _, GetModuleHandleW(ptr::null()), ptr::null());
        if h.is_null() { return; }
        WND.set(h as isize);
        let corner = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(h, 33, &corner as *const _ as _, 4);
        let rgn = CreateRoundRectRgn(0, 0, wd + 1, ht + 1, ui::sc(12), ui::sc(12));
        SetWindowRgn(h, rgn, 1);
        ShowWindow(h, SW_SHOWNORMAL);
        SetForegroundWindow(h);
        SetFocus(h);
    }
}

unsafe extern "system" fn proc_settings(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match m {
            WM_ERASEBKGND => 1,
            WM_PAINT => { paint(h); 0 }
            WM_MOUSEMOVE => {
                let (x, y) = xy(lp);
                let mut cr: RECT = std::mem::zeroed();
                GetClientRect(h, &mut cr);
                let l = LAY.with_borrow(|l| *l);
                let hot = if inside(&l.close, x, y) { 9 } else { (0..ROWS).find(|&i| inside(&l.rows[i], x, y)).map_or(-1, |i| i as i32) };
                if HOT.replace(hot) != hot { InvalidateRect(h, ptr::null(), 0); }
                let mut tm = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                TrackMouseEvent(&mut tm);
                0
            }
            WM_MOUSELEAVE => { if HOT.replace(-1) != -1 { InvalidateRect(h, ptr::null(), 0); } 0 }
            WM_LBUTTONDOWN => {
                let (x, y) = xy(lp);
                let l = LAY.with_borrow(|l| *l);
                if inside(&l.close, x, y) {
                    DestroyWindow(h);
                } else if inside(&l.rows[0], x, y) {
                    SHOW_CAP.fetch_xor(true, Relaxed);
                    save_globals();
                    ui::nav_dirty();
                    InvalidateRect(h, ptr::null(), 0);
                } else if inside(&l.rows[1], x, y) && !BUSY.replace(true) {
                    let wnd = h as isize;
                    std::thread::spawn(move || {
                        crate::hotkey::toggle();
                        PostMessageW(wnd as HWND, WM_DONE, 0, 0);
                    });
                }
                0
            }
            WM_DONE => { BUSY.set(false); InvalidateRect(h, ptr::null(), 0); 0 }
            WM_KEYDOWN if wp as u16 == VK_ESCAPE => { DestroyWindow(h); 0 }
            WM_ACTIVATE => {
                if (wp & 0xffff) as u32 == WA_INACTIVE { DestroyWindow(h); }
                0
            }
            WM_CLOSE => { DestroyWindow(h); 0 }
            WM_DESTROY => { WND.set(0); 0 }
            _ => DefWindowProcW(h, m, wp, lp),
        }
    }
}

fn xy(lp: LPARAM) -> (i32, i32) { ((lp & 0xffff) as u16 as i16 as i32, ((lp >> 16) & 0xffff) as u16 as i16 as i32) }
fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

unsafe fn paint(h: HWND) {
    let p = ui::pal();
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(h, &mut ps);
    let mut cr: RECT = std::mem::zeroed();
    GetClientRect(h, &mut cr);
    let (wd, ht) = (cr.right, cr.bottom);
    let l = layout(wd);
    LAY.with_borrow_mut(|x| *x = l);
    let mem = CreateCompatibleDC(dc);
    let buf = CreateCompatibleBitmap(dc, wd, ht);
    let old = SelectObject(mem, buf);
    SetBkMode(mem, TRANSPARENT as i32);

    let all = RECT { left: 0, top: 0, right: wd, bottom: ht };
    let bg = CreateSolidBrush(p.card);
    FillRect(mem, &all, bg);
    DeleteObject(bg as HGDIOBJ);

    // the 1px card edge
    let pen = CreatePen(PS_SOLID as i32, 1, ui::mix(p.pill_line, p.text, 0.12));
    let ob = SelectObject(mem, GetStockObject(NULL_BRUSH));
    let op = SelectObject(mem, pen as HGDIOBJ);
    RoundRect(mem, 0, 0, wd, ht, ui::sc(12), ui::sc(12));
    SelectObject(mem, op);
    SelectObject(mem, ob);
    DeleteObject(pen as HGDIOBJ);

    let f_title = ui::font(16, 600, ui::ui_face());
    let f_row = ui::font(14, 400, ui::ui_face());
    let f_sub = ui::font(12, 400, ui::ui_face());
    let pad = ui::sc(20);

    SelectObject(mem, f_title as HGDIOBJ);
    ui::text(mem, t!("Настройки", "Settings", ja: "設定", pt: "Configurações", zh: "设置"), RECT { left: pad, top: 0, right: wd - ui::sc(16), bottom: ui::sc(HEADER) }, p.text, DT_LEFT | DT_VCENTER);

    // close button: a Win11 close-red disc and two strokes
    let close_hot = HOT.with(|h| h.get()) == 9;
    if close_hot { ui::fill(mem, l.close, 0x001C2BC4, ui::sc(6)); } // COLORREF: #C42B1C close red
    let c = ui::sc(9);
    let cx = (l.close.left + l.close.right) / 2;
    let cy = (l.close.top + l.close.bottom) / 2;
    let xp = CreatePen(PS_SOLID as i32, ui::sc(1).max(1), if close_hot { 0x00FFFFFF } else { p.dim });
    let oldp = SelectObject(mem, xp as HGDIOBJ);
    MoveToEx(mem, cx - c, cy - c, ptr::null_mut());
    LineTo(mem, cx + c + 1, cy + c + 1);
    MoveToEx(mem, cx + c, cy - c, ptr::null_mut());
    LineTo(mem, cx - c - 1, cy + c + 1);
    SelectObject(mem, oldp);
    DeleteObject(xp as HGDIOBJ);

    // a hairline under the title, like the Settings app's flyouts
    let line = RECT { left: 0, top: ui::sc(HEADER) - 1, right: wd, bottom: ui::sc(HEADER) };
    let lb = CreateSolidBrush(p.line);
    FillRect(mem, &line, lb);
    DeleteObject(lb as HGDIOBJ);

    let hot = HOT.with(|h| h.get());
    let titles = [
        t!("Показывать занятость дисков в области переходов", "Show drive usage in the navigation pane", ja: "ナビゲーション ウィンドウにドライブの使用量を表示", pt: "Mostrar uso das unidades no painel de navegação", zh: "在导航窗格显示磁盘容量"),
        t!("Забирать Win+E (запуск при входе в Windows)", "Take over Win+E (start with Windows)", ja: "Win+E を引き継ぐ (Windows ログオン時に起動)", pt: "Assumir Win+E (iniciar com o Windows)", zh: "接管 Win+E（随 Windows 登录启动）"),
    ];
    let subs = [
        None,
        Some(t!("Фоновая копия программы ловит клавишу и открывает это окно", "A small background copy catches the key and opens this window", ja: "バックグラウンドの小さなコピーがキーを横取りしてこのウィンドウを開きます", pt: "Uma pequena cópia em segundo plano captura a tecla e abre esta janela", zh: "后台驻留一个极小进程，按 Win+E 时打开本程序")),
    ];
    for i in 0..ROWS {
        if hot == i as i32 { ui::fill(mem, l.rows[i], p.hover, ui::sc(6)); }
        let text_right = l.switch[i].left - ui::sc(16);
        let row = l.rows[i];
        SelectObject(mem, f_row as HGDIOBJ);
        if i == 1 && subs[i].is_some() {
            ui::text(mem, titles[i], RECT { left: row.left + ui::sc(8), top: row.top + ui::sc(10), right: text_right, bottom: row.top + ui::sc(34) }, p.text, DT_LEFT | DT_VCENTER);
            SelectObject(mem, f_sub as HGDIOBJ);
            ui::text(mem, subs[i].unwrap_or(""), RECT { left: row.left + ui::sc(8), top: row.top + ui::sc(34), right: text_right, bottom: row.bottom - ui::sc(8) }, p.dim, DT_LEFT | DT_VCENTER);
        } else {
            ui::text(mem, titles[i], RECT { left: row.left + ui::sc(8), top: row.top, right: text_right, bottom: row.bottom }, p.text, DT_LEFT | DT_VCENTER);
        }
        let on = match i { 0 => SHOW_CAP.load(Relaxed), _ => crate::hotkey::installed() };
        switch(mem, l.switch[i], on, &p);
    }

    BitBlt(dc, 0, 0, wd, ht, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(buf as HGDIOBJ);
    DeleteDC(mem);
    for f in [f_title, f_row, f_sub] { DeleteObject(f as HGDIOBJ); }
    EndPaint(h, &ps);
}

unsafe fn switch(dc: HDC, at: RECT, on: bool, p: &ui::Palette) {
    let rad = at.bottom - at.top;
    ui::fill(dc, at, if on { p.accent } else { ui::mix(p.card, p.text, 0.32) }, rad);
    let d = rad - ui::sc(6);
    let y = at.top + ui::sc(3);
    let x = if on { at.right - ui::sc(3) - d } else { at.left + ui::sc(3) };
    ui::fill(dc, RECT { left: x, top: y, right: x + d, bottom: y + d }, 0x00FF_FFFF, d);
}
