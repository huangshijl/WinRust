// rust-explorer [path]  — file manager window (Explorer replacement).
// Virtual list (LVS_OWNERDATA, only visible rows are asked for text/icon), system icons, tabs,
// IFileOperation (recycle bin / undo / elevation prompts), the Win11-style multi-column context menu (ctxmenu.rs), OLE drag and drop.
// Keys: Enter open, Backspace up, F2 rename, Del recycle, Ctrl+C/X/V copy/cut/paste, Ctrl+N new folder,
// Ctrl+T/W/Tab tabs, Ctrl+M thumbnails, Ctrl+L/F4 address bar, Ctrl+F or "? terms" in the bar = instant search through the rs service, right click / Menu key = shell context menu.
// Look: topbar's palette, painted tabs / toolbar / address pill and status line (src/ui.rs), dark title bar and list.
// Clipboard is the system one (Ctrl+C/X/V interoperate with Explorer). Also: Alt+Left/Right back/forward, Alt+Up up,
// Alt+Enter properties, F5 refresh, Ctrl+H hidden files, click a column header to sort, right click on empty space = folder menu.
// shortcuts: std::fs listing (not FindFirstFileEx large fetch), icons are extension-based (exe/lnk/ico/url/scr
// per file) and synchronous (thumbnails: one background thread, 96px, no disk cache), internal copy/cut clipboard, silent rename errors,
// search blocks the UI while it waits for the service (no async), the service pipe is admin-only so run as admin,
// drops always land in the current folder (not on a folder row), tabs remember only the folder (no selection/scroll).
// upgrade: each of those one at a time; then register as the folder-open handler.
// rust-explorer --desktop  - the desktop for rs-shell: borderless bottom-most color-keyed window with the Desktop folders
//   (user + public) as big icons over the wallpaper it paints itself (without explorer nothing else does): reads the BMP
//   that rs-shell converts into %LOCALAPPDATA%\rs-shell\wallpaper.bmp, "fill" style, scaled once at start.
//   shortcuts: wallpaper is loaded once (changing it needs a restart), icon labels are always white, no background menu /
//   drag-to-position / auto-arrange, items from the public desktop can't be renamed.
#![windows_subsystem = "windows"]
use std::{cell::RefCell, env, fs, os::windows::fs::MetadataExt, path::{Path, PathBuf}, ptr, sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed}};
use winrust_common::{t, tf};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::{GetStockObject, InvalidateRect, ScreenToClient}, System::LibraryLoader::GetModuleHandleW, UI::{Controls::*, Input::KeyboardAndMouse::{GetCapture, GetKeyState, SetFocus, VK_CONTROL, VK_SHIFT}, Shell::*, WindowsAndMessaging::*}};

mod zipx; // our own ZIP engine (deflate, inflate, container)
mod jobs; // our own copier, ZIP packer, ZIP/7z unpacker (background, progress in the status line)
mod shellwin; // registers the window as a shell window: "Show in folder" from other apps selects the file
mod ctxmenu; // the Win11-style multi-column context menu (shell items laid out to the right of the click)
mod fileops; // copy / move / delete: queue, progress window, conflicts, undo
mod ui; // the look: palette, painted tabs / toolbar / address pill / status line, dark theme
mod cfg; // settings.ini: the window place, the show switches, per-folder views, folded sections
mod hotkey; // Win+E: a resident copy of this exe hooks the key and starts the real one
mod settings; // the gear's popup: preferences with their own window
mod everything; // search through the running Everything over its WM_COPYDATA IPC

static DESKTOP: AtomicBool = AtomicBool::new(false);
fn desk() -> bool { DESKTOP.load(Relaxed) }
const CLR_NONE: isize = 0xFFFF_FFFF; // list background / text background: let the parent's wallpaper show through
thread_local! { static WALL: std::cell::Cell<(isize, i32, i32)> = const { std::cell::Cell::new((0, 0, 0)) }; } // (HBITMAP, w, h)
/// The virtual screen (every monitor): x, y, width, height. The desktop window covers it so each monitor gets its wallpaper.
fn virt() -> (i32, i32, i32, i32) {
    unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)) }
}
/// The desktop's icon list stays on the primary monitor (at screen 0,0, i.e. -vx, -vy inside the desktop window), between the bar's strips.
fn place_desk_list(list: HWND) {
    let (vx, vy, ..) = virt();
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    unsafe { MoveWindow(list, -vx, -vy + desk_top(), sw, (sh - desk_top() - desk_bottom()).max(0), 1) };
}
/// Renders the wallpaper for the current monitor layout (at start and when monitors change), freeing the old one.
fn reload_wall() {
    let (vx, vy, cw, ch) = virt();
    let old = WALL.replace((wall::load(vx, vy, cw, ch), cw, ch)).0;
    if old != 0 { unsafe { windows_sys::Win32::Graphics::Gdi::DeleteObject(old as _) }; }
}
fn parse_place(s: &str) -> Option<(bool, RECT)> {
    let mut it = s.split_whitespace();
    let max = match it.next()? { "max" => true, "normal" => false, _ => return None };
    let n: Vec<i32> = it.map(str::parse).collect::<Result<_, _>>().ok()?;
    let [left, top, right, bottom] = n[..] else { return None };
    (right - left >= 200 && bottom - top >= 150).then_some((max, RECT { left, top, right, bottom })) // junk or a collapsed window: default size
}
fn save_place(h: HWND) {
    let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    wp.length = size_of::<WINDOWPLACEMENT>() as u32;
    if unsafe { GetWindowPlacement(h, &mut wp) } == 0 { return; }
    let r = wp.rcNormalPosition;
    cfg::set("window", "place", &format!("{} {} {} {} {}", if wp.showCmd == SW_SHOWMAXIMIZED as u32 { "max" } else { "normal" }, r.left, r.top, r.right, r.bottom));
}
/// The saved place, if it is still on a monitor (one may have been unplugged since).
fn saved_place() -> Option<(bool, RECT)> {
    use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
    let (max, r) = parse_place(&cfg::get("window", "place")?)?;
    (!unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONULL) }.is_null()).then_some((max, r))
}

// ---------- per-folder view memory (view, sort, grouping, column widths) and global show switches ----------

fn pref_key(dir: &PathBuf) -> String { dir.to_string_lossy().to_lowercase() }

/// The folded sections of a tile page, one flag per heading (the number of headings can change between runs).
fn saved_folds(dir: &Path) -> Vec<bool> { cfg::get("fold", &dir.to_string_lossy().to_lowercase()).map(|s| s.chars().map(|c| c == '1').collect()).unwrap_or_default() }
fn save_folds() {
    let (dir, flags) = ST.with_borrow(|st| (st.dir.clone(), st.tile_collapsed.clone()));
    if flags.is_empty() { return; }
    let s: String = flags.iter().map(|b| if *b { '1' } else { '0' }).collect();
    cfg::set("fold", &pref_key(&dir), &s);
}

/// (view, sort column, ascending, group mode, the four column widths) saved for `dir`.
fn read_view_pref(dir: &PathBuf) -> Option<(ViewMode, usize, bool, usize, [i32; 4])> {
    let l = cfg::get("view", &pref_key(dir))?;
    let mut it = l.split('\t');
    let view = ViewMode::from_code(it.next()?.parse().ok()?)?;
    let sort = it.next()?.parse().ok()?;
    let asc = it.next()? == "1";
    let group = it.next()?.parse().ok()?;
    let w: Vec<i32> = it.filter_map(|x| x.parse().ok()).collect();
    if w.len() == 4 { return Some((view, sort, asc, group, [w[0], w[1], w[2], w[3]])); }
    None
}

/// Saves how the current folder is shown (called after a view, sort or grouping change and column drags).
pub(crate) fn save_current_view() {
    if desk() || in_zip() { return; }
    let (dir, view) = ST.with_borrow(|st| (st.dir.clone(), st.view));
    let list = ST.with_borrow(|st| st.list);
    let defaults = [ui::sc(560), ui::sc(160), ui::sc(140), ui::sc(96)];
    // icon views report no column widths: keep the ones saved for this folder (or the defaults)
    let widths = if view == ViewMode::Details {
        let w: Vec<i32> = (0..4).map(|k| put(list, LVM_GETCOLUMNWIDTH, k as usize, 0) as i32).collect();
        if w.iter().all(|&x| x > 0) { [w[0], w[1], w[2], w[3]] } else { defaults }
    } else {
        read_view_pref(&dir).map(|p| p.4).filter(|w| w.iter().all(|&x| x > 0)).unwrap_or(defaults)
    };
    let line = format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}", view.code(), SORT_COL.load(Relaxed), u8::from(SORT_ASC.load(Relaxed)), GROUP.load(Relaxed), widths[0], widths[1], widths[2], widths[3]);
    cfg::set("view", &pref_key(&dir), &line);
}

/// (compact, extensions, navigation pane, drive usage, check boxes, hidden items)
fn read_globals() -> (bool, bool, bool, bool, bool, bool) {
    (
        cfg::boolean("ui", "compact").unwrap_or(false),
        cfg::boolean("ui", "ext").unwrap_or_else(|| !hide_extensions()),
        cfg::boolean("ui", "nav").unwrap_or(true),
        cfg::boolean("ui", "cap").unwrap_or(true),
        cfg::boolean("ui", "checks").unwrap_or(false),
        cfg::boolean("ui", "hidden").unwrap_or(false),
    )
}

pub(crate) fn save_globals() {
    let mut ini = cfg::load();
    for (k, on) in [
        ("compact", COMPACT.load(Relaxed)),
        ("ext", SHOW_EXT.load(Relaxed)),
        ("nav", NAV_ON.load(Relaxed)),
        ("cap", SHOW_CAP.load(Relaxed)),
        ("checks", CHECK_BOXES.load(Relaxed)),
        ("hidden", SHOW_HIDDEN.load(Relaxed)),
    ] {
        ini.set("ui", k, if on { "1" } else { "0" });
    }
    ini.save();
}
/// What the bar reserves at the top and bottom of the screen: the work area topbar sets when there is no explorer.
fn work_strips() -> (i32, i32) {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut _ as *mut _, 0) } == 0 { return (0, 0); }
    (r.top.max(0), (unsafe { GetSystemMetrics(SM_CYSCREEN) } - r.bottom).max(0))
}
/// Margins of the desktop's icon list, from the bar's real size: its reserved strip (any height, top or bottom).
/// The dock style reserves nothing but floats over the bottom edge, so it gets a fixed gap while topbar runs.
fn desk_top() -> i32 { work_strips().0 + ui::sc(8) }
fn desk_bottom() -> i32 {
    let reserved = work_strips().1;
    if reserved > 0 { return reserved + ui::sc(4); }
    let bar_up = unsafe { !FindWindowW(w("topbar").as_ptr(), ptr::null()).is_null() };
    if bar_up && winrust_common::style::style() == winrust_common::style::Style::Dock { ui::sc(52) } else { ui::sc(8) }
}
const THUMB: i32 = 96;
const WM_THUMB: u32 = WM_APP + 2; // worker -> UI: wp = Box<(era, row, HBITMAP)>
static GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0); // bumped on every new listing; stale thumbnails are dropped
static TX: std::sync::OnceLock<std::sync::mpsc::Sender<(u32, usize, PathBuf, i32)>> = std::sync::OnceLock::new();
const WM_GO: u32 = WM_APP + 1; // address bar: Enter (wp 13) / Esc (wp 27)
const WM_SEARCH: u32 = WM_APP + 7; // search box: Enter (wp 13) / Esc (wp 27)
static EDIT_OLD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static SEARCH_OLD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static LABEL_OLD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0); // the rename box (only one exists at a time)

#[derive(Clone)]
struct Row { name: String, origin: Option<PathBuf>, target: Option<PathBuf>, kind: Option<String>, dir: bool, size: u64, time: u64, icon: i32, thumb: i32, hdr: bool } // origin: where a recycle item came from; target: real path of a virtual page row; kind: the Type column there

/// Explorer's view modes. Details and the icon sizes; Tiles shows 48 px pictures with type and size.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode { Details, List, Small, Medium, Large, ExtraLarge, Tiles }
impl ViewMode {
    fn from_action(n: usize) -> Option<Self> {
        Some(match n { 80 => Self::ExtraLarge, 81 => Self::Large, 82 => Self::Medium, 83 => Self::Small, 84 => Self::List, 85 => Self::Details, 86 => Self::Tiles, _ => return None })
    }
    fn code(self) -> u8 { match self { Self::Details => 0, Self::List => 1, Self::Small => 2, Self::Medium => 3, Self::Large => 4, Self::ExtraLarge => 5, Self::Tiles => 6 } }
    fn from_code(n: u8) -> Option<Self> { Some(match n { 0 => Self::Details, 1 => Self::List, 2 => Self::Small, 3 => Self::Medium, 4 => Self::Large, 5 => Self::ExtraLarge, 6 => Self::Tiles, _ => return None }) }
    fn icon_px(self) -> i32 { match self { Self::Medium | Self::Tiles => 48, Self::Large => 96, Self::ExtraLarge => 256, _ => 16 } }
    fn wants_thumbs(self) -> bool { matches!(self, Self::Medium | Self::Large | Self::ExtraLarge | Self::Tiles) }
}

/// Back / forward trail of visited folders (one for the whole window, not per tab).
#[derive(Default)]
struct Hist { v: Vec<PathBuf>, pos: usize }
impl Hist {
    fn visit(&mut self, p: PathBuf) {
        if self.v.get(self.pos) == Some(&p) { return; }
        self.v.truncate(self.pos + usize::from(!self.v.is_empty())); // a new visit drops the forward trail
        self.v.push(p);
        self.pos = self.v.len() - 1;
    }
    fn back(&mut self) -> Option<PathBuf> { if self.pos > 0 { self.pos -= 1; Some(self.v[self.pos].clone()) } else { None } }
    fn forward(&mut self) -> Option<PathBuf> { if self.pos + 1 < self.v.len() { self.pos += 1; Some(self.v[self.pos].clone()) } else { None } }
}

struct State { list: HWND, tab: HWND, edit: HWND, search: HWND, dir: PathBuf, rows: Vec<Row>, all: Vec<Row>, thumbs: bool, tiles: bool, tile_starts: Vec<(String, usize)>, tile_bands: Vec<(String, i32, bool)>, tile_collapsed: Vec<bool>, view: ViewMode, himl: isize, iml_px: i32, tabs: Vec<PathBuf>, active: usize, hist: Hist, closed: Vec<PathBuf> }
thread_local! {
    static ST: RefCell<State> = RefCell::new(State { list: 0 as _, tab: 0 as _, edit: 0 as _, search: 0 as _, dir: PathBuf::new(), rows: vec![], all: vec![], thumbs: false, tiles: false, tile_starts: vec![], tile_bands: vec![], tile_collapsed: vec![], view: ViewMode::Details, himl: 0, iml_px: 96, tabs: vec![], active: 0, hist: Hist { v: vec![], pos: 0 }, closed: vec![] });
    static COLLAPSED: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new()); // group headings the user collapsed
}
static SORT_COL: AtomicUsize = AtomicUsize::new(0); // 0 name, 1 size, 2 modified
static SORT_ASC: AtomicBool = AtomicBool::new(true);
static SHOW_HIDDEN: AtomicBool = AtomicBool::new(false); // Ctrl+H
static SHOW_EXT: AtomicBool = AtomicBool::new(true); // View > File name extensions
static COMPACT: AtomicBool = AtomicBool::new(false); // View > Compact view
pub static NAV_ON: AtomicBool = AtomicBool::new(true); // View > Show > Navigation pane
pub static SHOW_CAP: AtomicBool = AtomicBool::new(true); // settings: drive usage under the drive names in the pane
pub static CHECK_BOXES: AtomicBool = AtomicBool::new(false); // View > Show > Item check boxes (drawn by us)
thread_local! { static CHECKED: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new()); } // names with their box ticked
pub fn checked(name: &str) -> bool { CHECKED.with_borrow(|s| s.contains(name)) }
fn tick(name: &str) -> bool { CHECKED.with_borrow_mut(|s| if s.remove(name) { false } else { s.insert(name.to_string()); true }) }
static NAV: AtomicBool = AtomicBool::new(false); // loading from the back / forward trail: do not record it again
const WM_NAV: u32 = WM_APP + 3; // wp: 1 back, 2 forward, 3 properties, 4 up (sent from the message loop for Alt+key)
const WM_JOB: u32 = WM_APP + 5; // jobs.rs worker finished: wp = Box<Option<String>> (the error, if any)
const WM_REFRESH: u32 = WM_APP + 8; // the folder changed on disk (watch thread)
static WATCH_GEN: AtomicU32 = AtomicU32::new(0); // bumped on every load: older watch threads exit

fn row(name: String, m: Option<&fs::Metadata>) -> Row {
    Row { name, origin: None, target: None, kind: None, dir: m.is_some_and(|m| m.is_dir()), size: m.map_or(0, |m| m.len()), time: m.map_or(0, |m| m.last_write_time()), icon: -1, thumb: -2, hdr: false }
}

/// Folders first when ungrouped; grouped by date / name / type / size, then by the chosen column.
fn sort_rows(rows: &mut [Row], col: usize, asc: bool) {
    let key = |r: &Row| r.name.rsplit('\\').next().unwrap_or("").to_lowercase();
    let order = |a: &Row, b: &Row| {
        let o = match col { 1 => a.time.cmp(&b.time), 2 => type_text(&a.name, a.dir).cmp(&type_text(&b.name, b.dir)), 3 => a.size.cmp(&b.size), _ => key(a).cmp(&key(b)) };
        if asc { o } else { o.reverse() }
    };
    if group_on() {
        if GROUP.load(Relaxed) == 1 { bucket_refresh(); }
        rows.sort_by(|a, b| group_key(a).0.cmp(&group_key(b).0).then_with(|| order(a, b)).then_with(|| key(a).cmp(&key(b))));
    } else {
        rows.sort_by(|a, b| (!a.dir).cmp(&!b.dir).then_with(|| order(a, b)).then_with(|| key(a).cmp(&key(b))));
    }
}

// ---------- grouping ("Group by" in the view menu): date, name, type, size ----------

const DAY: i64 = 864_000_000_000; // FILETIME ticks per day
/// The date headings of "Group by date", in bucket order: shown above the rows of each group.
fn bucket_name(i: usize) -> &'static str {
    [
        t!("Сегодня", "Today", ja: "今日", pt: "Hoje", zh: "今天"),
        t!("Вчера", "Yesterday", ja: "昨日", pt: "Ontem", zh: "昨天"),
        t!("Ранее на этой неделе", "Earlier this week", ja: "今週", pt: "No início desta semana", zh: "本周早些时候"),
        t!("На прошлой неделе", "Last week", ja: "先週", pt: "Semana passada", zh: "上周"),
        t!("Ранее в этом месяце", "Earlier this month", ja: "今月", pt: "No início deste mês", zh: "本月早些时候"),
        t!("В прошлом месяце", "Last month", ja: "先月", pt: "Mês passado", zh: "上个月"),
        t!("Ранее в этом году", "Earlier this year", ja: "今年", pt: "No início deste ano", zh: "今年早些时候"),
        t!("Давно", "A long time ago", ja: "ずっと前", pt: "Há muito tempo", zh: "很久以前"),
    ][i]
}

thread_local! {
    static BCTX: std::cell::Cell<(i64, i64)> = const { std::cell::Cell::new((0, 0)) }; // (today's local day number, local - UTC in ticks)
}

/// 0 none, 1 date, 2 name, 3 type, 4 size.
static GROUP: AtomicUsize = AtomicUsize::new(0);
static THUMBS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false); // not the details view: no headings
fn group_on() -> bool { !desk() && !THUMBS.load(Relaxed) && GROUP.load(Relaxed) != 0 }

/// Takes "now" and the time zone offset once, so bucket() is plain arithmetic while a folder is sorted and painted.
fn bucket_refresh() {
    use windows_sys::Win32::{Foundation::FILETIME, Storage::FileSystem::FileTimeToLocalFileTime};
    let now = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() / 100) as i64) + 116_444_736_000_000_000;
    let (utc, mut local) = (FILETIME { dwLowDateTime: now as u32, dwHighDateTime: (now >> 32) as u32 }, FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 });
    let off = if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } != 0 { ((local.dwHighDateTime as i64) << 32 | local.dwLowDateTime as i64) - now } else { 0 };
    BCTX.set(((now + off) / DAY, off));
}

/// Index into BUCKETS of a FILETIME (UTC ticks).
fn bucket(time: u64) -> i32 {
    let (today, off) = BCTX.get();
    match today - (time as i64 + off) / DAY {
        i64::MIN..=0 => 0,
        1 => 1,
        2..=6 => 2,
        7..=13 => 3,
        14..=29 => 4,
        30..=59 => 5,
        60..=364 => 6,
        _ => 7,
    }
}

/// The Windows 11 size groups: folders first, then Empty, Tiny, Small, Medium, Large, Huge, Gigantic.
fn size_group(r: &Row) -> (u8, String) {
    if r.dir { return (0, t!("Папка с файлами", "File folder", ja: "ファイル フォルダー", pt: "Pasta de arquivos", zh: "文件夹").into()); }
    let b = r.size;
    let (n, label) = if b == 0 {
        (1, t!("Пустой файл", "Empty", ja: "空", pt: "Vazio", zh: "空") )
    } else if b < 16 << 10 {
        (2, t!("Крошечный", "Tiny", ja: "極小", pt: "Mínimo", zh: "极小"))
    } else if b < 1 << 20 {
        (3, t!("Маленький", "Small", ja: "小", pt: "Pequeno", zh: "小"))
    } else if b < 16 << 20 {
        (4, t!("Средний", "Medium", ja: "中", pt: "Médio", zh: "中"))
    } else if b < 128 << 20 {
        (5, t!("Большой", "Large", ja: "大", pt: "Grande", zh: "大"))
    } else if b < 1 << 30 {
        (6, t!("Огромный", "Huge", ja: "特大", pt: "Enorme", zh: "巨大"))
    } else {
        (7, t!("Гигантский", "Gigantic", ja: "巨大", pt: "Gigantesco", zh: "超大"))
    };
    (n, label.into())
}

/// (sort key, heading) of the row in the current grouping.
fn group_key(r: &Row) -> (String, String) {
    match GROUP.load(Relaxed) {
        1 => { let b = bucket(r.time) as usize; (format!("{b:02}"), bucket_name(b).to_string()) }
        2 => {
            let c = r.name.rsplit('\\').next().unwrap_or(&r.name).chars().next().unwrap_or('#').to_uppercase().next().unwrap_or('#');
            let label = if c.is_alphanumeric() { c.to_string() } else { "#".into() };
            (label.to_lowercase(), label)
        }
        3 => { let label = type_text(&r.name, r.dir); (label.to_lowercase(), label) }
        _ => { let (n, label) = size_group(r); (format!("{n:02}"), label) }
    }
}

/// A heading row before the first row of each group. Its name starts with '?', which no file can have.
fn add_headers(rows: &mut Vec<Row>) {
    if !group_on() { return; }
    if GROUP.load(Relaxed) == 1 { bucket_refresh(); }
    let mut out = Vec::with_capacity(rows.len() + 8);
    let mut last = String::new();
    for r in rows.drain(..) {
        let (_, label) = group_key(&r);
        if label != last {
            last = label.clone();
            out.push(Row { name: format!("?{label}"), origin: None, target: None, kind: None, dir: false, size: 0, time: 0, icon: -1, thumb: -1, hdr: true });
        }
        out.push(r);
    }
    *rows = out;
}

/// Notes the group headings the user collapsed and removes their rows from the display list.
pub(crate) fn group_collapsed(label: &str) -> bool { COLLAPSED.with_borrow(|s| s.contains(label)) }

fn visible_rows(all: &[Row]) -> Vec<Row> {
    let mut out = Vec::with_capacity(all.len());
    let mut closed = false;
    for r in all {
        if r.hdr { closed = group_collapsed(&r.name[1..]); out.push(r.clone()); }
        else if !closed { out.push(r.clone()); }
    }
    out
}

fn toggle_group(h: HWND, label: &str) {
    COLLAPSED.with_borrow_mut(|s| { if !s.remove(label) { s.insert(label.to_string()); } });
    refilter(h);
}

fn refilter(h: HWND) {
    let (list, n) = ST.with_borrow_mut(|st| {
        st.all.iter_mut().for_each(|r| r.thumb = -2);
        st.rows = visible_rows(&st.all);
        (st.list, st.rows.len())
    });
    reset_thumbs();
    put(list, LVM_DELETEALLITEMS, 0, 0);
    put(list, LVM_SETITEMCOUNT, n, 0);
    if n > 0 { put(list, LVM_REDRAWITEMS, 0, n as isize - 1); }
    unsafe { ui::status_dirty(h) };
}

/// Explorer's "Type" column in short form: "File folder", "TXT file", "File".
fn type_text(name: &str, dir: bool) -> String {
    if dir { return t!("Папка с файлами", "File folder", ja: "ファイル フォルダー", pt: "Pasta de arquivos", zh: "文件夹").into(); }
    let file = name.rsplit('\\').next().unwrap_or(name);
    match file.rsplit_once('.') { Some((stem, e)) if !stem.is_empty() && !e.is_empty() => tf!("Файл {}", "{} file", ja: "{} ファイル", pt: "Arquivo {}", zh: "{} 文件", e.to_uppercase()), _ => t!("Файл", "File", ja: "ファイル", pt: "Arquivo", zh: "文件").into() }
}

/// Explorer shows file sizes in KB; the bigger ones read better in MB / GB.
fn size_text(b: u64) -> String {
    const MB: u64 = 1 << 20;
    if b < MB { format!("{} KB", b.div_ceil(1024)) } else if b < 1 << 30 { format!("{:.1} MB", b as f64 / MB as f64) } else { format!("{:.2} GB", b as f64 / (1u64 << 30) as f64) }
}

fn date_text(ft: u64) -> String {
    use windows_sys::Win32::{Foundation::{FILETIME, SYSTEMTIME}, Globalization::{GetDateFormatW, GetTimeFormatW, DATE_SHORTDATE, LOCALE_USER_DEFAULT, TIME_NOSECONDS}, System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime}};
    if ft == 0 { return String::new(); }
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let (mut u, mut l): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&f, &mut u) == 0 || SystemTimeToTzSpecificLocalTime(ptr::null(), &u, &mut l) == 0 { return String::new(); }
        let (mut d, mut t) = ([0u16; 64], [0u16; 64]);
        let dn = GetDateFormatW(LOCALE_USER_DEFAULT, DATE_SHORTDATE, &l, ptr::null(), d.as_mut_ptr(), d.len() as i32);
        let tn = GetTimeFormatW(LOCALE_USER_DEFAULT, TIME_NOSECONDS, &l, ptr::null(), t.as_mut_ptr(), t.len() as i32);
        if dn <= 0 || tn <= 0 { return String::new(); }
        format!("{} {}", String::from_utf16_lossy(&d[..(dn - 1) as usize]), String::from_utf16_lossy(&t[..(tn - 1) as usize]))
    }
}

/// Our own tools installed by the launcher (photoview, wiztree): the exe in Program Files, None when not installed.
fn tool(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env::var_os("ProgramFiles")?).join(name).join(format!("{name}.exe"));
    p.is_file().then_some(p)
}

/// Opens a console in `dir`: Windows Terminal when installed (Win11's "Open in Terminal"), a classic cmd window otherwise.
fn open_terminal(dir: &Path) {
    let wt = env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join(r"Microsoft\WindowsApps\wt.exe"));
    match wt.filter(|p| p.is_file()) {
        Some(wt) => { let _ = std::process::Command::new(wt).arg("-d").arg(dir).spawn(); }
        None => { let _ = std::process::Command::new("cmd.exe").current_dir(dir).spawn(); }
    }
}

/// `prog`, or `prog.exe` / .cmd / .bat / .com, found in the PATH (a path with a directory in it is taken as it is).
fn which(prog: &str) -> Option<PathBuf> {
    let p = Path::new(prog);
    if p.components().count() > 1 { return p.is_file().then(|| p.to_path_buf()); }
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        for ext in ["", ".exe", ".cmd", ".bat", ".com"] {
            let c = dir.join(format!("{prog}{ext}"));
            if c.is_file() { return Some(c); }
        }
    }
    None
}

/// Explorer's address bar behaviour: text that is not a folder but names a program runs it here, with the rest as arguments
/// ("cmd", "powershell -NoLogo", "wt"). true when something was started.
fn run_command_line(owner: HWND, dir: &Path, line: &str) -> bool {
    let Some(prog) = line.split_whitespace().next() else { return false };
    let Some(exe) = which(prog) else { return false };
    let params = line[prog.len()..].trim_start();
    // ShellExecute, like Explorer: it gives a console program a console of its own; Command would hand it our (missing)
    // stdin and cmd would exit at once
    let (verb, file, args, cwd) = (w("open"), w(&exe.to_string_lossy()), w(params), w(&dir.to_string_lossy()));
    let r = unsafe { ShellExecuteW(owner, verb.as_ptr(), file.as_ptr(), if params.is_empty() { ptr::null() } else { args.as_ptr() }, cwd.as_ptr(), SW_SHOWNORMAL) };
    r as usize > 32
}

/// Same list as photoview's EXTS.
fn is_pic(p: &std::path::Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| ["jpg", "jpeg", "jfif", "png", "bmp", "gif", "tif", "tiff", "webp", "heic", "avif"].contains(&e.to_ascii_lowercase().as_str()))
}

fn properties(path: &PathBuf) {
    let (f, v) = (w(&path.to_string_lossy()), w("properties"));
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = 0x0C; // SEE_MASK_INVOKEIDLIST
    sei.lpVerb = v.as_ptr(); sei.lpFile = f.as_ptr(); sei.nShow = SW_SHOW;
    unsafe { ShellExecuteExW(&mut sei) };
}

/// New folder / new empty file with a free name ("New folder", "New folder (2)", ...).
fn new_entry(dir: &PathBuf, base: &str, folder: bool) -> Result<PathBuf, String> {
    if is_virtual(dir) {
        return Err(t!("Действие недоступно на этой странице", "Not available on this page", ja: "このページでは使用できません", pt: "Indisponível nesta página", zh: "此页面不支持该操作").into());
    }
    if zipx::split_zip(dir).is_some() {
        return Err(t!("Архив открыт только для чтения", "An archive is read-only", ja: "アーカイブは読み取り専用です", pt: "Um arquivo é somente leitura", zh: "压缩包在这里是只读的").into());
    }
    let (stem, ext) = match base.rsplit_once('.') { Some((s, e)) if !folder => (s.to_string(), format!(".{e}")), _ => (base.to_string(), String::new()) };
    let path = (1..).map(|n| dir.join(if n == 1 { base.to_string() } else { format!("{stem} ({n}){ext}") })).find(|p| !p.exists()).ok_or("no free name")?;
    if folder { fs::create_dir(&path) } else { fs::File::create(&path).map(drop) }.map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Explorer's way: the new row is selected with its name in the rename box, so it is clear it exists and where it went.
fn create_new(h: HWND, dir: &PathBuf, base: &str, folder: bool) {
    match new_entry(dir, base, folder) {
        Ok(p) => {
            reload(h);
            let name = p.file_name().map_or_else(|| p.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
            select_edit(&name);
        }
        Err(e) => warn(h, &e),
    }
}

/// Selects the row called `name` and opens its rename box.
fn select_edit(name: &str) {
    let (list, i) = ST.with_borrow(|st| (st.list, st.rows.iter().position(|r| !r.hdr && r.name.eq_ignore_ascii_case(name))));
    let Some(i) = i else { return };
    unsafe {
        let mut it: LVITEMW = std::mem::zeroed();
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        it.state = LVIS_SELECTED | LVIS_FOCUSED;
        put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
        put(list, LVM_ENSUREVISIBLE, i, 0);
        SetFocus(list);
        put(list, LVM_EDITLABELW, i, 0);
    }
}

/// Pastes what is on the system clipboard (files copied or cut here or in any other file manager). Real folders go through
/// our own engine (queue, progress window, conflicts, undo); a folder that needs elevation is left to the shell.
fn paste(h: HWND, dir: &PathBuf) {
    if is_virtual(dir) || zipx::split_zip(dir).is_some() { return; }
    let Some((paths, cut)) = sh::clip_get() else { return };
    if jobs::writable(dir) {
        fileops::submit(h, if cut { fileops::Kind::Move } else { fileops::Kind::Copy }, paths, dir.clone());
    } else {
        let _ = sh::fileop(h, if cut { sh::Op::Move } else { sh::Op::Copy }, &paths, dir);
    }
    if cut { sh::clip_clear(); }
}

const MENU_ZIP: u32 = 0x9001;
const MENU_EXTRACT: u32 = 0x9002;
const MENU_PIN: u32 = 0x9003;
const MENU_RENAME: u32 = 0x9004;

/// True while the window shows the inside of a .zip: read-only, its rows are no files on disk.
fn in_zip() -> bool { zipx::split_zip(&cur_dir()).is_some() }

fn warn(h: HWND, text: &str) {
    unsafe { MessageBoxW(h, w(text).as_ptr(), w("WinRust Explorer").as_ptr(), MB_ICONWARNING) };
}

/// (archive, archive paths) when every one of `paths` is something inside one and the same .zip.
fn zip_names(paths: &[PathBuf]) -> Option<(PathBuf, Vec<String>)> {
    let mut arc = None;
    let mut names = vec![];
    for p in paths {
        let (a, inner) = zipx::split_zip(p).filter(|(_, i)| !i.is_empty())?;
        if arc.get_or_insert_with(|| a.clone()) != &a { return None; }
        names.push(inner);
    }
    Some((arc?, names))
}

/// Things from an archive as real files in a temp folder: the clipboard, drag and "open with" need files on disk. One path per name.
/// Folders of earlier days are cleaned up here.
fn materialize(arc: &Path, names: &[String]) -> Result<Vec<PathBuf>, String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let root = env::temp_dir().join("WinRust-zip");
    for e in fs::read_dir(&root).into_iter().flatten().flatten() {
        if e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > std::time::Duration::from_secs(86_400)) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    let out = root.join(format!("{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
    zipx::extract_some(arc, names, &out)?;
    Ok(names.iter().map(|n| out.join(n.rsplit('/').next().unwrap_or(n))).collect())
}

/// `paths` as files on disk: unchanged, or taken out of the archive they are in first.
fn real_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    match zip_names(paths) {
        Some((arc, names)) => materialize(&arc, &names),
        None => Ok(paths.to_vec()),
    }
}

/// Enter / double click on a row.
unsafe fn activate(h: HWND, p: PathBuf, is_dir: bool) {
    // a file inside a zip: a real copy in the temp folder first
    let p = match zip_names(std::slice::from_ref(&p)) {
        Some((arc, names)) if !is_dir => match materialize(&arc, &names) {
            Ok(mut v) => v.remove(0),
            Err(e) => return warn(h, &e),
        },
        _ => p,
    };
    if is_dir && desk() { let _ = std::process::Command::new(env::current_exe().unwrap_or_default()).arg(p).spawn(); }
    else if is_dir { load(h, p) }
    else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) && p.is_file() { load(h, p) } // a zip opens like a folder; "Extract here" is in its menu
    else if let Some(exe) = tool("photoview").filter(|_| is_pic(&p)) { let _ = std::process::Command::new(exe).arg(&p).spawn(); }
    else {
        // like Explorer: the exe starts in its own folder (games look for files next to it)
        let dir = p.parent().map(|d| w(&d.to_string_lossy()));
        unsafe { ShellExecuteW(h, ptr::null(), w(&p.to_string_lossy()).as_ptr(), ptr::null(), dir.as_ref().map_or(ptr::null(), |d| d.as_ptr()), SW_SHOWNORMAL) };
    }
}

/// The menu for rows inside a zip. They are no shell items, so this is a small menu of our own.
fn zip_menu(h: HWND, paths: &[PathBuf]) {
    let id = unsafe {
        let m = CreatePopupMenu();
        let items = [
            (1usize, t!("Открыть", "Open", ja: "開く", pt: "Abrir", zh: "打开")),
            (2, t!("Копировать", "Copy", ja: "コピー", pt: "Copiar", zh: "复制")),
            (3, t!("Извлечь сюда", "Extract here", ja: "ここに展開", pt: "Extrair aqui", zh: "解压到此处")),
        ];
        for (i, lbl) in items { AppendMenuW(m, MF_STRING, i, w(lbl).as_ptr()); }
        let mut pt: POINT = std::mem::zeroed();
        GetCursorPos(&mut pt);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, h, ptr::null());
        DestroyMenu(m);
        id
    };
    let Some((arc, names)) = zip_names(paths) else { return };
    match id {
        1 => if let [p] = paths {
            let is_dir = ST.with_borrow(|st| st.rows.iter().any(|r| r.dir && st.dir.join(&r.name) == *p));
            unsafe { activate(h, p.clone(), is_dir) };
        },
        2 => match materialize(&arc, &names) {
            Ok(v) => { let _ = sh::clip_set(h, &v, false); }
            Err(e) => warn(h, &e),
        },
        3 => {
            // next to the archive, in a new folder named after it; no progress yet (shortcut: fire and forget)
            let (Some(parent), Some(stem)) = (arc.parent(), arc.file_stem()) else { return };
            let out = jobs::free_name(parent, &stem.to_string_lossy(), true);
            std::thread::spawn(move || _ = zipx::extract_some(&arc, &names, &out));
        }
        _ => {}
    }
}

/// The shell menu for the selection, with our "Compress to ZIP" / "Extract here" on top.
fn item_menu(h: HWND, dir: &Path, paths: &[PathBuf]) {
    if zipx::split_zip(dir).is_some() { return zip_menu(h, paths); }
    if cur_virtual() { // drives and quick access folders get their own shell menu
        if sh::show(h, paths, &[]).ok().flatten().is_some() { reload_keep(h); }
        return;
    }
    let mut extra = vec![(MENU_ZIP, t!("Сжать в ZIP", "Compress to ZIP", ja: "ZIP に圧縮", pt: "Compactar em ZIP", zh: "压缩为 ZIP"))];
    if paths.iter().all(|p| p.is_file() && jobs::is_archive(p)) {
        extra.insert(0, (MENU_EXTRACT, t!("Извлечь сюда", "Extract here", ja: "ここに展開", pt: "Extrair aqui", zh: "解压到此处")));
    }
    // the shell menu has no Rename here (it needs the shell's own folder view to answer it): the same edit box as F2
    if paths.len() == 1 { extra.push((MENU_RENAME, t!("Переименовать", "Rename", ja: "名前の変更", pt: "Renomear", zh: "重命名"))); }
    if let [p] = paths { if p.is_dir() { extra.push((MENU_PIN, if ui::pins().contains(p) { t!("Открепить от быстрого доступа", "Unpin from Quick access", ja: "クイック アクセスからピン留めを解除", pt: "Desafixar do Acesso rápido", zh: "从快速访问取消固定") } else { t!("Закрепить в быстром доступе", "Pin to Quick access", ja: "クイック アクセスにピン留め", pt: "Fixar no Acesso rápido", zh: "固定到快速访问") })); } }
    let picked = sh::show(h, paths, if jobs::busy() || fileops::busy() { &[] } else { &extra }).ok().flatten();
    match picked {
        Some(MENU_PIN) => ui::toggle_pin(&paths[0]),
        Some(MENU_RENAME) => unsafe {
            let (list, ..) = selected();
            let i = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
            if i >= 0 {
                SetFocus(list);
                put(list, LVM_EDITLABELW, i as usize, 0);
            }
        },
        Some(MENU_ZIP) => {
            let base = if paths.len() == 1 { paths[0].file_stem().unwrap_or_default().to_string_lossy().into_owned() } else { dir.file_name().map_or("Archive".into(), |n| n.to_string_lossy().into_owned()) };
            let dest = jobs::free_name(dir, &format!("{base}.zip"), false);
            jobs::zip(h as isize, paths.to_vec(), dest);
        }
        Some(MENU_EXTRACT) => _ = jobs::extract(h as isize, paths.to_vec(), dir.to_path_buf()),
        Some(_) => reload_keep(h), // a shell verb ran: refresh, keeping the selection and the scroll position
        None => {}                 // dismissed (blank click / Esc): nothing changed, and the list must not jump to the top
    }
}

/// "Hide desktop icons" is a marker file, so it survives restarts.
fn icons_marker() -> PathBuf { PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join(r"WinRust\desktop-icons-hidden.txt") }
fn small_marker() -> PathBuf { icons_marker().with_file_name("small-icons") }
/// Desktop icon size: the system's large (32 px, default) or small (16 px) icons with a tighter grid. Same icon indexes in both.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn apply_icon_size(list: HWND) {
    let small = small_marker().exists();
    let mut fi: SHFILEINFOW = std::mem::zeroed();
    let il = SHGetFileInfoW(w("C:\\").as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | if small { SHGFI_SMALLICON } else { SHGFI_LARGEICON });
    put(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, il as isize);
    let (cx, cy) = if small { (76, 64) } else { (88, 96) };
    put(list, LVM_SETICONSPACING, 0, ((ui::sc(cy) as isize) << 16) | ui::sc(cx) as isize);
}
fn icons_hidden() -> bool { icons_marker().exists() }
fn set_icons_hidden(hide: bool) {
    let m = icons_marker();
    if hide { if let Some(d) = m.parent() { let _ = fs::create_dir_all(d); } let _ = fs::write(&m, "1"); } else { let _ = fs::remove_file(&m); }
    let list = ST.with_borrow(|st| st.list);
    unsafe { ShowWindow(list, if hide { SW_HIDE } else { SW_SHOW }) };
}

static DRAG_DIR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// While a drag hovers near the top / bottom of the list, scroll it (WM_TIMER 11 does the work).
pub fn drag_scroll_at(root: HWND, x: i32, y: i32) -> i32 {
    unsafe {
        let list = ST.with_borrow(|st| st.list);
        let mut lr = RECT::default();
        if GetWindowRect(list, &mut lr) == 0 { return 0; }
        let mut o = POINT { x: lr.left, y: lr.top };
        ScreenToClient(root, &mut o);
        let h = lr.bottom - lr.top;
        let ly = y - o.y;
        if x < o.x || x >= o.x + (lr.right - lr.left) || ly < 0 || ly >= h { return 0; }
        let edge = ui::sc(30);
        if ly < edge { -1 } else if ly > h - edge { 1 } else { 0 }
    }
}

pub fn drag_scroll_stop(h: HWND) {
    DRAG_DIR.store(0, Relaxed);
    unsafe { KillTimer(h, 11); }
}

/// Where a drag would land: the shown folder, a folder under the pointer, or nowhere (a virtual page).
pub enum DropPlace { Here, Folder(PathBuf), No }

/// Resolves a drag's screen point: a folder row, the navigation pane, a tab, or a part of the address pill.
pub fn drop_place_at(root: HWND, x: i32, y: i32) -> DropPlace {
    unsafe {
        let mut c = POINT { x, y };
        ScreenToClient(root, &mut c);
        let (list, dir, tabs) = ST.with_borrow(|st| (st.list, st.dir.clone(), st.tabs.clone()));
        let mut lr = RECT::default();
        if GetWindowRect(list, &mut lr) != 0 {
            let mut o = POINT { x: lr.left, y: lr.top };
            ScreenToClient(root, &mut o);
            let mut lc = RECT::default();
            GetClientRect(list, &mut lc);
            let p = POINT { x: c.x - o.x, y: c.y - o.y };
            if p.x >= 0 && p.y >= 0 && p.x < lc.right && p.y < lc.bottom {
                let mut ht: LVHITTESTINFO = std::mem::zeroed();
                ht.pt = p;
                let i = put(list, LVM_HITTEST, 0, &mut ht as *mut _ as isize);
                let row = usize::try_from(i).ok().and_then(|i| ST.with_borrow(|st| st.rows.get(i).cloned()));
                if let Some(r) = row.filter(|r| !r.hdr && r.dir) {
                    let p = r.target.clone().unwrap_or_else(|| dir.join(&r.name));
                    let s = p.to_string_lossy();
                    if s.starts_with("::") { return DropPlace::No; }
                    if p.is_dir() { return DropPlace::Folder(p); }
                }
                return DropPlace::Here;
            }
        }
        let nav = ui::nav_hwnd();
        if !nav.is_null() {
            let mut nr = RECT::default();
            if GetWindowRect(nav, &mut nr) != 0 {
                let mut o = POINT { x: nr.left, y: nr.top };
                ScreenToClient(root, &mut o);
                if c.x >= o.x && c.y >= o.y && c.x < o.x + (nr.right - nr.left) && c.y < o.y + (nr.bottom - nr.top) {
                    return match ui::nav_target_at(c.x - o.x, c.y - o.y) {
                        Some(p) if p.to_string_lossy().starts_with("::") => DropPlace::No,
                        Some(p) if p.is_dir() => DropPlace::Folder(p),
                        _ => DropPlace::Here,
                    };
                }
            }
        }
        if c.y < ui::sc(ui::CHROME_H) {
            let mut cr = RECT::default();
            GetClientRect(root, &mut cr);
            if let Some(i) = ui::tab_at(cr.right, tabs.len(), c.x, c.y) {
                return match tabs.get(i) {
                    Some(p) if p.is_dir() && !p.to_string_lossy().starts_with("::") => DropPlace::Folder(p.clone()),
                    Some(_) => DropPlace::No,
                    None => DropPlace::Here,
                };
            }
            if let Some(p) = ui::crumb_path_at(c.x, c.y) { return DropPlace::Folder(p); }
            return DropPlace::Here;
        }
        DropPlace::Here
    }
}

/// Right click on empty space: the folder's own menu, in the same Win11 look as the item menu.
#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn bg_menu(h: HWND) {
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    if !desk() && cur_virtual() {
        let mut items = vec![ctxmenu::Item { id: 1, text: t!("Обновить", "Refresh", ja: "最新の情報に更新", pt: "Atualizar", zh: "刷新").into(), glyph: "\u{E72C}", ..Default::default() }];
        if cur_dir().to_string_lossy() == "::recycle" {
            items.push(ctxmenu::Item::separator());
            items.push(ctxmenu::Item { id: 2, text: t!("Очистить корзину", "Empty Recycle Bin", ja: "ごみ箱を空にする", pt: "Esvaziar Lixeira", zh: "清空回收站").into(), glyph: "\u{E74D}", ..Default::default() });
        }
        match ctxmenu::show(h, items, (pt.x, pt.y)) {
            ctxmenu::Pick::Id(1) => refresh_after_ops(h),
            ctxmenu::Pick::Id(2) => recycle_empty(h),
            _ => {}
        }
        return;
    }
    let disk = tool("wiztree");
    if !desk() {
        let action = |id: u32, text: &str, glyph: &'static str| ctxmenu::Item { id, text: text.into(), glyph, ..Default::default() };
        let check = |id: u32, text: &str, on: bool| ctxmenu::Item { id, text: text.into(), checked: on, ..Default::default() };
        let radio = |id: u32, text: &str, on: bool| ctxmenu::Item { id, text: text.into(), radio: true, checked: on, ..Default::default() };
        let sub = |text: &str, glyph: &'static str, items: Vec<ctxmenu::Item>| ctxmenu::Item { text: text.into(), glyph, sub: items, ..Default::default() };
        let checks = CHECK_BOXES.load(Relaxed);
        let show = vec![
            check(90, t!("Область переходов", "Navigation pane", ja: "ナビゲーション ウィンドウ", pt: "Painel de navegação", zh: "导航窗格"), NAV_ON.load(Relaxed)),
            check(93, t!("Флажки элементов", "Item check boxes", ja: "項目のチェック ボックス", pt: "Caixas de seleção de itens", zh: "项目复选框"), checks),
            check(94, t!("Расширения имён файлов", "File name extensions", ja: "ファイル名拡張子", pt: "Extensões de nome de arquivo", zh: "文件扩展名"), SHOW_EXT.load(Relaxed)),
            check(95, t!("Скрытые элементы", "Hidden items", ja: "隠しファイル", pt: "Itens ocultos", zh: "隐藏的项目"), SHOW_HIDDEN.load(Relaxed)),
            check(96, t!("Компактный вид", "Compact view", ja: "コンパクト表示", pt: "Exibição compacta", zh: "紧凑视图"), COMPACT.load(Relaxed)),
        ];
        let view = ST.with_borrow(|st| st.view);
        let modes = [
            (80u32, ViewMode::ExtraLarge, t!("Очень крупные значки", "Extra large icons", ja: "特大アイコン", pt: "Ícones extragrandes", zh: "超大图标")),
            (81, ViewMode::Large, t!("Крупные значки", "Large icons", ja: "大アイコン", pt: "Ícones grandes", zh: "大图标")),
            (82, ViewMode::Medium, t!("Обычные значки", "Medium icons", ja: "中アイコン", pt: "Ícones médios", zh: "中等图标")),
            (83, ViewMode::Small, t!("Мелкие значки", "Small icons", ja: "小アイコン", pt: "Ícones pequenos", zh: "小图标")),
            (84, ViewMode::List, t!("Список", "List", ja: "一覧", pt: "Lista", zh: "列表")),
            (85, ViewMode::Details, t!("Таблица", "Details", ja: "詳細", pt: "Detalhes", zh: "详细信息")),
            (86, ViewMode::Tiles, t!("Плитка", "Tiles", ja: "タイル", pt: "Blocos", zh: "平铺")),
        ];
        let mut views: Vec<ctxmenu::Item> = modes.iter().map(|(id, m, label)| radio(*id, label, view == *m)).collect();
        views.push(ctxmenu::Item::separator());
        views.push(sub(t!("Показать", "Show", ja: "表示", pt: "Mostrar", zh: "显示"), "\u{E7B3}", show));
        let (col, asc) = (SORT_COL.load(Relaxed) as u32, SORT_ASC.load(Relaxed));
        let sorts = vec![
            radio(60, t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"), col == 0),
            radio(62, t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"), col == 1),
            radio(64, t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"), col == 2),
            radio(66, t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"), col == 3),
            ctxmenu::Item::separator(),
            radio(75, t!("По возрастанию", "Ascending", ja: "昇順", pt: "Crescente", zh: "递增"), asc),
            radio(76, t!("По убыванию", "Descending", ja: "降順", pt: "Decrescente", zh: "递减"), !asc),
        ];
        let g = GROUP.load(Relaxed);
        let groups = vec![
            radio(70, t!("Нет", "(None)", ja: "なし", pt: "(Nenhum)", zh: "(无)"), g == 0),
            radio(71, t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"), g == 2),
            radio(72, t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"), g == 1),
            radio(73, t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"), g == 3),
            radio(74, t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"), g == 4),
        ];
        let new = vec![
            action(15, t!("Папка", "Folder", ja: "フォルダー", pt: "Pasta", zh: "文件夹"), ""),
            action(18, t!("Текстовый документ", "Text document", ja: "テキスト ドキュメント", pt: "Documento de texto", zh: "文本文档"), ""),
            action(19, t!("Сжатая ZIP-папка", "Compressed (zipped) folder", ja: "圧縮 (zip 形式) フォルダー", pt: "Pasta compactada (zip)", zh: "压缩(zipped)文件夹"), ""),
        ];
        let paste_on = windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable(15) != 0; // CF_HDROP
        let mut items = vec![
            sub(t!("Просмотр", "View", ja: "表示", pt: "Exibir", zh: "查看"), "\u{E890}", views),
            sub(t!("Сортировка", "Sort by", ja: "並べ替え", pt: "Classificar por", zh: "排序方式"), "\u{E8CB}", sorts),
            sub(t!("Группировать", "Group by", ja: "グループ化", pt: "Agrupar por", zh: "分组依据"), "\u{E8FD}", groups),
            ctxmenu::Item::separator(),
            action(7, t!("Обновить", "Refresh", ja: "最新の情報に更新", pt: "Atualizar", zh: "刷新"), "\u{E72C}"),
            ctxmenu::Item { id: 12, text: t!("Вставить", "Paste", ja: "貼り付け", pt: "Colar", zh: "粘贴").into(), glyph: "\u{E77F}", disabled: !paste_on, ..Default::default() },
            sub(t!("Создать", "New", ja: "新規作成", pt: "Novo", zh: "新建"), "\u{E710}", new),
            action(8, t!("Открыть в терминале", "Open in Terminal", ja: "ターミナルで開く", pt: "Abrir no Terminal", zh: "在终端中打开"), "\u{E756}"),
            ctxmenu::Item::separator(),
        ];
        if disk.is_some() {
            items.push(action(6, t!("Занятое место (WinRust Disk)", "Disk usage (WinRust Disk)", ja: "ディスク使用量 (WinRust Disk)", pt: "Uso do disco (WinRust Disk)", zh: "磁盘占用（WinRust Disk）"), "\u{EDA2}"));
        }
        items.push(action(5, t!("Свойства", "Properties", ja: "プロパティ", pt: "Propriedades", zh: "属性"), "\u{E713}"));
        match ctxmenu::show(h, items, (pt.x, pt.y)) {
            ctxmenu::Pick::Id(8) => open_terminal(&cur_dir()),
            ctxmenu::Pick::Id(5) => properties(&cur_dir()),
            ctxmenu::Pick::Id(6) => { // wiztree takes the drive letter and asks for admin rights itself
                let dir = cur_dir();
                let drive: String = dir.to_string_lossy().chars().take(2).collect();
                if let Some(exe) = disk { let _ = std::process::Command::new(exe).arg(drive).spawn(); }
            }
            ctxmenu::Pick::Id(id) => {
                let id = match id { 75 => 60 + col * 2, 76 => 61 + col * 2, x => x }; // "ascending / descending": the sorted column too
                PostMessageW(h, ui::WM_UI, id as usize, 0);
            }
            ctxmenu::Pick::None => {}
        }
        return;
    }
    let m = CreatePopupMenu();
    let items = [
        (1usize, t!("Новая папка", "New folder", ja: "新しいフォルダー", pt: "Nova pasta", zh: "新建文件夹")),
        (2, t!("Новый текстовый файл", "New text file", ja: "新しいテキスト ドキュメント", pt: "Novo arquivo de texto", zh: "新建文本文件")),
        (3, t!("Вставить", "Paste", ja: "貼り付け", pt: "Colar", zh: "粘贴")),
        (4, t!("Обновить", "Refresh", ja: "最新の情報に更新", pt: "Atualizar", zh: "刷新")),
        (5, t!("Свойства", "Properties", ja: "プロパティ", pt: "Propriedades", zh: "属性")),
    ];
    for (id, lbl) in items { AppendMenuW(m, MF_STRING, id, w(lbl).as_ptr()); }
    if disk.is_some() { AppendMenuW(m, MF_STRING, 6, w(t!("Занятое место (WinRust Disk)", "Disk usage (WinRust Disk)", ja: "ディスク使用量 (WinRust Disk)", pt: "Uso do disco (WinRust Disk)", zh: "磁盘占用（WinRust Disk）")).as_ptr()); }
    if desk() {
        let sort = [
            (8usize, t!("Упорядочить значки по имени", "Sort icons by name", ja: "名前順にアイコンを並べ替え", pt: "Classificar ícones por nome", zh: "按名称排列图标")),
            (9, t!("Упорядочить значки по дате", "Sort icons by date", ja: "日付順にアイコンを並べ替え", pt: "Classificar ícones por data", zh: "按日期排列图标")),
            (10, t!("Упорядочить значки по размеру", "Sort icons by size", ja: "サイズ順にアイコンを並べ替え", pt: "Classificar ícones por tamanho", zh: "按大小排列图标")),
            (11, t!("Выровнять значки", "Arrange icons", ja: "アイコンの整列", pt: "Alinhar ícones", zh: "排列图标")),
        ];
        for (id, lbl) in sort { AppendMenuW(m, MF_STRING, id, w(lbl).as_ptr()); }
    }
    if !desk() {
        let lbl = if GROUP.load(Relaxed) == 1 { t!("Без группировки", "No grouping", ja: "グループ化しない", pt: "Sem agrupamento", zh: "不分组") } else { t!("Группировать по дате", "Group by date", ja: "日付でグループ化", pt: "Agrupar por data", zh: "按日期分组") };
        AppendMenuW(m, MF_STRING, 13, w(lbl).as_ptr());
    }
    if desk() {
        let lbl = if small_marker().exists() { t!("Крупные значки", "Large icons", ja: "大きいアイコン", pt: "Ícones grandes", zh: "大图标") } else { t!("Мелкие значки", "Small icons", ja: "小さいアイコン", pt: "Ícones pequenos", zh: "小图标") };
        AppendMenuW(m, MF_STRING, 12, w(lbl).as_ptr());
    }
    if desk() {
        let lbl = if icons_hidden() { t!("Показать значки рабочего стола", "Show desktop icons", ja: "デスクトップ アイコンを表示", pt: "Mostrar ícones da área de trabalho", zh: "显示桌面图标") } else { t!("Скрыть значки рабочего стола", "Hide desktop icons", ja: "デスクトップ アイコンを非表示", pt: "Ocultar ícones da área de trabalho", zh: "隐藏桌面图标") };
        AppendMenuW(m, MF_STRING, 7, w(lbl).as_ptr());
    }
    let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, h, ptr::null());
    DestroyMenu(m);
    let dir = cur_dir();
    match id {
        7 => set_icons_hidden(!icons_hidden()),
        13 => group_now(h, if GROUP.load(Relaxed) == 1 { 0 } else { 1 }),
        12 => { // the grid changes with the size: icons are laid out again
            let k = small_marker();
            if k.exists() { let _ = fs::remove_file(&k); } else { let _ = fs::write(&k, "1"); }
            apply_icon_size(ST.with_borrow(|st| st.list));
            let _ = fs::remove_file(desk::pos_file());
        }
        8..=11 => { // the saved cells are dropped: reload lays every icon out again, down the columns in sort order
            if id != 11 { SORT_COL.store([0, 1, 3][id as usize - 8], Relaxed); SORT_ASC.store(true, Relaxed); }
            let _ = fs::remove_file(desk::pos_file());
        }
        6 => { // wiztree takes the drive letter and asks for admin rights itself
            let drive: String = dir.to_string_lossy().chars().take(2).collect();
            if let Some(exe) = disk { let _ = std::process::Command::new(exe).arg(drive).spawn(); }
        }
        1 => { create_new(h, &dir, t!("Новая папка", "New folder", ja: "新しいフォルダー", pt: "Nova pasta", zh: "新建文件夹"), true); return; }
        2 => { create_new(h, &dir, t!("Новый текстовый документ.txt", "New text document.txt", ja: "新しいテキスト ドキュメント.txt", pt: "Novo documento de texto.txt", zh: "新建文本文档.txt"), false); return; }
        3 => paste(h, &dir),
        5 => properties(&dir),
        _ => {}
    }
    if id != 0 { refresh_after_ops(h); }
}

/// Command line -> (folder to open, file to select there). Understands `/select,"C:\a\b.txt"` (what browsers and other
/// apps pass for "show in folder"), a plain folder, or a plain file path (its folder, file selected).
fn start_target() -> (PathBuf, Option<String>) {
    let mut a = env::args().skip(1).filter(|a| a != "--focus");
    let mut p = a.next().unwrap_or_default();
    if p.to_lowercase().starts_with("/select,") || p.to_lowercase().starts_with("--select") {
        p = p.get(8..).unwrap_or("").trim_start_matches(',').to_string();
        if p.is_empty() { p = a.next().unwrap_or_default(); }
    }
    let p = PathBuf::from(p.trim_matches('"'));
    // a path given with forward slashes (a command line or a URL-like string) is normalized: shell APIs want backslashes
    let p = if p.to_string_lossy().contains('/') { PathBuf::from(p.to_string_lossy().replace('/', "\\")) } else { p };
    if p.as_os_str().is_empty() { return (PathBuf::from("::home"), None); } // a fresh start: the Home page, as Win11 Explorer
    match (p.is_file(), p.parent()) {
        (true, Some(d)) => (d.to_path_buf(), p.file_name().map(|n| n.to_string_lossy().into_owned())),
        _ => (p, None),
    }
}

/// Another app asked the shell to show `p` in a folder window (SHOpenFolderAndSelectItems): open its folder here, select it, come to front.
fn shell_select(h: isize, p: PathBuf) {
    let h = h as HWND;
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return };
    if !cur_dir().as_os_str().eq_ignore_ascii_case(dir.as_os_str()) { load(h, dir.to_path_buf()); }
    select_row(&name.to_string_lossy());
    unsafe {
        if IsIconic(h) != 0 { ShowWindow(h, SW_RESTORE); }
        SetForegroundWindow(h);
    }
}

/// Selects and scrolls to the row called `name` in the current folder.
fn select_row(name: &str) {
    let (list, i) = ST.with_borrow(|st| (st.list, st.rows.iter().position(|r| r.name.eq_ignore_ascii_case(name))));
    let Some(i) = i else { return };
    unsafe {
        let mut it: LVITEMW = std::mem::zeroed();
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize); // the shell selects it alone
        it.state = LVIS_SELECTED | LVIS_FOCUSED;
        put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
        put(list, LVM_ENSUREVISIBLE, i, 0);
    }
}

/// Back / forward along the visited-folder trail.
fn nav(h: HWND, back: bool) {
    let to = ST.with_borrow_mut(|st| if back { st.hist.back() } else { st.hist.forward() });
    if let Some(p) = to {
        NAV.store(true, Relaxed);
        load(h, p);
        NAV.store(false, Relaxed);
    }
}

fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

fn put(h: HWND, msg: u32, wp: usize, lp: isize) -> isize { unsafe { SendMessageW(h, msg, wp, lp) } }

fn wstr(p: *const u16) -> String {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 { n += 1; }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

pub fn cur_dir() -> PathBuf { ST.with_borrow(|st| st.dir.clone()) }

/// System image-list index. Extension-based (no disk access) except for files that carry their own icon.
fn icon_of(p: &PathBuf, dir: bool) -> i32 {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let s = p.to_string_lossy();
    let drive = s.len() == 3 && s.ends_with(":\\"); // a drive root gets its own icon from the shell
    let own = drive || (!dir && matches!(ext.as_str(), "exe" | "lnk" | "ico" | "url" | "scr"));
    let mut fi: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let attr = if dir { 0x10 } else { 0x80 }; // DIRECTORY / NORMAL
    let flags = SHGFI_SYSICONINDEX | SHGFI_SMALLICON | if own { 0 } else { SHGFI_USEFILEATTRIBUTES };
    unsafe { SHGetFileInfoW(w(&p.to_string_lossy()).as_ptr(), attr, &mut fi, size_of::<SHFILEINFOW>() as u32, flags) };
    fi.iIcon
}

/// The tabs are painted by the chrome window from State.tabs: "setting a tab's text" is just a repaint.
fn tab_text(tab: HWND, _i: usize, _text: &str, _insert: bool) {
    if !tab.is_null() { unsafe { InvalidateRect(tab, ptr::null(), 0) }; }
}

/// The text of a desktop row. Rows from the public desktop carry absolute paths: only the file name is shown, and shortcuts
/// lose their ".lnk" / ".url" like in Explorer. Other windows show the row as it is, with extensions hidden if the view says so.
pub(crate) fn shown_name(name: &str, desktop: bool, dir: bool) -> String {
    if desktop {
        let file = std::path::Path::new(name).file_name().map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned());
        let lower = file.to_lowercase();
        return if lower.ends_with(".lnk") || lower.ends_with(".url") { file[..file.len() - 4].to_string() } else { file };
    }
    if dir || SHOW_EXT.load(Relaxed) { return name.to_string(); }
    let file = name.rsplit('\\').next().unwrap_or(name);
    match file.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() && !e.is_empty() => stem.to_string(),
        _ => file.to_string(),
    }
}

fn hide_extensions() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (mut v, mut cb) = (0u32, 4u32);
    let sub = w(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced");
    let rc = unsafe { RegGetValueW(HKEY_CURRENT_USER, sub.as_ptr(), w("HideFileExt").as_ptr(), RRF_RT_REG_DWORD, ptr::null_mut(), &mut v as *mut _ as *mut _, &mut cb) };
    rc == 0 && v != 0
}

fn label(dir: &PathBuf) -> String {
    if let Some(p) = page_name(dir) { return p.to_string(); }
    dir.file_name().map_or_else(|| dir.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

/// The virtual pages the navigation pane can open ("::home", "::thispc").
pub(crate) fn page_name(dir: &Path) -> Option<&'static str> {
    match dir.to_string_lossy().as_ref() {
        "::home" => Some(t!("Главная", "Home", ja: "ホーム", pt: "Início", zh: "主页")),
        "::thispc" => Some(t!("Этот компьютер", "This PC", ja: "PC", pt: "Este Computador", zh: "此电脑")),
        "::recycle" => Some(t!("Корзина", "Recycle Bin", ja: "ごみ箱", pt: "Lixeira", zh: "回收站")),
        _ => None,
    }
}

fn is_virtual(dir: &Path) -> bool { dir.to_string_lossy().starts_with("::") }
fn cur_virtual() -> bool { is_virtual(&cur_dir()) }

/// "winrust - File Explorer", like the window title in Windows 11.
fn window_title(dir: &PathBuf) -> String {
    let name = if let Some(p) = page_name(dir) { p.to_string() } else if dir.file_name().is_some() { label(dir) } else { dir.to_string_lossy().trim_end_matches(['\\', '/']).to_string() };
    let app = t!("Проводник", "File Explorer", ja: "エクスプローラー", pt: "Explorador de Arquivos", zh: "文件资源管理器");
    format!("{name} - {app}")
}

/// One deleted item from a `$I` record: original path, deletion time and the `$R` data next to it.
fn parse_recycle(meta: &Path, b: &[u8]) -> Option<Row> {
    if b.len() < 28 { return None; }
    if u64::from_le_bytes(b[0..8].try_into().ok()?) != 2 { return None; }
    let size = u64::from_le_bytes(b[8..16].try_into().ok()?);
    let time = u64::from_le_bytes(b[16..24].try_into().ok()?);
    let chars = u32::from_le_bytes(b[24..28].try_into().ok()?) as usize;
    if chars == 0 || b.len() < 28 + chars * 2 { return None; }
    let path: Vec<u16> = (0..chars).map(|i| u16::from_le_bytes([b[28 + i * 2], b[29 + i * 2]])).collect();
    let original = PathBuf::from(String::from_utf16_lossy(&path).trim_end_matches('\0'));
    if original.as_os_str().is_empty() { return None; }
    let meta_name = meta.file_name()?.to_string_lossy();
    if meta_name.len() < 3 { return None; }
    let target = meta.with_file_name(format!("$R{}", &meta_name[2..]));
    let m = fs::metadata(&target).ok()?;
    let name = original.file_name().map_or_else(|| target.file_name().map_or_else(|| target.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned()), |n| n.to_string_lossy().into_owned());
    let kind = original.parent().map_or_else(|| String::new(), |p| p.to_string_lossy().into_owned());
    Some(Row { name, origin: Some(original), target: Some(target), kind: Some(kind), dir: m.is_dir(), size, time, icon: -1, thumb: -2, hdr: false })
}

/// The items of this user's Recycle Bin on every fixed / removable drive, newest first.
fn recycle_rows() -> Vec<Row> {
    let mut out = vec![];
    for d in ui::drive_list() {
        if d.network { continue; }
        let Ok(sids) = fs::read_dir(d.root.join("$Recycle.Bin")) else { continue };
        for sid in sids.flatten() {
            if !sid.path().is_dir() { continue; }
            let Ok(items) = fs::read_dir(sid.path()) else { continue };
            for e in items.flatten() {
                let p = e.path();
                if !e.file_name().to_string_lossy().to_lowercase().starts_with("$i") { continue; }
                if let Ok(bytes) = fs::read(&p) { if let Some(r) = parse_recycle(&p, &bytes) { out.push(r); } }
            }
        }
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    out
}

/// Rows of the virtual pages: the drives of This PC, the quick access folders of Home and the Recycle Bin.
fn virtual_rows(dir: &Path) -> Option<Vec<Row>> {
    let head = |label: &str| Row { name: format!("?{label}"), origin: None, target: None, kind: None, dir: false, size: 0, time: 0, icon: -1, thumb: -1, hdr: true };
    match dir.to_string_lossy().as_ref() {
        "::thispc" => Some(drive_rows()),
        "::home" => {
            let mut folders: Vec<Row> = ui::quick_folders().into_iter().map(|(title, p, _)| {
                let m = fs::metadata(&p).ok();
                Row { name: title, origin: None, target: Some(p), kind: None, dir: true, size: 0, time: m.map_or(0, |m| m.last_write_time()), icon: -1, thumb: -2, hdr: false }
            }).collect();
            folders.sort_by_key(|r| r.name.to_lowercase());
            // folders first, then the drives: the page is drawn as tiles under section headings
            let mut rows = folders;
            rows.extend(drive_rows());
            Some(rows)
        }
        "::recycle" => {
            let items = recycle_rows();
            if items.is_empty() { return Some(vec![]); }
            let mut rows = vec![head(t!("Корзина", "Recycle Bin", ja: "ごみ箱", pt: "Lixeira", zh: "回收站"))];
            rows.extend(items);
            Some(rows)
        }
        _ => None,
    }
}

/// Drives as rows: name, the kind column and the total size (the free space is read per row when the row is drawn).
fn drive_rows() -> Vec<Row> {
    let mut drives: Vec<Row> = ui::drive_list().into_iter().map(|d| {
        let kind = if d.network { t!("Сетевой диск", "Network Drive", ja: "ネットワーク ドライブ", pt: "Unidade de rede", zh: "网络驱动器") } else { t!("Локальный диск", "Local Disk", ja: "ローカル ディスク", pt: "Disco local", zh: "本地磁盘") };
        Row { name: d.title, origin: None, target: Some(d.root), kind: Some(kind.into()), dir: true, size: d.cap.map_or(0, |(_, total)| total), time: 0, icon: -1, thumb: -2, hdr: false }
    }).collect();
    drives.sort_by_key(|r| r.target.as_ref().map(|p| p.to_string_lossy().to_lowercase()).unwrap_or_default());
    drives
}

/// The `$I` record next to a `$R` item.
fn recycle_meta(target: &Path) -> Option<PathBuf> {
    let n = target.file_name()?.to_string_lossy();
    n.starts_with("$R").then(|| target.with_file_name(format!("$I{}", &n[2..])))
}

/// Selected Recycle Bin rows as ($R data, original path, is folder).
fn recycle_selected() -> Vec<(PathBuf, PathBuf, bool)> {
    ST.with_borrow(|st| {
        let (mut out, mut i) = (vec![], -1isize);
        loop {
            i = put(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            if let (Some(t), Some(o)) = (&r.target, &r.origin) { out.push((t.clone(), o.clone(), r.dir)); }
        }
        out
    })
}

/// Moves the deleted data back to its original place; a name clash gets "(2)" like Explorer.
fn recycle_restore_one(h: HWND, target: &Path, origin: &Path, dir: bool) -> bool {
    let Some(parent) = origin.parent() else { return false };
    if !parent.is_dir() {
        warn(h, &tf!("Исходная папка не существует: {}", "The original folder no longer exists: {}", ja: "元のフォルダーがありません: {}", pt: "A pasta original não existe: {}", zh: "原文件夹已不存在：{}", parent.display()));
        return false;
    }
    let name = origin.file_name().map_or_else(|| target.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
    let dest = if origin.exists() { jobs::free_name(parent, &name, dir) } else { origin.to_path_buf() };
    match fs::rename(target, &dest) {
        Ok(()) => {
            if let Some(meta) = recycle_meta(target) { let _ = fs::remove_file(meta); }
            true
        }
        Err(e) => { warn(h, &format!("{}: {e}", dest.display())); false }
    }
}

/// Undo of a delete: puts `original` back from the Recycle Bin (the newest record for that path).
fn recycle_restore_original(h: HWND, original: &Path) -> bool {
    let want = original.to_string_lossy();
    let mut best: Option<Row> = None;
    for r in recycle_rows() {
        let same = r.origin.as_ref().is_some_and(|o| o.to_string_lossy().eq_ignore_ascii_case(&want));
        if same && best.as_ref().is_none_or(|b| r.time > b.time) { best = Some(r); }
    }
    match best {
        Some(r) => match (r.target, r.origin) {
            (Some(t), Some(o)) => recycle_restore_one(h, &t, &o, r.dir),
            _ => false,
        },
        None => false,
    }
}

fn recycle_restore_selected(h: HWND) {
    for (target, origin, dir) in recycle_selected() { recycle_restore_one(h, &target, &origin, dir); }
    refresh_after_ops(h);
}

fn recycle_restore_all(h: HWND) {
    let rows: Vec<(PathBuf, PathBuf, bool)> = ST.with_borrow(|st| st.rows.iter().filter(|r| !r.hdr).filter_map(|r| Some((r.target.clone()?, r.origin.clone()?, r.dir))).collect());
    for (target, origin, dir) in rows { recycle_restore_one(h, &target, &origin, dir); }
    refresh_after_ops(h);
}

fn recycle_delete_selected(h: HWND) {
    let sel = recycle_selected();
    if sel.is_empty() { return; }
    let text = tf!("Удалить безвозвратно: {}?", "Delete permanently: {}?", ja: "完全に削除しますか: {}?", pt: "Excluir permanentemente: {}?", zh: "永久删除 {} 项？", sel.len());
    if unsafe { MessageBoxW(h, w(&text).as_ptr(), w("WinRust Explorer").as_ptr(), MB_YESNO | MB_ICONWARNING) } != IDYES { return; }
    for (target, _, dir) in sel {
        let r = if dir { fs::remove_dir_all(&target) } else { fs::remove_file(&target) };
        if let Err(e) = r { warn(h, &format!("{}: {e}", target.display())); }
        if let Some(meta) = recycle_meta(&target) { let _ = fs::remove_file(meta); }
    }
    refresh_after_ops(h);
}

fn recycle_empty(h: HWND) {
    unsafe { SHEmptyRecycleBinW(h, ptr::null(), 0) };
    refresh_after_ops(h);
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn recycle_menu(h: HWND) {
    let sel = recycle_selected();
    let m = CreatePopupMenu();
    AppendMenuW(m, MF_STRING | if sel.is_empty() { MF_GRAYED } else { 0 }, 1, w(t!("Восстановить", "Restore", ja: "元に戻す", pt: "Restaurar", zh: "还原")).as_ptr());
    AppendMenuW(m, MF_STRING | if sel.is_empty() { MF_GRAYED } else { 0 }, 2, w(t!("Удалить безвозвратно", "Delete permanently", ja: "完全に削除", pt: "Excluir permanentemente", zh: "永久删除")).as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, ptr::null());
    AppendMenuW(m, MF_STRING, 3, w(t!("Восстановить все", "Restore all", ja: "すべて元に戻す", pt: "Restaurar tudo", zh: "全部还原")).as_ptr());
    AppendMenuW(m, MF_STRING, 4, w(t!("Очистить корзину", "Empty Recycle Bin", ja: "ごみ箱を空にする", pt: "Esvaziar Lixeira", zh: "清空回收站")).as_ptr());
    AppendMenuW(m, MF_SEPARATOR, 0, ptr::null());
    AppendMenuW(m, MF_STRING, 5, w(t!("Обновить", "Refresh", ja: "最新の情報に更新", pt: "Atualizar", zh: "刷新")).as_ptr());
    let mut pt: POINT = std::mem::zeroed();
    GetCursorPos(&mut pt);
    let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, h, ptr::null());
    DestroyMenu(m);
    match id { 1 => recycle_restore_selected(h), 2 => recycle_delete_selected(h), 3 => recycle_restore_all(h), 4 => recycle_empty(h), 5 => reload(h), _ => {} }
}

fn rows_of(dir: &PathBuf) -> Option<Vec<Row>> {
    if !desk() {
        if let Some(rows) = virtual_rows(dir) { return Some(rows); }
        if let Some((arc, inner)) = zipx::split_zip(dir) { // inside a .zip: its entries are the rows
            let mut rows: Vec<Row> = zipx::list_dir(&arc, &inner).ok()?.into_iter().map(|i| Row { name: i.name, origin: None, target: None, kind: None, dir: i.dir, size: i.size, time: zipx::dos_to_filetime(i.time), icon: -1, thumb: -2, hdr: false }).collect();
            sort_rows(&mut rows, SORT_COL.load(Relaxed), SORT_ASC.load(Relaxed));
            add_headers(&mut rows);
            return Some(rows);
        }
    }
    let mut dirs = vec![dir.clone()];
    if desk() { dirs.extend(sh::desktop_dirs().into_iter().skip(1)); }
    let mut rows = vec![];
    for (n, d) in dirs.iter().enumerate() {
        let Ok(rd) = fs::read_dir(d) else { if n == 0 { return None } else { continue } };
        for e in rd.flatten() {
            let m = e.metadata().ok();
            if (desk() || !SHOW_HIDDEN.load(Relaxed)) && m.as_ref().is_some_and(|m| m.file_attributes() & 2 != 0) { continue; } // FILE_ATTRIBUTE_HIDDEN
            let name = if n == 0 { e.file_name().to_string_lossy().into_owned() } else { e.path().to_string_lossy().into_owned() };
            rows.push(row(name, m.as_ref()));
        }
    }
    sort_rows(&mut rows, SORT_COL.load(Relaxed), SORT_ASC.load(Relaxed));
    add_headers(&mut rows);
    Some(rows)
}

/// Loads `dir` into the virtual list and the active tab. Unreadable dir (access denied, gone) leaves the view unchanged.
/// Never call with ST borrowed: the list sends GETDISPINFO back into the window proc.
/// The pages that are drawn as big tiles (folders and drives with their usage), like Win11's Home / This PC.
fn is_tiles_page(dir: &Path) -> bool {
    matches!(dir.to_string_lossy().as_ref(), "::home" | "::thispc")
}

fn load(h: HWND, dir: PathBuf) {
    if !ST.with_borrow(|st| st.dir == dir) {
        COLLAPSED.with_borrow_mut(|s| s.clear()); // another folder: its groups start open
        CHECKED.with_borrow_mut(|s| s.clear());
    }
    if !desk() {
        let list = ST.with_borrow(|st| st.list);
        // the list still holds the previous folder's items: restyling it with those inside lays every one of them out again
        put(list, LVM_DELETEALLITEMS, 0, 0);
        if let Some((view, sort, asc, group, w)) = read_view_pref(&dir) {
            SORT_COL.store(sort, Relaxed);
            SORT_ASC.store(asc, Relaxed);
            GROUP.store(group, Relaxed);
            ST.with_borrow_mut(|st| { st.view = view; st.thumbs = view != ViewMode::Details; });
            THUMBS.store(view != ViewMode::Details, Relaxed);
            apply_view(list, view);
            if w.iter().all(|&x| x > 0) { for (k, wd) in w.iter().enumerate() { put(list, LVM_SETCOLUMNWIDTH, k, *wd as isize); } }
        }
        let tiles = is_tiles_page(&dir);
        if tiles {
            ST.with_borrow_mut(|st| { st.view = ViewMode::Tiles; st.thumbs = true; st.tiles = true; });
            THUMBS.store(true, Relaxed);
            apply_view(list, ViewMode::Tiles);
        } else if ST.with_borrow(|st| st.tiles) {
            // leaving a tile page: back to the normal listing
            ST.with_borrow_mut(|st| { st.tiles = false; st.view = ViewMode::Details; st.thumbs = false; });
            THUMBS.store(false, Relaxed);
            apply_view(list, ViewMode::Details);
        }
    }
    let Some(rows) = rows_of(&dir) else { return };
    if !desk() { unsafe { SetWindowTextW(h, w(&window_title(&dir)).as_ptr()) }; }
    let (list, tab, edit, search, active, n) = ST.with_borrow_mut(|st| {
        if let Some(t) = st.tabs.get_mut(st.active) { *t = dir.clone(); }
        st.dir = dir.clone();
        st.all = rows;
        st.rows = visible_rows(&st.all);
        if !NAV.load(Relaxed) { st.hist.visit(dir.clone()); }
        (st.list, st.tab, st.edit, st.search, st.active, st.rows.len())
    });
    if ST.with_borrow(|st| st.tiles) {
        // heading rows of the tile page: folders first, then the drives
        let heads = ST.with_borrow(|st| {
            let n = st.rows.len();
            let nf = st.rows.iter().filter(|r| r.kind.is_none()).count();
            let mut v = vec![];
            if nf > 0 { v.push((tf!("Папки ({})", "Folders ({})", ja: "フォルダー ({})", pt: "Pastas ({})", zh: "文件夹 ({})", nf), 0)); }
            if n > nf { v.push((tf!("Устройства и диски ({})", "Devices and drives ({})", ja: "デバイスとドライブ ({})", pt: "Dispositivos e unidades ({})", zh: "设备和驱动器 ({})", n - nf), nf)); }
            v
        });
        let folds = saved_folds(&dir);
        ST.with_borrow_mut(|st| { st.tile_starts = heads; st.tile_collapsed = (0..st.tile_starts.len()).map(|i| folds.get(i).copied().unwrap_or(false)).collect(); });
    }
    let shown_dir = page_name(&dir).map_or_else(|| dir.to_string_lossy().into_owned(), str::to_string);
    unsafe { SetWindowTextW(edit, w(&shown_dir).as_ptr()) };
    if search != 0 as HWND { unsafe { SetWindowTextW(search, w("").as_ptr()) }; }
    if !is_virtual(&dir) { shellwin::navigated(&dir); }
    let titles: [&str; 4] = match dir.to_string_lossy().as_ref() {
        "::recycle" => [
            t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"),
            t!("Дата удаления", "Date deleted", ja: "削除日時", pt: "Data de exclusão", zh: "删除日期"),
            t!("Исходное расположение", "Original location", ja: "元の場所", pt: "Local original", zh: "原位置"),
            t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"),
        ],
        "::thispc" => [
            t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"), "",
            t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"),
            t!("Общий размер", "Total size", ja: "合計サイズ", pt: "Tamanho total", zh: "总大小"),
        ],
        _ => [
            t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"),
            t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"),
            t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"),
            t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"),
        ],
    };
    set_column_titles(list, titles);
    ui::set_search(None); // a folder again: the pill shows its path
    tab_text(tab, active, &label(&dir), false);
    ui::nav_dirty(); // the pane highlights where we are
    if !desk() { ui::nav_sync(&dir); }
    reset_thumbs();
    if desk() { desk::fill(list, n); } else { fill_items(); }
    if !desk() { unsafe { ui::fit_filler(list) }; }
    unsafe { ui::status_dirty(h) }; // the counts belong to the folder that was just opened
    start_watch(h, dir);
}

/// Asks the rust-search service of drive `d` (see plugins/rust-search): terms in, one path per line out.
/// None = no service for that drive or no access (its pipe is admin-only unless started with --allow).
fn ask(d: char, q: &str) -> Option<String> {
    use std::{fs::OpenOptions, io::{Read, Write}};
    let mut p = OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\rs-search-{d}")).ok()?;
    p.write_all(q.as_bytes()).ok()?;
    let (mut out, mut chunk) = (Vec::new(), [0u8; 1 << 16]);
    // the server ends its reply by hanging up, which surfaces as a read error: treat any error as EOF
    while let Ok(n @ 1..) = p.read(&mut chunk) { out.extend_from_slice(&chunk[..n]); }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Fills the list with search hits from every drive's service. Rows hold absolute paths, so joining them
/// onto the current dir still yields the right file. Blocks the UI for the duration of the query (fast).
/// Never call with ST borrowed.
fn search(h: HWND, q: &str) {
    // Everything first (it is the index the user asked for); without it the rust-search services, then a local walk
    everything::ensure_started();
    let mut rows = vec![];
    if let Some(hits) = everything::search(q, 2000) {
        // target = the absolute path: the Name column shows where each hit is (the hits come from every drive)
        rows = hits.into_iter().map(|x| Row {
            name: x.path.to_string_lossy().into_owned(),
            origin: None, target: Some(x.path), kind: None,
            dir: x.dir, size: x.size, time: x.modified,
            icon: -1, thumb: -2, hdr: false,
        }).collect();
    } else {
        let mut indexed = false; // some drive had a rust-search service
        for d in 'A'..='Z' {
            let Some(reply) = ask(d, q) else { continue };
            indexed = true;
            for line in reply.lines() {
                if line.starts_with("... ") { continue; }
                let m = fs::metadata(line).ok();
                rows.push(row(line.to_string(), m.as_ref()));
            }
        }
        if !indexed {
            // no rust-search installed: look through the current folder and below instead
            let root = if cur_virtual() { PathBuf::from(env::var_os("USERPROFILE").unwrap_or_else(|| "C:\\".into())) } else { cur_dir() };
            rows = local_search(&root, q).0;
        }
    }
    // results are a plain list even when the search started on Home / This PC (their tile bands would draw over it)
    let (list, was_tiles) = ST.with_borrow(|st| (st.list, st.tiles));
    if was_tiles {
        ST.with_borrow_mut(|st| { st.tiles = false; st.view = ViewMode::Details; st.thumbs = false; st.tile_starts.clear(); st.tile_bands.clear(); st.tile_collapsed.clear(); });
        THUMBS.store(false, Relaxed);
        apply_view(list, ViewMode::Details);
        set_column_titles(list, [
            t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"),
            t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"),
            t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"),
            t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"),
        ]);
    }
    ui::set_search(Some(q.to_string()));
    if let Some(se) = ST.with_borrow(|st| (st.search != 0 as HWND).then_some(st.search)) { unsafe { SetWindowTextW(se, w(q).as_ptr()) }; }
    ST.with_borrow_mut(|st| { st.all = rows; st.rows = visible_rows(&st.all) });
    reset_thumbs();
    fill_items();
    unsafe { ui::status_dirty(h) };
}

/// The same keys typed in the other layout (QWERTY <-> ЙЦУКЕН): "сфкпщ" -> "cargo", "ljrevtyns" -> "документы".
fn flip_layout(q: &str) -> String {
    let en: Vec<char> = "qwertyuiop[]asdfghjkl;'zxcvbnm,.`QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>~".chars().collect();
    let ru: Vec<char> = "йцукенгшщзхъфывапролджэячсмитьбюёЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮЁ".chars().collect();
    q.chars().map(|c| en.iter().position(|&x| x == c).map(|i| ru[i]).or_else(|| ru.iter().position(|&x| x == c).map(|i| en[i])).unwrap_or(c)).collect()
}

/// Files and folders under `dir` whose name contains `q` (any case), as rows with absolute names. Walks breadth-first and
/// stops after LOCAL_MAX hits or LOCAL_MS so a search from C:\ cannot hang the window. The same walk also matches `q` typed
/// in the other keyboard layout; those hits are used only when `q` itself found nothing.
/// Returns (rows, stopped early, rows are for the other layout).
/// shortcut: blocks the UI while it walks (up to LOCAL_MS); move to a thread if deeper searches are wanted.
fn local_search(dir: &Path, q: &str) -> (Vec<Row>, bool, bool) {
    const LOCAL_MAX: usize = 500;
    const LOCAL_MS: u128 = 1500;
    let (q, t0) = (q.to_lowercase(), std::time::Instant::now());
    let alt = flip_layout(&q);
    let (mut out, mut other, mut queue) = (vec![], vec![], std::collections::VecDeque::from([dir.to_path_buf()]));
    let mut cut = false;
    'walk: while let Some(d) = queue.pop_front() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            if out.len() >= LOCAL_MAX || t0.elapsed().as_millis() > LOCAL_MS { cut = true; break 'walk; }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() { queue.push_back(e.path()); } // no junction loops
            let name = e.file_name().to_string_lossy().to_lowercase();
            let hit = |v: &mut Vec<Row>| v.push(row(e.path().to_string_lossy().into_owned(), e.metadata().ok().as_ref()));
            if name.contains(&q) { hit(&mut out); } else if alt != q && other.len() < LOCAL_MAX && name.contains(&alt) { hit(&mut other); }
        }
    }
    if out.is_empty() && !other.is_empty() { (other, cut, true) } else { (out, cut, false) }
}

/// Drops all thumbnails and invalidates in-flight ones; call whenever the rows are replaced.
/// Fills the list with real items (name label and icon). The list is not virtual, so icon views lay the items out and
/// thumbnails are attached to their items as they arrive.
fn fill_items() {
    let (list, view, dir, rows, tiles) = ST.with_borrow(|st| (st.list, st.view, st.dir.clone(), st.rows.clone(), st.tiles));
    let desk = desk();
    let mut data: Vec<(Vec<u16>, i32)> = Vec::with_capacity(rows.len());
    for r in &rows {
        let label = if r.target.is_some() { r.name.clone() } else { shown_name(&r.name, desk, r.dir) };
        let img = if view.wants_thumbs() {
            r.thumb // -1 when not loaded yet: the thumbnail replaces it later
        } else if r.icon < 0 {
            icon_of(&r.target.clone().unwrap_or_else(|| dir.join(&r.name)), r.dir)
        } else {
            r.icon
        };
        data.push((label.encode_utf16().chain([0]).collect(), img));
    }
    unsafe {
        SendMessageW(list, 0x000B, 0, 0); // WM_SETREDRAW off while the items go in
        put(list, LVM_DELETEALLITEMS, 0, 0);
        for (i, (text, img)) in data.iter().enumerate() {
            let mut it: LVITEMW = std::mem::zeroed();
            it.mask = LVIF_TEXT | LVIF_IMAGE;
            it.iItem = i as i32;
            it.pszText = text.as_ptr() as *mut u16;
            it.iImage = if *img < 0 { I_IMAGENONE } else { *img };
            put(list, LVM_INSERTITEMW, 0, &it as *const _ as isize);
        }
        if tiles { tile_layout(list); } else if !matches!(view, ViewMode::Details | ViewMode::List) { put(list, 0x1016, 0, 0); } // LVM_ARRANGE
        SendMessageW(list, 0x000B, 1, 0);
        InvalidateRect(list, ptr::null(), 0);
    }
    if view.wants_thumbs() {
        // the thumbnails for this view (a virtual list used to ask for them as it drew)
        let px = view.icon_px();
        for (i, r) in rows.iter().enumerate() {
            if r.thumb == -2 {
                if let Some(tx) = TX.get() {
                    let path = r.target.clone().unwrap_or_else(|| dir.join(&r.name));
                    let _ = tx.send((GEN.load(Relaxed), i, path, px));
                }
            }
        }
    }
}

/// Cells of a tile page: a heading per section, then the tiles, left to right.
pub(crate) fn tile_layout(list: HWND) {
    let (cw, _) = unsafe { let mut r: RECT = std::mem::zeroed(); GetClientRect(list, &mut r); (r.right, r.bottom) };
    let (cell_w, cell_h, head_h, pad) = (ui::sc(330), ui::sc(84), ui::sc(40), ui::sc(14));
    let cols = ((cw - 2 * pad) / cell_w).max(1);
    let starts = ST.with_borrow(|st| (st.tile_starts.clone(), st.tile_collapsed.clone()));
    let (starts, folded) = starts;
    let n = put(list, LVM_GETITEMCOUNT, 0, 0) as usize;
    let (mut y, mut bands) = (pad, Vec::new());
    for (si, (title, at)) in starts.iter().enumerate() {
        let end = starts.get(si + 1).map_or(n, |s| s.1).min(n);
        let closed = folded.get(si).copied().unwrap_or(false);
        bands.push((title.clone(), y, closed));
        y += head_h;
        if closed {
            // stacked under their heading and skipped when drawn: no far-away cells (those would add a scrollbar)
            for k in *at..end { put(list, LVM_SETITEMPOSITION, k, ((pad as isize) & 0xFFFF) | (((y - head_h) as isize) << 16)); }
            continue;
        }
        let mut col = 0;
        for k in *at..end {
            let x = pad + col * cell_w;
            put(list, LVM_SETITEMPOSITION, k, ((x as isize) & 0xFFFF) | ((y as isize) << 16));
            col += 1;
            if col == cols {
                col = 0;
                y += cell_h;
            }
        }
        if col != 0 { y += cell_h; }
    }
    ST.with_borrow_mut(|st| st.tile_bands = bands);
}

/// The tile page is up (Home / This PC).
pub(crate) fn tiles_active() -> bool { ST.with_borrow(|st| st.tiles && st.view == ViewMode::Tiles) }

/// True when the item sits in a folded section: neither drawn nor hit.
pub(crate) fn tile_item_hidden(i: usize) -> bool {
    ST.with_borrow(|st| {
        let n = st.rows.len();
        st.tile_starts.iter().enumerate().any(|(k, (_, at))| {
            let end = st.tile_starts.get(k + 1).map_or(n, |s| s.1);
            st.tile_collapsed.get(k).copied().unwrap_or(false) && i >= *at && i < end
        })
    })
}

/// Item under a client point of the list, measured against the tile cells.
pub(crate) fn tile_at(list: HWND, x: i32, y: i32) -> Option<usize> {
    if !tiles_active() { return None; }
    let n = put(list, LVM_GETITEMCOUNT, 0, 0) as usize;
    let (cw, chh) = (ui::sc(330), ui::sc(84));
    for i in 0..n {
        if tile_item_hidden(i) { continue; }
        let mut p: POINT = unsafe { std::mem::zeroed() };
        if put(list, LVM_GETITEMPOSITION, i, &mut p as *mut _ as isize) == 0 || p.x < 0 { continue; }
        if x >= p.x && x < p.x + cw && y >= p.y && y < p.y + chh { return Some(i); }
    }
    None
}

/// Selects one tile (a click on the wide part of the cell that the control's own hit test misses).
pub(crate) fn tile_select(list: HWND, i: usize) {
    unsafe { SetFocus(list) };
    let mut it: LVITEMW = unsafe { std::mem::zeroed() };
    it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    it.state = LVIS_SELECTED | LVIS_FOCUSED;
    put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
    put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
}

/// A click on a section heading: folds that section.
pub(crate) fn tile_toggle_at(list: HWND, x: i32, y: i32) -> bool {
    if !tiles_active() { return false; }
    let head_h = ui::sc(40);
    let k = ST.with_borrow(|st| st.tile_bands.iter().position(|(_, by, _)| y >= *by && y < by + head_h && x >= ui::sc(6)));
    let Some(k) = k else { return false };
    ST.with_borrow_mut(|st| { if let Some(c) = st.tile_collapsed.get_mut(k) { *c = !*c; } });
    save_folds();
    tile_layout(list);
    unsafe { InvalidateRect(list, ptr::null(), 0) };
    true
}

/// Opens the row like a double click does.
pub(crate) fn open_row(h: HWND, i: usize) {
    let hit = ST.with_borrow(|st| st.rows.get(i).filter(|r| !r.hdr).map(|r| (r.target.clone().unwrap_or_else(|| st.dir.join(&r.name)), r.dir)));
    if let Some((p, is_dir)) = hit { unsafe { activate(h, p, is_dir) }; }
}

/// The item menu of one row (a right click on the wide part of a tile).
pub(crate) fn row_menu(h: HWND, i: usize) {
    let hit = ST.with_borrow(|st| st.rows.get(i).filter(|r| !r.hdr).map(|r| (st.dir.clone(), r.target.clone().unwrap_or_else(|| st.dir.join(&r.name)))));
    let Some((dir, p)) = hit else { return };
    if dir.to_string_lossy() == "::recycle" { unsafe { recycle_menu(h) } } else { item_menu(h, &dir, &[p]) }
}

fn reset_thumbs() {
    GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ST.with_borrow(|st| unsafe { ImageList_Remove(st.himl as _, -1) });
    // the indexes are gone: every row asks for its thumbnail again
    ST.with(|c| if let Ok(mut st) = c.try_borrow_mut() { st.rows.iter_mut().for_each(|r| r.thumb = -2); });
}

fn reload(h: HWND) { load(h, cur_dir()); }

/// After a file operation / a drop: a folder refreshes in place, and a search re-runs instead of being left behind
/// (`load` would clear the search results and show the folder under them again).
pub(crate) fn refresh_after_ops(h: HWND) {
    if desk() { reload(h); return; }
    if let Some(q) = ui::search_text() { search(h, &q); return; }
    reload_keep(h);
}

/// Up / Alt+Up: the parent folder, and from a drive root (or a virtual page other than Home) the Home page.
fn go_up(h: HWND) {
    let dir = cur_dir();
    let s = dir.to_string_lossy();
    if s.starts_with("::") {
        if s != "::home" { load(h, PathBuf::from("::home")); }
        return;
    }
    match dir.parent() {
        Some(p) if p != dir && !p.as_os_str().is_empty() => load(h, p.to_path_buf()),
        _ => load(h, PathBuf::from("::home")), // C:\ and other roots: Up is the Home page, as in Win11
    }
}

/// Watches `dir` and posts WM_REFRESH when its listing changes; the previous watcher stops on the generation bump.
fn start_watch(h: HWND, dir: PathBuf) {
    let era = WATCH_GEN.fetch_add(1, Relaxed) + 1;
    if desk() || is_virtual(&dir) || zipx::split_zip(&dir).is_some() { return; }
    let path = dir.to_string_lossy().replace('/', "\\");
    let main = h as isize;
    std::thread::spawn(move || unsafe {
        use windows_sys::Win32::{Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT, INVALID_HANDLE_VALUE}, Storage::FileSystem::{FindCloseChangeNotification, FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE}, System::Threading::WaitForSingleObject};
        let p = w(&path);
        let ch = FindFirstChangeNotificationW(p.as_ptr(), 0, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE);
        if ch == INVALID_HANDLE_VALUE { return; }
        loop {
            if WATCH_GEN.load(Relaxed) != era { break; }
            match WaitForSingleObject(ch, 500) {
                WAIT_OBJECT_0 => {
                    if WATCH_GEN.load(Relaxed) != era { break; }
                    let _ = PostMessageW(main as HWND, WM_REFRESH, 0, 0);
                    if FindNextChangeNotification(ch) == 0 { break; }
                }
                WAIT_TIMEOUT => {}
                _ => break,
            }
        }
        let _ = FindCloseChangeNotification(ch);
    });
}

/// Reloads the current folder after a change on disk, keeping the selection and the scroll position.
fn reload_keep(h: HWND) {
    if ui::searching() || in_zip() { return; }
    let (names, top) = ST.with_borrow(|st| {
        let (mut names, mut i) = (vec![], -1isize);
        loop {
            i = put(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            if !r.hdr { names.push(r.name.clone()); }
        }
        (names, put(st.list, LVM_GETTOPINDEX, 0, 0).max(0) as usize)
    });
    load(h, cur_dir());
    let list = ST.with_borrow(|st| st.list);
    if !names.is_empty() {
        let rows: Vec<String> = ST.with_borrow(|st| st.rows.iter().map(|r| r.name.clone()).collect());
        let mut it: LVITEMW = unsafe { std::mem::zeroed() };
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        for name in &names {
            if let Some(i) = rows.iter().position(|n| n.eq_ignore_ascii_case(name)) {
                it.state = LVIS_SELECTED;
                put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
            }
        }
    }
    put(list, LVM_ENSUREVISIBLE, top, 0);
    unsafe { ui::status_dirty(h) };
}

/// (list, current dir, full paths of the selected rows)
fn selected() -> (HWND, PathBuf, Vec<PathBuf>) {
    ST.with_borrow(|st| {
        let (mut sel, mut i) = (vec![], -1isize);
        loop {
            i = put(st.list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            let Some(r) = usize::try_from(i).ok().and_then(|i| st.rows.get(i)) else { break };
            if !r.hdr { sel.push(r.target.clone().unwrap_or_else(|| st.dir.join(&r.name))); }
        }
        (st.list, st.dir.clone(), sel)
    })
}

fn col(list: HWND, i: i32, text: &str, width: i32, right: bool) {
    let mut t = w(text);
    let mut c: LVCOLUMNW = unsafe { std::mem::zeroed() };
    c.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT; c.pszText = t.as_mut_ptr(); c.cx = width;
    c.fmt = if right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
    put(list, LVM_INSERTCOLUMNW, i as usize, &c as *const _ as isize);
}

/// The four real column titles follow the page (the Recycle Bin shows the deletion date and the original location).
fn set_column_titles(list: HWND, titles: [&str; 4]) {
    let hdr = put(list, LVM_GETHEADER, 0, 0) as HWND;
    if hdr.is_null() { return; }
    for (i, title) in titles.iter().enumerate() {
        let mut t = w(title);
        let mut it: HDITEMW = unsafe { std::mem::zeroed() };
        it.mask = HDI_TEXT;
        it.pszText = t.as_mut_ptr();
        it.cchTextMax = t.len() as i32;
        unsafe { SendMessageW(hdr, HDM_SETITEMW, i, &mut it as *mut _ as isize) };
    }
    unsafe { InvalidateRect(hdr, ptr::null(), 0) };
}

/// Opens a new tab on the current folder.
fn tab_new() {
    let tab = ST.with_borrow_mut(|st| { st.tabs.push(st.dir.clone()); st.active = st.tabs.len() - 1; st.tab });
    tab_text(tab, 0, "", true);
}

/// Reopens the tab that was closed last (Ctrl+Shift+T), like Explorer.
fn tab_reopen(h: HWND) {
    let Some(dir) = ST.with_borrow_mut(|st| st.closed.pop()) else { return };
    let (tab, active) = ST.with_borrow_mut(|st| { st.tabs.push(dir.clone()); st.active = st.tabs.len() - 1; (st.tab, st.active) });
    tab_text(tab, active, "", true);
    load(h, dir);
}

/// A second window on the current folder (Ctrl+N in Explorer opens a window, not a folder).
fn new_window() {
    let Ok(exe) = env::current_exe() else { return };
    let _ = std::process::Command::new(exe).arg(cur_dir()).spawn();
}

/// Plain text on the system clipboard (Ctrl+Shift+C copies the selected paths).
fn clip_text(s: &str) -> bool {
    use windows_sys::Win32::System::{DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData}, Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE}, Ole::CF_UNICODETEXT};
    unsafe {
        if OpenClipboard(ptr::null_mut()) == 0 { return false; }
        EmptyClipboard();
        let u: Vec<u16> = s.encode_utf16().chain([0]).collect();
        let h = GlobalAlloc(GMEM_MOVEABLE, u.len() * 2);
        if h.is_null() { CloseClipboard(); return false; }
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() { CloseClipboard(); return false; }
        ptr::copy_nonoverlapping(u.as_ptr(), p, u.len());
        GlobalUnlock(h);
        let ok = !SetClipboardData(CF_UNICODETEXT as u32, h).is_null(); // the clipboard owns the memory from here
        CloseClipboard();
        ok
    }
}

/// Copy path / Copy as path: the selection, or the folder itself when nothing is selected.
fn copy_path(h: HWND) {
    let (_, dir, paths) = selected();
    let text = if paths.is_empty() { dir.to_string_lossy().into_owned() } else { paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\r\n") };
    if !clip_text(&text) { warn(h, "Cannot open the clipboard"); }
}

fn tab_select(h: HWND, i: usize) {
    let dir = ST.with_borrow_mut(|st| { st.active = i; st.tabs[i].clone() });
    load(h, dir);
}

fn tab_close(h: HWND) { let a = ST.with_borrow(|st| st.active); tab_close_at(h, a); }

/// Closes tab `i` (the last one stays). The listing changes only when the active tab was the one closed.
fn tab_close_at(h: HWND, i: usize) {
    if ST.with_borrow(|st| st.tabs.len() < 2) { unsafe { PostMessageW(h, WM_CLOSE, 0, 0) }; return; } // the last tab closes the window
    let (tab, was_active) = ST.with_borrow_mut(|st| {
        if i >= st.tabs.len() { return (st.tab, None); }
        st.closed.push(st.tabs[i].clone());
        if st.closed.len() > 20 { st.closed.remove(0); }
        st.tabs.remove(i);
        let before = st.active;
        st.active = if i < before { before - 1 } else { before.min(st.tabs.len() - 1) };
        (st.tab, Some(i == before))
    });
    match was_active {
        Some(true) => { let a = ST.with_borrow(|st| st.active); tab_select(h, a); }
        Some(false) => tab_text(tab, 0, "", false),
        None => {}
    }
}

fn toggle_view(h: HWND) {
    let view = ST.with_borrow(|st| st.view);
    set_view(h, if view == ViewMode::Details { ViewMode::Large } else { ViewMode::Details });
}

/// A thumbnail image list of `px` pixels for the icon views; rebuilt when the size changes.
fn ensure_iml(list: HWND, px: i32) {
    let (old, cur) = ST.with_borrow(|st| (st.himl, st.iml_px));
    if cur == px && old != 0 { return; }
    let new = unsafe { ImageList_Create(px, px, ILC_COLOR32, 64, 64) };
    put(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, new as isize);
    if old != 0 { unsafe { ImageList_Destroy(old as _) }; }
    ST.with_borrow_mut(|st| { st.himl = new as isize; st.iml_px = px; });
    let gap = match px { 256 => 56, 96 => 34, 48 => 28, _ => 24 };
    put(list, LVM_SETICONSPACING, 0, (((px + gap) as isize) << 16) | (px + gap) as isize);
    reset_thumbs();
}

/// Applies a view mode to the list without reloading the folder (also used when a folder's saved view is restored).
fn apply_view(list: HWND, view: ViewMode) {
    ui::row_height(list, view == ViewMode::Details);
    if view.icon_px() > 16 { ensure_iml(list, view.icon_px()); }
    if view == ViewMode::Tiles {
        // icon view cells sized for a 48 px icon, the name and the drive usage line (this list ignores LVM_SETVIEW)
        unsafe {
            let style = GetWindowLongPtrW(list, GWL_STYLE) as u32;
            // the tile positions come from tile_layout: auto arranging would move them back into a plain grid
            SetWindowLongPtrW(list, GWL_STYLE, ((style & !(LVS_TYPEMASK | LVS_AUTOARRANGE)) | LVS_ICON) as isize);
            SetWindowPos(list, ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
        put(list, LVM_SETICONSPACING, 0, ((ui::sc(108) as isize) << 16) | ui::sc(238) as isize);
        return;
    }
    // an owner-data list ignores LVM_SETVIEW (it only hides the header): the view is the window style's type bits.
    // Icon views also need LVS_AUTOARRANGE, or the control leaves every virtual item at the same spot.
    let ty = match view {
        ViewMode::Details => LVS_REPORT,
        ViewMode::List => LVS_LIST,
        ViewMode::Small => LVS_SMALLICON,
        _ => LVS_ICON, // the icon sizes and tiles differ by the image list
    };
    unsafe {
        let style = GetWindowLongPtrW(list, GWL_STYLE) as u32;
        SetWindowLongPtrW(list, GWL_STYLE, ((style & !(LVS_TYPEMASK | LVS_AUTOARRANGE)) | ty) as isize);
        SetWindowPos(list, ptr::null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
    }
    if view == ViewMode::Details { unsafe { ui::fit_filler(list) } }
}

/// Details, list, the four icon sizes or tiles (the View menu and Ctrl+wheel).
fn set_view(_h: HWND, view: ViewMode) {
    let list = ST.with_borrow_mut(|st| { st.view = view; st.thumbs = view != ViewMode::Details; st.list });
    THUMBS.store(view != ViewMode::Details, Relaxed);
    // empty the list first: restyling a list full of items makes the control lay all of them out again (seconds on thousands)
    put(list, LVM_DELETEALLITEMS, 0, 0);
    apply_view(list, view);
    save_current_view();
    fill_items();
    unsafe { InvalidateRect(list, ptr::null(), 0) };
}

/// Ctrl+wheel: details -> list -> small / medium / large / extra large icons, and back.
fn zoom_view(h: HWND, up: bool) {
    let order = [ViewMode::Details, ViewMode::List, ViewMode::Small, ViewMode::Medium, ViewMode::Large, ViewMode::ExtraLarge];
    let cur = ST.with_borrow(|st| st.view);
    let i = order.iter().position(|&v| v == cur).unwrap_or(4);
    let j = if up { (i + 1).min(order.len() - 1) } else { i.saturating_sub(1) };
    if order[j] != cur { set_view(h, order[j]); }
}

/// Sorts the rows that are on screen (a folder or the hits of a search) and repaints.
fn sort_now(h: HWND, c: usize, asc: bool) {
    SORT_COL.store(c, Relaxed);
    SORT_ASC.store(asc, Relaxed);
    let n = ST.with_borrow_mut(|st| {
        st.all.retain(|r| !r.hdr);
        sort_rows(&mut st.all, c, asc);
        add_headers(&mut st.all);
        st.all.iter_mut().for_each(|r| r.thumb = -2);
        st.rows = visible_rows(&st.all);
        st.rows.len()
    });
    let _ = n;
    reset_thumbs();
    // the rows just changed: refill, then put the selection back on the same names
    let sel: Vec<String> = ST.with_borrow(|st| {
        let list = st.list;
        (0..st.rows.len()).filter(|&i| put(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0).map(|i| st.rows[i].name.clone()).collect()
    });
    fill_items();
    if !sel.is_empty() {
        ST.with_borrow(|st| {
            let list = st.list;
            for (i, r) in st.rows.iter().enumerate() {
                if sel.iter().any(|n| n == &r.name) {
                    let mut it: LVITEMW = unsafe { std::mem::zeroed() };
                    it.state = LVIS_SELECTED;
                    it.stateMask = LVIS_SELECTED;
                    put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
                }
            }
        });
    }
    let list = ST.with_borrow(|st| st.list);
    let hdr = put(list, LVM_GETHEADER, 0, 0) as HWND;
    unsafe { if !hdr.is_null() { InvalidateRect(hdr, ptr::null(), 0); } }
    unsafe { ui::status_dirty(h) };
    save_current_view();
}

/// "Group by": 0 none, 1 date, 2 name, 3 type, 4 size; then the rows are grouped again.
fn group_now(h: HWND, mode: usize) {
    if GROUP.load(Relaxed) != mode { COLLAPSED.with_borrow_mut(|s| s.clear()); }
    GROUP.store(mode, Relaxed);
    sort_now(h, SORT_COL.load(Relaxed), SORT_ASC.load(Relaxed));
}

/// Select all, or invert the selection.
fn select_all(h: HWND, invert: bool) {
    let (list, n, rows) = ST.with_borrow(|st| (st.list, st.rows.len(), st.rows.iter().map(|r| r.hdr).collect::<Vec<_>>()));
    let mut it: LVITEMW = unsafe { std::mem::zeroed() };
    it.stateMask = LVIS_SELECTED;
    if !invert {
        it.state = LVIS_SELECTED;
        put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
    } else {
        for i in 0..n {
            if rows[i] { continue; }
            let on = put(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
            it.state = if on { 0 } else { LVIS_SELECTED };
            put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
        }
    }
    unsafe { ui::status_dirty(h) };
}

fn toggle_checkboxes() {
    CHECK_BOXES.fetch_xor(true, Relaxed);
    save_globals();
    let list = ST.with_borrow(|st| st.list);
    unsafe { InvalidateRect(list, ptr::null(), 0) };
}

fn duplicate_tab(h: HWND, i: usize) {
    let Some(p) = ST.with_borrow(|st| st.tabs.get(i).cloned()) else { return };
    let (tab, active) = ST.with_borrow_mut(|st| {
        let at = (st.active + 1).min(st.tabs.len());
        st.tabs.insert(at, p.clone());
        st.active = at;
        (st.tab, st.active)
    });
    tab_text(tab, active, "", true);
    load(h, p);
}

fn close_other_tabs(h: HWND, i: usize) {
    let Some(p) = ST.with_borrow(|st| st.tabs.get(i).cloned()) else { return };
    ST.with_borrow_mut(|st| { st.tabs = vec![p.clone()]; st.active = 0; });
    load(h, p);
}

fn copy_tab_path(h: HWND, i: usize) {
    if let Some(p) = ST.with_borrow(|st| st.tabs.get(i).cloned()) { if !clip_text(&p.to_string_lossy()) { warn(h, "Cannot open the clipboard"); } }
}

/// "New > Compressed (zipped) folder": an empty, valid ZIP named like Explorer names it.
fn new_zip(h: HWND, dir: &PathBuf) {
    if zipx::split_zip(dir).is_some() { warn(h, &t!("Архив открыт только для чтения", "An archive is read-only", ja: "アーカイブは読み取り専用です", pt: "Um arquivo é somente leitura", zh: "压缩包在这里是只读的")); return; }
    let base = t!("Новый архив.zip", "New Compressed (zipped) Folder.zip", ja: "新しい圧縮フォルダー.zip", pt: "Nova pasta compactada.zip", zh: "新建压缩文件夹.zip");
    match new_entry(dir, base, false) {
        Ok(p) => {
            let made = fs::File::create(&p).map_err(|e| e.to_string()).and_then(|f| zipx::Writer::new(f).finish().map(drop).map_err(|e| e.to_string()));
            if let Err(e) = made { let _ = fs::remove_file(&p); warn(h, &e); return; }
            reload(h);
            if let Some(n) = p.file_name() { select_edit(&n.to_string_lossy()); }
        }
        Err(e) => warn(h, &e),
    }
}

/// Focus the search box (Ctrl+F / Ctrl+E), like Explorer.
fn search_focus() {
    let search = ST.with_borrow(|st| st.search);
    if search == 0 as HWND { return; }
    unsafe {
        SetFocus(search);
        put(search, EM_SETSEL, 0, -1);
    }
}

/// Wallpaper for the desktop window.
mod wall {
    use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*, UI::WindowsAndMessaging::{IMAGE_BITMAP, LR_LOADFROMFILE, LoadImageW}};

    /// The part of a bw x bh picture that "fill" shows on a cw x ch screen: centered, cropped to the screen's aspect.
    /// (x, y, w, h) in the picture.
    pub fn fill_src(bw: i32, bh: i32, cw: i32, ch: i32) -> (i32, i32, i32, i32) {
        let (sw, sh) = if bw as i64 * ch as i64 > bh as i64 * cw as i64 { (bh * cw / ch.max(1), bh) } else { (bw, bw * ch / cw.max(1)) };
        ((bw - sw) / 2, (bh - sh) / 2, sw, sh)
    }

    /// Every monitor's rectangle, relative to the virtual screen's top-left (vx, vy).
    fn monitors(vx: i32, vy: i32) -> Vec<RECT> {
        unsafe extern "system" fn each(_: HMONITOR, _: HDC, r: *mut RECT, lp: isize) -> i32 {
            unsafe { (*(lp as *mut Vec<RECT>)).push(*r) };
            1
        }
        let mut v: Vec<RECT> = Vec::new();
        unsafe { EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(each), &mut v as *mut _ as isize) };
        v.iter().map(|r| RECT { left: r.left - vx, top: r.top - vy, right: r.right - vx, bottom: r.bottom - vy }).collect()
    }

    /// The BMP for the whole virtual screen (vx, vy, cw x ch): scaled with "fill" onto each monitor on its own, so a second
    /// monitor of another size or aspect gets a whole picture too. 0 = no wallpaper file.
    pub fn load(vx: i32, vy: i32, cw: i32, ch: i32) -> isize {
        let Ok(base) = std::env::var("LOCALAPPDATA") else { return 0 };
        let path: Vec<u16> = format!(r"{base}\rs-shell\wallpaper.bmp").encode_utf16().chain([0]).collect();
        unsafe {
            let src = LoadImageW(std::ptr::null_mut(), path.as_ptr(), IMAGE_BITMAP, 0, 0, LR_LOADFROMFILE);
            if src.is_null() { return 0; }
            let mut bm: BITMAP = std::mem::zeroed();
            GetObjectW(src, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _);
            let screen = GetDC(std::ptr::null_mut());
            let dst = CreateCompatibleBitmap(screen, cw, ch);
            let (sdc, ddc) = (CreateCompatibleDC(screen), CreateCompatibleDC(screen));
            let (o1, o2) = (SelectObject(sdc, src), SelectObject(ddc, dst));
            PatBlt(ddc, 0, 0, cw, ch, BLACKNESS); // the gaps between monitors of different heights
            SetStretchBltMode(ddc, HALFTONE);
            let mut mons = monitors(vx, vy);
            if mons.is_empty() { mons.push(RECT { left: 0, top: 0, right: cw, bottom: ch }); }
            for m in mons {
                let (mw, mh) = (m.right - m.left, m.bottom - m.top);
                let (sx, sy, sw, sh) = fill_src(bm.bmWidth, bm.bmHeight, mw, mh);
                StretchBlt(ddc, m.left, m.top, mw, mh, sdc, sx, sy, sw, sh, SRCCOPY);
            }
            SelectObject(sdc, o1); SelectObject(ddc, o2);
            DeleteDC(sdc); DeleteDC(ddc); ReleaseDC(std::ptr::null_mut(), screen); DeleteObject(src);
            dst as isize
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn fill_crops_to_the_screen_aspect() {
            assert_eq!(super::fill_src(1920, 1080, 1920, 1080), (0, 0, 1920, 1080));
            assert_eq!(super::fill_src(1920, 1080, 1080, 1080), (420, 0, 1080, 1080)); // square screen: the sides go
            assert_eq!(super::fill_src(1920, 1080, 2560, 1080), (0, 135, 1920, 810)); // ultrawide: top and bottom go
            assert_eq!(super::fill_src(100, 100, 0, 0).2, 100); // no division by zero
        }
    }

    pub fn paint(dc: HDC, bmp: isize, cw: i32, ch: i32) {
        unsafe {
            let m = CreateCompatibleDC(dc);
            let old = SelectObject(m, bmp as _);
            BitBlt(dc, 0, 0, cw, ch, m, 0, 0, SRCCOPY);
            SelectObject(m, old);
            DeleteDC(m);
        }
    }
}

/// Shell integration through COM (windows crate): file operations, context menu, drag and drop.
mod sh {
    use std::{cell::RefCell, ffi::c_void, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, sync::Mutex};
    use windows::{core::*, Win32::{Foundation::*, System::{Com::*, DataExchange::RegisterClipboardFormatW, Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock}, Ole::*, SystemServices::MODIFIERKEYS_FLAGS}, UI::{Shell::{Common::ITEMIDLIST, *}, WindowsAndMessaging::*}}};

    #[derive(Clone, Copy)]
    pub enum Op { Copy, Move }

    /// [user Desktop, public Desktop] (the user one may be redirected, e.g. to OneDrive).
    pub fn desktop_dirs() -> Vec<PathBuf> {
        [&FOLDERID_Desktop, &FOLDERID_PublicDesktop].iter().filter_map(|id| unsafe {
            let p = SHGetKnownFolderPath(*id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s.map(PathBuf::from)
        }).collect()
    }

    /// A known folder where Windows says it is (it may be moved to another drive).
    pub fn known_dir(id: &GUID) -> Option<PathBuf> {
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s.map(PathBuf::from)
        }
    }

    pub fn init() { unsafe { let _ = OleInitialize(None); } }

    fn wide(p: &PathBuf) -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() }

    /// Copy / move into `to`, or recycle. Must not be called while ST is borrowed (the shell pumps messages).
    pub fn fileop(h: *mut c_void, op: Op, from: &[PathBuf], to: &PathBuf) -> Result<()> {
        unsafe {
            let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            fo.SetOwnerWindow(HWND(h))?;
            fo.SetOperationFlags(FOF_RENAMEONCOLLISION)?;
            let item = |p: &PathBuf| SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide(p).as_ptr()), None);
            let dest = item(to)?;
            for p in from {
                let it = item(p)?;
                match op {
                    Op::Copy => fo.CopyItem(&it, &dest, PCWSTR::null(), None)?,
                    Op::Move => fo.MoveItem(&it, &dest, PCWSTR::null(), None)?,
                }
            }
            fo.PerformOperations()
        }
    }

    /// Delete to the Recycle Bin without the shell's own dialogs: our progress window and summary are the UI.
    pub fn recycle_silent(h: *mut core::ffi::c_void, paths: &[PathBuf]) -> Result<()> {
        unsafe {
            let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            fo.SetOwnerWindow(HWND(h))?;
            fo.SetOperationFlags(FOF_ALLOWUNDO | FOF_SILENT | FOF_NOCONFIRMATION | FOF_NOERRORUI)?;
            let item = |p: &PathBuf| SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide(p).as_ptr()), None);
            for p in paths { fo.DeleteItem(&item(p)?, None)?; }
            fo.PerformOperations()
        }
    }

    /// Background thumbnail loader: GetImage for each (era, row, path, px) asked for, result posted to the main window.
    pub fn start_thumbs(main: isize) -> std::sync::mpsc::Sender<(u32, usize, PathBuf, i32)> {
        let (tx, rx) = std::sync::mpsc::channel::<(u32, usize, PathBuf, i32)>();
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            for (era, row, path, px) in rx {
                if era != super::GEN.load(std::sync::atomic::Ordering::Relaxed) { continue; }
                let Ok(f) = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(PCWSTR(wide(&path).as_ptr()), None) else { continue };
                let Ok(bmp) = f.GetImage(SIZE { cx: px, cy: px }, SIIGBF_BIGGERSIZEOK) else { continue };
                let padded = pad(bmp.0, px);
                windows_sys::Win32::Graphics::Gdi::DeleteObject(bmp.0 as _);
                if padded.is_null() { continue; }
                let b = Box::new((era, row, padded as isize));
                let _ = PostMessageW(Some(HWND(main as *mut c_void)), super::WM_THUMB, WPARAM(Box::into_raw(b) as usize), LPARAM(0));
            }
        });
        tx
    }

    /// A px x px shell image of a file, folder or drive (the icon, not a thumbnail), 32-bit with alpha. 0 on failure.
    pub fn tile_image(p: &Path, px: i32) -> isize {
        unsafe {
            let file = p.to_path_buf();
            let Ok(f) = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(PCWSTR(wide(&file).as_ptr()), None) else { return 0 };
            let Ok(bmp) = f.GetImage(SIZE { cx: px, cy: px }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK) else { return 0 };
            let padded = pad(bmp.0, px);
            windows_sys::Win32::Graphics::Gdi::DeleteObject(bmp.0 as _);
            padded as isize
        }
    }

    /// Centers `src` (any size up to t, 32-bit with alpha) on a transparent t x t bitmap: ImageList_Add needs exact sizes.
    unsafe fn pad(src: *mut c_void, t: i32) -> *mut c_void {
        use windows_sys::Win32::Graphics::Gdi::*;
        unsafe {
            let mut bm: BITMAP = std::mem::zeroed();
            GetObjectW(src as _, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _);
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader = BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: t, biHeight: -t, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..std::mem::zeroed() };
            let mut bits = std::ptr::null_mut();
            let dst = CreateDIBSection(std::ptr::null_mut(), &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if dst.is_null() { return std::ptr::null_mut(); }
            let (dc, sdc) = (CreateCompatibleDC(std::ptr::null_mut()), CreateCompatibleDC(std::ptr::null_mut()));
            let (o1, o2) = (SelectObject(dc, dst), SelectObject(sdc, src as _));
            BitBlt(dc, (t - bm.bmWidth) / 2, (t - bm.bmHeight) / 2, bm.bmWidth, bm.bmHeight, sdc, 0, 0, SRCCOPY);
            SelectObject(dc, o1); SelectObject(sdc, o2);
            DeleteDC(dc); DeleteDC(sdc);
            dst
        }
    }

    /// Puts `paths` on the system clipboard like Explorer: the shell's own data object plus the "Preferred DropEffect"
    /// marker (copy or cut), so Ctrl+V works in a real Explorer window and in other file managers.
    pub fn clip_set(h: *mut c_void, paths: &[PathBuf], cut: bool) -> Result<()> {
        unsafe {
            let data: IDataObject = ui(HWND(h), paths)?;
            let fmt = FORMATETC { cfFormat: RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0 as u32, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let hg = GlobalAlloc(GMEM_MOVEABLE, 4)?;
            *(GlobalLock(hg) as *mut u32) = if cut { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 };
            let _ = GlobalUnlock(hg);
            let med = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: hg }, pUnkForRelease: std::mem::ManuallyDrop::new(None) };
            data.SetData(&fmt, &med, true)?; // the data object owns the memory from here
            OleSetClipboard(&data)
        }
    }

    /// What is on the system clipboard as files: (paths, cut?). None = no files there.
    pub fn clip_get() -> Option<(Vec<PathBuf>, bool)> {
        unsafe {
            let o = OleGetClipboard().ok()?;
            let paths = hdrop_paths(&o).ok().filter(|p| !p.is_empty())?;
            let fmt = FORMATETC { cfFormat: RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0 as u32, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let cut = o.GetData(&fmt).ok().is_some_and(|mut m| {
                let p = GlobalLock(m.u.hGlobal) as *const u32;
                let v = if p.is_null() { 0 } else { *p };
                let _ = GlobalUnlock(m.u.hGlobal);
                ReleaseStgMedium(&mut m);
                v & DROPEFFECT_MOVE.0 != 0
            });
            Some((paths, cut))
        }
    }

    pub fn clip_clear() { unsafe { let _ = OleSetClipboard(None); } }

    struct Pidls(Vec<*mut ITEMIDLIST>);
    impl Drop for Pidls { fn drop(&mut self) { for p in &self.0 { unsafe { CoTaskMemFree(Some(*p as *const c_void)) }; } } }

    /// A shell UI object (IContextMenu, IDataObject...) for items that all live in one folder.
    fn ui<T: Interface>(h: HWND, paths: &[PathBuf]) -> Result<T> {
        unsafe {
            let mut abs = Pidls(vec![]);
            for p in paths {
                let mut pidl = std::ptr::null_mut();
                SHParseDisplayName(PCWSTR(wide(p).as_ptr()), None, &mut pidl, 0, None)?;
                abs.0.push(pidl);
            }
            let parent: IShellFolder = SHBindToParent(abs.0[0], None)?;
            let kids: Vec<*const ITEMIDLIST> = abs.0.iter().map(|&p| ILFindLastID(p) as *const _).collect();
            parent.GetUIObjectOf(h, &kids, None)
        }
    }

    thread_local! { static CM2: RefCell<Option<IContextMenu2>> = const { RefCell::new(None) }; }

    /// Forwards owner-draw / init-popup menu messages so submenus like "Send to" get filled in.
    pub fn menu_msg(m: u32, wp: usize, lp: isize) -> bool {
        CM2.with_borrow(|c| c.as_ref().is_some_and(|c| unsafe { c.HandleMenuMsg(m, WPARAM(wp), LPARAM(lp)).is_ok() }))
    }

    /// The real shell context menu for `paths`, so 7-Zip/Git/etc. entries show up. `extra` = our own items on top
    /// (ids above the shell's range); returns the one picked, None when the shell handled the pick or nothing was picked.
    /// The name Windows shows for its own "Send to > Compressed (zipped) folder": the stem of the .ZFSendToTarget file in SendTo.
    pub(crate) fn system_zip_name() -> Option<String> {
        let dir = PathBuf::from(std::env::var_os("APPDATA")?).join(r"Microsoft\Windows\SendTo");
        let f = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ZFSendToTarget")))?;
        f.file_stem().map(|s| s.to_string_lossy().into_owned())
    }

    /// Takes Windows' own zip entry out of the menu: our "Compress to ZIP" is the one. (Menus of other zip tools, 7-Zip etc., stay.)
    unsafe fn hide_system_zip(cm: &IContextMenu, menu: HMENU) {
        use windows::Win32::UI::WindowsAndMessaging::{DeleteMenu, GetMenuItemCount, GetMenuStringW, GetSubMenu, MF_BYPOSITION};
        let Some(name) = system_zip_name() else { return };
        // WM_INITMENUPOPUP: the Send to submenu fills itself only when it is opened
        if let Ok(c2) = cm.cast::<IContextMenu2>() { let _ = unsafe { c2.HandleMenuMsg(0x0117, WPARAM(menu.0 as usize), LPARAM(0)) }; }
        unsafe fn strip(m: HMENU, name: &str) { unsafe {
            for i in (0..GetMenuItemCount(Some(m)).max(0)).rev() {
                let mut b = [0u16; 256];
                let n = GetMenuStringW(m, i as u32, Some(&mut b), MF_BYPOSITION);
                if String::from_utf16_lossy(&b[..n.max(0) as usize]).replace('&', "").eq_ignore_ascii_case(name) {
                    let _ = DeleteMenu(m, i as u32, MF_BYPOSITION);
                    continue;
                }
                let sub = GetSubMenu(m, i);
                if !sub.is_invalid() { strip(sub, name); }
            }
        } }
        unsafe { strip(menu, &name) };
    }

    /// Windows' own "Pin to Quick access" writes Explorer's list, which our pane never reads: our own entry is the one.
    /// Found by the canonical verbs pintohome / unpinfromhome, with the labels of the five languages as a fallback.
    unsafe fn hide_shell_pins(cm: &IContextMenu, menu: HMENU) {
        use windows::Win32::UI::WindowsAndMessaging::{DeleteMenu, GetMenuItemCount, GetMenuItemID, GetMenuStringW, MF_BYPOSITION};
        let labels = ["pin to quick access", "unpin from quick access", "固定到快速访问", "从快速访问取消固定", "закрепить в быстром доступе", "открепить от быстрого доступа", "クイック アクセスにピン留め", "クイック アクセスからピン留めを解除", "fixar no acesso rápido", "desafixar do acesso rápido"];
        let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0);
        for i in (0..count).rev() {
            unsafe {
                let id = GetMenuItemID(menu, i);
                if id == 0 || id == u32::MAX { continue; }
                let mut verb = [0u16; 64];
                let mut hit = false;
                if cm.GetCommandString(id as usize - 1, GCS_VERBW, None, PSTR(verb.as_mut_ptr() as *mut u8), verb.len() as u32).is_ok() {
                    let n = verb.iter().position(|&c| c == 0).unwrap_or(0);
                    let v = String::from_utf16_lossy(&verb[..n]).to_lowercase();
                    hit = v == "pintohome" || v == "unpinfromhome";
                }
                if !hit {
                    let mut b = [0u16; 256];
                    let n = GetMenuStringW(menu, i as u32, Some(&mut b), MF_BYPOSITION);
                    let t = String::from_utf16_lossy(&b[..n.max(0) as usize]).replace('&', "").to_lowercase();
                    hit = labels.contains(&t.as_str());
                }
                if hit { let _ = DeleteMenu(menu, i as u32, MF_BYPOSITION); }
            }
        }
    }

    unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> BOOL {
        unsafe { (*(lp.0 as *mut Vec<isize>)).push(h.0 as isize); }
        TRUE
    }

    fn top_windows() -> Vec<isize> {
        let mut v: Vec<isize> = vec![];
        unsafe { let _ = EnumWindows(Some(collect), LPARAM(&mut v as *mut _ as isize)); }
        v
    }

    /// Dialogs the shell opens for a command ("Open with" > choose another app, Properties) can land partly off screen on some
    /// setups. For a few seconds after the command, any new window that sticks out of its monitor is moved back inside.
    fn keep_on_screen(before: Vec<isize>) {
        std::thread::spawn(move || unsafe {
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(150));
                for h in top_windows().into_iter().filter(|h| !before.contains(h)) {
                    let h = HWND(h as *mut c_void);
                    let mut r = RECT::default();
                    if !IsWindowVisible(h).as_bool() || IsZoomed(h).as_bool() || GetWindowRect(h, &mut r).is_err() || r.right <= r.left { continue; }
                    let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO { cbSize: size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32, ..Default::default() };
                    let mon = windows::Win32::Graphics::Gdi::MonitorFromWindow(h, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST);
                    if !windows::Win32::Graphics::Gdi::GetMonitorInfoW(mon, &mut mi).as_bool() { continue; }
                    let a = mi.rcWork;
                    if r.right - r.left > a.right - a.left || r.bottom - r.top > a.bottom - a.top { continue; } // maximized-like: not a stray dialog
                    let x = r.left.min(a.right - (r.right - r.left)).max(a.left);
                    let y = r.top.min(a.bottom - (r.bottom - r.top)).max(a.top);
                    if (x, y) != (r.left, r.top) {
                        let _ = SetWindowPos(h, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
            }
        });
    }

    /// The items the custom menu shows: the shell's own menu, our extras (already in `menu`), and "Show more options".
    unsafe fn model(cm: &IContextMenu, menu: HMENU) -> Vec<crate::ctxmenu::Item> {
        use windows::Win32::Graphics::Gdi::{GetObjectW, BITMAP, HGDIOBJ};
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetMenuItemInfoW, GetSubMenu, MENUITEMINFOW,
            MFS_CHECKED, MFS_DEFAULT, MFS_DISABLED, MFS_GRAYED, MFT_RADIOCHECK, MFT_SEPARATOR,
            MIIM_BITMAP, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU};
        fn glyph(id: u32, verb: &str) -> &'static str {
            match id {
                crate::MENU_ZIP => return "\u{E7B8}",
                crate::MENU_EXTRACT => return "\u{E838}",
                crate::MENU_RENAME => return "\u{E8AC}",
                crate::MENU_PIN => return "\u{E840}",
                _ => {}
            }
            match verb.to_ascii_lowercase().as_str() {
                "open" => "\u{E8E5}",
                "opennew" | "opennewwindow" => "\u{E8A7}",
                "edit" => "\u{E70F}",
                "print" | "printto" => "\u{E749}",
                "cut" => "\u{E8C6}",
                "copy" => "\u{E8C8}",
                "paste" => "\u{E77F}",
                "link" => "\u{E71B}",
                "delete" => "\u{E74D}",
                "rename" => "\u{E8AC}",
                "properties" => "\u{E946}",
                "sendto" => "\u{E724}",
                "pintostartscreen" => "\u{E840}",
                "openwith" => "\u{E7AC}",
                "previousversions" => "\u{E81C}",
                "share" => "\u{E72D}",
                "runas" => "\u{E72E}",
                _ => "",
            }
        }
        fn read(cm: &IContextMenu, m: HMENU, depth: usize) -> Vec<crate::ctxmenu::Item> {
            use windows::core::PWSTR;
            let mut out = Vec::new();
            if depth > 6 { return out; }
            unsafe {
                for i in 0..GetMenuItemCount(Some(m)).max(0) {
                    let mut buf = [0u16; 512];
                    let mut mi = MENUITEMINFOW { cbSize: size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_ID | MIIM_STRING | MIIM_FTYPE | MIIM_STATE | MIIM_SUBMENU | MIIM_BITMAP,
                        dwTypeData: PWSTR(buf.as_mut_ptr()), cch: buf.len() as u32, ..Default::default() };
                    if GetMenuItemInfoW(m, i as u32, true.into(), &mut mi).is_err() { continue; }
                    if mi.fType.0 & MFT_SEPARATOR.0 != 0 { out.push(crate::ctxmenu::Item::separator()); continue; }
                    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
                    let raw = String::from_utf16_lossy(&buf[..n]);
                    let id = GetMenuItemID(m, i as i32);
                    let mut verb = [0u16; 128];
                    let v = if id > 0 && id != u32::MAX && cm.GetCommandString(id as usize - 1, GCS_VERBW, None, PSTR(verb.as_mut_ptr() as *mut u8), verb.len() as u32).is_ok() {
                        let k = verb.iter().position(|&c| c == 0).unwrap_or(0);
                        String::from_utf16_lossy(&verb[..k])
                    } else { String::new() };
                    let sub_menu = GetSubMenu(m, i as i32);
                    let sub = if !sub_menu.is_invalid() {
                        CM2.with_borrow(|c| c.as_ref().is_some_and(|c| c.HandleMenuMsg(0x0117, WPARAM(sub_menu.0 as usize), LPARAM(i as isize)).is_ok()));
                        read(cm, sub_menu, depth + 1)
                    } else { Vec::new() };
                    let (mut hbmp, mut bmp) = (0isize, (0, 0, 0));
                    if !mi.hbmpItem.0.is_null() {
                        let mut bm = BITMAP::default();
                        if GetObjectW(HGDIOBJ(mi.hbmpItem.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) != 0 && bm.bmWidth > 0 && bm.bmWidth <= 256 {
                            hbmp = mi.hbmpItem.0 as isize;
                            bmp = (bm.bmWidth, bm.bmHeight, bm.bmBitsPixel as i32);
                        }
                    }
                    let (text, accel, hint) = crate::ctxmenu::Item::split_label(&raw);
                    let (text, accel) = if text.is_empty() && accel.is_none() && !v.is_empty() { (v.clone(), None) } else { (text, accel) };
                    out.push(crate::ctxmenu::Item {
                        id, text, hint, accel,
                        disabled: mi.fState.0 & (MFS_DISABLED.0 | MFS_GRAYED.0) != 0,
                        checked: mi.fState.0 & MFS_CHECKED.0 != 0,
                        radio: mi.fType.0 & MFT_RADIOCHECK.0 != 0,
                        default: mi.fState.0 & MFS_DEFAULT.0 != 0,
                        sep: false,
                        glyph: glyph(id, &v),
                        hbmp, bmp, sub,
                    });
                }
            }
            out
        }
        let mut items = read(cm, menu, 0);
        let more = { use winrust_common::t; t!("Показать дополнительные параметры", "Show more options", ja: "その他のオプションを表示", pt: "Mostrar mais opções", zh: "显示更多选项") };
        items.push(crate::ctxmenu::Item::separator());
        items.push(crate::ctxmenu::Item { id: crate::ctxmenu::MORE, text: more.into(), glyph: "\u{E712}", ..Default::default() });
        items
    }

    unsafe fn invoke(cm: &IContextMenu, hwnd: HWND, id: i32, pt: POINT) -> Result<()> {
        // ptInvoke: without it dialogs the shell opens ("Open with" > choose another app) land off screen
        let ci = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_PTINVOKE,
            hwnd,
            lpVerb: PCSTR((id - 1) as usize as *const u8),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: pt,
            ..Default::default()
        };
        let before = top_windows();
        unsafe { cm.InvokeCommand(&ci as *const _ as *const CMINVOKECOMMANDINFO)?; }
        keep_on_screen(before);
        Ok(())
    }

    pub fn show(h: *mut c_void, paths: &[PathBuf], extra: &[(u32, &str)]) -> Result<Option<u32>> {
        unsafe {
            let hwnd = HWND(h);
            let cm: IContextMenu = ui(hwnd, paths)?;
            let menu = CreatePopupMenu()?;
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok()?;
            hide_system_zip(&cm, menu);
            hide_shell_pins(&cm, menu);
            if !extra.is_empty() {
                let _ = InsertMenuW(menu, 0, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
                for (id, text) in extra.iter().rev() {
                    let t: Vec<u16> = text.encode_utf16().chain([0]).collect();
                    let _ = InsertMenuW(menu, 0, MF_BYPOSITION | MF_STRING, *id as usize, PCWSTR(t.as_ptr()));
                }
            }
            CM2.set(cm.cast::<IContextMenu2>().ok());
            let mut pt = POINT::default();
            GetCursorPos(&mut pt)?;
            let items = model(&cm, menu);
            let pick = crate::ctxmenu::show(h, items, (pt.x, pt.y));
            CM2.set(None);
            let id = match pick { crate::ctxmenu::Pick::Id(id) => id, _ => 0 };
            if id == crate::ctxmenu::MORE {
                CM2.set(cm.cast::<IContextMenu2>().ok());
                let id = TrackPopupMenuEx(menu, TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0, pt.x, pt.y, hwnd, None).0;
                CM2.set(None);
                if let Some(e) = extra.iter().find(|e| e.0 as i32 == id) {
                    DestroyMenu(menu)?;
                    return Ok(Some(e.0));
                }
                if id > 0 { invoke(&cm, hwnd, id, pt)?; }
                DestroyMenu(menu)?;
                return Ok(None);
            }
            if extra.iter().any(|e| e.0 == id) {
                DestroyMenu(menu)?;
                return Ok(Some(id));
            }
            if id > 0 {
                let mut verb = [0u16; 64];
                let delete = cm.GetCommandString(id as usize - 1, GCS_VERBW, None, PSTR(verb.as_mut_ptr() as *mut u8), verb.len() as u32).is_ok() && {
                    let k = verb.iter().position(|&c| c == 0).unwrap_or(0);
                    String::from_utf16_lossy(&verb[..k]).eq_ignore_ascii_case("delete")
                };
                if delete && !super::cur_virtual() {
                    let _ = super::fileops::delete(h, paths.to_vec(), false);
                    DestroyMenu(menu)?;
                    return Ok(None);
                }
                invoke(&cm, hwnd, id as i32, pt)?;
            }
            DestroyMenu(menu)?;
            Ok(None)
        }
    }

    #[cfg(test)]
    pub fn menu_dump(paths: &[PathBuf]) -> Vec<String> {
        use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetMenuStringW, GetSubMenu, MF_BYPOSITION};
        fn walk(m: HMENU, depth: usize, out: &mut Vec<String>) {
            unsafe {
                for i in 0..GetMenuItemCount(Some(m)).max(0) as u32 {
                    let mut b = [0u16; 256];
                    let n = GetMenuStringW(m, i, Some(&mut b), MF_BYPOSITION);
                    out.push(format!("{}{} (id {})", "  ".repeat(depth), String::from_utf16_lossy(&b[..n.max(0) as usize]), GetMenuItemID(m, i as i32)));
                    let sub = GetSubMenu(m, i as i32);
                    if !sub.is_invalid() { walk(sub, depth + 1, out); }
                }
            }
        }
        let mut out = vec![];
        unsafe {
            let cm: IContextMenu = ui(HWND::default(), paths).unwrap();
            let menu = CreatePopupMenu().unwrap();
            cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok().unwrap();
            hide_system_zip(&cm, menu);
            walk(menu, 0, &mut out);
        }
        out
    }

    /// Starts an OLE drag of `paths` (blocks until dropped).
    pub fn drag(h: *mut c_void, paths: &[PathBuf]) -> Result<()> {
        unsafe {
            let hwnd = HWND(h);
            let data: IDataObject = ui(hwnd, paths)?;
            SHDoDragDrop(Some(hwnd), &data, None, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK)?;
            Ok(())
        }
    }

    fn hdrop_paths(o: &IDataObject) -> Result<Vec<PathBuf>> {
        unsafe {
            let fmt = FORMATETC { cfFormat: CF_HDROP.0, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0 as u32, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
            let mut med = o.GetData(&fmt)?;
            let hd = HDROP(med.u.hGlobal.0);
            let mut out = vec![];
            for i in 0..DragQueryFileW(hd, u32::MAX, None) {
                let mut buf = vec![0u16; DragQueryFileW(hd, i, None) as usize + 1];
                let n = DragQueryFileW(hd, i, Some(&mut buf)) as usize;
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
            }
            ReleaseStgMedium(&mut med);
            Ok(out)
        }
    }

    #[implement(IDropTarget)]
    struct DropT { main: isize, paths: Mutex<Vec<PathBuf>>, to: Mutex<Option<PathBuf>> }

    fn place(main: isize, pt: &POINTL) -> Option<PathBuf> {
        match super::drop_place_at(main as *mut core::ffi::c_void, pt.x, pt.y) {
            super::DropPlace::Folder(p) => Some(p),
            super::DropPlace::Here => Some(super::cur_dir()),
            super::DropPlace::No => None,
        }
    }

    fn effect(k: MODIFIERKEYS_FLAGS, from: &[PathBuf], to: Option<&PathBuf>) -> DROPEFFECT {
        let Some(to) = to else { return DROPEFFECT_NONE };
        if to.to_string_lossy().starts_with("::") || super::in_zip() { return DROPEFFECT_NONE; }
        if from.iter().any(|p| to.starts_with(p)) { return DROPEFFECT_NONE; } // a folder into itself
        let volume = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
        // like Explorer: same drive moves, other drive copies; Ctrl copies, Shift moves
        if k.0 & 8 != 0 { DROPEFFECT_COPY }
        else if k.0 & 4 != 0 { DROPEFFECT_MOVE }
        else if from.first().and_then(|p| volume(p)) == volume(to) { DROPEFFECT_MOVE }
        else { DROPEFFECT_COPY }
    }

    impl IDropTarget_Impl for DropT_Impl {
        fn DragEnter(&self, o: Ref<'_, IDataObject>, k: MODIFIERKEYS_FLAGS, pt: &POINTL, e: *mut DROPEFFECT) -> Result<()> {
            let paths = match o.ok() { Ok(o) => hdrop_paths(&o).unwrap_or_default(), Err(_) => Vec::new() };
            let to = place(self.main, pt);
            *self.paths.lock().unwrap() = paths.clone();
            *self.to.lock().unwrap() = to.clone();
            unsafe { *e = effect(k, &paths, to.as_ref()) };
            Ok(())
        }
        fn DragOver(&self, k: MODIFIERKEYS_FLAGS, pt: &POINTL, e: *mut DROPEFFECT) -> Result<()> {
            let to = place(self.main, pt);
            *self.to.lock().unwrap() = to.clone();
            let paths = self.paths.lock().unwrap().clone();
            unsafe { *e = effect(k, &paths, to.as_ref()) };
            let d = super::drag_scroll_at(self.main as *mut core::ffi::c_void, pt.x, pt.y);
            super::DRAG_DIR.store(d, std::sync::atomic::Ordering::Relaxed);
            unsafe {
                let h = HWND(self.main as *mut core::ffi::c_void);
                if d != 0 { SetTimer(Some(h), 11, 70, None); } else { let _ = KillTimer(Some(h), 11); }
            }
            Ok(())
        }
        fn DragLeave(&self) -> Result<()> {
            *self.to.lock().unwrap() = None;
            super::drag_scroll_stop(self.main as *mut core::ffi::c_void);
            Ok(())
        }
        fn Drop(&self, o: Ref<'_, IDataObject>, k: MODIFIERKEYS_FLAGS, pt: &POINTL, e: *mut DROPEFFECT) -> Result<()> {
            if super::desk::dragging() {
                // desktop icons dropped back on the desktop: a move of the icons, not of the files (the dragger finishes it)
                super::desk::dropped_here((pt.x, pt.y));
                unsafe { *e = DROPEFFECT_NONE };
                return Ok(());
            }
            let paths = hdrop_paths(o.ok()?)?;
            let to = place(self.main, pt);
            let eff = effect(k, &paths, to.as_ref());
            unsafe { *e = eff };
            super::drag_scroll_stop(self.main as *mut core::ffi::c_void);
            let Some(to) = to.filter(|_| eff != DROPEFFECT_NONE) else { return Ok(()) };
            let copy = eff == DROPEFFECT_COPY;
            if super::jobs::writable(&to) {
                super::fileops::submit(self.main as *mut core::ffi::c_void, if copy { super::fileops::Kind::Copy } else { super::fileops::Kind::Move }, paths, to);
            } else {
                let _ = fileop(self.main as *mut core::ffi::c_void, if copy { Op::Copy } else { Op::Move }, &paths, &to);
                unsafe { let _ = PostMessageW(Some(HWND(self.main as *mut core::ffi::c_void)), WM_APP, WPARAM(0), LPARAM(0)); }
            }
            Ok(())
        }
    }

    pub fn register_drop(list: *mut c_void, main: isize) -> Result<()> {
        let t: IDropTarget = DropT { main, paths: Mutex::new(vec![]), to: Mutex::new(None) }.into();
        unsafe { RegisterDragDrop(HWND(list), &t) }
    }
}


/// The desktop's own logic: icons stay where the user puts them (positions saved per name), dropping one onto a folder
/// icon moves it into the folder, and a host window for Wallpaper Engine (it draws behind the icons).
mod desk {
    #![allow(unsafe_op_in_unsafe_fn)]
    use super::*;
    use std::{cell::Cell, collections::{HashMap, HashSet}};
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, HDC, ScreenToClient};

    pub const SPAWN_WORKERW: u32 = 0x052C; // what wallpaper apps send to "Progman" to get a window behind the icons
    const KEY: u32 = 0x0001_0001; // rgb(1, 0, 1): the list's background colour, made see-through while a live wallpaper runs

    thread_local! {
        static DRAG_FROM: Cell<Option<(i32, i32)>> = const { Cell::new(None) }; // our own icon drag: where it started (list coords)
        static DROP_AT: Cell<Option<(i32, i32)>> = const { Cell::new(None) }; // ...and where it was dropped (screen coords)
        static WORKERW: Cell<isize> = const { Cell::new(0) };
        static LIVE: Cell<bool> = const { Cell::new(false) }; // a live wallpaper is running in WORKERW
    }

    /// The live-wallpaper window follows the desktop's size (monitors added, removed or resized).
    pub unsafe fn size_workerw(cw: i32, ch: i32) {
        let ww = WORKERW.get() as HWND;
        if !ww.is_null() && IsWindow(ww) != 0 {
            MoveWindow(ww, 0, 0, cw, ch, 1);
        }
    }

    pub fn pos_file() -> PathBuf {
        PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join(r"rust-explorer\desktop-icons.txt")
    }

    /// Saved icon positions: name -> (x, y) in list pixels.
    fn saved() -> HashMap<String, (i32, i32)> {
        fs::read_to_string(pos_file()).unwrap_or_default().lines().filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some((p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.to_string()))
        }).map(|(x, y, n): (i32, i32, String)| (n, (x, y))).collect()
    }

    fn names() -> Vec<String> { ST.with_borrow(|st| st.rows.iter().map(|r| r.name.clone()).collect()) }

    fn pos(list: HWND, i: usize) -> (i32, i32) {
        let mut p = POINT { x: 0, y: 0 };
        put(list, LVM_GETITEMPOSITION, i, &mut p as *mut _ as isize);
        (p.x, p.y)
    }

    fn set_pos(list: HWND, i: usize, (x, y): (i32, i32)) {
        let p = POINT { x, y };
        put(list, LVM_SETITEMPOSITION32, i, &p as *const _ as isize);
    }

    /// Writes every icon's position (after a move or a fresh layout).
    pub fn save(list: HWND) {
        let lines: String = names().iter().enumerate().map(|(i, n)| { let (x, y) = pos(list, i); format!("{x}\t{y}\t{n}\n") }).collect();
        let f = pos_file();
        let _ = fs::create_dir_all(f.parent().unwrap_or(&f));
        let _ = fs::write(f, lines);
    }

    /// The icon grid: cell size, and how many columns / rows fit.
    struct Grid { cx: i32, cy: i32, cols: i32, rows: i32 }

    impl Grid {
        fn of(list: HWND) -> Grid {
            let s = put(list, LVM_GETITEMSPACING, 0, 0) as u32;
            let (cx, cy) = (((s & 0xFFFF) as i32).max(1), ((s >> 16) as i32).max(1));
            let mut r: RECT = unsafe { std::mem::zeroed() };
            unsafe { GetClientRect(list, &mut r) };
            if r.bottom <= 0 {
                // the first fill runs in WM_CREATE, before WM_SIZE gives the list its size: the size it is about to get
                r = unsafe { RECT { left: 0, top: 0, right: GetSystemMetrics(SM_CXSCREEN), bottom: GetSystemMetrics(SM_CYSCREEN) - desk_top() - desk_bottom() } };
            }
            Grid { cx, cy, cols: (r.right / cx).max(1), rows: (r.bottom / cy).max(1) }
        }
        /// Nearest cell to a position; None if it is off the grid (screen got smaller).
        fn cell(&self, (x, y): (i32, i32)) -> Option<(i32, i32)> {
            let c = ((x + self.cx / 2).div_euclid(self.cx), (y + self.cy / 2).div_euclid(self.cy));
            (c.0 >= 0 && c.1 >= 0 && c.0 < self.cols && c.1 < self.rows).then_some(c)
        }
        fn at(&self, (c, r): (i32, i32)) -> (i32, i32) { (c * self.cx, r * self.cy) }
        /// Free cell nearest to `want`; with no wish the first free one going down the columns, like Windows.
        fn free(&self, taken: &HashSet<(i32, i32)>, want: Option<(i32, i32)>) -> (i32, i32) {
            let all = (0..self.cols).flat_map(|c| (0..self.rows).map(move |r| (c, r))).filter(|c| !taken.contains(c));
            match want {
                Some((wc, wr)) => all.min_by_key(|&(c, r)| (c - wc).pow(2) + (r - wr).pow(2)),
                None => all.min(),
            }
            .unwrap_or((0, 0))
        }
    }

    /// Fills the (non-virtual) desktop list with `n` rows and puts every icon on its saved cell, new ones on free cells.
    pub fn fill(list: HWND, n: usize) {
        for i in 0..n {
            let mut it: LVITEMW = unsafe { std::mem::zeroed() };
            it.mask = LVIF_TEXT | LVIF_IMAGE;
            it.iItem = i as i32;
            it.pszText = usize::MAX as *mut u16; // LPSTR_TEXTCALLBACKW: text and icon come from LVN_GETDISPINFO
            it.iImage = -1; // I_IMAGECALLBACK
            put(list, LVM_INSERTITEMW, 0, &it as *const _ as isize);
        }
        let (g, old, names) = (Grid::of(list), saved(), names());
        let mut taken = HashSet::new();
        let mut later = vec![];
        for (i, name) in names.iter().enumerate() {
            match old.get(name).and_then(|&p| g.cell(p)).filter(|c| !taken.contains(c)) {
                Some(c) => { taken.insert(c); set_pos(list, i, g.at(c)); }
                None => later.push(i),
            }
        }
        for i in later {
            let c = g.free(&taken, None);
            taken.insert(c);
            set_pos(list, i, g.at(c));
        }
        save(list);
    }

    // ---------- moving icons ----------

    /// Our own drag of desktop icons starts (`from` in list coordinates).
    pub fn drag_start(from: (i32, i32)) { DRAG_FROM.set(Some(from)); DROP_AT.set(None); }
    pub fn dragging() -> bool { DRAG_FROM.get().is_some() }
    /// Called by the drop target: the drag ended on the desktop itself at screen point `at`.
    pub fn dropped_here(at: (i32, i32)) { DROP_AT.set(Some(at)); }

    /// After the drag: on a folder icon (not one of the dragged) the selection moves into that folder; anywhere else the
    /// icons move by the drag distance, snapped to the grid, each to the nearest free cell. true = handled here.
    pub fn drag_end(h: HWND, list: HWND) -> bool {
        let Some(from) = DRAG_FROM.take() else { return false };
        // normally our drop target recorded where it ended; over a live wallpaper OLE never reaches it (the wallpaper window of
        // another process is under the pointer), so then: the pointer is over our desktop window = the drop was on the desktop
        let at = DROP_AT.take().or_else(|| unsafe {
            let mut c = POINT { x: 0, y: 0 };
            GetCursorPos(&mut c);
            (GetAncestor(WindowFromPoint(c), GA_ROOT) == h).then_some((c.x, c.y))
        });
        let Some(at) = at else { return false };
        let mut pt = POINT { x: at.0, y: at.1 };
        unsafe { ScreenToClient(list, &mut pt) };
        let (_, dir, paths) = selected();
        let mut sel = vec![];
        let mut i = -1isize;
        loop {
            i = put(list, LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            if i < 0 { break; }
            sel.push(i as usize);
        }
        // onto a folder icon
        let mut ht: LVHITTESTINFO = unsafe { std::mem::zeroed() };
        ht.pt = pt;
        let tgt = put(list, LVM_HITTEST, 0, &mut ht as *mut _ as isize);
        if tgt >= 0 && !sel.contains(&(tgt as usize)) {
            let target = ST.with_borrow(|st| st.rows.get(tgt as usize).filter(|r| r.dir).map(|r| st.dir.join(&r.name)));
            if let Some(t) = target {
                let _ = sh::fileop(h as _, sh::Op::Move, &paths, &t);
                let _ = dir;
                unsafe { PostMessageW(h, WM_APP, 0, 0) }; // reload: the moved icons are gone from here
                return true;
            }
        }
        let g = Grid::of(list);
        let n = put(list, LVM_GETITEMCOUNT, 0, 0).max(0) as usize;
        let mut taken: HashSet<(i32, i32)> = (0..n).filter(|i| !sel.contains(i)).filter_map(|i| g.cell(pos(list, i))).collect();
        let (dx, dy) = (pt.x - from.0, pt.y - from.1);
        for &i in &sel {
            let (x, y) = pos(list, i);
            let want = g.cell((x + dx, y + dy)).unwrap_or_else(|| {
                // dragged off the grid: the nearest edge cell
                (((x + dx) / g.cx).clamp(0, g.cols - 1), ((y + dy) / g.cy).clamp(0, g.rows - 1))
            });
            let c = if taken.contains(&want) { g.free(&taken, Some(want)) } else { want };
            taken.insert(c);
            set_pos(list, i, g.at(c));
        }
        save(list);
        true
    }

    // ---------- Wallpaper Engine (and other live wallpapers) ----------
    //
    // They look for the "Progman" window (that is our desktop's class), send it 0x052C and put their window into the
    // "WorkerW" that appears: Windows 11 24H2 has it as a child of Progman, under the icons. Ours is the same, and while it
    // holds a wallpaper the icon list turns see-through (colour key) so the wallpaper shows between the icons.

    pub unsafe fn spawn_workerw(h: HWND) {
        if WORKERW.get() != 0 && IsWindow(WORKERW.get() as HWND) != 0 { return; }
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(h, &mut r);
        let ww = CreateWindowExW(0, w("WorkerW").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE, 0, 0, r.right, r.bottom, h, 0 as _, GetModuleHandleW(ptr::null()), ptr::null());
        SetWindowPos(ww, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE); // under the icon list
        WORKERW.set(ww as isize);
    }

    /// Every couple of seconds: a wallpaper window inside WorkerW = live mode on, none = back to the picture.
    pub unsafe fn check_live(list: HWND) {
        let ww = WORKERW.get() as HWND;
        let live = !ww.is_null() && IsWindow(ww) != 0 && !GetWindow(ww, GW_CHILD).is_null();
        if live == LIVE.replace(live) { return; }
        // the list stays "transparent" (its background is what the desktop paints: the key colour while live, see erase()),
        // so it keeps drawing the label shadows; the colour key then cuts the background out
        let ex = GetWindowLongPtrW(list, GWL_EXSTYLE);
        if live {
            SetWindowLongPtrW(list, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
            SetLayeredWindowAttributes(list, KEY, 0, LWA_COLORKEY);
        } else {
            SetWindowLongPtrW(list, GWL_EXSTYLE, ex & !(WS_EX_LAYERED as isize));
        }
        InvalidateRect(list, ptr::null(), 1);
    }

    /// The desktop's background: the key colour while a live wallpaper shows through, else the picture. false = none to paint.
    pub unsafe fn erase(dc: HDC, h: HWND) -> bool {
        let (bmp, cw, ch) = WALL.get();
        if LIVE.get() {
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(h, &mut r);
            let b = CreateSolidBrush(KEY);
            FillRect(dc, &r, b);
            DeleteObject(b);
        } else if bmp != 0 {
            wall::paint(dc, bmp, cw, ch);
        } else {
            return false;
        }
        true
    }

    /// WorkerW without a wallpaper in it shows the picture wallpaper, like the desktop behind it.
    pub unsafe extern "system" fn workerw_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if m == WM_ERASEBKGND {
            let (bmp, cw, ch) = WALL.get();
            if bmp != 0 { wall::paint(wp as _, bmp, cw, ch); } else { let mut r: RECT = std::mem::zeroed(); GetClientRect(h, &mut r); FillRect(wp as _, &r, windows_sys::Win32::Graphics::Gdi::GetSysColorBrush(1) as _); }
            return 1;
        }
        DefWindowProcW(h, m, wp, lp)
    }

    #[cfg(test)]
    mod tests {
        use super::Grid;
        use std::collections::HashSet;

        #[test]
        fn grid_cells() {
            let g = Grid { cx: 100, cy: 90, cols: 5, rows: 4 };
            assert_eq!(g.cell((0, 0)), Some((0, 0)));
            assert_eq!(g.cell((149, 44)), Some((1, 0))); // rounds to the nearest cell
            assert_eq!(g.cell((900, 0)), None); // off the grid
            let mut taken: HashSet<_> = [(0, 0), (0, 1)].into();
            assert_eq!(g.free(&taken, None), (0, 2)); // down the first column first
            taken.insert((2, 2));
            assert_eq!(g.free(&taken, Some((2, 2))), (1, 2)); // nearest free to the wish (ties: lowest)
        }
    }
}

/// Trace of the icon drags (%TEMP%\rx-drag.log), to find out where one stops working. Cheap: a few lines per drag.
fn drag_log(s: &str) {
    use std::io::Write;
    let p = std::env::temp_dir().join("rx-drag.log");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(p) { let _ = writeln!(f, "{s}"); }
}

/// Ctrl+Backspace in an edit box (it arrives as WM_CHAR 0x7F): deletes back to the start of the previous word.
unsafe fn delete_word(h: HWND) {
    unsafe {
        let (mut a, mut b) = (0u32, 0u32);
        SendMessageW(h, EM_GETSEL, &mut a as *mut u32 as usize, &mut b as *mut u32 as isize);
        if a == b {
            let n = GetWindowTextLengthW(h) as usize;
            let mut buf = vec![0u16; n + 1];
            GetWindowTextW(h, buf.as_mut_ptr(), n as i32 + 1);
            let mut i = a as usize;
            while i > 0 && buf[i - 1] == b' ' as u16 { i -= 1; }
            while i > 0 && buf[i - 1] != b' ' as u16 { i -= 1; }
            SendMessageW(h, EM_SETSEL, i, a as isize);
        }
        SendMessageW(h, EM_REPLACESEL, 1, w("").as_ptr() as isize);
    }
}

/// Rename box subclass: only Ctrl+Backspace is ours.
unsafe extern "system" fn label_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if m == WM_CHAR && wp == 0x7F { delete_word(h); return 0; }
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(LABEL_OLD.load(std::sync::atomic::Ordering::Relaxed)), h, m, wp, lp)
    }
}

/// Address bar subclass: Enter / Esc go to the main window, everything else is the normal edit control.
unsafe extern "system" fn edit_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if matches!(wp, 13 | 27) && matches!(m, WM_KEYDOWN | WM_CHAR) {
            if m == WM_KEYDOWN { PostMessageW(GetAncestor(h, GA_ROOT), WM_GO, wp, 0); }
            return 0; // WM_CHAR swallowed so there is no beep
        }
        if m == WM_CHAR && wp == 0x7F { delete_word(h); return 0; }
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(EDIT_OLD.load(std::sync::atomic::Ordering::Relaxed)), h, m, wp, lp)
    }
}

/// Search box subclass: Enter runs the search, Esc clears it; the rest is the normal edit control.
unsafe extern "system" fn search_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if matches!(wp, 13 | 27) && matches!(m, WM_KEYDOWN | WM_CHAR) {
            if m == WM_KEYDOWN { PostMessageW(GetAncestor(h, GA_ROOT), WM_SEARCH, wp, 0); }
            return 0;
        }
        if m == WM_CHAR && wp == 0x7F { delete_word(h); return 0; }
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(SEARCH_OLD.load(std::sync::atomic::Ordering::Relaxed)), h, m, wp, lp)
    }
}

fn edit_text(h: HWND) -> String {
    let n = unsafe { GetWindowTextLengthW(h) }.max(0) as usize;
    let mut buf = vec![0u16; n + 1];
    let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

unsafe extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if m == winrust_common::style::msg() {
            // the topbar switched the WinRust style: take it now (the desktop's list keeps its see-through colours)
            winrust_common::style::reload();
            if desk() { InvalidateRect(h, ptr::null(), 1); } else { ui::restyle(h); }
            return 0;
        }
        match m {
            WM_CREATE => {
                let inst = GetModuleHandleW(ptr::null());
                // desktop mode has no tab bar / address bar: the null handles are harmless to every SendMessage below
                let (mut tab, mut edit, mut search) = (0 as HWND, 0 as HWND, 0 as HWND);
                if !desk() {
                    ui::dark_title(h);
                    // the chrome (tabs, toolbar, address pill) is one painted window; the address and search boxes are real edits
                    tab = CreateWindowExW(0, w("RxChrome").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    ui::nav_init(CreateWindowExW(0, w("RxNav").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE, 0, 0, 0, 0, h, 0 as _, inst, ptr::null()));
                    edit = CreateWindowExW(0, w("EDIT").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 0, 0, tab, 0 as _, inst, ptr::null());
                    put(edit, WM_SETFONT, ui::font(13, 400, ui::ui_face()) as usize, 1);
                    EDIT_OLD.store(SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize), std::sync::atomic::Ordering::Relaxed);
                    SHAutoComplete(edit, SHACF_FILESYSTEM | SHACF_AUTOSUGGEST_FORCE_ON);
                    search = CreateWindowExW(0, w("EDIT").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | ES_AUTOHSCROLL as u32, 0, 0, 0, 0, tab, 0 as _, inst, ptr::null());
                    put(search, WM_SETFONT, ui::font(13, 400, ui::ui_face()) as usize, 1);
                    SEARCH_OLD.store(SetWindowLongPtrW(search, GWLP_WNDPROC, search_proc as *const () as isize), std::sync::atomic::Ordering::Relaxed);
                    let cue = t!("Поиск", "Search", ja: "検索", pt: "Pesquisar", zh: "搜索");
                    SendMessageW(search, 0x1501, 1, w(&cue).as_ptr() as isize); // EM_SETCUEBANNER
                }
                let view = if desk() { LVS_ICON } else { LVS_REPORT };
                // real items, not a virtual list: the control lays icon views out only for items it owns (fill_items)
                let list = CreateWindowExW(0, w("SysListView32").as_ptr(), ptr::null(), WS_CHILD | WS_VISIBLE | view | LVS_EDITLABELS | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                put(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_INFOTIP) as isize);
                let mut fi: SHFILEINFOW = std::mem::zeroed();
                let il = SHGetFileInfoW(w("C:\\").as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_SMALLICON);
                put(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, il as isize);
                if desk() {
                    // system large icons; indexes are shared with the small list, so icon_of() works unchanged
                    let big = SHGetFileInfoW(w("C:\\").as_ptr(), 0, &mut fi, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_LARGEICON);
                    put(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, big as isize);
                    apply_icon_size(list);
                    // transparent background (the desktop paints it) and shadowed labels, readable on light wallpapers too
                    let ex = LVS_EX_TRANSPARENTBKGND | 0x0080_0000; // LVS_EX_TRANSPARENTSHADOWTEXT
                    put(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
                    for msg in [LVM_SETBKCOLOR, LVM_SETTEXTBKCOLOR] { put(list, msg, 0, CLR_NONE); }
                    put(list, LVM_SETTEXTCOLOR, 0, 0x00FF_FFFF);
                    reload_wall();
                    // grey (not ClearType) smoothing: on a live wallpaper the labels sit on a colour key, where ClearType leaves coloured fringes
                    put(list, WM_SETFONT, windows_sys::Win32::Graphics::Gdi::CreateFontW(-ui::sc(13), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 4, 0, w(ui::ui_face()).as_ptr()) as usize, 1);
                    // like explorer's desktop: wallpaper apps look for a SHELLDLL_DefView under Progman
                    CreateWindowExW(0, w("SHELLDLL_DefView").as_ptr(), ptr::null(), WS_CHILD, 0, 0, 0, 0, h, 0 as _, inst, ptr::null());
                    SetTimer(h, 7, 2000, None); // live wallpaper on / off (desk::check_live)
                } else {
                    let himl = ImageList_Create(THUMB, THUMB, ILC_COLOR32, 64, 64);
                    put(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, himl as isize);
                    put(list, LVM_SETICONSPACING, 0, ((THUMB as isize + 34) << 16) | (THUMB as isize + 24));
                    let _ = TX.set(sh::start_thumbs(h as isize));
                    ST.with_borrow_mut(|st| { st.himl = himl as isize; st.iml_px = THUMB; });
                    // the same columns, in the same order, as Explorer's details view
                    col(list, 0, t!("Имя", "Name", ja: "名前", pt: "Nome", zh: "名称"), ui::sc(560), false);
                    col(list, 1, t!("Дата изменения", "Date modified", ja: "更新日時", pt: "Data de modificação", zh: "修改日期"), ui::sc(160), false);
                    col(list, 2, t!("Тип", "Type", ja: "種類", pt: "Tipo", zh: "类型"), ui::sc(140), false);
                    col(list, 3, t!("Размер", "Size", ja: "サイズ", pt: "Tamanho", zh: "大小"), ui::sc(96), true);
                    col(list, 4, "", ui::sc(100), false); // blank strip after the real columns
                    ui::style_list(list);
                }
                ST.with_borrow_mut(|st| { st.list = list; st.tab = tab; st.edit = edit; st.search = search; st.tabs = vec![PathBuf::new()]; });
                if !desk() { ui::place_edit(tab); }
                let _ = sh::register_drop(list as _, h as isize);
                if !desk() {
                    let _ = sh::register_drop(tab as _, h as isize); // tabs, address pill
                    let nav = ui::nav_hwnd();
                    if !nav.is_null() { let _ = sh::register_drop(nav as _, h as isize); }
                    let _ = sh::register_drop(h as _, h as isize); // status strip / window edges
                }
                if desk() && icons_hidden() { ShowWindow(list, SW_HIDE); }
                // live wallpaper: the list is see-through (colour key), so over empty desktop OLE finds the wallpaper window under it,
                // and walks up from there to the desktop window: it must accept the drop too
                if desk() { let _ = sh::register_drop(h as _, h as isize); }
                let (start, pick) = if desk() { (sh::desktop_dirs().into_iter().next().unwrap_or_default(), None) } else { start_target() };
                load(h, start);
                if let Some(name) = pick { select_row(&name); }
                if desk() {
                    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos};
                    let mut dirs: Vec<PathBuf> = [&FOLDERID_Desktop, &FOLDERID_Downloads, &FOLDERID_Documents, &FOLDERID_Pictures, &FOLDERID_Music, &FOLDERID_Videos].iter().filter_map(|id| sh::known_dir(id)).collect();
                    dirs.extend(ui::pins());
                    shellwin::register_resident(&dirs);
                } else {
                    shellwin::navigated(&cur_dir());
                    shellwin::register(h as isize);
                }
                0
            }
            // the bar came, went or changed size (it sets the work area), or the screen changed: the icon list follows
            WM_SETTINGCHANGE | WM_DISPLAYCHANGE if desk() => {
                if m == WM_DISPLAYCHANGE {
                    reload_wall(); // monitors came, went or changed resolution: each one gets its picture again
                    let (vx, vy, vw, vh) = virt();
                    SetWindowPos(h, HWND_BOTTOM, vx, vy, vw, vh, SWP_NOACTIVATE); // WM_SIZE lays out
                    InvalidateRect(h, ptr::null(), 1);
                } else if wp == SPI_SETWORKAREA as usize {
                    place_desk_list(ST.with_borrow(|st| st.list));
                }
                0
            }
            WM_SETTINGCHANGE if !desk() && lp != 0 && wstr(lp as *const u16).eq_ignore_ascii_case("ImmersiveColorSet") => { ui::theme_invalidate(); ui::restyle(h); 0 }
            WM_THEMECHANGED if !desk() => { ui::theme_invalidate(); ui::restyle(h); 0 }
            // mouse buttons 4 / 5 (and keyboard media keys) arrive as APPCOMMAND_BROWSER_BACKWARD / _FORWARD
            0x0319 if !desk() && matches!((lp >> 16) & 0x7FF, 1 | 2) => { nav(h, (lp >> 16) & 0x7FF == 1); 1 }
            // the caption is ours (the tab row): no band at the top at all — restored, the client starts at the window edge;
            // maximized, it is the work area itself. The top edge stays resizable through the chrome's hit test.
            WM_NCCALCSIZE if !desk() && wp != 0 => {
                let p = lp as *mut NCCALCSIZE_PARAMS;
                if IsZoomed(h) != 0 {
                    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
                    let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
                    let mut mi: MONITORINFO = std::mem::zeroed();
                    mi.cbSize = size_of::<MONITORINFO>() as u32;
                    if GetMonitorInfoW(mon, &mut mi) != 0 { (*p).rgrc[0] = mi.rcWork; return 0; }
                }
                let top = (*p).rgrc[0].top;
                let r = DefWindowProcW(h, m, wp, lp);
                (*p).rgrc[0].top = top;
                r
            }
            // the window has no title bar: the top edge is client, so the parent answers the chrome's pass-through there
            WM_NCHITTEST if !desk() => {
                let mut c = POINT { x: (lp & 0xffff) as u16 as i16 as i32, y: ((lp >> 16) & 0xffff) as u16 as i16 as i32 };
                ScreenToClient(h, &mut c);
                if c.y >= 0 && c.y < ui::sc(5) { return HTTOP as LRESULT; }
                DefWindowProcW(h, m, wp, lp)
            }
            WM_SIZE => {
                let list = ST.with_borrow(|st| st.list);
                let (cw, ch) = ((lp & 0xFFFF) as i32, ((lp >> 16) & 0xFFFF) as i32);
                if desk() {
                    place_desk_list(list);
                    desk::size_workerw(cw, ch);
                } else {
                    ui::layout_children(cw, ch);
                    InvalidateRect(h, ptr::null(), 0);
                }
                0
            }
            WM_PAINT if !desk() => { ui::paint_status(h); 0 }
            ui::WM_UI => {
                match wp {
                    1 => { let n = ST.with_borrow(|st| st.tabs.len()); if (lp as usize) < n { tab_select(h, lp as usize); } }
                    2 => tab_close_at(h, lp as usize),
                    3 => tab_new(),
                    4 => nav(h, true),
                    5 => nav(h, false),
                    6 => go_up(h),
                    7 => refresh_after_ops(h),
                    8 => toggle_view(h),
                    9 => search_focus(),
                    10..=15 => {
                        let (list, dir, paths) = selected();
                        match wp {
                            10 | 11 if !paths.is_empty() && (!cur_virtual() || ui::searching()) => match real_paths(&paths) { Ok(p) => { let _ = sh::clip_set(h, &p, wp == 10 && !in_zip()); } Err(e) => warn(h, &e) },
                            12 => { paste(h, &dir); refresh_after_ops(h); }
                            13 if !in_zip() && (!cur_virtual() || ui::searching()) => {
                                let i = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
                                if i >= 0 { SetFocus(list); put(list, LVM_EDITLABELW, i as usize, 0); }
                            }
                            14 if !paths.is_empty() && !in_zip() && (!cur_virtual() || ui::searching()) => { let _ = fileops::delete(h, paths, false); }
                            15 if !cur_virtual() => create_new(h, &dir, t!("Новая папка", "New folder", ja: "新しいフォルダー", pt: "Nova pasta", zh: "新建文件夹"), true),
                            _ => {}
                        }
                    }
                    16..=19 => {
                        let dir = cur_dir();
                        if cur_virtual() { return 0; }
                        match wp {
                            18 => create_new(h, &dir, t!("Новый текстовый документ.txt", "New text document.txt", ja: "新しいテキスト ドキュメント.txt", pt: "Novo documento de texto.txt", zh: "新建文本文档.txt"), false),
                            19 => new_zip(h, &dir),
                            _ => {}
                        }
                    }
                    23 => { // a folder from the back / forward history
                        let i = lp as usize;
                        if let Some(p) = ST.with_borrow(|st| st.hist.v.get(i).cloned()) {
                            ST.with_borrow_mut(|st| st.hist.pos = i);
                            NAV.store(true, Relaxed);
                            load(h, p);
                            NAV.store(false, Relaxed);
                        }
                    }
                    24..=31 => match wp {
                        29 => duplicate_tab(h, lp as usize),
                        30 => close_other_tabs(h, lp as usize),
                        31 => copy_tab_path(h, lp as usize),
                        _ => {}
                    },
                    33 => { // a folder from the navigation pane: a new tab there
                        if let Some(p) = ui::nav_path(lp as usize) {
                            let (tab, active) = ST.with_borrow_mut(|st| { st.tabs.push(p.clone()); st.active = st.tabs.len() - 1; (st.tab, st.active) });
                            tab_text(tab, active, "", true);
                            load(h, p);
                        }
                    }
                    34 => { if let Some(p) = ui::nav_path(lp as usize) { if let Ok(exe) = env::current_exe() { let _ = std::process::Command::new(exe).arg(p).spawn(); } } }
                    35 => { if let Some(p) = ui::nav_path(lp as usize) { if !clip_text(&p.to_string_lossy()) { warn(h, "Cannot open the clipboard"); } } }
                    56 => { // the pane splitter moved
                        let mut r: RECT = std::mem::zeroed();
                        GetClientRect(h, &mut r);
                        ui::layout_children(r.right, r.bottom);
                    }
                    40..=50 => match wp {
                        44 => select_all(h, false),
                        45 => select_all(h, true),
                        46 => copy_path(h),
                        47 => new_window(),
                        48 => { let (_, dir, sel) = selected(); properties(&sel.first().cloned().unwrap_or(dir)); }
                        _ => {}
                    },
                    60..=67 => sort_now(h, ((wp - 60) / 2) as usize, (wp - 60) % 2 == 0),
                    70..=74 => group_now(h, [0, 2, 1, 3, 4][(wp - 70) as usize]),
                    80..=86 => { if let Some(v) = ViewMode::from_action(wp) { set_view(h, v); } }
                    87 => zoom_view(h, lp > 0),
                    90 => { // View > Show > Navigation pane
                        NAV_ON.fetch_xor(true, Relaxed);
                        save_globals();
                        let mut r: RECT = std::mem::zeroed();
                        GetClientRect(h, &mut r);
                        ui::layout_children(r.right, r.bottom);
                        InvalidateRect(h, ptr::null(), 0);
                    }
                    93 => toggle_checkboxes(),
                    94 => { SHOW_EXT.fetch_xor(true, Relaxed); save_globals(); refresh_after_ops(h); }
                    95 => { SHOW_HIDDEN.fetch_xor(true, Relaxed); save_globals(); refresh_after_ops(h); }
                    96 => {
                        COMPACT.fetch_xor(true, Relaxed);
                        save_globals();
                        // the row height is measured again only by a view change; icon views are left alone
                        if ST.with_borrow(|st| st.view) == ViewMode::Details {
                            let (list, n) = ST.with_borrow(|st| (st.list, st.rows.len()));
                            ui::row_height(list, true);
                            put(list, LVM_SETVIEW, LV_VIEW_LIST as usize, 0);
                            put(list, LVM_SETVIEW, LV_VIEW_DETAILS as usize, 0);
                            fill_items();
                            put(list, LVM_REDRAWITEMS, 0, n as isize - 1);
                            InvalidateRect(list, ptr::null(), 0);
                        }
                    }
                    20 => { if let Some(p) = ui::nav_path(lp as usize) { load(h, p); } } // a folder in the left pane
                    21 => { if let Some(p) = ui::crumb_path(lp as usize) { load(h, p); } } // a part of the path in the address pill
                    _ => {}
                }
                0
            }
            WM_ERASEBKGND if desk() && desk::erase(wp as _, h) => 1,
            WM_WINDOWPOSCHANGING if desk() => { // the desktop never rises above other windows
                let wp = &mut *(lp as *mut WINDOWPOS);
                wp.hwndInsertAfter = HWND_BOTTOM;
                wp.flags |= SWP_NOACTIVATE;
                0
            }
            WM_INITMENUPOPUP if sh::menu_msg(m, wp, lp) => 0,
            WM_DRAWITEM | WM_MEASUREITEM if sh::menu_msg(m, wp, lp) => 1,
            WM_NOTIFY => {
                let nm = &*(lp as *const NMHDR);
                let list = ST.with_borrow(|st| st.list);
                match nm.code {
                    NM_CUSTOMDRAW if nm.hwndFrom == list && !desk() => return ui::list_draw(lp), // the rows are painted by hand
                    LVN_ITEMCHANGED | LVN_ODSTATECHANGED => ui::status_dirty(h), // selection count in the status line
                    // typing a letter jumps to the next row that starts with it (#81): a virtual list asks us to search
                    LVN_ODFINDITEMW if nm.hwndFrom == list => {
                        let f = &*(lp as *const NMLVFINDITEMW);
                        if f.lvfi.flags & (LVFI_STRING | LVFI_PARTIAL) == 0 || f.lvfi.psz.is_null() { return -1; }
                        let mut n = 0;
                        while *f.lvfi.psz.add(n) != 0 { n += 1; }
                        let q = String::from_utf16_lossy(std::slice::from_raw_parts(f.lvfi.psz, n)).to_lowercase();
                        return ST.with_borrow(|st| {
                            let len = st.rows.len();
                            let start = usize::try_from(f.iStart).unwrap_or(0);
                            (0..len).map(|k| (start + k) % len).find(|&i| !st.rows[i].hdr && st.rows[i].name.to_lowercase().starts_with(&q)).map_or(-1, |i| i as isize)
                        });
                    }
                    LVN_GETINFOTIPW => {
                        // the tooltip over a row: name, type and size (the date too when there is room)
                        let it = &mut *(lp as *mut NMLVGETINFOTIPW);
                        let row = ST.with_borrow(|st| usize::try_from(it.iItem).ok().and_then(|i| st.rows.get(i)).filter(|r| !r.hdr).map(|r| (r.name.clone(), r.dir, r.size, r.time)));
                        if let Some((name, dir, size, time)) = row {
                            let mut s = shown_name(&name, false, dir);
                            s.push_str("\r\n");
                            s.push_str(&type_text(&name, dir));
                            if !dir { s.push_str(&format!("  \u{00B7}  {}", size_text(size))); }
                            let date = date_text(time);
                            if !date.is_empty() { s.push_str(&format!("\r\n{date}")); }
                            let u: Vec<u16> = s.encode_utf16().take(it.cchTextMax.max(1) as usize - 1).collect();
                            ptr::copy_nonoverlapping(u.as_ptr(), it.pszText, u.len());
                            *it.pszText.add(u.len()) = 0;
                        }
                    }
                    LVN_GETDISPINFOW => {
                        let it = &mut (*(lp as *mut NMLVDISPINFOW)).item;
                        // a request that arrives while ST is borrowed is skipped; the list repaints later
                        ST.with(|c| {
                            let Ok(mut st) = c.try_borrow_mut() else { return };
                            let State { dir, rows, view, .. } = &mut *st;
                            let Some(i) = usize::try_from(it.iItem).ok() else { return };
                            let Some(r) = rows.get_mut(i) else { return };
                            let actual = r.origin.clone().or_else(|| r.target.clone()).unwrap_or_else(|| dir.join(&r.name));
                            let thumb_path = r.target.clone().unwrap_or_else(|| dir.join(&r.name));
                            if it.mask & LVIF_TEXT != 0 && it.cchTextMax > 0 {
                                let s = if r.hdr { if it.iSubItem == 0 { r.name[1..].to_string() } else { String::new() } } else { match it.iSubItem {
                                    0 => if r.target.is_some() { r.name.clone() } else { shown_name(&r.name, desk(), r.dir) },
                                    1 => date_text(r.time),
                                    2 => r.kind.clone().unwrap_or_else(|| type_text(&r.name, r.dir)),
                                    _ if r.dir && r.target.is_none() => String::new(),
                                    _ => size_text(r.size),
                                } };
                                let u: Vec<u16> = s.encode_utf16().take(it.cchTextMax as usize - 1).collect();
                                ptr::copy_nonoverlapping(u.as_ptr(), it.pszText, u.len());
                                *it.pszText.add(u.len()) = 0;
                            }
                            if r.hdr { if it.mask & LVIF_IMAGE != 0 { it.iImage = I_IMAGENONE; } return; }
                            if it.mask & LVIF_IMAGE != 0 && view.wants_thumbs() {
                                if r.thumb == -2 {
                                    r.thumb = -1;
                                    if let Some(tx) = TX.get() { let _ = tx.send((GEN.load(std::sync::atomic::Ordering::Relaxed), i, thumb_path, view.icon_px())); }
                                }
                                it.iImage = if r.thumb >= 0 { r.thumb } else { I_IMAGENONE };
                            } else if it.mask & LVIF_IMAGE != 0 {
                                if r.icon < 0 { r.icon = icon_of(&actual, r.dir); }
                                it.iImage = r.icon;
                            }
                        });
                    }
                    LVN_ITEMACTIVATE => {
                        let i = (*(lp as *const NMITEMACTIVATE)).iItem;
                        let hit = ST.with_borrow(|st| usize::try_from(i).ok().and_then(|i| st.rows.get(i)).filter(|r| !r.hdr).map(|r| (r.target.clone().unwrap_or_else(|| st.dir.join(&r.name)), r.dir)));
                        if let Some((p, is_dir)) = hit { activate(h, p, is_dir); }
                    }
                    NM_CLICK if nm.hwndFrom == list => {
                        let a = &*(lp as *const NMITEMACTIVATE);
                        let i = a.iItem;
                        // the item check box, at the head of the name column
                        if CHECK_BOXES.load(Relaxed) && ST.with_borrow(|st| st.view) == ViewMode::Details {
                            let mut r: RECT = std::mem::zeroed();
                            r.left = LVIR_BOUNDS as i32;
                            let ok = i >= 0 && put(list, LVM_GETITEMRECT, i as usize, &mut r as *mut _ as isize) != 0;
                            let inside = ok && a.ptAction.x >= r.left + ui::sc(6) && a.ptAction.x <= r.left + ui::sc(24);
                            if inside {
                                if let Some(name) = ST.with_borrow(|st| st.rows.get(i as usize).filter(|r| !r.hdr).map(|r| r.name.clone())) {
                                    tick(&name);
                                    InvalidateRect(list, ptr::null(), 0);
                                }
                                return 0;
                            }
                        }
                        // a group heading: fold its rows away, or bring them back
                        if let Some(label) = ST.with_borrow(|st| st.rows.get(i as usize).filter(|r| r.hdr).map(|r| r.name[1..].to_string())) {
                            toggle_group(h, &label);
                        }
                    }
                    NM_RCLICK if nm.hwndFrom == list => {
                        // like Explorer: right click selects the row under the pointer (an already selected one keeps the
                        // whole selection); empty space clears it and gets the folder's own menu
                        let pt = (*(lp as *const NMLISTVIEW)).ptAction;
                        let mut ht: LVHITTESTINFO = std::mem::zeroed();
                        ht.pt = pt;
                        let hit = put(list, LVM_HITTEST, 0, &mut ht as *mut _ as isize);
                        let row = usize::try_from(hit).ok().filter(|&i| ST.with_borrow(|st| st.rows.get(i).is_some_and(|r| !r.hdr)));
                        match row {
                            Some(i) => {
                                let mut it: LVITEMW = std::mem::zeroed();
                                it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                                if put(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED == 0 {
                                    put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
                                    it.state = LVIS_SELECTED | LVIS_FOCUSED;
                                    put(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
                                }
                                SetFocus(list);
                                let (_, dir, paths) = selected();
                                if !paths.is_empty() {
                                    if dir.to_string_lossy() == "::recycle" { recycle_menu(h) } else { item_menu(h, &dir, &paths) }
                                }
                            }
                            None => {
                                let mut it: LVITEMW = std::mem::zeroed();
                                it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                                put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
                                bg_menu(h);
                            }
                        }
                    }
                    LVN_COLUMNCLICK => {
                        let c = (*(lp as *const NMLISTVIEW)).iSubItem as usize;
                        if c >= 4 { return 0; } // the blank filler column does not sort
                        let asc = if SORT_COL.load(Relaxed) == c { !SORT_ASC.load(Relaxed) } else { true };
                        SORT_COL.store(c, Relaxed);
                        SORT_ASC.store(asc, Relaxed);
                        let n = ST.with_borrow_mut(|st| {
                            st.rows.retain(|r| !r.hdr);
                            sort_rows(&mut st.rows, c, asc);
                            add_headers(&mut st.rows);
                            st.rows.iter_mut().for_each(|r| r.thumb = -2); // rows moved: thumbnails are asked for again
                            st.rows.len()
                        });
                        reset_thumbs();
                        put(list, LVM_REDRAWITEMS, 0, n as isize - 1);
                    }
                    LVN_BEGINDRAG => {
                        let (_, _, paths) = selected();
                        let paths = match real_paths(&paths) { Ok(p) => p, Err(e) => { warn(h, &e); return 0; } }; // out of a zip: real files first
                        drag_log(&format!("BEGINDRAG desk={} selected={}", desk(), paths.len()));
                        if !paths.is_empty() {
                            if desk() { let p = (*(lp as *const NMLISTVIEW)).ptAction; desk::drag_start((p.x, p.y)); }
                            let r = sh::drag(h, &paths);
                            drag_log(&format!("drag finished: {r:?}"));
                            // dropped on the desktop itself: the icons were moved (or went into a folder icon), no reload needed
                            let handled = desk() && desk::drag_end(h, list);
                            drag_log(&format!("moved on the desktop: {handled}"));
                            if !handled { refresh_after_ops(h); }
                        }
                    }
                    LVN_KEYDOWN => {
                        let vk = (*(lp as *const NMLVKEYDOWN)).wVKey as u32;
                        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                        let shift = GetKeyState(VK_SHIFT as i32) < 0;
                        if desk() && matches!(vk, 0x08 | 0x46 | 0x45 | 0x4D | 0x4E | 0x54 | 0x57 | 0x09 | 0x4C | 0x73) { return 0; } // browsing keys make no sense on the desktop
                        let (list, dir, paths) = selected();
                        match vk {
                            0x08 => if let Some(p) = dir.parent() { load(h, p.to_path_buf()) },
                            0x1B if fileops::busy() => { fileops::cancel_all(); return 0; } // Esc stops copying / moving / deleting
                            0x1B if jobs::busy() => { jobs::cancel(); return 0; } // Esc stops packing / unpacking
                            0x2E if !paths.is_empty() && !in_zip() && (!cur_virtual() || ui::searching()) => { let _ = fileops::delete(h, paths, shift); return 0; }
                            0x5A if ctrl => { fileops::undo(h); return 0; }
                            // Space: a quick look. Pictures open in photoview, videos / text / anything else in its default app
                            0x20 if !desk() => if let Some(p) = paths.first().filter(|p| p.is_file()) { activate(h, p.clone(), false); },
                            0x71 if !in_zip() && (!cur_virtual() || ui::searching()) => {
                                let i = put(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
                                if i >= 0 { put(list, LVM_EDITLABELW, i as usize, 0); }
                            }
                            0x46 | 0x45 if ctrl => { search_focus(); return 0; } // Ctrl+F / Ctrl+E: the search box
                            0x73 | 0x4C if vk == 0x73 || ctrl => { // F4 / Ctrl+L: address bar
                                let edit = ST.with_borrow(|st| st.edit);
                                SetFocus(edit);
                                put(edit, EM_SETSEL, 0, -1);
                                return 0;
                            }
                            0x5D if !paths.is_empty() => item_menu(h, &dir, &paths),
                            0x43 if ctrl && shift => { copy_path(h); return 0; } // Ctrl+Shift+C: copy path
                            0x43 | 0x58 if ctrl && !paths.is_empty() && !cur_virtual() => { // from a zip: real files in the temp folder, and only a copy (nothing can be cut out of an archive)
                                match real_paths(&paths) { Ok(p) => { let _ = sh::clip_set(h, &p, vk == 0x58 && !in_zip()); } Err(e) => warn(h, &e) }
                                return 0;
                            }
                            0x41 if ctrl => { // Ctrl+A: select all (state mask LVIS_SELECTED, item -1 = every row)
                                let mut it: LVITEMW = std::mem::zeroed();
                                it.stateMask = LVIS_SELECTED;
                                it.state = LVIS_SELECTED;
                                put(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
                                return 0;
                            }
                            0x56 if ctrl => paste(h, &dir),
                            0x4E if ctrl && shift && !cur_virtual() => { create_new(h, &dir, t!("Новая папка", "New folder", ja: "新しいフォルダー", pt: "Nova pasta", zh: "新建文件夹"), true); return 0; }
                            0x4E if ctrl => { new_window(); return 0; } // Ctrl+N: a new window, like Explorer
                            0x48 if ctrl => { SHOW_HIDDEN.fetch_xor(true, Relaxed); save_globals(); } // Ctrl+H: hidden files
                            0x74 => {} // F5: just the reload below
                            0x4D if ctrl => { toggle_view(h); return 0; } // Ctrl+M: details <-> thumbnails
                            0x54 if ctrl && shift => { tab_reopen(h); return 0; } // Ctrl+Shift+T: reopen the closed tab
                            0x54 if ctrl => tab_new(),
                            0x57 if ctrl && shift => { PostMessageW(h, WM_CLOSE, 0, 0); return 0; } // Ctrl+Shift+W: close the window
                            0x57 if ctrl => { tab_close(h); return 0; }
                            0x09 if ctrl => {
                                let next = ST.with_borrow(|st| (st.active + 1) % st.tabs.len());
                                tab_select(h, next);
                                return 0;
                            }
                            _ => return 0,
                        }
                        refresh_after_ops(h);
                    }
                    LVN_BEGINLABELEDITW => {
                        if cur_virtual() { return 1; }
                        let hdr = ST.with_borrow(|st| usize::try_from((*(lp as *const NMLVDISPINFOW)).item.iItem).ok().and_then(|i| st.rows.get(i)).is_some_and(|r| r.hdr));
                        if hdr { return 1; } // a heading cannot be renamed
                        let e = SendMessageW(ST.with_borrow(|st| st.list), LVM_GETEDITCONTROL, 0, 0) as HWND;
                        if e != 0 as HWND {
                            LABEL_OLD.store(SetWindowLongPtrW(e, GWLP_WNDPROC, label_proc as *const () as isize), std::sync::atomic::Ordering::Relaxed);
                            // extensions are hidden: the box starts with just the name selected, like Explorer
                            if !SHOW_EXT.load(Relaxed) {
                                let name = ST.with_borrow(|st| usize::try_from((*(lp as *const NMLVDISPINFOW)).item.iItem).ok().and_then(|i| st.rows.get(i)).map(|r| (r.name.rsplit('\\').next().unwrap_or(&r.name).to_string(), r.dir)));
                                if let Some((file, false)) = name {
                                    if let Some((stem, _)) = file.rsplit_once('.') {
                                        if !stem.is_empty() { SendMessageW(e, EM_SETSEL, 0, stem.encode_utf16().count() as isize); }
                                    }
                                }
                            }
                        }
                    }
                    LVN_ENDLABELEDITW => {
                        let it = &(*(lp as *const NMLVDISPINFOW)).item;
                        if !it.pszText.is_null() {
                            let new = wstr(it.pszText);
                            // reject separators so a rename can never move the item
                            if !new.is_empty() && !new.contains(['\\', '/', ':']) {
                                ST.with_borrow(|st| if let Some(r) = usize::try_from(it.iItem).ok().and_then(|i| st.rows.get(i)) { let _ = fs::rename(st.dir.join(&r.name), st.dir.join(&new)); });
                                PostMessageW(h, WM_APP, 0, 0); // reload after the edit control is gone
                            }
                        }
                    }
                    _ => {}
                }
                0
            }
            WM_SETFOCUS => { SetFocus(ST.with_borrow(|st| st.list)); 0 }
            WM_APP => { refresh_after_ops(h); 0 }
            WM_REFRESH if !desk() => { SetTimer(h, 9, 300, None); 0 } // debounce bursts of changes
            WM_TIMER if !desk() && wp == 11 => {
                let d = DRAG_DIR.load(Relaxed);
                if d != 0 {
                    let list = ST.with_borrow(|st| st.list);
                    put(list, LVM_SCROLL, 0, (d * ui::sc(26)) as isize);
                }
                0
            }
            WM_TIMER if !desk() && wp == 9 => {
                let list = ST.with_borrow(|st| st.list);
                let editing = SendMessageW(list, LVM_GETEDITCONTROL, 0, 0) as HWND;
                if jobs::busy() || fileops::busy() || GetCapture() != 0 as HWND || !editing.is_null() {
                    SetTimer(h, 9, 500, None);
                } else {
                    KillTimer(h, 9);
                    reload_keep(h);
                }
                0
            }
            shellwin::WM_SHELL_SELECT => { shell_select(h as isize, *Box::from_raw(lp as *mut PathBuf)); 0 }
            fileops::WM_FILEOPS_DONE => {
                let report = *Box::from_raw(wp as *mut fileops::Report);
                fileops::finished(h, report);
                0
            }
            fileops::WM_FILEOPS_CONFLICT => { fileops::conflict(h, lp as *mut fileops::Conflict); 0 }
            WM_JOB => {
                let err = *Box::from_raw(wp as *mut Option<String>);
                refresh_after_ops(h);
                ui::status_dirty(h);
                if let Some(e) = err { MessageBoxW(h, w(&e).as_ptr(), w("WinRust Explorer").as_ptr(), MB_ICONWARNING); }
                0
            }
            WM_NAV => {
                match wp {
                    1 => nav(h, true),
                    2 => nav(h, false),
                    3 => { let (_, dir, sel) = selected(); properties(&sel.first().cloned().unwrap_or(dir)); }
                    4 => go_up(h),
                    5 => { // Alt+D: the address bar, all selected
                        let edit = ST.with_borrow(|st| st.edit);
                        SetFocus(edit);
                        put(edit, EM_SETSEL, 0, -1);
                    }
                    _ => {}
                }
                0
            }
            WM_THUMB => {
                let (era, row, bmp) = *Box::from_raw(wp as *mut (u32, usize, isize));
                let (list, himl, thumbs) = ST.with_borrow(|st| (st.list, st.himl, st.view.wants_thumbs()));
                if era == GEN.load(std::sync::atomic::Ordering::Relaxed) {
                    let idx = ImageList_Add(himl as _, bmp as _, ptr::null_mut());
                    let ok = idx >= 0 && ST.with_borrow_mut(|st| st.rows.get_mut(row).map(|r| r.thumb = idx).is_some());
                    if ok && thumbs {
                        let mut it: LVITEMW = std::mem::zeroed();
                        it.mask = LVIF_IMAGE;
                        it.iItem = row as i32;
                        it.iImage = idx;
                        put(list, LVM_SETITEMW, 0, &it as *const _ as isize);
                        put(list, LVM_UPDATE, row, 0);
                    } else if ok {
                        put(list, LVM_REDRAWITEMS, row, row as isize);
                    }
                }
                windows_sys::Win32::Graphics::Gdi::DeleteObject(bmp as _);
                0
            }
            WM_SEARCH => {
                let (se, list) = ST.with_borrow(|st| (st.search, st.list));
                if wp == 13 {
                    let q = edit_text(se);
                    if !q.trim().is_empty() { search(h, q.trim()); SetFocus(list); }
                } else {
                    SetWindowTextW(se, w("").as_ptr());
                    if ui::searching() { ui::set_search(None); load(h, cur_dir()); }
                    SetFocus(list);
                }
                0
            }
            WM_GO => {
                let (edit, list) = ST.with_borrow(|st| (st.edit, st.list));
                if wp == 13 {
                    let mut buf = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
                    let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32) as usize;
                    let p = PathBuf::from(String::from_utf16_lossy(&buf[..n]).trim().trim_matches('"'));
                    let text = String::from_utf16_lossy(&buf[..n]);
                    if let Some(q) = text.trim().strip_prefix('?') {
                        if !q.trim().is_empty() { search(h, q.trim()); SetFocus(list); return 0; } // keep the query in the bar
                    } else if p.is_dir() || zipx::split_zip(&p).is_some() { load(h, p); } // also a path inside a .zip
                    else if !text.trim().is_empty() { // like Explorer: a program (or a file path) typed here runs in this folder
                        let _ = run_command_line(h, &cur_dir(), text.trim());
                    }
                }
                SetWindowTextW(edit, w(&page_name(&cur_dir()).map_or_else(|| cur_dir().to_string_lossy().into_owned(), str::to_string)).as_ptr()); // also undoes a bad path / Esc
                SetFocus(list);
                0
            }
            // icons hidden: the list is not shown, so the click lands on the desktop itself; the menu must still open
            WM_CONTEXTMENU if desk() && icons_hidden() => { bg_menu(h); 0 }
            desk::SPAWN_WORKERW if desk() => { desk::spawn_workerw(h); 0 }
            WM_TIMER if desk() && wp == 7 => { desk::check_live(ST.with_borrow(|st| st.list)); 0 }
            WM_DESTROY => { if !desk() { save_place(h); } fileops::shutdown(h); shellwin::revoke(); PostQuitMessage(0); 0 }
            _ => DefWindowProcW(h, m, wp, lp),
        }
    }
}

/// Control Panel, Recycle Bin, "This PC" arrive as shell addresses (`::{GUID}`, `shell:...`), not folders: not ours to show (#85, #80, #70).
fn namespace_target() -> Option<String> {
    env::args().skip(1).find_map(|a| {
        let t = a.trim_matches('"').rsplit(',').next().unwrap_or("").to_string(); // `/e,::{...}`
        let l = t.to_lowercase();
        (l.starts_with("::{") || l.starts_with("shell:")).then_some(t)
    })
}

fn main() {
    // the Win+E resident mode: no window, no COM, only the keyboard hook (see hotkey.rs)
    if env::args().nth(1).as_deref() == Some("--hotkey") {
        hotkey::run(env::args().nth(2).map(PathBuf::from).unwrap_or_else(|| env::current_exe().unwrap_or_default()));
        return;
    }
    if let Some(t) = namespace_target() {
        // control.exe opens its own window; explorer.exe given a namespace address opens it directly (no folder verb involved)
        // Only the "open" verb of folders is ours: control.exe and explorer.exe <address> both come back here through it
        // (an endless control.exe <-> rust-explorer loop, flickering cursor). The "explore" verb still has Explorer's own
        // handler, so it shows the panel / Recycle Bin in a real Explorer window. The hop mark stops any loop that remains.
        if env::var_os("RX_NS_HOP").is_none() {
            let addr = if t.starts_with("::") { format!("shell:{t}") } else { t };
            unsafe { env::set_var("RX_NS_HOP", "1") }; // inherited by whatever the shell starts from here
            let (f, v) = (w(&addr), w("explore"));
            let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
            sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
            sei.fMask = 0x0000_0100 | 0x0000_0400; // SEE_MASK_NOASYNC (we exit right after) | SEE_MASK_FLAG_NO_UI
            sei.lpVerb = v.as_ptr();
            sei.lpFile = f.as_ptr();
            sei.nShow = SW_SHOWNORMAL;
            unsafe { ShellExecuteExW(&mut sei) };
        }
        return;
    }
    // the old marker file meant "group by date"; the view menu now keeps the grouping itself
    let legacy = PathBuf::from(env::var_os("APPDATA").unwrap_or_default()).join(r"rust-explorer\group-by-date");
    if legacy.exists() { GROUP.store(1, Relaxed); let _ = fs::remove_file(&legacy); }
    let (compact, ext, nav_on, show_cap, checks, hidden) = read_globals();
    COMPACT.store(compact, Relaxed);
    SHOW_EXT.store(ext, Relaxed); // the saved switch, or the Windows "Hide extensions for known file types" setting
    NAV_ON.store(nav_on, Relaxed);
    SHOW_CAP.store(show_cap, Relaxed);
    CHECK_BOXES.store(checks, Relaxed);
    SHOW_HIDDEN.store(hidden, Relaxed);
    sh::init();
    unsafe {
        SetProcessDPIAware(); // sharp text at any scaling; every size below goes through ui::sc
        ui::init_scale();
        ui::gdiplus_start();
        ui::dark_mode();
        InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES });
        let inst = GetModuleHandleW(ptr::null());
        let cls = w("RustExplorer");
        let desktop = env::args().nth(1).as_deref() == Some("--desktop");
        // desktop without a wallpaper file shows black, not window white
        // desktop: COLOR_BACKGROUND, the user's solid colour when there is no picture, never black (#108, #111)
        let bg = if desktop { windows_sys::Win32::Graphics::Gdi::GetSysColorBrush(1) as _ } else { windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(ui::pal().bg) };
        RegisterClassW(&WNDCLASSW { style: CS_DBLCLKS, lpfnWndProc: Some(ui::proc), hInstance: inst, lpszClassName: w("RxChrome").as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(ui::nav_proc), hInstance: inst, lpszClassName: w("RxNav").as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), ..std::mem::zeroed() });
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst, lpszClassName: cls.as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), hIcon: LoadIconW(inst, 1 as _), hbrBackground: bg, ..std::mem::zeroed() });
        if desktop {
            DESKTOP.store(true, Relaxed);
            // the whole screen: it paints the wallpaper too; the icon list sits above the bar (desk_top / desk_bottom)
            // class "Progman" like explorer's desktop, so Wallpaper Engine & co. find it; title "rs-desktop" tells it apart
            // from explorer's ("Program Manager") for rs-shell
            let pm = w("Progman");
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst, lpszClassName: pm.as_ptr(), hCursor: LoadCursorW(0 as _, IDC_ARROW), hIcon: LoadIconW(inst, 1 as _), hbrBackground: bg, ..std::mem::zeroed() });
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(desk::workerw_proc), hInstance: inst, lpszClassName: w("WorkerW").as_ptr(), ..std::mem::zeroed() });
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: inst, lpszClassName: w("SHELLDLL_DefView").as_ptr(), ..std::mem::zeroed() });
            let (vx, vy, vw, vh) = virt(); // every monitor: the wallpaper reaches the secondary ones too (#67)
            let d = CreateWindowExW(WS_EX_TOOLWINDOW, pm.as_ptr(), w("rs-desktop").as_ptr(), WS_POPUP, vx, vy, vw, vh, 0 as _, 0 as _, inst, ptr::null());
            SetWindowPos(d, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        } else {
            // where it was last closed, maximized again if it was (#71); a first start gets the default size
            // Windows puts WS_CAPTION back on a thick-framed window; WM_NCCALCSIZE below hands the caption area to the client
            let win = CreateWindowExW(0, cls.as_ptr(), ptr::null(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, ui::sc(1100), ui::sc(700), 0 as _, 0 as _, inst, ptr::null());
            match saved_place() {
                // the same API that saved it: rcNormalPosition is in work-area coordinates, which CreateWindow does not use
                Some((max, r)) => {
                    let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
                    wp.length = size_of::<WINDOWPLACEMENT>() as u32;
                    wp.showCmd = if max { SW_SHOWMAXIMIZED } else { SW_SHOWNORMAL } as u32;
                    wp.rcNormalPosition = r;
                    SetWindowPlacement(win, &wp);
                }
                None => { ShowWindow(win, SW_SHOWNORMAL); }
            }
            // Win+E started this window from the background hook (which allowed it): take the foreground
            if env::args().any(|a| a == "--focus") { SetForegroundWindow(win); }
        }
        let mut msg = std::mem::zeroed();
        while GetMessageW(&mut msg, 0 as _, 0, 0) > 0 {
            // Alt+Left / Right / Up / Enter arrive as WM_SYSKEYDOWN at the list, which never reports them: handle them here
            if msg.message == WM_SYSKEYDOWN && !desk() {
                let act = match msg.wParam { 0x25 => 1, 0x27 => 2, 0x0D => 3, 0x26 => 4, 0x44 => 5, _ => 0 };
                if act != 0 { PostMessageW(GetAncestor(msg.hwnd, GA_ROOT), WM_NAV, act, 0); continue; }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn recycle_records_give_the_original_name_and_place() {
        let d = std::env::temp_dir().join(format!("rx-recycle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let original = r"C:\Temp\note.txt";
        let mut b = Vec::new();
        b.extend(2u64.to_le_bytes());
        b.extend(123u64.to_le_bytes());
        b.extend(132_000_000_000_000_000u64.to_le_bytes());
        let u: Vec<u16> = original.encode_utf16().chain([0]).collect();
        b.extend((u.len() as u32).to_le_bytes());
        for c in &u { b.extend(c.to_le_bytes()); }
        let meta = d.join("$IABC");
        fs::write(&meta, &b).unwrap();
        fs::write(d.join("$RABC"), "x").unwrap();
        let r = parse_recycle(&meta, &b).unwrap();
        assert_eq!(r.name, "note.txt");
        assert_eq!(r.origin.as_deref(), Some(Path::new(original)));
        assert!(r.target.as_ref().is_some_and(|p| p.ends_with("$RABC")));
        assert_eq!(r.size, 123);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn window_place_parses_and_rejects_junk() {
        let (max, r) = super::parse_place("max 10 20 1010 680").unwrap();
        assert!(max);
        assert_eq!((r.left, r.top, r.right, r.bottom), (10, 20, 1010, 680));
        assert!(!super::parse_place("normal -1900 5 -900 700").unwrap().0); // a monitor left of the primary
        assert!(super::parse_place("max 0 0 50 50").is_none()); // collapsed
        assert!(super::parse_place("max 0 0 800").is_none());
        assert!(super::parse_place("big 0 0 800 600").is_none());
        assert!(super::parse_place("").is_none());
    }

    #[test]
    fn a_zip_opens_like_a_folder_and_gives_real_files() {
        let d = std::env::temp_dir().join(format!("rx-zipview-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("f.txt"), "hello").unwrap();
        let z = d.join("a.zip");
        let mut w = zipx::Writer::new(fs::File::create(&z).unwrap());
        w.add_prepared(zipx::prepare("inner/f.txt".into(), &d.join("f.txt")).unwrap()).unwrap();
        w.add_prepared(zipx::prepare("top.txt".into(), &d.join("f.txt")).unwrap()).unwrap();
        w.finish().unwrap();
        let names = |dir: &PathBuf| rows_of(dir).unwrap().into_iter().map(|r| format!("{}{}", r.name, if r.dir { "/" } else { "" })).collect::<Vec<_>>();
        assert_eq!(names(&z), ["inner/", "top.txt"]); // folders first
        assert_eq!(names(&z.join("inner")), ["f.txt"]);
        assert!(rows_of(&d.join("nothing.zip")).is_none());
        let paths = [z.join("inner").join("f.txt"), z.join("top.txt")];
        let real = real_paths(&paths).unwrap();
        assert!(real.iter().all(|p| p.is_file() && fs::read_to_string(p).unwrap() == "hello"), "{real:?}");
        assert_eq!(real_paths(&[d.join("f.txt")]).unwrap(), [d.join("f.txt")], "ordinary files stay as they are");
        assert!(zip_names(&[z.join("top.txt"), d.join("f.txt")]).is_none(), "mixed selections are not archive selections");
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(real[0].parent().unwrap());
    }

    /// By hand (needs the real shell): `cargo test -p rust-explorer system_zip -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn system_zip_is_not_in_the_menu() {
        sh::init();
        let f = std::env::temp_dir().join("rx-menu-dump.txt");
        std::fs::write(&f, "x").unwrap();
        let menu = sh::menu_dump(&[f.clone()]);
        let _ = std::fs::remove_file(f);
        println!("{}", menu.join("
"));
        assert!(menu.iter().any(|l| l.contains("Se&nd to") || l.contains("Send to") || l.contains("&Delete") || l.contains("Delete")), "the menu is there");
        if let Some(n) = sh::system_zip_name() { assert!(!menu.iter().any(|l| l.trim().starts_with(&n)), "{n} should be gone"); }
    }

    use super::*;

    fn r(name: &str, dir: bool, size: u64, time: u64) -> Row { Row { name: name.into(), origin: None, target: None, kind: None, dir, size, time, icon: -1, thumb: -2 , hdr: false } }
    fn names(v: &[Row]) -> Vec<&str> { v.iter().map(|r| r.name.as_str()).collect() }

    #[test]
    fn desktop_rows_show_file_names_without_shortcut_extensions() {
        assert_eq!(shown_name(r"C:\Users\Public\Desktop\Steam.lnk", true, false), "Steam");
        assert_eq!(shown_name("Notes.txt", true, false), "Notes.txt");
        assert_eq!(shown_name(r"C:\x\Game.URL", true, false), "Game");
        assert_eq!(shown_name(r"C:\x\Steam.lnk", false, false), r"C:\x\Steam.lnk");
    }

    #[test]
    fn folders_first_then_by_the_chosen_column() {
        let mut v = vec![r("b.txt", false, 5, 1), r("A", true, 0, 2), r("a.txt", false, 9, 3)];
        sort_rows(&mut v, 0, true);
        assert_eq!(names(&v), ["A", "a.txt", "b.txt"]);
        sort_rows(&mut v, 3, false); // biggest first, the folder still on top
        assert_eq!(names(&v), ["A", "a.txt", "b.txt"]);
        sort_rows(&mut v, 1, true); // oldest first
        assert_eq!(names(&v), ["A", "b.txt", "a.txt"]);
    }

    #[test]
    fn search_hit_paths_sort_by_file_name() {
        let mut v = vec![r("C:\\z\\b.txt", false, 1, 1), r("D:\\a\\a.txt", false, 1, 1)];
        sort_rows(&mut v, 0, true);
        assert_eq!(names(&v), ["D:\\a\\a.txt", "C:\\z\\b.txt"]);
    }

    #[test]
    fn history_goes_back_and_forward_and_a_new_visit_drops_the_forward_trail() {
        let mut h = Hist::default();
        for p in ["a", "b", "c"] { h.visit(p.into()); }
        assert_eq!(h.back(), Some("b".into()));
        assert_eq!(h.back(), Some("a".into()));
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), Some("b".into()));
        h.visit("x".into()); // from b: c is no longer ahead
        assert_eq!(h.forward(), None);
        assert_eq!(h.back(), Some("b".into()));
        h.visit("x".into()); h.visit("x".into()); // same folder twice is one step
        assert_eq!(h.v.len(), 3);
    }

    #[test]
    fn type_and_size_read_like_explorers() {
        // the tests run in Russian (lang!() in a test build), like Explorer's own Type column
        assert_eq!(type_text("x", true), "Папка с файлами");
        assert_eq!(type_text("a.txt", false), "Файл TXT");
        assert_eq!(type_text("C:\\d\\b.tar.gz", false), "Файл GZ");
        assert_eq!(type_text(".gitignore", false), "Файл"); // a dot file has no extension
        assert_eq!(type_text("Makefile", false), "Файл");
        assert_eq!(size_text(1), "1 KB");
        assert_eq!(size_text(2048), "2 KB");
        assert_eq!(size_text(5 << 20), "5.0 MB");
        assert_eq!(size_text(3 << 30), "3.00 GB");
    }

    #[test]
    fn dates_are_formatted_and_zero_is_blank() {
        assert_eq!(date_text(0), "");
        let t = date_text(132_000_000_000_000_000); // some day in 2019, local time zone
        assert!(t.contains(':') && t.contains("19"), "{t}"); // a short date and a time, in the user's format
    }
}

#[cfg(test)]
mod search_tests {
    #[test]
    fn local_search_finds_names_below_the_folder() {
        let d = std::env::temp_dir().join(format!("rx-search-test-{}", std::process::id()));
        std::fs::create_dir_all(d.join(r"sub\deep")).unwrap();
        for f in [r"Report.txt", r"sub\notes.md", r"sub\deep\REPORT-2.pdf"] { std::fs::write(d.join(f), "x").unwrap(); }
        let (rows, cut, flipped) = super::local_search(&d, "report");
        let (wrong, _, was_flipped) = super::local_search(&d, "кузщке"); // "report" typed in the Russian layout
        let mut names: Vec<String> = rows.iter().map(|r| std::path::Path::new(&r.name).file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(names, ["REPORT-2.pdf", "Report.txt"]); // any case, any depth, absolute paths
        assert!(!cut && !flipped);
        assert!(was_flipped && wrong.len() == 2);
    }

    #[test]
    fn layout_flip_both_ways() {
        assert_eq!(super::flip_layout("сфкпщ"), "cargo");
        assert_eq!(super::flip_layout("ljrevtyns"), "документы");
        assert_eq!(super::flip_layout("Ыфму"), "Save");
    }
}
