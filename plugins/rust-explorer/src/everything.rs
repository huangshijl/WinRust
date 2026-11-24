//! Everything search over its own IPC: a WM_COPYDATA query (QUERY2) to the running Everything, the reply Windows sends
//! back to a hidden window of ours. No SDK DLL needed; without Everything running the caller falls back.
//! Protocol: ipc/everything_ipc.h from the Everything SDK (packed structs, offsets from the list base).
#![allow(unsafe_op_in_unsafe_fn)]
use std::{
    cell::RefCell,
    path::PathBuf,
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{
        DataExchange::COPYDATASTRUCT,
        LibraryLoader::GetModuleHandleW,
        Threading::Sleep,
    },
    UI::WindowsAndMessaging::*,
};

const CLASS: &str = "RxEverythingReply";
const QUERY2W: usize = 18; // EVERYTHING_IPC_COPYDATA_QUERY2W
const MAGIC: u32 = 0x5258_4531; // reply dwData: our own tag
const REQ_FULL_PATH: u32 = 0x0000_0004; // EVERYTHING_IPC_QUERY2_REQUEST_FULL_PATH_AND_NAME
const REQ_SIZE: u32 = 0x0000_0010;
const REQ_MODIFIED: u32 = 0x0000_0040;
const SORT_NAME_ASC: u32 = 1;

pub struct Hit {
    pub path: PathBuf,
    pub dir: bool,
    pub size: u64,
    pub modified: u64, // FILETIME, like fs::Metadata::last_write_time
}

thread_local! { static HITS: RefCell<Option<Vec<Hit>>> = const { RefCell::new(None) }; }

fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

/// The Everything client is a separate program from its service: start it in the background when it is installed but not
/// running, so the next query has something to talk to (this query falls back if it is still loading its database).
pub fn ensure_started() {
    unsafe {
        if !FindWindowW(w("EVERYTHING_TASKBAR_NOTIFICATION").as_ptr(), ptr::null()).is_null() { return; }
    }
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(dir) = std::env::var_os(var) { candidates.push(PathBuf::from(dir).join(r"Everything\Everything.exe")); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() { candidates.push(dir.join("Everything.exe")); }
    }
    if let Some(exe) = candidates.into_iter().find(|p| p.is_file()) {
        let _ = std::process::Command::new(exe).arg("-startup").spawn();
    }
}

/// Everything's index for `q`; None when it is not running or did not answer in time (the caller falls back).
pub fn search(q: &str, max: usize) -> Option<Vec<Hit>> {
    let q = q.to_string();
    std::thread::spawn(move || query(&q, max)).join().ok().flatten()
}

fn query(q: &str, max: usize) -> Option<Vec<Hit>> {
    unsafe {
        let everything = FindWindowW(w("EVERYTHING_TASKBAR_NOTIFICATION").as_ptr(), ptr::null());
        if everything.is_null() { return None; }
        HITS.with(|h| *h.borrow_mut() = None);
        let inst = GetModuleHandleW(ptr::null());
        let cls = w(CLASS);
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(reply_proc), hInstance: inst, lpszClassName: cls.as_ptr(), ..std::mem::zeroed() });
        let hwnd = CreateWindowExW(0, cls.as_ptr(), w("").as_ptr(), WS_POPUP, 0, 0, 0, 0, 0 as _, 0 as _, inst, ptr::null());
        if hwnd.is_null() { return None; }
        ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, ptr::null_mut());

        // EVERYTHING_IPC_QUERY2 is packed: 7 DWORDs, then the wide search string
        let mut buf = Vec::with_capacity(28 + (q.encode_utf16().count() + 1) * 2);
        for v in [hwnd as usize as u32, MAGIC, 0, 0, max.min(u32::MAX as usize) as u32, REQ_FULL_PATH | REQ_SIZE | REQ_MODIFIED, SORT_NAME_ASC] {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        for u in q.encode_utf16().chain([0]) { buf.extend_from_slice(&u.to_le_bytes()); }
        let mut cds = COPYDATASTRUCT { dwData: QUERY2W, cbData: buf.len() as u32, lpData: buf.as_mut_ptr() as *mut _ };
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut sent = 0usize;
        // a busy Everything (loading a big database) must not freeze the caller for minutes: give up and fall back
        SendMessageTimeoutW(everything, WM_COPYDATA, hwnd as usize, &mut cds as *mut _ as isize, 0x2 | 0x1, 1500, &mut sent);
        let mut msg: MSG = std::mem::zeroed();
        while !HITS.with(|h| h.borrow().is_some()) && Instant::now() < deadline {
            while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT { break; }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if HITS.with(|h| h.borrow().is_some()) { break; }
            Sleep(2);
        }
        DestroyWindow(hwnd);
        HITS.with(|h| h.borrow_mut().take())
    }
}

unsafe extern "system" fn reply_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if m == WM_COPYDATA {
            let cds = &*(lp as *const COPYDATASTRUCT);
            if cds.dwData == MAGIC as usize && !cds.lpData.is_null() {
                HITS.with(|h| *h.borrow_mut() = Some(parse(cds.lpData as *const u8, cds.cbData as usize)));
                PostQuitMessage(0);
                return 1;
            }
        }
        DefWindowProcW(h, m, wp, lp)
    }
}

/// EVERYTHING_IPC_LIST2 + ITEM2 entries; every offset is from the list base. The buffer is only valid during the message.
unsafe fn parse(base: *const u8, cb: usize) -> Vec<Hit> {
    unsafe {
        let u32_at = |o: usize| -> Option<u32> { (o + 4 <= cb).then(|| u32::from_le_bytes([*base.add(o), *base.add(o + 1), *base.add(o + 2), *base.add(o + 3)])) };
        let u64_at = |o: usize| -> Option<u64> { (o + 8 <= cb).then(|| u64::from_le_bytes(std::slice::from_raw_parts(base.add(o), 8).try_into().unwrap())) };
        let n = u32_at(4).unwrap_or(0) as usize; // numitems
        let req = u32_at(12).unwrap_or(0); // request_flags the reply actually carries
        if n == 0 || req & REQ_FULL_PATH == 0 { return Vec::new(); }
        let mut out = Vec::with_capacity(n.min(4096));
        for i in 0..n.min(4096) {
            let o = 20 + i * 8; // LIST2 header, then ITEM2 { flags, data_offset }
            let (Some(flags), Some(off)) = (u32_at(o), u32_at(o + 4)) else { break };
            let off = off as usize;
            let Some(len) = u32_at(off) else { break };
            let len = len as usize;
            let start = off + 4;
            let end = start + (len + 1) * 2;
            if len > 32768 || end > cb { break; }
            let s = std::slice::from_raw_parts(base.add(start) as *const u16, len);
            let path = String::from_utf16_lossy(s);
            let mut p = end;
            let size = if req & REQ_SIZE != 0 { let v = u64_at(p).unwrap_or(0); p += 8; v } else { 0 };
            let modified = if req & REQ_MODIFIED != 0 { u64_at(p).unwrap_or(0) } else { 0 };
            out.push(Hit { path: PathBuf::from(path), dir: flags & 1 != 0, size, modified });
        }
        out
    }
}
