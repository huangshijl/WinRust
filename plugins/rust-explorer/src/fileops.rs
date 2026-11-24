//! File operations the Explorer way: one queue, a progress window (speed, time left, pause, cancel), a conflict page
//! (Replace / Keep both / Skip / Cancel, "do this for all"), and undo (Ctrl+Z). Copy, move across drives, recycle and
//! permanent delete all go through here. The old shell path stays for folders ours cannot write (elevation).
#![allow(unsafe_op_in_unsafe_fn)]
use super::*;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Graphics::Gdi::*,
    Storage::FileSystem::{COPY_FILE_NO_BUFFERING, CopyFileExW},
    System::SystemInformation::GetTickCount64,
    UI::Input::KeyboardAndMouse::*,
};
use winrust_common::{t, tf};

pub const WM_FILEOPS_DONE: u32 = WM_APP + 9; // worker -> UI: wp = Box<Report>
pub const WM_FILEOPS_CONFLICT: u32 = WM_APP + 10; // worker -> UI (SendMessage): lp = *mut Conflict

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind { Copy, Move, Recycle, Delete }

#[derive(Clone)]
struct Task {
    kind: Kind,
    from: Vec<PathBuf>,
    to: PathBuf,
    undo: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice { Replace, Skip, KeepBoth, Cancel }

/// Everything the conflict page shows about the two files.
pub struct Conflict {
    pub name: String,
    pub src: String,
    pub dst: String,
    pub src_size: u64,
    pub dst_size: u64,
    pub src_time: u64,
    pub dst_time: u64,
    pub src_dir: bool,
    pub dst_dir: bool,
    pub choice: Choice,
    pub all: bool,
}

pub struct Job {
    kind: Kind,
    owner: isize,
    top: u32,
    from: String,
    to: String,
    bytes: AtomicU64,
    total_bytes: AtomicU64,
    files: AtomicU64,
    total_files: AtomicU64,
    current: Mutex<String>,
    paused: AtomicBool,
    cancel: AtomicBool,
    apply_all: Mutex<Option<Choice>>,
}

impl Job {
    fn new(task: &Task, owner: isize) -> Job {
        let from = task.from.first().map_or_else(String::new, |p| p.parent().unwrap_or(p).to_string_lossy().into_owned());
        let to = if matches!(task.kind, Kind::Copy | Kind::Move) { task.to.to_string_lossy().into_owned() } else { String::new() };
        Job {
            kind: task.kind, owner, top: task.from.len() as u32, from, to,
            bytes: 0.into(), total_bytes: 0.into(), files: 0.into(), total_files: 0.into(),
            current: Mutex::new(String::new()),
            paused: false.into(), cancel: false.into(),
            apply_all: Mutex::new(None),
        }
    }
}

#[derive(Default)]
struct Done {
    undo: Option<Undo>,
    errors: Vec<String>,
    cancelled: bool,
}

#[derive(Default)]
pub struct Report {
    tasks: Vec<Done>,
}

#[derive(Default, Clone)]
struct Undo {
    created: Vec<PathBuf>,          // new files or whole new top-level folders
    moved: Vec<(PathBuf, PathBuf)>, // (where it came from, where it is now)
    recycled: Vec<PathBuf>,         // originals of the items taken to the Recycle Bin
}

static QUEUE: Mutex<VecDeque<Task>> = Mutex::new(VecDeque::new());
static CUR: Mutex<Option<Arc<Job>>> = Mutex::new(None);
static ONLY: Mutex<isize> = Mutex::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
static UNDO: Mutex<Vec<Undo>> = Mutex::new(Vec::new());
const UNDO_MAX: usize = 40;

fn kind_word(k: Kind) -> &'static str {
    match k {
        Kind::Copy => t!("Копирование", "Copying", ja: "コピー中", pt: "Copiando", zh: "正在复制"),
        Kind::Move => t!("Перемещение", "Moving", ja: "移動中", pt: "Movendo", zh: "正在移动"),
        Kind::Recycle => t!("Удаление", "Deleting", ja: "削除中", pt: "Excluindo", zh: "正在删除"),
        Kind::Delete => t!("Удаление", "Deleting", ja: "削除中", pt: "Excluindo", zh: "正在删除"),
    }
}

/// Queues a copy / move into `to`. Call on the UI thread.
pub fn submit(h: HWND, kind: Kind, from: Vec<PathBuf>, to: PathBuf) -> bool {
    submit_inner(h, Task { kind, from, to, undo: false })
}

/// Delete to the Recycle Bin, or permanently (Ask first).
pub fn delete(h: HWND, paths: Vec<PathBuf>, permanent: bool) -> bool {
    if permanent {
        let name = paths.first().and_then(|p| p.file_name()).map_or_else(|| paths[0].to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        let text = if paths.len() == 1 {
            tf!("Удалить безвозвратно \"{}\"?", "Permanently delete \"{}\"?", ja: "\"{}\" を完全に削除しますか?", pt: "Excluir \"{}\" permanentemente?", zh: "要永久删除“{}”吗？", name)
        } else {
            tf!("Удалить безвозвратно элементов: {}?", "Permanently delete {} items?", ja: "{} 項目を完全に削除しますか?", pt: "Excluir {} itens permanentemente?", zh: "要永久删除 {} 项吗？", paths.len())
        };
        let yes = unsafe { MessageBoxW(h, w(&text).as_ptr(), w("WinRust Explorer").as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) } == IDYES;
        if !yes { return false; }
    }
    submit(h, if permanent { Kind::Delete } else { Kind::Recycle }, paths, PathBuf::new())
}

fn submit_inner(h: HWND, task: Task) -> bool {
    if task.from.is_empty() { return false; }
    {
        let mut only = ONLY.lock().unwrap();
        if *only == 0 { *only = h as isize; }
    }
    QUEUE.lock().unwrap().push_back(task);
    show_progress(h);
    if !RUNNING.swap(true, Relaxed) { std::thread::spawn(worker); }
    true
}

pub fn busy() -> bool { RUNNING.load(Relaxed) }

/// Esc / the progress window's Cancel: stop the current item and drop what is still queued.
pub fn cancel_all() {
    if let Some(j) = CUR.lock().unwrap().as_ref() { j.cancel.store(true, Relaxed); }
    QUEUE.lock().unwrap().clear();
}

/// The owner window is going away: nothing may talk to it any more. The worker stops at the next step.
pub fn shutdown(h: HWND) {
    if *ONLY.lock().unwrap() == h as isize {
        cancel_all();
        *ONLY.lock().unwrap() = 0;
    }
    if PROGRESS.with(|p| p.get()) == h as isize { PROGRESS.with(|p| p.set(0)); }
}

fn worker() {
    unsafe { let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED); }
    let owner = *ONLY.lock().unwrap();
    let mut report = Report::default();
    loop {
        let task = QUEUE.lock().unwrap().pop_front();
        let Some(task) = task else { break };
        let job = Arc::new(Job::new(&task, owner));
        *CUR.lock().unwrap() = Some(job.clone());
        let r = run(&task, &job);
        *CUR.lock().unwrap() = None;
        let cancelled = r.cancelled;
        report.tasks.push(r);
        if cancelled { QUEUE.lock().unwrap().clear(); }
    }
    if owner != 0 {
        let b = Box::into_raw(Box::new(report));
        let ok = unsafe { PostMessageW(owner as HWND, WM_FILEOPS_DONE, b as usize, 0) } != 0;
        if !ok { unsafe { drop(Box::from_raw(b)); } }
    }
}

/// Called by the main window when the worker is done: closes the progress window, remembers undo and reloads.
pub fn finished(h: HWND, report: Report) {
    hide_progress();
    let mut errors = Vec::new();
    for t in report.tasks {
        if let Some(u) = t.undo { push_undo(u); }
        errors.extend(t.errors);
    }
    let more = QUEUE.lock().unwrap().len();
    if more > 0 {
        // something was queued in the instant the worker was shutting down
        show_progress(h);
        RUNNING.store(false, Relaxed);
        if !RUNNING.swap(true, Relaxed) { std::thread::spawn(worker); }
    } else {
        RUNNING.store(false, Relaxed);
        *ONLY.lock().unwrap() = 0;
    }
    super::reload(h);
    if !errors.is_empty() {
        let more = if errors.len() > 5 { format!("\n… +{}", errors.len() - 5) } else { String::new() };
        let text = tf!("Не удалось выполнить операции: {}\n{}{}", "{} item(s) failed:\n{}{}", ja: "{} 件失敗しました:\n{}{}", pt: "{} item(ns) falharam:\n{}{}", zh: "{} 项操作失败：\n{}{}", errors.len(), errors[..errors.len().min(5)].join("\n"), more);
        super::warn(h, &text);
    }
}

fn push_undo(u: Undo) {
    let mut st = UNDO.lock().unwrap();
    if st.len() >= UNDO_MAX { st.remove(0); }
    st.push(u);
}

/// Ctrl+Z. Call on the UI thread.
pub fn undo(h: HWND) {
    if busy() {
        super::warn(h, t!("Дождитесь окончания текущей операции", "Wait for the current operation to finish", ja: "現在の操作が終わるまでお待ちください", pt: "Aguarde a operação atual terminar", zh: "请等待当前操作完成"));
        return;
    }
    let Some(u) = UNDO.lock().unwrap().pop() else { return };
    if !u.recycled.is_empty() {
        let mut n = 0;
        for p in &u.recycled { if super::recycle_restore_original(h, p) { n += 1; } }
        super::reload(h);
        if n == 0 {
            super::warn(h, t!("В корзине больше нет этих элементов", "The deleted items are no longer in the Recycle Bin", ja: "削除された項目がごみ箱にありません", pt: "Os itens excluídos não estão mais na Lixeira", zh: "回收站中已找不到被删除的项目"));
        }
        return;
    }
    if !u.moved.is_empty() {
        let mut groups: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> = Default::default();
        for (orig, now) in u.moved {
            if let Some(parent) = orig.parent() { groups.entry(parent.to_path_buf()).or_default().push(now); }
        }
        for (parent, files) in groups { submit_inner(h, Task { kind: Kind::Move, from: files, to: parent, undo: true }); }
    }
    if !u.created.is_empty() {
        submit_inner(h, Task { kind: Kind::Recycle, from: u.created, to: PathBuf::new(), undo: true });
    }
}

// ---------- one task ----------

fn run(task: &Task, job: &Arc<Job>) -> Done {
    match task.kind {
        Kind::Copy => transfer(task, job, false),
        Kind::Move => transfer(task, job, true),
        Kind::Recycle => recycle(task, job),
        Kind::Delete => remove(task, job),
    }
}

fn ticking(job: &Job) -> bool {
    while job.paused.load(Relaxed) {
        if job.cancel.load(Relaxed) { return false; }
        std::thread::sleep(Duration::from_millis(60));
    }
    !job.cancel.load(Relaxed)
}

struct CopyCtx<'a> { job: &'a Job, last: u64 }
unsafe extern "system" fn copy_cb(_t: i64, done: i64, _: i64, _: i64, _: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void, data: *const core::ffi::c_void) -> u32 {
    let cx = unsafe { &mut *(data as *mut CopyCtx) };
    while cx.job.paused.load(Relaxed) {
        if cx.job.cancel.load(Relaxed) { return 1; } // PROGRESS_CANCEL
        std::thread::sleep(Duration::from_millis(50));
    }
    if cx.job.cancel.load(Relaxed) { return 1; }
    let now = done as u64;
    if now > cx.last { cx.job.bytes.fetch_add(now - cx.last, Relaxed); cx.last = now; }
    0
}

fn copy_file(src: &Path, dst: &Path, size: u64, job: &Job) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let w = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain([0]).collect() };
    let mut cx = CopyCtx { job, last: 0 };
    let flags = if size >= 256 << 20 { COPY_FILE_NO_BUFFERING } else { 0 };
    let ok = unsafe { CopyFileExW(w(src).as_ptr(), w(dst).as_ptr(), Some(copy_cb), &mut cx as *mut _ as _, std::ptr::null_mut(), flags) } != 0;
    job.bytes.fetch_add(size.saturating_sub(cx.last), Relaxed);
    if ok { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn remove_path(p: &Path) -> std::io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.permissions().readonly() {
        let mut perm = md.permissions(); perm.set_readonly(false); let _ = fs::set_permissions(p, perm);
    }
    if md.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

fn keep_name(dst: &Path, is_dir: bool) -> PathBuf {
    let parent = dst.parent().unwrap_or(Path::new("."));
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    jobs::free_name(parent, &name, is_dir)
}

fn ask(job: &Job, src: &Path, dst: &Path, src_dir: bool) -> Choice {
    if let Some(c) = *job.apply_all.lock().unwrap() { return c; }
    let meta = |p: &Path| fs::metadata(p).ok();
    let (sm, dm) = (meta(src), meta(dst));
    let name = dst.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut req = Conflict {
        name, src: src.to_string_lossy().into_owned(), dst: dst.to_string_lossy().into_owned(),
        src_size: sm.as_ref().map_or(0, |m| m.len()), dst_size: dm.as_ref().map_or(0, |m| m.len()),
        src_time: sm.as_ref().map_or(0, |m| m.last_write_time()), dst_time: dm.as_ref().map_or(0, |m| m.last_write_time()),
        src_dir, dst_dir: dm.as_ref().is_some_and(|m| m.is_dir()),
        choice: Choice::Cancel, all: false,
    };
    if job.owner != 0 && unsafe { IsWindow(job.owner as HWND) } != 0 {
        unsafe { SendMessageW(job.owner as HWND, WM_FILEOPS_CONFLICT, 0, &mut req as *mut _ as isize) };
    }
    if req.all { *job.apply_all.lock().unwrap() = Some(req.choice); }
    req.choice
}

/// Shared by copy and move. On the same drive files are renamed one by one; otherwise they are copied and the
/// sources go away only when every file of the task arrived.
/// The source path a walked entry came from (the first top-level item whose name starts the relative path).
fn task_src(task: &Task, rel: &Path) -> PathBuf {
    let first = rel.components().next().map(|c| c.as_os_str());
    task.from.iter().find(|s| s.file_name() == first).map_or_else(|| rel.to_path_buf(), |s| s.parent().map_or_else(|| s.clone(), |p| p.join(rel)))
}

fn transfer(task: &Task, job: &Arc<Job>, moving: bool) -> Done {
    let mut done = Done::default();
    let mut entries: Vec<(PathBuf, PathBuf, Option<u64>)> = Vec::new();
    for src in &task.from {
        let Some(name) = src.file_name() else { done.errors.push(format!("{}: no name", src.display())); continue };
        if let Err(e) = jobs::walk(src, PathBuf::from(name), &mut entries) { done.errors.push(format!("{}: {e}", src.display())); }
    }
    job.total_files.store(entries.iter().filter(|e| e.2.is_some()).count() as u64, Relaxed);
    job.total_bytes.store(entries.iter().filter_map(|e| e.2).sum(), Relaxed);
    let mut remap: Vec<(PathBuf, PathBuf)> = Vec::new();
    let fix = |dst: PathBuf, remap: &[(PathBuf, PathBuf)]| -> PathBuf {
        for (from, to) in remap { if let Ok(rest) = dst.strip_prefix(from) { return to.join(rest); } }
        dst
    };
    let mut created_files: Vec<PathBuf> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    let mut moved_pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut new_tops: Vec<PathBuf> = Vec::new();
    for src in &task.from {
        let top = task.to.join(src.file_name().unwrap_or_default());
        if !top.exists() { new_tops.push(top); }
    }
    let mut skip: Option<PathBuf> = None;

    'entries: for (src, rel, size) in &entries {
        if let Some(prefix) = &skip { if rel.starts_with(prefix) { continue; } }
        if !ticking(job) { done.cancelled = true; break; }
        let mut dst = fix(task.to.join(rel), &remap);
        let shown = src.file_name().map_or_else(|| src.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned());
        *job.current.lock().unwrap() = shown;
        match size {
            None => {
                if dst.is_dir() { continue; }
                if dst.exists() {
                    match ask(job, src, &dst, true) {
                        Choice::Replace => { if let Err(e) = remove_path(&dst) { done.errors.push(format!("{}: {e}", dst.display())); skip = Some(rel.clone()); continue } }
                        Choice::Skip => { skip = Some(rel.clone()); continue }
                        Choice::KeepBoth => { let nd = keep_name(&dst, true); remap.push((dst.clone(), nd.clone())); dst = nd; }
                        Choice::Cancel => { job.cancel.store(true, Relaxed); done.cancelled = true; break 'entries; }
                    }
                }
                match fs::create_dir_all(&dst) {
                    Ok(()) => created_dirs.push(dst),
                    Err(e) => { done.errors.push(format!("{}: {e}", dst.display())); skip = Some(rel.clone()); }
                }
            }
            Some(sz) => {
                if dst.exists() {
                    match ask(job, src, &dst, false) {
                        Choice::Replace => { if let Err(e) = remove_path(&dst) { done.errors.push(format!("{}: {e}", dst.display())); continue } }
                        Choice::Skip => continue,
                        Choice::KeepBoth => dst = keep_name(&dst, false),
                        Choice::Cancel => { job.cancel.store(true, Relaxed); done.cancelled = true; break 'entries; }
                    }
                }
                if moving && jobs::same_volume(src, &dst) {
                    if let Ok(()) = fs::rename(src, &dst) {
                        job.bytes.fetch_add(*sz, Relaxed); job.files.fetch_add(1, Relaxed);
                        renamed.push((src.clone(), dst));
                        continue;
                    }
                }
                if let Some(parent) = dst.parent() {
                    if !parent.is_dir() {
                        if let Err(e) = fs::create_dir_all(parent) { done.errors.push(format!("{}: {e}", parent.display())); continue }
                        created_dirs.push(parent.to_path_buf());
                    }
                }
                match copy_file(src, &dst, *sz, job) {
                    Ok(()) => {
                        job.files.fetch_add(1, Relaxed);
                        if moving { moved_pairs.push((src.clone(), dst.clone())); } else { created_files.push(dst); }
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&dst);
                        if job.cancel.load(Relaxed) { done.cancelled = true; break 'entries; }
                        done.errors.push(format!("{}: {e}", src.display()));
                    }
                }
            }
        }
    }
    job.current.lock().unwrap().clear();

    let all_ok = !done.cancelled && done.errors.is_empty();
    if moving {
        if all_ok {
            // take away exactly what arrived: a skipped file stays put, on purpose
            for (src, _) in &moved_pairs { let _ = fs::remove_file(src); }
            for (_, rel, size) in entries.iter().rev() {
                if size.is_none() { let _ = fs::remove_dir(task_src(task, rel)); }
            }
        }
    }
    if !task.undo {
        let mut u = Undo::default();
        if moving && all_ok {
            u.moved = renamed.into_iter().chain(moved_pairs).collect();
        } else if moving {
            u.moved = renamed;
            let tops: Vec<PathBuf> = new_tops.into_iter().filter(|p| p.exists()).collect();
            u.created = if tops.is_empty() { moved_pairs.into_iter().map(|(_, d)| d).collect() } else { tops };
        } else {
            u.created = if new_tops.len() == task.from.len() { new_tops } else { created_files };
        }
        if !u.moved.is_empty() || !u.created.is_empty() { done.undo = Some(u); }
    }
    done
}

fn recycle(task: &Task, job: &Arc<Job>) -> Done {
    job.total_files.store(0, Relaxed);
    job.total_bytes.store(0, Relaxed);
    *job.current.lock().unwrap() = task.from.first().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).unwrap_or_default();
    let mut done = Done::default();
    match sh::recycle_silent(job.owner as *mut core::ffi::c_void, &task.from) {
        Ok(()) => { job.files.store(task.from.len() as u64, Relaxed); }
        Err(e) => done.errors.push(format!("Recycle Bin: {e}")),
    }
    if !task.undo { done.undo = Some(Undo { recycled: task.from.clone(), ..Default::default() }); }
    done
}

fn remove(task: &Task, job: &Arc<Job>) -> Done {
    let mut done = Done::default();
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for src in &task.from {
        let mut e = Vec::new();
        let walked = jobs::walk(src, PathBuf::new(), &mut e).is_ok();
        if walked {
            for (p, _, size) in e {
                if size.is_some() { files.push(p); } else { dirs.push(p); }
            }
            if files.is_empty() && dirs.is_empty() && fs::symlink_metadata(src).is_ok() { files.push(src.clone()); } // a link to a folder
        } else if src.exists() { files.push(src.clone()); }
    }
    job.total_files.store(files.len() as u64, Relaxed);
    job.total_bytes.store(files.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum(), Relaxed);
    for p in &files {
        if !ticking(job) { done.cancelled = true; break; }
        *job.current.lock().unwrap() = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let sz = fs::metadata(p).map_or(0, |m| m.len());
        match remove_path(p) {
            Ok(()) => { job.files.fetch_add(1, Relaxed); job.bytes.fetch_add(sz, Relaxed); }
            Err(e) => done.errors.push(format!("{}: {e}", p.display())),
        }
    }
    if !done.cancelled {
        for d in dirs.iter().rev() {
            if let Err(e) = fs::remove_dir(d) {
                if e.kind() != std::io::ErrorKind::NotFound { done.errors.push(format!("{}: {e}", d.display())); }
            }
        }
    }
    job.current.lock().unwrap().clear();
    done
}

// ---------- progress window ----------

const PROG_CLASS: &str = "WinRustFileOps";
thread_local! {
    static PROGRESS: Cell<isize> = const { Cell::new(0) };
    static PUI: RefCell<PUi> = RefCell::new(PUi::default());
}

#[derive(Default)]
struct PUi { hot: i32, pause: RECT, cancel: RECT, speed: f64, last_bytes: u64, last: Option<Instant> }

fn current() -> Option<Arc<Job>> { CUR.lock().unwrap().clone() }


pub fn show_progress(owner: HWND) {
    let old = PROGRESS.get();
    if old != 0 && unsafe { IsWindow(old as HWND) } != 0 { return; }
    unsafe {
        register_class();
        let mut r = RECT::default();
        GetWindowRect(owner, &mut r);
        let (wd, ht) = (ui::sc(520), ui::sc(252));
        let x = r.left + ((r.right - r.left) - wd) / 2;
        let y = r.top + ((r.bottom - r.top) - ht) / 2;
        let title = w(t!("Операция с файлами", "File operation", ja: "ファイル操作", pt: "Operação de arquivo", zh: "文件操作"));
        let hwnd = CreateWindowExW(0, w(PROG_CLASS).as_ptr(), title.as_ptr(), WS_POPUP | WS_CAPTION | WS_SYSMENU, x, y, wd, ht,
            owner, std::ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
        if hwnd.is_null() { return; }
        PROGRESS.set(hwnd as isize);
        ui::dark_title(hwnd);
        ShowWindow(hwnd, SW_SHOWNA);
        SetTimer(hwnd, 1, 150, None);
    }
}

fn hide_progress() {
    let h = PROGRESS.with(|p| p.replace(0));
    if h != 0 && unsafe { IsWindow(h as HWND) } != 0 { unsafe { DestroyWindow(h as HWND); } }
}

fn register_class() {
    thread_local! { static REG: Cell<bool> = const { Cell::new(false) }; }
    REG.with(|r| {
        if r.get() { return; }
        unsafe {
            let mut c: WNDCLASSEXW = std::mem::zeroed();
            let cls = w(PROG_CLASS);
            c.cbSize = size_of::<WNDCLASSEXW>() as u32;
            c.lpfnWndProc = Some(progress_proc);
            c.hInstance = GetModuleHandleW(ptr::null());
            c.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
            c.lpszClassName = cls.as_ptr();
            RegisterClassExW(&c);
        }
        r.set(true);
    });
}

fn fmt_left(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 { format!("{} h {} min", s / 3600, s % 3600 / 60) }
    else if s >= 60 { format!("{} min {} s", s / 60, s % 60) }
    else { format!("{s} s") }
}

unsafe extern "system" fn progress_proc(h: HWND, m: u32, wp: usize, lp: isize) -> isize {
    match m {
        WM_ERASEBKGND => 1,
        WM_PAINT => { paint_progress(h); 0 }
        WM_TIMER => {
            InvalidateRect(h, ptr::null(), 0);
            if let Some(j) = current() {
                let mut cur = [0u16; 64];
                let n = GetWindowTextW(h, cur.as_mut_ptr(), cur.len() as i32).max(0) as usize;
                let have = String::from_utf16_lossy(&cur[..n]);
                let want = kind_word(j.kind);
                if have != want { SetWindowTextW(h, w(want).as_ptr()); }
            }
            0
        }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            let hot = PUI.with_borrow(|u| if inside(&u.pause, x, y) { 1 } else if inside(&u.cancel, x, y) { 2 } else { 0 });
            if PUI.with_borrow(|u| u.hot) != hot { PUI.with_borrow_mut(|u| u.hot = hot); InvalidateRect(h, ptr::null(), 0); }
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lp);
            let which = PUI.with_borrow(|u| if inside(&u.pause, x, y) { 1 } else if inside(&u.cancel, x, y) { 2 } else { 0 });
            if which == 1 {
                if let Some(j) = current() { if j.kind != Kind::Recycle { let p = !j.paused.load(Relaxed); j.paused.store(p, Relaxed); } }
            } else if which == 2 {
                cancel_all();
            }
            InvalidateRect(h, ptr::null(), 0);
            0
        }
        WM_CLOSE => { cancel_all(); 0 }
        WM_DESTROY => { KillTimer(h, 1); 0 }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

fn xy(lp: isize) -> (i32, i32) { ((lp & 0xffff) as u16 as i16 as i32, ((lp >> 16) & 0xffff) as u16 as i16 as i32) }

fn paint_progress(h: HWND) {
    let p = ui::pal();
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(h, &mut ps);
        let mut r = RECT::default();
        GetClientRect(h, &mut r);
        let (w, ht) = (r.right, r.bottom);
        let mem = CreateCompatibleDC(dc);
        let buf = CreateCompatibleBitmap(dc, w, ht);
        let old = SelectObject(mem, buf);
        SetBkMode(mem, TRANSPARENT as i32);
        let all = RECT { left: 0, top: 0, right: w, bottom: ht };
        let bg = CreateSolidBrush(p.card);
        FillRect(mem, &all, bg);
        DeleteObject(bg as HGDIOBJ);

        let pad = ui::sc(18);
        let f_title = ui::font(16, 600, ui::ui_face());
        let f_body = ui::font(13, 400, ui::ui_face());
        let f_small = ui::font(12, 400, ui::ui_face());
        let job = current();
        let (kind, top, from, to) = job.as_ref().map_or((Kind::Copy, 0, String::new(), String::new()), |j| (j.kind, j.top, j.from.clone(), j.to.clone()));
        let (b, tb, f, tf_) = job.as_ref().map_or((0, 0, 0, 0), |j| (j.bytes.load(Relaxed), j.total_bytes.load(Relaxed), j.files.load(Relaxed), j.total_files.load(Relaxed)));
        let paused = job.as_ref().is_some_and(|j| j.paused.load(Relaxed));
        let canceling = job.as_ref().is_some_and(|j| j.cancel.load(Relaxed));

        // speed (bytes per second, smoothed)
        let now = Instant::now();
        let mut speed = 0.0;
        PUI.with_borrow_mut(|u| {
            if let Some(last) = u.last {
                let dt = now.duration_since(last).as_secs_f64().max(0.001);
                let inst = b.saturating_sub(u.last_bytes) as f64 / dt;
                u.speed = if u.speed == 0.0 { inst } else { u.speed * 0.55 + inst * 0.45 };
                speed = u.speed;
            }
            u.last = Some(now);
            u.last_bytes = b;
        });
        if paused || canceling { speed = 0.0; }

        SelectObject(mem, f_title as HGDIOBJ);
        let title = if top > 0 { format!("{} · {}", kind_word(kind), tf!("элементов: {}", "items: {}", ja: "項目: {}", pt: "itens: {}", zh: "项目：{}", top)) }
            else { kind_word(kind).to_string() };
        ui::text(mem, &title, RECT { left: pad, top: ui::sc(12), right: w - pad, bottom: ui::sc(36) }, p.text, DT_LEFT | DT_VCENTER);
        SelectObject(mem, f_small as HGDIOBJ);
        let sub = if to.is_empty() { from } else { format!("{}  \u{2192}  {}", from, to) };
        if !sub.is_empty() {
            ui::text(mem, &sub, RECT { left: pad, top: ui::sc(36), right: w - pad, bottom: ui::sc(54) }, p.dim, DT_LEFT | DT_VCENTER);
        }

        // bar
        let bar = RECT { left: pad, top: ui::sc(60), right: w - pad, bottom: ui::sc(68) };
        ui::fill(mem, bar, p.line, ui::sc(4));
        if tb > 0 || tf_ > 0 {
            let frac = if tb > 0 { (b as f64 / tb as f64).clamp(0.0, 1.0) } else { (f as f64 / tf_.max(1) as f64).clamp(0.0, 1.0) };
            if frac > 0.001 {
                let fw = ((bar.right - bar.left) as f64 * frac).max(2.0) as i32;
                ui::fill(mem, RECT { left: bar.left, top: bar.top, right: bar.left + fw, bottom: bar.bottom }, p.accent, ui::sc(4));
            }
        } else if job.is_some() {
            let span = (bar.right - bar.left) as f64;
            let pos = (GetTickCount64() as f64 / 1200.0).fract();
            let seg = span * 0.22;
            let x = bar.left as f64 + (span + seg) * pos - seg;
            let (x0, x1) = ((x.max(bar.left as f64)) as i32, ((x + seg).min(bar.right as f64)) as i32);
            if x1 > x0 { ui::fill(mem, RECT { left: x0, top: bar.top, right: x1, bottom: bar.bottom }, p.accent, ui::sc(4)); }
        }

        // stats
        SelectObject(mem, f_body as HGDIOBJ);
        let mut line = if tb > 0 {
            format!("{} / {} ({}%)  \u{00B7}  {}", size_text(b), size_text(tb), b * 100 / tb.max(1), tf!("файлов: {} из {}", "{} of {} items", ja: "{} / {} 項目", pt: "{} de {} itens", zh: "{} / {} 项", f, tf_))
        } else if tf_ > 0 {
            tf!("файлов: {} из {}", "{} of {} items", ja: "{} / {} 項目", pt: "{} de {} itens", zh: "{} / {} 项", f, tf_)
        } else if kind == Kind::Recycle { format!("{}…", kind_word(kind)) } else { String::new() };
        if canceling { line = t!("Отмена…", "Cancelling…", ja: "キャンセル中…", pt: "Cancelando…", zh: "正在取消…").to_string(); }
        else if paused { line = format!("{}  \u{00B7}  {}", line, t!("Пауза", "Paused", ja: "一時停止", pt: "Pausado", zh: "已暂停")); }
        else if speed > 1.0 {
            line.push_str(&format!("  \u{00B7}  {}/s", size_text(speed as u64)));
            if tb > b { line.push_str(&format!("  \u{00B7}  ~{}", fmt_left((tb - b) as f64 / speed))); }
        }
        ui::text(mem, &line, RECT { left: pad, top: ui::sc(72), right: w - pad, bottom: ui::sc(94) }, p.text, DT_LEFT | DT_VCENTER);
        SelectObject(mem, f_small as HGDIOBJ);
        let cur = job.as_ref().map_or(String::new(), |j| j.current.lock().unwrap().clone());
        if !cur.is_empty() {
            ui::text(mem, &cur, RECT { left: pad, top: ui::sc(94), right: w - pad, bottom: ui::sc(114) }, p.dim, DT_LEFT | DT_VCENTER);
        }
        let queued = QUEUE.lock().unwrap().len();
        if queued > 0 {
            let q = tf!("ещё {} в очереди", "{} more queued", ja: "あと {} 件", pt: "mais {} na fila", zh: "还有 {} 项在排队", queued);
            ui::text(mem, &q, RECT { left: pad, top: ui::sc(114), right: w - pad, bottom: ui::sc(132) }, p.dim, DT_LEFT | DT_VCENTER);
        }

        // buttons
        let (bw, bh) = (ui::sc(96), ui::sc(30));
        let by = ht - ui::sc(46);
        let cancel = RECT { left: w - pad - bw, top: by, right: w - pad, bottom: by + bh };
        let pause = RECT { left: cancel.left - ui::sc(10) - bw, top: by, right: cancel.left - ui::sc(10), bottom: by + bh };
        let can_pause = job.as_ref().is_some_and(|j| j.kind != Kind::Recycle) && !canceling;
        let hot = PUI.with_borrow(|u| u.hot);
        button(mem, pause, if paused { t!("Продолжить", "Resume", ja: "再開", pt: "Retomar", zh: "继续") } else { t!("Пауза", "Pause", ja: "一時停止", pt: "Pausar", zh: "暂停") }, can_pause, hot == 1, false, &p, f_body);
        button(mem, cancel, if canceling { t!("Отмена…", "Cancelling…", ja: "キャンセル中…", pt: "Cancelando…", zh: "正在取消…") } else { t!("Отмена", "Cancel", ja: "キャンセル", pt: "Cancelar", zh: "取消") }, !canceling, hot == 2, false, &p, f_body);
        PUI.with_borrow_mut(|u| { u.pause = pause; u.cancel = cancel; });

        BitBlt(dc, 0, 0, w, ht, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(buf as HGDIOBJ);
        DeleteDC(mem);
        for f in [f_title, f_body, f_small] { DeleteObject(f as HGDIOBJ); }
        EndPaint(h, &ps);
    }
}

unsafe fn button(dc: HDC, r: RECT, label: &str, enabled: bool, hot: bool, primary: bool, p: &ui::Palette, font: isize) {
    let (bg, fg) = if !enabled { (p.card, ui::off()) } else if primary { (p.accent, 0x00FFFFFF) } else if hot { (p.active, p.text) } else { (p.pill, p.text) };
    ui::fill(dc, r, bg, ui::sc(4));
    let pen = CreatePen(PS_SOLID as i32, 1, if primary { bg } else { p.pill_line });
    let ob = SelectObject(dc, GetStockObject(NULL_BRUSH));
    let op = SelectObject(dc, pen as HGDIOBJ);
    RoundRect(dc, r.left, r.top, r.right, r.bottom, ui::sc(8), ui::sc(8));
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(pen as HGDIOBJ);
    SelectObject(dc, font as HGDIOBJ);
    ui::text(dc, label, r, fg, DT_CENTER | DT_VCENTER);
}

fn inside(r: &RECT, x: i32, y: i32) -> bool { x >= r.left && x < r.right && y >= r.top && y < r.bottom }

// ---------- conflict page ----------

const CONFLICT_CLASS: &str = "WinRustConflict";
thread_local! {
    static CREQ: Cell<isize> = const { Cell::new(0) };
    static CWIN: Cell<isize> = const { Cell::new(0) };
    static CBUTTONS: RefCell<[RECT; 4]> = const { RefCell::new([RECT { left: 0, top: 0, right: 0, bottom: 0 }; 4]) };
    static CCHECK: Cell<RECT> = const { Cell::new(RECT { left: 0, top: 0, right: 0, bottom: 0 }) };
    static CHOT: Cell<i32> = const { Cell::new(0) };
    static CALL: Cell<bool> = const { Cell::new(false) };
    static CCHOICE: Cell<Choice> = const { Cell::new(Choice::Cancel) };
    static CDONE: Cell<bool> = const { Cell::new(false) };
}

/// Shows the conflict page for `req` and leaves the answer in it (UI thread; the worker waits in SendMessage).
pub fn conflict(h: HWND, req: *mut Conflict) {
    unsafe {
        if req.is_null() { return; }
        CREQ.set(req as isize);
        CALL.set(false); CCHOICE.set(Choice::Cancel); CDONE.set(false);
        register_conflict();
        let (wd, ht) = (ui::sc(540), ui::sc(280));
        let mut r = RECT::default();
        GetWindowRect(h, &mut r);
        let x = r.left + ((r.right - r.left) - wd) / 2;
        let y = r.top + ((r.bottom - r.top) - ht) / 2;
        let title = w(t!("Заменить или пропустить файлы", "Replace or skip files", ja: "ファイルを置き換えるかスキップします", pt: "Substituir ou ignorar arquivos", zh: "替换或跳过文件"));
        let hwnd = CreateWindowExW(0, w(CONFLICT_CLASS).as_ptr(), title.as_ptr(), WS_POPUP | WS_CAPTION | WS_SYSMENU, x, y, wd, ht,
            h, std::ptr::null_mut(), GetModuleHandleW(ptr::null()), ptr::null());
        if hwnd.is_null() { CREQ.set(0); return; }
        CWIN.set(hwnd as isize);
        ui::dark_title(hwnd);
        EnableWindow(h, 0);
        let prog = PROGRESS.get();
        if prog != 0 { EnableWindow(prog as HWND, 0); }
        ShowWindow(hwnd, SW_SHOW);
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
            if CDONE.get() { break; }
        }
        let c = &mut *(req);
        c.choice = CCHOICE.get();
        c.all = CALL.get();
        if IsWindow(hwnd) != 0 { DestroyWindow(hwnd); }
        CWIN.set(0);
        CREQ.set(0);
        EnableWindow(h, 1);
        if prog != 0 && IsWindow(prog as HWND) != 0 { EnableWindow(prog as HWND, 1); }
        SetForegroundWindow(h);
        SetFocus(h);
    }
}

fn register_conflict() {
    thread_local! { static REG: Cell<bool> = const { Cell::new(false) }; }
    REG.with(|r| {
        if r.get() { return; }
        unsafe {
            let mut c: WNDCLASSEXW = std::mem::zeroed();
            let cls = w(CONFLICT_CLASS);
            c.cbSize = size_of::<WNDCLASSEXW>() as u32;
            c.lpfnWndProc = Some(conflict_proc);
            c.hInstance = GetModuleHandleW(ptr::null());
            c.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
            c.lpszClassName = cls.as_ptr();
            RegisterClassExW(&c);
        }
        r.set(true);
    });
}

fn answer(choice: Choice) {
    CCHOICE.set(choice);
    CDONE.set(true);
    let h = CWIN.get();
    if h != 0 { unsafe { PostMessageW(h as HWND, WM_NULL, 0, 0); } }
}

unsafe extern "system" fn conflict_proc(h: HWND, m: u32, wp: usize, lp: isize) -> isize {
    match m {
        WM_ERASEBKGND => 1,
        WM_PAINT => { paint_conflict(h); 0 }
        WM_MOUSEMOVE => {
            let (x, y) = xy(lp);
            let hot = if CDONE.get() { 0 } else {
                let mut which = 0;
                CBUTTONS.with(|b| for (i, r) in b.borrow().iter().enumerate() { if inside(r, x, y) { which = i as i32 + 1; } });
                which
            };
            if CHOT.get() != hot { CHOT.set(hot); InvalidateRect(h, ptr::null(), 0); }
            0
        }
        WM_LBUTTONDOWN => {
            let (x, y) = xy(lp);
            if CDONE.get() { return 0; }
            let hit = CBUTTONS.with(|b| b.borrow().iter().position(|r| inside(r, x, y)));
            match hit {
                Some(0) => answer(Choice::Replace),
                Some(1) => answer(Choice::KeepBoth),
                Some(2) => answer(Choice::Skip),
                Some(_) => answer(Choice::Cancel),
                None => if inside(&CCHECK.with(|r| r.get()), x, y) { CALL.set(!CALL.get()); InvalidateRect(h, ptr::null(), 0); },
            }
            0
        }
        WM_KEYDOWN => {
            match wp as u16 {
                VK_ESCAPE => answer(Choice::Cancel),
                VK_RETURN => answer(Choice::Replace),
                x if x == b'S' as u16 => answer(Choice::Skip),
                x if x == b'K' as u16 => answer(Choice::KeepBoth),
                _ => {}
            }
            0
        }
        WM_CLOSE => { answer(Choice::Cancel); 0 }
        _ => DefWindowProcW(h, m, wp, lp),
    }
}

fn paint_conflict(h: HWND) {
    let p = ui::pal();
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(h, &mut ps);
        let mut r = RECT::default();
        GetClientRect(h, &mut r);
        let (w, ht) = (r.right, r.bottom);
        let mem = CreateCompatibleDC(dc);
        let buf = CreateCompatibleBitmap(dc, w, ht);
        let old = SelectObject(mem, buf);
        SetBkMode(mem, TRANSPARENT as i32);
        let all = RECT { left: 0, top: 0, right: w, bottom: ht };
        let bg = CreateSolidBrush(p.card);
        FillRect(mem, &all, bg);
        DeleteObject(bg as HGDIOBJ);
        let pad = ui::sc(20);
        let f_head = ui::font(15, 600, ui::ui_face());
        let f_body = ui::font(13, 400, ui::ui_face());
        let f_small = ui::font(12, 400, ui::ui_face());

        let req = CREQ.get() as *const Conflict;
        if req.is_null() { EndPaint(h, &ps); return; }
        let c = &*req;
        SelectObject(mem, f_head as HGDIOBJ);
        let head = if c.src_dir || c.dst_dir {
            tf!("Папка с именем \"{}\" уже существует", "A folder named \"{}\" already exists", ja: "「{}」という名前のフォルダーは既に存在します", pt: "Já existe uma pasta chamada \"{}\"", zh: "已存在名为“{}”的文件夹", c.name)
        } else {
            tf!("Файл с именем \"{}\" уже существует", "A file named \"{}\" already exists", ja: "「{}」という名前のファイルは既に存在します", pt: "Já existe um arquivo chamado \"{}\"", zh: "已存在名为“{}”的文件", c.name)
        };
        ui::text(mem, &head, RECT { left: pad, top: ui::sc(14), right: w - pad, bottom: ui::sc(44) }, p.text, DT_LEFT | DT_VCENTER);
        SelectObject(mem, f_body as HGDIOBJ);
        let size = |sdir: bool, sz: u64, t: u64| format!("{}  \u{00B7}  {}", if sdir { t!("папка", "folder", ja: "フォルダー", pt: "pasta", zh: "文件夹").to_string() } else { size_text(sz) }, date_text(t));
        let l1 = format!("{}  {}", t!("Источник:", "Source:", ja: "コピー元:", pt: "Origem:", zh: "源文件："), size(c.src_dir, c.src_size, c.src_time));
        let l2 = format!("{}  {}", t!("Назначение:", "Destination:", ja: "コピー先:", pt: "Destino:", zh: "目标文件："), size(c.dst_dir, c.dst_size, c.dst_time));
        ui::text(mem, &l1, RECT { left: pad, top: ui::sc(48), right: w - pad, bottom: ui::sc(68) }, p.dim, DT_LEFT | DT_VCENTER);
        SelectObject(mem, f_small as HGDIOBJ);
        ui::text(mem, &c.src, RECT { left: pad, top: ui::sc(68), right: w - pad, bottom: ui::sc(86) }, ui::off(), DT_LEFT | DT_VCENTER);
        SelectObject(mem, f_body as HGDIOBJ);
        ui::text(mem, &l2, RECT { left: pad, top: ui::sc(88), right: w - pad, bottom: ui::sc(108) }, p.dim, DT_LEFT | DT_VCENTER);
        SelectObject(mem, f_small as HGDIOBJ);
        ui::text(mem, &c.dst, RECT { left: pad, top: ui::sc(108), right: w - pad, bottom: ui::sc(126) }, ui::off(), DT_LEFT | DT_VCENTER);

        // "do this for all"
        let bx = RECT { left: pad, top: ui::sc(138), right: pad + ui::sc(16), bottom: ui::sc(154) };
        ui::fill(mem, bx, if CALL.get() { p.accent } else { p.pill }, ui::sc(3));
        let pen = CreatePen(PS_SOLID as i32, 1, p.pill_line);
        let ob = SelectObject(mem, GetStockObject(NULL_BRUSH));
        let op = SelectObject(mem, pen as HGDIOBJ);
        RoundRect(mem, bx.left, bx.top, bx.right, bx.bottom, ui::sc(6), ui::sc(6));
        SelectObject(mem, op);
        SelectObject(mem, ob);
        DeleteObject(pen as HGDIOBJ);
        if CALL.get() {
            SelectObject(mem, f_small as HGDIOBJ);
            ui::text(mem, "\u{E73E}", bx, 0x00FFFFFF, DT_CENTER | DT_VCENTER);
        }
        CCHECK.set(RECT { left: bx.left - ui::sc(4), top: bx.top - ui::sc(6), right: w - pad, bottom: bx.bottom + ui::sc(6) });
        SelectObject(mem, f_small as HGDIOBJ);
        ui::text(mem, t!("Применить ко всем конфликтам", "Do this for all conflicts", ja: "すべての競合に適用", pt: "Fazer isso para todos os conflitos", zh: "对全部冲突执行此操作"),
            RECT { left: bx.right + ui::sc(8), top: bx.top - ui::sc(6), right: w - pad, bottom: bx.bottom + ui::sc(6) }, p.text, DT_LEFT | DT_VCENTER);

        // buttons
        SelectObject(mem, f_body as HGDIOBJ);
        let (bw, bh) = (ui::sc(108), ui::sc(32));
        let gap = ui::sc(8);
        let y = ht - ui::sc(50);
        let labels = [
            (t!("Заменить", "Replace", ja: "置き換える", pt: "Substituir", zh: "替换"), true),
            (t!("Сохранить оба", "Keep both", ja: "両方を保持", pt: "Manter ambos", zh: "保留两者"), false),
            (t!("Пропустить", "Skip", ja: "スキップ", pt: "Ignorar", zh: "跳过"), false),
            (t!("Отмена", "Cancel", ja: "キャンセル", pt: "Cancelar", zh: "取消"), false),
        ];
        let total = bw * 4 + gap * 3;
        let mut x = w - pad - total;
        let hot = CHOT.get();
        for (i, (label, primary)) in labels.iter().enumerate() {
            let rect = RECT { left: x, top: y, right: x + bw, bottom: y + bh };
            button(mem, rect, label, true, hot == i as i32 + 1, *primary, &p, f_body);
            CBUTTONS.with(|b| b.borrow_mut()[i] = RECT { left: rect.left, top: rect.top - ui::sc(20), right: rect.right, bottom: rect.bottom + ui::sc(20) });
            x += bw + gap;
        }

        BitBlt(dc, 0, 0, w, ht, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(buf as HGDIOBJ);
        DeleteDC(mem);
        for f in [f_head, f_body, f_small] { DeleteObject(f as HGDIOBJ); }
        EndPaint(h, &ps);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rx-ops-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(kind: Kind) -> Arc<Job> {
        Arc::new(Job::new(&Task { kind, from: vec![], to: PathBuf::new(), undo: false }, 0))
    }

    #[test]
    fn copies_and_says_what_to_undo() {
        let d = tmp("copy");
        fs::create_dir_all(d.join("src/a")).unwrap();
        fs::write(d.join("src/one.txt"), "hello").unwrap();
        fs::write(d.join("src/a/two.txt"), "two").unwrap();
        fs::create_dir(d.join("dst")).unwrap();
        let done = run(&Task { kind: Kind::Copy, from: vec![d.join("src")], to: d.join("dst"), undo: false }, &job(Kind::Copy));
        assert!(done.errors.is_empty() && !done.cancelled);
        assert_eq!(fs::read_to_string(d.join("dst/src/a/two.txt")).unwrap(), "two");
        let u = done.undo.unwrap();
        assert_eq!(u.created, vec![d.join("dst/src")]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn conflicts_follow_the_answer_for_all() {
        let d = tmp("conflict");
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("dst")).unwrap();
        fs::write(d.join("src/x.txt"), "new").unwrap();
        fs::write(d.join("dst/x.txt"), "old").unwrap();
        let j = job(Kind::Copy);
        *j.apply_all.lock().unwrap() = Some(Choice::Replace);
        let done = run(&Task { kind: Kind::Copy, from: vec![d.join("src/x.txt")], to: d.join("dst"), undo: false }, &j);
        assert!(done.errors.is_empty());
        assert_eq!(fs::read_to_string(d.join("dst/x.txt")).unwrap(), "new");
        fs::write(d.join("dst/x.txt"), "old2").unwrap();
        let j = job(Kind::Copy);
        *j.apply_all.lock().unwrap() = Some(Choice::KeepBoth);
        run(&Task { kind: Kind::Copy, from: vec![d.join("src/x.txt")], to: d.join("dst"), undo: false }, &j);
        assert_eq!(fs::read_to_string(d.join("dst/x (2).txt")).unwrap(), "new");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn moves_take_the_source_away() {
        let d = tmp("move");
        fs::create_dir_all(d.join("src/sub")).unwrap();
        fs::write(d.join("src/sub/a.txt"), "a").unwrap();
        fs::create_dir(d.join("dst")).unwrap();
        let done = run(&Task { kind: Kind::Move, from: vec![d.join("src")], to: d.join("dst"), undo: false }, &job(Kind::Move));
        assert!(done.errors.is_empty(), "{:?}", done.errors);
        assert!(!d.join("src").exists() && d.join("dst/src/sub/a.txt").is_file());
        let u = done.undo.unwrap();
        assert!(u.created.is_empty());
        assert!(u.moved.iter().any(|(s, t)| s.ends_with("a.txt") && t.ends_with("a.txt")), "{:?}", u.moved);
        let _ = fs::remove_dir_all(&d);
    }
}
