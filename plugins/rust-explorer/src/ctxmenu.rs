//! The Win11-style context menu: the shell's classic items laid out in columns to the right of the click, and, while the
//! pointer is over an item with a submenu, the columns after it are replaced by that submenu (so a long menu never runs
//! off the bottom of the screen). The last item opens the real Windows menu as a fallback.
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::cell::{Cell, RefCell};
use windows_sys::Win32::{Graphics::Gdi::*, System::Threading::GetCurrentThreadId, UI::Input::KeyboardAndMouse::*};

/// Returned when the user picks "Show more options": the caller opens the real shell menu.
pub const MORE: u32 = 0xFFFE_FFFE;

#[derive(Clone, Copy, PartialEq)]
pub enum Pick { None, Id(u32) }

#[derive(Clone, Default)]
pub struct Item {
    pub id: u32,
    pub text: String,
    pub hint: String,          // text after a tab: the shortcut, drawn dim on the right
    pub accel: Option<char>,   // the letter after '&'
    pub disabled: bool,
    pub checked: bool,
    pub radio: bool,
    pub default: bool,
    pub sep: bool,
    pub glyph: &'static str,
    pub hbmp: isize,
    pub bmp: (i32, i32, i32), // width, height, bits per pixel (0: no bitmap)
    pub sub: Vec<Item>,
}

impl Item {
    pub fn separator() -> Item { Item { sep: true, ..Default::default() } }
    pub fn submenu(&self) -> bool { !self.sub.is_empty() }
    /// `"Open &with\tCtrl+O"` -> ("Open with", Some('w'), "Ctrl+O").
    pub fn split_label(raw: &str) -> (String, Option<char>, String) {
        let (main, hint) = match raw.split_once('\t') { Some((a, b)) => (a, b.trim().to_string()), None => (raw, String::new()) };
        let mut text = String::new();
        let mut accel = None;
        let mut chars = main.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '&' {
                match chars.peek() {
                    Some('&') => { chars.next(); text.push('&'); }
                    Some(&a) if !a.is_whitespace() && accel.is_none() => { chars.next(); accel = Some(a); text.push(a); }
                    _ => {}
                }
            } else {
                text.push(c);
            }
        }
        (text, accel, hint)
    }
}

const ROW_H: i32 = 32;
const SEP_H: i32 = 11;
const PAD_X: i32 = 12;
const PAD_Y: i32 = 6;
const ICON_W: i32 = 20;
const ICON_GAP: i32 = 12;
const ARROW_W: i32 = 24;
const MIN_COL: i32 = 220;
const MAX_COL: i32 = 440;
const RADIUS: i32 = 8;
const CLASS: &str = "WinRustCtxMenu";


struct Fonts { txt: isize, bold: isize, glyph: isize, small: isize, arrow: isize, dc: HDC }
impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            for f in [self.txt, self.bold, self.glyph, self.small, self.arrow] { if f != 0 { DeleteObject(f as HGDIOBJ); } }
            ReleaseDC(ptr::null_mut(), self.dc);
        }
    }
}

struct Col { items: Vec<Item>, owner: Option<(usize, usize)> }
struct ColGeom { x: i32, w: i32, h: i32, rows: Vec<(i32, i32, usize)> } // (y, height, item index)

struct State {
    menu: HWND,
    chunks: Vec<Vec<Item>>,
    show: usize,        // root chunks shown (all when no submenu is open)
    base: usize,        // column index of the first submenu column; usize::MAX when none
    path: Vec<usize>,   // path[i]: the row in column base+i-1 that opens column base+i
    cols: Vec<Col>,
    hot: Option<(usize, usize)>,
    mouse: Option<(i32, i32)>,
    pressed: Option<(usize, usize)>,
    geom: Vec<ColGeom>,
    fonts: Fonts,
    radius: i32,
    row_h: i32,
    sep_h: i32,
    pad_x: i32,
    pad_y: i32,
    gutter: i32,
    arrow: i32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static DONE: Cell<Option<Pick>> = const { Cell::new(None) };
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    static MENU_H: Cell<isize> = const { Cell::new(0) };
}

/// Called from inside `STATE.with_borrow_mut` (a click, a key, a mnemonic), so it must not borrow STATE: the window handle
/// lives in MENU_H.
fn done(p: Pick) {
    DONE.with(|d| if d.get().is_none() { d.set(Some(p)); });
    let h = MENU_H.with(|c| c.get());
    if h != 0 { unsafe { PostMessageW(h as HWND, WM_NULL, 0, 0); } }
}

// ---------- model -> columns ----------

/// Splits the root items into columns of at most `max_h` (separators never lead or trail a column).
fn chunk(items: &[Item], max_h: i32, row_h: i32, sep_h: i32, pad: i32) -> Vec<Vec<Item>> {
    let mut out: Vec<Vec<Item>> = Vec::new();
    let mut cur: Vec<Item> = Vec::new();
    let mut h = 2 * pad;
    for it in items {
        let ih = if it.sep { sep_h } else { row_h };
        if !cur.is_empty() && h + ih > max_h {
            while cur.last().is_some_and(|x| x.sep) { cur.pop(); }
            out.push(std::mem::take(&mut cur));
            h = 2 * pad;
        }
        if cur.is_empty() && it.sep { continue; }
        cur.push(it.clone());
        h += ih;
    }
    while cur.last().is_some_and(|x| x.sep) { cur.pop(); }
    if !cur.is_empty() { out.push(cur); }
    if out.is_empty() { out.push(Vec::new()); }
    out
}

impl State {
    fn rebuild(&mut self) {
        self.cols.clear();
        for c in 0..self.show.min(self.chunks.len()) { self.cols.push(Col { items: self.chunks[c].clone(), owner: None }); }
        let mut owner = self.show.checked_sub(1);
        for &row in &self.path {
            let Some(oc) = owner else { break };
            let Some(it) = self.cols.get(oc).and_then(|c| c.items.get(row)) else { break };
            let items = it.sub.clone();
            let nc = self.cols.len();
            self.cols.push(Col { items, owner: Some((oc, row)) });
            owner = Some(nc);
        }
    }

    /// The pointer (or the arrow keys) is over item `row` of visible column `col`.
    fn hover(&mut self, col: usize, row: usize) {
        let chain_open = self.base != usize::MAX;
        if self.cols.get(col).and_then(|c| c.items.get(row)).is_none() { return; }
        let prev = (self.base != usize::MAX).then(|| (self.base, self.path.clone(), self.show));
        if col < self.show && !chain_open {
            self.root_hover(col, row);
        } else if col < self.show {
            // a root column left of the open submenu: act on the item, and keep the chain when it owns it
            let owns = self.path.first() == Some(&row) && self.base == col + 1;
            if !owns { self.root_hover(col, row); }
        } else if col >= self.base {
            let depth = col - self.base;
            if self.path.len() > depth + 1 { self.path.truncate(depth + 1); }
            if !self.disabled_at(col, row) && !self.cols[col].items[row].sub.is_empty() { self.path.push(row); }
        }
        self.hot = Some((col, row));
        self.rebuild();
        let changed = prev.as_ref().map_or(true, |(b, p, s)| *b != self.base || *p != self.path || *s != self.show);
        if changed { self.layout(); }
        // the hover moves even when no column opens or closes: repaint so the highlight actually follows the pointer
        if !self.menu.is_null() { unsafe { InvalidateRect(self.menu, ptr::null(), 0) }; }
    }

    fn root_hover(&mut self, col: usize, row: usize) {
        let sub = !self.cols[col].items[row].sub.is_empty() && !self.disabled_at(col, row);
        if sub {
            self.show = col + 1;
            self.base = col + 1;
            self.path = vec![row];
        } else {
            self.show = self.chunks.len();
            self.base = usize::MAX;
            self.path.clear();
        }
    }

    fn disabled_at(&self, col: usize, row: usize) -> bool { self.cols.get(col).and_then(|c| c.items.get(row)).is_some_and(|i| i.disabled) }

    /// Hot item moved into the first selectable row of column `col` (keyboard).
    fn first_in(&mut self, col: usize) -> Option<usize> {
        let c = self.cols.get(col)?;
        let r = c.items.iter().position(|i| !i.sep && !i.disabled);
        r.map(|r| { self.hover(col, r); r })
    }

    fn last_in(&mut self, col: usize) -> Option<usize> {
        let c = self.cols.get(col)?;
        let r = c.items.iter().rposition(|i| !i.sep && !i.disabled);
        r.map(|r| { self.hover(col, r); r })
    }

    /// Keyboard: true when the key was consumed by the menu.
    fn on_key(&mut self, vk: u16, sys: bool) -> bool {
        match vk {
            VK_ESCAPE => { done(Pick::None); true }
            VK_UP => { self.step(-1); true }
            VK_DOWN => { self.step(1); true }
            VK_RIGHT => { self.right(); true }
            VK_LEFT => { self.left(); true }
            VK_RETURN => { self.open_current(); true }
            VK_HOME => { let c = self.hot.map_or(0, |h| h.0); self.first_in(c); true }
            VK_END => { let c = self.hot.map_or(0, |h| h.0); self.last_in(c); true }
            _ if sys && (b'A'..=b'Z').contains(&(vk as u8)) => { self.mnemonic((vk as u8) as char); true }
            _ => false,
        }
    }

    fn right(&mut self) {
        let Some((col, row)) = self.hot else { self.first_in(0); return };
        let sub = self.cols.get(col).and_then(|c| c.items.get(row)).is_some_and(|i| i.submenu());
        if sub { self.open_current(); return; }
        if self.base == usize::MAX && col + 1 < self.show {
            let n = self.cols.get(col + 1).map_or(0, |c| row.min(c.items.len().saturating_sub(1)));
            if let Some(r) = self.cols.get(col + 1).and_then(|c| c.items.get(n)).filter(|i| !i.sep).map(|_| n) {
                self.hover(col + 1, r);
            } else {
                self.first_in(col + 1);
            }
        }
    }

    fn left(&mut self) {
        if self.base != usize::MAX {
            if self.hot.is_some_and(|(c, _)| c < self.base) {
                // back on a root column: the open chain closes
                self.show = self.chunks.len();
                self.base = usize::MAX;
                self.path.clear();
                self.rebuild();
                self.layout();
            } else {
                self.parent();
            }
            return;
        }
        let Some((col, row)) = self.hot else { return };
        if col == 0 { return; }
        let n = self.cols.get(col - 1).map_or(0, |c| row.min(c.items.len().saturating_sub(1)));
        if self.cols.get(col - 1).and_then(|c| c.items.get(n)).is_some_and(|i| !i.sep) { self.hover(col - 1, n); } else { self.last_in(col - 1); }
    }

    fn step(&mut self, dir: isize) {
        let Some((col, row)) = self.hot else {
            if dir > 0 { self.first_in(0); }
            return;
        };
        let Some(c) = self.cols.get(col) else { return };
        let n = c.items.len() as isize;
        let mut r = row as isize + dir;
        while r >= 0 && r < n {
            if !self.cols[col].items[r as usize].sep && !self.cols[col].items[r as usize].disabled {
                self.hover(col, r as usize);
                return;
            }
            r += dir;
        }
        // past the end of a root column: on to the next / previous one (closing an open chain first)
        let is_root = if self.base == usize::MAX { col < self.show } else { col < self.base };
        if is_root {
            if self.base != usize::MAX {
                self.show = self.chunks.len();
                self.base = usize::MAX;
                self.path.clear();
                self.rebuild();
                self.layout();
            }
            let nc = col as isize + dir;
            if nc >= 0 && (nc as usize) < self.show {
                if dir > 0 { self.first_in(nc as usize); } else { self.last_in(nc as usize); }
            }
        }
    }

    fn parent(&mut self) {
        let Some((col, _)) = self.hot else { return };
        if col == 0 || self.base == usize::MAX { return; }
        let Some((oc, orow)) = self.cols.get(col).and_then(|c| c.owner) else { return };
        if oc + 1 == self.base {
            // the owner sits in the last visible root column: the whole chain closes
            self.show = self.chunks.len();
            self.base = usize::MAX;
            self.path.clear();
        } else {
            // keep the owner's own submenu column, drop the deeper ones
            self.path.truncate(oc - self.base + 1);
        }
        self.hot = Some((oc, orow));
        self.rebuild();
        self.layout();
    }

    fn open_current(&mut self) -> bool {
        let Some((col, row)) = self.hot else { return false };
        let Some(it) = self.cols.get(col).and_then(|c| c.items.get(row)) else { return false };
        if it.sep || it.disabled || (it.id == 0 && !it.submenu()) || it.id == u32::MAX { return false; }
        if it.submenu() {
            if col < self.show {
                self.root_hover(col, row);
            } else if col >= self.base {
                let depth = col - self.base;
                self.path.truncate(depth + 1);
                self.path.push(row);
            }
            self.hot = Some((col, row));
            self.rebuild();
            self.layout();
            if let Some(r) = self.cols.get(col + 1).and_then(|c| c.items.iter().position(|i| !i.sep)) {
                self.hot = Some((col + 1, r));
            }
            return true;
        }
        done(Pick::Id(it.id));
        true
    }

    fn mnemonic(&mut self, ch: char) -> bool {
        let mut hit = None;
        for (ci, col) in self.cols.iter().enumerate() {
            for (ri, it) in col.items.iter().enumerate() {
                if it.accel.is_some_and(|a| a.eq_ignore_ascii_case(&ch)) && !it.disabled && !it.sep { hit = Some((ci, ri)); break; }
            }
            if hit.is_some() { break; }
        }
        let Some((ci, ri)) = hit else { return false };
        self.hover(ci, ri);
        let sub = self.cols[ci].items[ri].submenu();
        let id = self.cols[ci].items[ri].id;
        if sub { let _ = self.open_current(); } else { done(Pick::Id(id)); }
        true
    }

    // ---------- geometry ----------

    fn text_w(&self, f: isize, s: &str) -> i32 {
        if s.is_empty() { return 0; }
        unsafe {
            let old = SelectObject(self.fonts.dc, f as HGDIOBJ);
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            DrawTextW(self.fonts.dc, w(s).as_ptr(), -1, &mut r, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(self.fonts.dc, old);
            r.right - r.left
        }
    }

    fn col_width(&self, items: &[Item]) -> i32 {
        let arrow = if items.iter().any(|i| !i.sub.is_empty()) { self.arrow } else { 0 };
        let hint = items.iter().map(|i| self.text_w(self.fonts.small, &i.hint)).max().unwrap_or(0);
        let hint = if hint > 0 { hint + ui::sc(14) } else { 0 };
        let mut w = 0;
        for i in items {
            if i.sep { continue; }
            let f = if i.default { self.fonts.bold } else { self.fonts.txt };
            let tw = self.text_w(f, &i.text).min(ui::sc(MAX_COL) - self.gutter - arrow - 2 * self.pad_x).max(0);
            w = w.max(tw);
        }
        (w + self.gutter + hint + arrow + 2 * self.pad_x).clamp(ui::sc(MIN_COL), ui::sc(MAX_COL))
    }

    fn layout(&mut self) {
        let mut x = 0;
        let mut geom = Vec::new();
        for col in &self.cols {
            let w = self.col_width(&col.items);
            let mut rows = Vec::new();
            let mut y = self.pad_y;
            for (i, it) in col.items.iter().enumerate() {
                let h = if it.sep { self.sep_h } else { self.row_h };
                rows.push((y, h, i));
                y += h;
            }
            let h = y + self.pad_y;
            geom.push(ColGeom { x, w, h, rows });
            x += w;
        }
        let nw = x.max(1);
        let nh = geom.iter().map(|c| c.h).max().unwrap_or(1).max(2 * self.pad_y + self.row_h);
        self.geom = geom;
        if self.menu.is_null() { return; }
        unsafe {
            let mut r = RECT::default();
            GetWindowRect(self.menu, &mut r);
            if r.right - r.left != nw || r.bottom - r.top != nh {
                SetWindowPos(self.menu, ptr::null_mut(), 0, 0, nw, nh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
                let rg = CreateRoundRectRgn(0, 0, nw + 1, nh + 1, self.radius * 2, self.radius * 2);
                SetWindowRgn(self.menu, rg, 1);
            }
            // the card grows to the right as submenus open: slide it back inside the monitor when it would run off
            GetWindowRect(self.menu, &mut r);
            let m = ui::sc(8);
            let work = work_rect(POINT { x: r.left + 3, y: r.top + 3 });
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let nx = if r.right > work.right - m { (work.right - m - w).max(work.left + m) } else { r.left };
            let ny = if r.bottom > work.bottom - m { (work.bottom - m - h).max(work.top + m) } else { r.top };
            if nx != r.left || ny != r.top { SetWindowPos(self.menu, ptr::null_mut(), nx, ny, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE); }
            InvalidateRect(self.menu, ptr::null(), 0);
        }
    }

    fn hit(&self, x: i32, y: i32) -> Option<(usize, usize)> {
        for (ci, g) in self.geom.iter().enumerate() {
            if x >= g.x && x < g.x + g.w {
                for &(ry, rh, ri) in &g.rows {
                    if y >= ry && y < ry + rh {
                        return self.cols[ci].items.get(ri).filter(|i| !i.sep).map(|_| (ci, ri));
                    }
                }
            }
        }
        None
    }

    fn initial_hot(&mut self) {
        let mut pt = POINT::default();
        unsafe { GetCursorPos(&mut pt); let mut r = RECT::default(); GetWindowRect(self.menu, &mut r); pt.x -= r.left; pt.y -= r.top; }
        self.mouse = Some((pt.x, pt.y));
        if let Some((c, r)) = self.hit(pt.x, pt.y) { self.hot = Some((c, r)); }
    }
}

// ---------- painting ----------

impl State {
    fn paint(&self, h: HWND) {
        let p = ui::pal();
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(h, &mut ps);
            let mut r = RECT::default();
            GetClientRect(h, &mut r);
            let (wd, ht) = (r.right, r.bottom);
            let mem = CreateCompatibleDC(dc);
            let buf = CreateCompatibleBitmap(dc, wd, ht);
            let old = SelectObject(mem, buf);
            SetBkMode(mem, TRANSPARENT as i32);
            let all = RECT { left: 0, top: 0, right: wd, bottom: ht };
            let brush = CreateSolidBrush(p.card);
            FillRect(mem, &all, brush);
            DeleteObject(brush as HGDIOBJ);

            let owners: Vec<(usize, usize)> = self.cols.iter().filter_map(|c| c.owner).collect();
            for (ci, col) in self.cols.iter().enumerate() {
                let g = &self.geom[ci];
                for &(y, h, ri) in &g.rows {
                    let it = &col.items[ri];
                    let rr = RECT { left: g.x + 4, top: y + 1, right: g.x + g.w - 4, bottom: y + h - 1 };
                    if it.sep {
                        let line = CreateSolidBrush(p.pill_line);
                        let lr = RECT { left: g.x + self.pad_x, top: y + h / 2, right: g.x + g.w - self.pad_x, bottom: y + h / 2 + 1 };
                        FillRect(mem, &lr, line);
                        DeleteObject(line as HGDIOBJ);
                        continue;
                    }
                    let hot = self.hot == Some((ci, ri));
                    let owns = owners.contains(&(ci, ri));
                    if hot || owns {
                        let hover = if p.hover == p.card { 0x00E8E8E8 } else { p.hover };
                        ui::fill(mem, rr, if hot { hover } else { ui::mix(p.card, hover, 0.7) }, ui::sc(6));
                    }
                    let text_c = if it.disabled { ui::off() } else { p.text };
                    let mut x = g.x + self.pad_x;
                    // a check / radio mark takes the place of the icon (radio: the mode this menu is on)
                    let ix = x + (self.gutter - self.pad_x) / 2;
                    if it.checked {
                        SelectObject(mem, self.fonts.glyph as HGDIOBJ);
                        ui::text(mem, if it.radio { "\u{25CF}" } else { "\u{E73E}" }, RECT { left: g.x, top: y, right: g.x + self.gutter - self.pad_x / 2, bottom: y + h }, if it.disabled { ui::off() } else { p.accent }, DT_CENTER | DT_VCENTER);
                    } else if it.hbmp != 0 {
                        draw_bmp(mem, it, ix, y + (h - it.bmp.1) / 2, if it.disabled { 110 } else { 255 });
                    } else if !it.glyph.is_empty() {
                        SelectObject(mem, self.fonts.glyph as HGDIOBJ);
                        ui::text(mem, it.glyph, RECT { left: ix, top: y, right: ix + ui::sc(ICON_W).max(it.bmp.0), bottom: y + h }, if it.disabled { ui::off() } else { p.dim }, DT_CENTER | DT_VCENTER);
                    }
                    x += self.gutter;
                    let right = g.x + g.w - self.pad_x - if it.submenu() { self.arrow } else { 0 };
                    if !it.hint.is_empty() {
                        let hw = self.text_w(self.fonts.small, &it.hint);
                        SelectObject(mem, self.fonts.small as HGDIOBJ);
                        ui::text(mem, &it.hint, RECT { left: right - hw, top: y, right, bottom: y + h }, ui::mix(text_c, p.card, 0.35), DT_RIGHT | DT_VCENTER);
                    }
                    SelectObject(mem, (if it.default { self.fonts.bold } else { self.fonts.txt }) as HGDIOBJ);
                    ui::text(mem, &it.text, RECT { left: x, top: y, right: right - if it.hint.is_empty() { 0 } else { self.text_w(self.fonts.small, &it.hint) + 12 }, bottom: y + h }, text_c, DT_LEFT | DT_VCENTER);
                    if it.submenu() {
                        SelectObject(mem, self.fonts.arrow as HGDIOBJ);
                        ui::text(mem, "\u{E76C}", RECT { left: g.x + g.w - self.arrow, top: y, right: g.x + g.w - 6, bottom: y + h }, if it.disabled { ui::off() } else { p.dim }, DT_CENTER | DT_VCENTER);
                    }
                }
            }
            // the outline: without it the card melts into a light list behind it
            let border = ui::mix(p.pill_line, p.text, 0.14);
            let pen = CreatePen(PS_SOLID, 1, border);
            let o_pen = SelectObject(mem, pen as HGDIOBJ);
            let o_brush = SelectObject(mem, GetStockObject(NULL_BRUSH) as HGDIOBJ);
            RoundRect(mem, 0, 0, wd, ht, self.radius * 2, self.radius * 2);
            SelectObject(mem, o_brush);
            SelectObject(mem, o_pen);
            DeleteObject(pen as HGDIOBJ);
            BitBlt(dc, 0, 0, wd, ht, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(buf as HGDIOBJ);
            DeleteDC(mem);
            EndPaint(h, &ps);
        }
    }
}

unsafe fn draw_bmp(dc: HDC, it: &Item, x: i32, y: i32, alpha: u8) {
    let (bw, bh, bpp) = it.bmp;
    if bw <= 0 || bh <= 0 { return; }
    let mem = CreateCompatibleDC(dc);
    let old = SelectObject(mem, it.hbmp as HGDIOBJ);
    if bpp == 32 {
        let bf = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        if AlphaBlend(dc, x, y, bw, bh, mem, 0, 0, bw, bh, bf) == 0 {
            BitBlt(dc, x, y, bw, bh, mem, 0, 0, SRCCOPY);
        }
    } else {
        BitBlt(dc, x, y, bw, bh, mem, 0, 0, SRCCOPY);
    }
    SelectObject(mem, old);
    DeleteDC(mem);
}

// ---------- window ----------

/// While the menu is up the parent window keeps the focus: keys are taken here, as a real menu does. Any click that is not
/// on the menu closes it, wherever capture ended up (a click into another window can take the capture away).
unsafe extern "system" fn hook_proc(code: i32, wp: usize, lp: isize) -> isize {
    if code == 0 {
        let msg = &*(lp as *const MSG);
        let menu = MENU_H.with(|c| c.get()) as HWND;
        if msg.message == WM_ACTIVATEAPP && msg.wParam == 0 {
            done(Pick::None);
        } else if matches!(msg.message, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN) && menu != std::ptr::null_mut() && msg.hwnd != menu {
            done(Pick::None);
        } else if msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN {
            let open = STATE.with_borrow(|s| s.is_some());
            if open {
                let vk = msg.wParam as u16;
                let handled = STATE.with_borrow_mut(|s| s.as_mut().is_some_and(|s| s.on_key(vk, msg.message == WM_SYSKEYDOWN)));
                if handled { return 1; }
            }
        }
    }
    CallNextHookEx(ptr::null_mut(), code, wp, lp)
}

unsafe extern "system" fn proc(h: HWND, m: u32, wp: usize, lp: isize) -> isize {
    match m {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            STATE.with_borrow(|s| { if let Some(s) = s.as_ref() { s.paint(h); } });
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = ((lp & 0xffff) as u16 as i16 as i32, ((lp >> 16) & 0xffff) as u16 as i16 as i32);
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    if s.mouse == Some((x, y)) { return; }
                    s.mouse = Some((x, y));
                    if let Some((c, r)) = s.hit(x, y) {
                        if s.hot != Some((c, r)) { s.hover(c, r); }
                    } else if x < 0 {
                        // moved off the left side: close the submenus, as the real menu does
                        if s.base != usize::MAX {
                            s.show = s.chunks.len();
                            s.base = usize::MAX;
                            s.path.clear();
                            s.hot = None;
                            s.rebuild();
                            s.layout();
                        }
                    }
                }
            });
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = ((lp & 0xffff) as u16 as i16 as i32, ((lp >> 16) & 0xffff) as u16 as i16 as i32);
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    match s.hit(x, y) {
                        Some((c, r)) => {
                            if s.disabled_at(c, r) { return; }
                            if s.hot != Some((c, r)) { s.hover(c, r); }
                            let id = s.cols[c].items[r].id;
                            if id == 0 || id == u32::MAX { return; }
                            if s.cols[c].items[r].submenu() {
                                s.pressed = None;
                                let _ = s.open_current();
                            } else {
                                s.pressed = Some((c, r));
                            }
                        }
                        None => done(Pick::None),
                    }
                }
            });
            0
        }
        WM_LBUTTONUP => {
            let (x, y) = ((lp & 0xffff) as u16 as i16 as i32, ((lp >> 16) & 0xffff) as u16 as i16 as i32);
            STATE.with_borrow_mut(|s| {
                if let Some(s) = s.as_mut() {
                    if let Some((c, r)) = s.pressed.take() {
                        if s.hit(x, y) == Some((c, r)) {
                            let it = &s.cols[c].items[r];
                            if !it.submenu() && !it.disabled && it.id != 0 && it.id != u32::MAX { done(Pick::Id(it.id)); }
                        }
                    }
                }
            });
            0
        }
        WM_RBUTTONDOWN | WM_MBUTTONDOWN => { done(Pick::None); 0 }
        WM_RBUTTONUP => 0,
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wp as u16;
            STATE.with_borrow_mut(|s| if let Some(s) = s.as_mut() { s.on_key(vk, m == WM_SYSKEYDOWN); });
            0
        }
        WM_KILLFOCUS => 0,
        _ => unsafe { DefWindowProcW(h, m, wp, lp) },
    }
}

unsafe fn register_class() {
    if REGISTERED.with(|r| r.get()) { return; }
    let mut c: WNDCLASSEXW = std::mem::zeroed();
    let cls = w(CLASS);
    c.cbSize = size_of::<WNDCLASSEXW>() as u32;
    c.style = CS_DROPSHADOW;
    c.lpfnWndProc = Some(proc);
    c.hInstance = GetModuleHandleW(ptr::null());
    c.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
    c.lpszClassName = cls.as_ptr();
    RegisterClassExW(&c);
    REGISTERED.with(|r| r.set(true));
}

unsafe fn work_rect(at: POINT) -> RECT {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    let mon = MonitorFromPoint(at, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(mon, &mut mi) != 0 { mi.rcWork } else { RECT { left: 0, top: 0, right: 1920, bottom: 1080 } }
}

/// Shows the menu at `at` and waits for a pick. Must be called on the UI thread.
pub fn show(parent: HWND, items: Vec<Item>, at: (i32, i32)) -> Pick {
    unsafe { show_inner(parent, items, POINT { x: at.0, y: at.1 }) }
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn show_inner(parent: HWND, items: Vec<Item>, at: POINT) -> Pick {
    let _ = ui::pal();
    DONE.with(|d| d.set(None));
    let dc = GetDC(ptr::null_mut());
    let fonts = Fonts {
        txt: ui::font(14, 400, ui::ui_face()),
        bold: ui::font(14, 600, ui::ui_face()),
        glyph: ui::font(16, 400, winrust_common::style::icon_face()),
        small: ui::font(12, 400, ui::ui_face()),
        arrow: ui::font(11, 400, winrust_common::style::icon_face()),
        dc,
    };
    let row_h = ui::sc(ROW_H);
    let sep_h = ui::sc(SEP_H);
    let pad_y = ui::sc(PAD_Y);
    let pad_x = ui::sc(PAD_X);
    let work = work_rect(at);
    let margin = ui::sc(8);
    let max_h = (work.bottom - work.top - 2 * margin).max(row_h * 4);
    let has_icons = items.iter().any(|i| !i.sep && (i.hbmp != 0 || !i.glyph.is_empty() || i.checked || i.radio));
    let gutter = if has_icons { ui::sc(ICON_W + ICON_GAP) } else { 0 };
    let height = |list: &[Item]| 2 * pad_y + list.iter().map(|i| if i.sep { sep_h } else { row_h }).sum::<i32>();
    fn deepest(list: &[Item], f: &dyn Fn(&[Item]) -> i32) -> i32 {
        let here = f(list);
        let sub = list.iter().map(|i| deepest(&i.sub, f)).max().unwrap_or(0);
        here.max(sub)
    }
    let tallest = deepest(&items, &height);
    let chunks = chunk(&items, max_h - 2 * pad_y, row_h, sep_h, pad_y);
    let mut st = State {
        menu: std::ptr::null_mut(), chunks,
        show: 0, base: usize::MAX, path: vec![], cols: vec![], hot: None, mouse: None, pressed: None, geom: vec![],
        fonts,
        radius: ui::sc(RADIUS), row_h, sep_h, pad_x, pad_y,
        gutter, arrow: ui::sc(ARROW_W),
    };
    st.show = st.chunks.len();
    st.rebuild();
    // anchored at the click / button corner; layout() slides the card inside the monitor once its real size is known
    let x = at.x.clamp(work.left + margin, (work.right - margin - row_h).max(work.left + margin));
    let y = at.y.clamp(work.top + margin, (work.bottom - margin - row_h).max(work.top + margin));
    let _ = (tallest, max_h);

    register_class();
    let menu = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        w(CLASS).as_ptr(),
        w("").as_ptr(),
        WS_POPUP,
        x, y, 10, 10,
        parent,
        ptr::null_mut(),
        GetModuleHandleW(ptr::null()),
        ptr::null(),
    );
    if menu.is_null() { return Pick::None; }
    MENU_H.with(|c| c.set(menu as isize));
    st.menu = menu;
    st.layout();
    STATE.with_borrow_mut(|s| *s = Some(st));
    STATE.with_borrow_mut(|s| if let Some(s) = s.as_mut() { s.initial_hot(); });
    // layout() has already sized the card and slid it inside the monitor: show it there, never at the raw click point
    // first (that made every menu near an edge flash outside the screen and jump back)
    ShowWindow(menu, SW_SHOWNA);
    SetWindowPos(menu, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    SetCapture(menu);
    SetFocus(menu);
    let hook = SetWindowsHookExW(WH_GETMESSAGE, Some(hook_proc), ptr::null_mut(), GetCurrentThreadId());

    let mut msg: MSG = std::mem::zeroed();
    loop {
        let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
        if r <= 0 { break; }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
        if DONE.get().is_some() { break; }
    }
    ReleaseCapture();
    if !hook.is_null() { UnhookWindowsHookEx(hook); }
    MENU_H.with(|c| c.set(0));
    STATE.with_borrow_mut(|s| *s = None);
    if !menu.is_null() { DestroyWindow(menu); }
    SetFocus(parent);
    SetForegroundWindow(parent);
    DONE.get().unwrap_or(Pick::None)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_lose_the_ampersands_and_keep_the_shortcut() {
        let (text, accel, hint) = Item::split_label("打开(&O)");
        assert_eq!((text.as_str(), accel, hint.as_str()), ("打开(O)", Some('O'), ""));
        let (text, accel, hint) = Item::split_label("Copy\tCtrl+C");
        assert_eq!((text.as_str(), accel, hint.as_str()), ("Copy", None, "Ctrl+C"));
        let (text, accel, _) = Item::split_label("A && B &x");
        assert_eq!((text.as_str(), accel), ("A & B x", Some('x')));
    }

    #[test]
    fn long_menus_fill_columns_without_edge_separators() {
        let mut items = Vec::new();
        for i in 0..10 {
            items.push(Item { text: format!("item {i}"), ..Default::default() });
            if i % 3 == 2 { items.push(Item::separator()); }
        }
        let cols = chunk(&items, 100, 30, 9, 5); // three rows fit a column
        assert!(cols.len() >= 3, "ten rows in columns of three: {}", cols.len());
        for c in &cols {
            assert!(!c.first().is_some_and(|i| i.sep));
            assert!(!c.last().is_some_and(|i| i.sep));
            assert!(c.iter().map(|i| if i.sep { 9 } else { 30 }).sum::<i32>() + 10 <= 100);
        }
        assert_eq!(cols.iter().flatten().filter(|i| !i.sep).count(), 10);
    }
}
