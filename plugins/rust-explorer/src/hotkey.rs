//! Win+E. Windows' shell owns the combination (RegisterHotKey fails with ERROR_HOTKEY_ALREADY_REGISTERED), so the only way
//! in is a low-level keyboard hook, and a hook needs a live process: `rust-explorer --hotkey <real exe>` is a windowless
//! resident mode. Settings copies the current exe to %LOCALAPPDATA%\rust-explorer\rx-hotkey.exe, puts it in the per-user Run
//! key and starts it; it always starts the real exe, so rebuilding that one never needs the hook stopped.
use std::{
    path::PathBuf,
    ptr,
    sync::{atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed}, OnceLock},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Registry::*},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, keybd_event, KEYEVENTF_KEYUP, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT},
        WindowsAndMessaging::*,
    },
};

const CLASS: &str = "WinRustRxHotkey";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "rust-explorer-hotkey";
const FIRE: u32 = WM_APP + 1; // the hook -> the window: start the real exe

fn w(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }

static WIN: AtomicIsize = AtomicIsize::new(0);
static ARMED: AtomicBool = AtomicBool::new(false); // Win+E is down and was ours: its key-up must not reach the shell
static TARGET: OnceLock<PathBuf> = OnceLock::new();

/// `--hotkey <exe>`: stay in the background and start `<exe>` on Win+E. Returns at once when another copy is already running.
pub fn run(target: PathBuf) {
    unsafe {
        if !FindWindowW(w(CLASS).as_ptr(), ptr::null()).is_null() { return; } // one resident copy is enough
        let _ = TARGET.set(target);
        let inst = GetModuleHandleW(ptr::null());
        let cls = w(CLASS);
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc_hot), hInstance: inst, lpszClassName: cls.as_ptr(), ..std::mem::zeroed() });
        let h = CreateWindowExW(WS_EX_TOOLWINDOW, cls.as_ptr(), w("rust-explorer Win+E").as_ptr(), WS_POPUP, 0, 0, 0, 0, ptr::null_mut(), ptr::null_mut(), inst, ptr::null());
        if h.is_null() { return; }
        WIN.store(h as isize, Relaxed);
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb), inst, 0);
        if hook.is_null() { return; }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn kb(n: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if n >= 0 {
            let k = &*(l as *const KBDLLHOOKSTRUCT);
            if k.flags & LLKHF_INJECTED == 0 && k.vkCode == b'E' as u32 {
                let down = w == WM_KEYDOWN as usize || w == WM_SYSKEYDOWN as usize;
                if down {
                    let held = |vk: u16| GetAsyncKeyState(vk as i32) < 0;
                    if (held(VK_LWIN) || held(VK_RWIN)) && !held(VK_CONTROL) && !held(VK_MENU) && !held(VK_SHIFT) {
                        if !ARMED.swap(true, Relaxed) {
                            // E never reaches the shell, so a lone Win release would open Start: a key in between makes it Win+<key>
                            keybd_event(0xE8, 0, 0, 0);
                            keybd_event(0xE8, 0, KEYEVENTF_KEYUP, 0);
                            PostMessageW(WIN.load(Relaxed) as HWND, FIRE, 0, 0);
                        }
                        return 1;
                    }
                } else if ARMED.swap(false, Relaxed) {
                    return 1; // our Win+E: Explorer must not open its own window on the release
                }
            }
        }
        CallNextHookEx(ptr::null_mut(), n, w, l)
    }
}

unsafe extern "system" fn proc_hot(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match m {
            FIRE => { fire(); 0 }
            WM_CLOSE => { DestroyWindow(h); 0 }
            WM_DESTROY => { PostQuitMessage(0); 0 }
            _ => DefWindowProcW(h, m, wp, lp),
        }
    }
}

fn fire() {
    let Some(t) = TARGET.get() else { return };
    if let Ok(c) = std::process::Command::new(t).arg("--focus").spawn() {
        unsafe { AllowSetForegroundWindow(c.id()) };
    }
}

/// The Run entry exists, i.e. Win+E is ours from the next sign-in (and was started at the last one).
pub fn installed() -> bool { winrust_common::reg::exists(RUN_KEY, Some(RUN_NAME)) }

/// Settings switch: take Win+E over, or hand it back to Explorer.
pub fn toggle() -> bool {
    if installed() { disable(); false } else { enable() }
}

fn helper_path() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join(r"rust-explorer\rx-hotkey.exe")
}

fn enable() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let dst = helper_path();
    if let Some(d) = dst.parent() { let _ = std::fs::create_dir_all(d); }
    stop();
    // the old copy may still be locked for a moment after its process ends: move it aside, then write the new one
    if std::fs::copy(&exe, &dst).is_err() {
        let old = dst.with_file_name("rx-hotkey.old.exe");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&dst, &old);
        if std::fs::copy(&exe, &dst).is_err() { return false; }
    }
    if !set_run(Some(&format!("\"{}\" --hotkey \"{}\"", dst.display(), exe.display()))) { return false; }
    let _ = std::process::Command::new(&dst).arg("--hotkey").arg(&exe).spawn();
    true
}

fn disable() {
    stop();
    set_run(None);
}

/// Asks the resident copy to quit and waits for it (up to two seconds), so its exe copy is free to replace.
fn stop() {
    let cls = w(CLASS);
    for _ in 0..40 {
        unsafe {
            let h = FindWindowW(cls.as_ptr(), ptr::null());
            if h.is_null() { return; }
            PostMessageW(h, WM_CLOSE, 0, 0);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn set_run(v: Option<&str>) -> bool {
    unsafe {
        let mut k: HKEY = ptr::null_mut();
        let mut disp = 0u32;
        let rc = RegCreateKeyExW(HKEY_CURRENT_USER, w(r"Software\Microsoft\Windows\CurrentVersion\Run").as_ptr(), 0, ptr::null(), 0, KEY_SET_VALUE, ptr::null(), &mut k, &mut disp);
        if rc != 0 { return false; }
        let name = w(RUN_NAME);
        let ok = match v {
            Some(v) => {
                let u: Vec<u16> = v.encode_utf16().chain([0]).collect();
                RegSetValueExW(k, name.as_ptr(), 0, REG_SZ, u.as_ptr() as *const u8, (u.len() * 2) as u32) == 0
            }
            None => {
                let rc = RegDeleteValueW(k, name.as_ptr());
                rc == 0 || GetLastError() == ERROR_FILE_NOT_FOUND
            }
        };
        RegCloseKey(k);
        ok
    }
}
