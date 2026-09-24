//! Tray icon with a minimal menu (Open config / Logs / Check updates / Exit).

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HMENU, HWND_BOTTOM,
    IDI_APPLICATION, MF_STRING, MSG, SW_HIDE, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_APP, WM_COMMAND,
    WM_DESTROY, WM_RBUTTONUP, WNDCLASSW,
};

const WM_TRAYICON: u32 = WM_APP + 1;
const ID_EXIT: u32 = 1001;
const ID_CONFIG: u32 = 1002;
const ID_LOGS: u32 = 1003;
const TRAY_ID: u32 = 1;

struct TrayState {
    port: u16,
    data_dir: std::path::PathBuf,
}

static TRAY_STATE: std::sync::Mutex<Option<TrayState>> = std::sync::Mutex::new(None);

pub fn spawn(port: u16, data_dir: std::path::PathBuf, exit_tx: std::sync::mpsc::Sender<()>) {
    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || unsafe {
            *TRAY_STATE.lock().unwrap() = Some(TrayState { port, data_dir });
            let result = run();
            let _ = exit_tx.send(());
            if let Err(e) = result {
                tracing::error!(error = %e, "tray loop failed");
            }
        })
        .expect("spawn tray thread");
}

unsafe fn run() -> windows::core::Result<()> {
    let hmodule = GetModuleHandleW(None)?;
    let class_name = w!("PlayPluginTray");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: windows::Win32::Foundation::HINSTANCE(hmodule.0),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&wc);

    // Marker so the wndproc knows which exit signal to send (single tray).
    let hwnd = CreateWindowExW(
        windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
        class_name,
        w!("PlayPlugin Tray"),
        windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        None,
        None,
        windows::Win32::Foundation::HINSTANCE(hmodule.0),
        None,
    )?;

    let icon: HICON = LoadIconW(
        windows::Win32::Foundation::HINSTANCE(hmodule.0),
        IDI_APPLICATION,
    )
    .unwrap_or_default();
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: icon,
        ..Default::default()
    };
    for (i, c) in format!(
        "PlayPlugin :{}\0",
        TRAY_STATE.lock().unwrap().as_ref().unwrap().port
    )
    .encode_utf16()
    .enumerate()
    .take(nid.szTip.len() - 1)
    {
        nid.szTip[i] = c;
    }
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // Cleanup
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    let _ = DestroyWindow(hwnd);
    Ok(())
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAYICON && (lparam.0 & 0xFFFF) as u32 == WM_RBUTTONUP {
        // Right-click on the tray icon → context menu.
        let _ = SetForegroundWindow(hwnd);
        let menu = CreatePopupMenu().unwrap_or_default();
        let state = TRAY_STATE.lock().unwrap();
        let data_dir = state.as_ref().map(|s| s.data_dir.clone());
        let _port = state.as_ref().map(|s| s.port);
        drop(state);
        if let Some(_dir) = &data_dir {
            let label: Vec<u16> = "Open config folder\0".encode_utf16().collect();
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                ID_CONFIG as usize,
                windows::core::PCWSTR(label.as_ptr()),
            );
            let label: Vec<u16> = "Open logs folder\0".encode_utf16().collect();
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                ID_LOGS as usize,
                windows::core::PCWSTR(label.as_ptr()),
            );
        }
        let label: Vec<u16> = "Exit\0".encode_utf16().collect();
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            ID_EXIT as usize,
            windows::core::PCWSTR(label.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        destroy_menu(menu);
        return LRESULT(0);
    }
    if msg == WM_COMMAND {
        let id = (wparam.0 & 0xFFFF) as u32;
        let dir = TRAY_STATE
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.data_dir.clone());
        match id {
            ID_CONFIG => {
                if let Some(dir) = dir {
                    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                }
            }
            ID_LOGS => {
                if let Some(dir) = dir {
                    let _ = std::process::Command::new("explorer")
                        .arg(dir.join("logs"))
                        .spawn();
                }
            }
            ID_EXIT => {
                PostQuitMessage(0);
            }
            _ => {}
        }
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn destroy_menu(menu: HMENU) {
    let _ = DestroyMenu(menu);
}

// Keep imports that are referenced indirectly.
#[allow(unused)]
fn _refs() {
    let _ = HWND_BOTTOM;
    let _ = SW_HIDE;
}
