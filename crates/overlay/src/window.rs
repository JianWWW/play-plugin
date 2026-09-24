//! Win32 overlay window plumbing: window class, creation, browser-owner
//! discovery, message pump helpers.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, EnumWindows, GetWindowRect, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, RegisterClassW, SetWindowLongPtrW, ShowWindow, GWL_HWNDPARENT,
    SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

pub const OVERLAY_CLASS: PCWSTR = w!("PlayPluginOverlay");

/// Browser processes eligible to own overlay windows. `msedgewebview2` is
/// deliberately absent — webview hosts are not browser windows.
pub const BROWSER_EXES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "chromium.exe",
];

/// Host-side window-proc hook: return `Some(value)` to short-circuit
/// `DefWindowProcW`. Installed once by the overlay host thread before any
/// window exists.
pub type WndProcFn = fn(HWND, u32, WPARAM, LPARAM) -> Option<isize>;
pub static WNDPROC_TARGET: Mutex<Option<WndProcFn>> = Mutex::new(None);

static FOUND_OWNER: AtomicIsize = AtomicIsize::new(0);
static TARGET_X: AtomicIsize = AtomicIsize::new(0);
static TARGET_Y: AtomicIsize = AtomicIsize::new(0);

pub fn register_overlay_class() -> Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: windows::Win32::Foundation::HINSTANCE(hinstance.0),
            lpszClassName: OVERLAY_CLASS,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
    }
    Ok(())
}

pub fn create_overlay_window(
    title: PCWSTR,
    owner: Option<HWND>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<HWND> {
    unsafe {
        let hmodule = GetModuleHandleW(None)?;
        let hinstance = windows::Win32::Foundation::HINSTANCE(hmodule.0);
        // Owned windows keep their z-order above the browser but below other
        // applications' windows; unowned (smoke tests) stay topmost explicitly.
        let mut ex = WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        if owner.is_none() {
            ex |= WS_EX_TOPMOST;
        }
        CreateWindowExW(
            ex,
            OVERLAY_CLASS,
            title,
            WS_POPUP,
            x,
            y,
            w.max(1),
            h.max(1),
            owner.unwrap_or_default(),
            None,
            hinstance,
            None,
        )
    }
}

pub fn set_owner(hwnd: HWND, owner: HWND) {
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_HWNDPARENT, owner.0 as isize);
    }
}

pub fn show_noactivate(hwnd: HWND, show: bool) {
    unsafe {
        let _ = ShowWindow(hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
}

/// Finds the browser top-level window that visually contains the point
/// `(x, y)` (physical pixels). Windows are enumerated in z-order (top first),
/// so the first match is the visible browser the user is looking at.
pub fn find_browser_owner(x: i32, y: i32) -> Option<HWND> {
    FOUND_OWNER.store(0, Ordering::SeqCst);
    TARGET_X.store(x as isize, Ordering::SeqCst);
    TARGET_Y.store(y as isize, Ordering::SeqCst);
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }
    let hwnd = FOUND_OWNER.load(Ordering::SeqCst);
    if hwnd == 0 {
        None
    } else {
        Some(HWND(hwnd as *mut _))
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    let tx = TARGET_X.load(Ordering::SeqCst) as i32;
    let ty = TARGET_Y.load(Ordering::SeqCst) as i32;
    if tx < rect.left || tx > rect.right || ty < rect.top || ty > rect.bottom {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == std::process::id() {
        return BOOL(1);
    }
    if let Some(exe) = process_image_name(pid) {
        let exe = exe.to_ascii_lowercase();
        if BROWSER_EXES.iter().any(|b| b.eq_ignore_ascii_case(&exe)) {
            FOUND_OWNER.store(hwnd.0 as isize, Ordering::SeqCst);
            return BOOL(0); // stop enumeration
        }
    }
    BOOL(1)
}

fn process_image_name(pid: u32) -> Option<String> {
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        ok.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handler = WNDPROC_TARGET.lock().unwrap().as_ref().copied();
    if let Some(f) = handler {
        if let Some(r) = f(hwnd, msg, wparam, lparam) {
            return LRESULT(r);
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
